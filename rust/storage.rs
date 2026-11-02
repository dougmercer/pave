use crate::index::writer;
use crate::tags::STRINGS;
use anyhow::{Result, ensure};
use rayon::prelude::*;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

pub const ROW_BYTES: usize = 98;
pub const VERTEX_COUNT: usize = ROW_BYTES - 8;
/// Everything in a row between `v` and the vertex count.
pub type Attributes = [u8; VERTEX_COUNT - 16];
/// Where each `STRINGS` code sits within `Attributes`.
pub const STRING_SLOTS: [usize; STRINGS.len()] = [30, 34, 38, 42, 46, 62, 66, 70];
pub const COLUMNS: [(&str, &str, usize, usize); 20] = [
    ("u", "<i8", 0, 8),
    ("v", "<i8", 8, 8),
    ("osmid", "<i8", 16, 8),
    ("width_m", "<f8", 24, 8),
    ("highway", "|u1", 32, 1),
    ("maxspeed_kmh", "<f4", 33, 4),
    ("maxweight_t", "<f4", 37, 4),
    ("maxheight_m", "<f4", 41, 4),
    ("flags", "|u1", 45, 1),
    ("access", "<i4", 46, 4),
    ("motor_vehicle", "<i4", 50, 4),
    ("hgv", "<i4", 54, 4),
    ("surface", "<i4", 58, 4),
    ("name", "<i4", 62, 4),
    ("length_m", "<f4", 66, 4),
    ("maxwidth_m", "<f4", 70, 4),
    ("maxlength_m", "<f4", 74, 4),
    ("vehicle", "<i4", 78, 4),
    ("barrier", "<i4", 82, 4),
    ("barrier_at_v", "<i4", 86, 4),
];

pub fn npy(path: &Path, dtype: &str, shape: &[u64]) -> Result<BufWriter<File>> {
    let shape = shape
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let mut header =
        format!("{{'descr': '{dtype}', 'fortran_order': False, 'shape': ({shape},), }}");
    let padding = (64 - ((10 + header.len() + 1) % 64)) % 64;
    header.extend(std::iter::repeat_n(' ', padding));
    header.push('\n');
    let mut file = writer(path)?;
    file.write_all(b"\x93NUMPY\x01\x00")?;
    file.write_all(&(header.len() as u16).to_le_bytes())?;
    file.write_all(header.as_bytes())?;
    Ok(file)
}

/// Distinct strings per field, coded in first-seen order. They grow with the road names
/// in an extract, far more slowly than its geometry, so they stay in RAM.
#[derive(Default)]
pub struct Dictionaries {
    codes: [HashMap<Box<str>, i32>; STRINGS.len()],
}

impl Dictionaries {
    pub fn code(&mut self, field: usize, value: Option<&str>) -> Result<i32> {
        let Some(value) = value else { return Ok(-1) };
        let codes = &mut self.codes[field];
        if let Some(&code) = codes.get(value) {
            return Ok(code);
        }
        let code = i32::try_from(codes.len())?;
        codes.insert(value.into(), code);
        Ok(code)
    }

    pub fn finish(self, out: &Path) -> Result<Value> {
        fs::create_dir(out.join("dictionaries"))?;
        let mut tables = serde_json::Map::new();
        for (name, codes) in STRINGS.iter().zip(&self.codes) {
            let mut values: Vec<_> = codes.iter().collect();
            values.sort_unstable_by_key(|(_, code)| **code);
            let size: usize = values.iter().map(|(value, _)| value.len()).sum();
            let data_path = format!("dictionaries/{name}.data.npy");
            let offsets_path = format!("dictionaries/{name}.offsets.npy");
            let mut data = npy(&out.join(&data_path), "|u1", &[size as u64])?;
            let mut offsets = npy(&out.join(&offsets_path), "<i8", &[values.len() as u64 + 1])?;
            let mut position = 0i64;
            offsets.write_all(&position.to_le_bytes())?;
            for (value, _) in &values {
                data.write_all(value.as_bytes())?;
                position += value.len() as i64;
                offsets.write_all(&position.to_le_bytes())?;
            }
            data.flush()?;
            offsets.flush()?;
            tables.insert(
                name.to_string(),
                json!({
                    "data": data_path, "offsets": offsets_path, "encoding": "utf-8",
                    "null_code": -1, "count": values.len(),
                }),
            );
        }
        Ok(Value::Object(tables))
    }
}

#[derive(Serialize)]
struct TileMeta {
    id: String,
    edge_start: u64,
    vertex_start: u64,
    edges: u64,
    vertices: u64,
    bbox: [f64; 4],
    grid_bbox: [f64; 4],
}

/// One spilled buffer's rows and coordinates in the shared spool log.
struct Chunk {
    start: u64,
    rows: usize,
    coords: usize,
}

impl Chunk {
    fn size(&self) -> usize {
        self.rows + self.coords
    }
}

struct Tile {
    meta: TileMeta,
    rows: Vec<u8>,
    coords: Vec<u8>,
    chunks: Vec<Chunk>,
}

pub struct Spools {
    log_path: PathBuf,
    log: BufWriter<File>,
    log_bytes: u64,
    degrees: f64,
    max_x: i64,
    max_y: i64,
    tiles: HashMap<(i32, i32), Tile>,
    buffered: usize,
    limit: usize,
    pub edges: u64,
    pub vertices: u64,
    pub flushes: u64,
}

impl Spools {
    pub fn new(scratch: &Path, degrees: f64, limit: usize) -> Result<Self> {
        let log_path = scratch.join("tiles.spool");
        Ok(Self {
            log: writer(&log_path)?,
            log_path,
            log_bytes: 0,
            degrees,
            max_x: (360.0 / degrees).ceil() as i64 - 1,
            max_y: (180.0 / degrees).ceil() as i64 - 1,
            tiles: HashMap::new(),
            buffered: 0,
            limit,
            edges: 0,
            vertices: 0,
            flushes: 0,
        })
    }

    /// Log the largest buffers, so each chunk moves as many bytes as possible, in tile order,
    /// so the chunks that later read back together sit close together in the log.
    fn spill(&mut self, target: usize) -> Result<()> {
        let mut tiles: Vec<_> = self.tiles.iter_mut().collect();
        tiles.sort_unstable_by_key(|(_, tile)| {
            std::cmp::Reverse(tile.rows.len() + tile.coords.len())
        });
        let mut buffered = self.buffered;
        let count = tiles
            .iter()
            .take_while(|(_, tile)| {
                let spill = buffered > target;
                buffered -= (tile.rows.len() + tile.coords.len()).min(buffered);
                spill
            })
            .count();
        tiles.truncate(count);
        tiles.sort_unstable_by_key(|(key, _)| **key);
        for (_, tile) in tiles {
            let bytes = tile.rows.len() + tile.coords.len();
            tile.chunks.push(Chunk {
                start: self.log_bytes,
                rows: tile.rows.len(),
                coords: tile.coords.len(),
            });
            self.log.write_all(&tile.rows)?;
            self.log.write_all(&tile.coords)?;
            self.log_bytes += bytes as u64;
            self.buffered -= bytes;
            self.flushes += 1;
            tile.rows = Vec::new();
            tile.coords = Vec::new();
        }
        Ok(())
    }

    pub fn close(&mut self) -> Result<()> {
        self.spill(0)?;
        self.log.flush()?;
        Ok(())
    }

    fn tile(&mut self, first: [f64; 2]) -> &mut Tile {
        let size = self.degrees;
        let x = (((first[0] + 180.0) / size).floor() as i64).min(self.max_x);
        let y = (((first[1] + 90.0) / size).floor() as i64).min(self.max_y);
        self.tiles.entry((x as i32, y as i32)).or_insert_with(|| {
            let id = format!("{x}_{y}");
            Tile {
                meta: TileMeta {
                    id,
                    edge_start: 0,
                    vertex_start: 0,
                    edges: 0,
                    vertices: 0,
                    bbox: [180.0, 90.0, -180.0, -90.0],
                    grid_bbox: [
                        x as f64 * size - 180.0,
                        y as f64 * size - 90.0,
                        ((x + 1) as f64 * size - 180.0).min(180.0),
                        ((y + 1) as f64 * size - 90.0).min(90.0),
                    ],
                },
                rows: Vec::new(),
                coords: Vec::new(),
                chunks: Vec::new(),
            }
        })
    }

    pub fn append(
        &mut self,
        u: i64,
        v: i64,
        geometry: &[[f64; 2]],
        reverse: bool,
        attributes: &Attributes,
        bbox: [f64; 4],
    ) -> Result<()> {
        let first = if reverse {
            *geometry.last().unwrap()
        } else {
            geometry[0]
        };
        let tile = self.tile(first);
        let mut row = [0u8; ROW_BYTES];
        row[..8].copy_from_slice(&u.to_le_bytes());
        row[8..16].copy_from_slice(&v.to_le_bytes());
        row[16..VERTEX_COUNT].copy_from_slice(attributes);
        row[VERTEX_COUNT..].copy_from_slice(&(geometry.len() as i64).to_le_bytes());
        tile.rows.extend_from_slice(&row);
        for i in 0..geometry.len() {
            let point = geometry[if reverse { geometry.len() - i - 1 } else { i }];
            tile.coords.extend_from_slice(&point[0].to_le_bytes());
            tile.coords.extend_from_slice(&point[1].to_le_bytes());
        }
        tile.meta.edges += 1;
        tile.meta.vertices += geometry.len() as u64;
        tile.meta.bbox = [
            tile.meta.bbox[0].min(bbox[0]),
            tile.meta.bbox[1].min(bbox[1]),
            tile.meta.bbox[2].max(bbox[2]),
            tile.meta.bbox[3].max(bbox[3]),
        ];
        self.edges += 1;
        self.vertices += geometry.len() as u64;
        self.buffered += ROW_BYTES + geometry.len() * 16;
        if self.buffered > self.limit {
            self.spill(self.limit / 2)?;
        }
        Ok(())
    }

    /// Read chunks concurrently, because a chunk evicted from the page cache costs a disk read.
    /// Chunks adjacent in the log share one read. Returns where each landed in `buffer`.
    fn read_chunks(
        log: &File,
        chunks: &[&Chunk],
        buffer: &mut Vec<u8>,
        pool: &rayon::ThreadPool,
    ) -> Result<Vec<usize>> {
        let mut order: Vec<_> = (0..chunks.len()).collect();
        order.sort_unstable_by_key(|&i| chunks[i].start);
        let mut places = vec![0; chunks.len()];
        let mut extents: Vec<(u64, usize)> = Vec::new();
        let mut size = 0;
        for i in order {
            let chunk = chunks[i];
            places[i] = size;
            size += chunk.size();
            match extents.last_mut() {
                Some((start, length)) if *start + *length as u64 == chunk.start => {
                    *length += chunk.size()
                }
                _ => extents.push((chunk.start, chunk.size())),
            }
        }
        buffer.resize(size, 0);
        let mut unread = buffer.as_mut_slice();
        let mut reads = Vec::with_capacity(extents.len());
        for (start, length) in extents {
            let (bytes, rest) = unread.split_at_mut(length);
            reads.push((bytes, start));
            unread = rest;
        }
        pool.install(|| {
            reads
                .into_par_iter()
                .try_for_each(|(bytes, start)| log.read_exact_at(bytes, start))
        })?;
        Ok(places)
    }

    pub fn finish(
        &mut self,
        out: &Path,
        metadata: &Value,
        pool: &rayon::ThreadPool,
    ) -> Result<u64> {
        self.close()?;
        let dir = out.join("columns");
        fs::create_dir(&dir)?;
        let column =
            |name: &str, dtype, shape: &[u64]| npy(&dir.join(format!("{name}.npy")), dtype, shape);
        let mut columns = COLUMNS
            .iter()
            .map(|(name, dtype, _, _)| column(name, dtype, &[self.edges]))
            .collect::<Result<Vec<_>>>()?;
        let mut offsets = column("offsets", "<i8", &[self.edges + 1])?;
        let mut vertices = column("vertices", "<f8", &[self.vertices, 2])?;
        offsets.write_all(&0i64.to_le_bytes())?;
        let mut manifest = writer(&out.join("manifest.json"))?;
        let header = serde_json::to_string_pretty(metadata)?;
        manifest.write_all(header.trim_end_matches('}').trim_end().as_bytes())?;
        manifest.write_all(b",\n  \"tiles\": [\n")?;
        let log = File::open(&self.log_path)?;
        let mut buffer = Vec::new();
        let (mut edge_start, mut vertex_start) = (0, 0);
        let mut position = 0i64;
        let mut count = 0u64;
        let mut keys: Vec<_> = self.tiles.keys().copied().collect();
        keys.sort_unstable();
        let mut remaining = keys.as_slice();
        while !remaining.is_empty() {
            // Tiles in file order, with at most `limit` bytes of spool in RAM unless one is larger.
            let mut size = 0;
            let batch = remaining
                .iter()
                .take_while(|key| {
                    size += self.tiles[key]
                        .chunks
                        .iter()
                        .map(Chunk::size)
                        .sum::<usize>();
                    size <= self.limit
                })
                .count()
                .max(1);
            let (batch, rest) = remaining.split_at(batch);
            remaining = rest;
            let chunks: Vec<_> = batch
                .iter()
                .flat_map(|key| &self.tiles[key].chunks)
                .collect();
            let places = Self::read_chunks(&log, &chunks, &mut buffer, pool)?;
            position = Self::write_columns(
                &buffer,
                &chunks,
                &places,
                &mut columns,
                &mut offsets,
                &mut vertices,
                position,
                pool,
            )?;
            for key in batch {
                let tile = self.tiles.get_mut(key).unwrap();
                tile.meta.edge_start = edge_start;
                tile.meta.vertex_start = vertex_start;
                edge_start += tile.meta.edges;
                vertex_start += tile.meta.vertices;
                if count > 0 {
                    manifest.write_all(b",\n")?;
                }
                serde_json::to_writer(&mut manifest, &tile.meta)?;
                count += 1;
            }
            ensure!(
                position == vertex_start as i64,
                "Vertex spool count mismatch"
            );
        }
        for column in &mut columns {
            column.flush()?;
        }
        offsets.flush()?;
        vertices.flush()?;
        manifest.write_all(b"\n  ]\n}\n")?;
        manifest.flush()?;
        fs::remove_file(&self.log_path)?;
        Ok(count)
    }

    /// Append spooled chunks to the shared columns, returning the next vertex position.
    /// Every file has its own writer, so they fill at once; offsets stay global across tiles.
    #[allow(clippy::too_many_arguments)]
    fn write_columns(
        spool: &[u8],
        chunks: &[&Chunk],
        places: &[usize],
        columns: &mut [BufWriter<File>],
        offsets: &mut BufWriter<File>,
        vertices: &mut BufWriter<File>,
        mut position: i64,
        pool: &rayon::ThreadPool,
    ) -> Result<i64> {
        let parts: Vec<_> = chunks
            .iter()
            .zip(places)
            .map(|(chunk, &place)| spool[place..place + chunk.size()].split_at(chunk.rows))
            .collect();
        let rows = || {
            parts
                .iter()
                .flat_map(|(rows, _)| rows.chunks_exact(ROW_BYTES))
        };
        let (filled, appended) = pool.install(|| {
            rayon::join(
                || {
                    columns.par_iter_mut().zip(&COLUMNS).try_for_each(
                        |(column, (_, _, start, width))| {
                            rows().try_for_each(|row| column.write_all(&row[*start..start + width]))
                        },
                    )
                },
                || -> std::io::Result<i64> {
                    for row in rows() {
                        position += i64::from_le_bytes(row[VERTEX_COUNT..].try_into().unwrap());
                        offsets.write_all(&position.to_le_bytes())?;
                    }
                    for (_, coords) in &parts {
                        vertices.write_all(coords)?;
                    }
                    Ok(position)
                },
            )
        });
        filled?;
        Ok(appended?)
    }
}
