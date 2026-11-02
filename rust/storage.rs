use crate::index::writer;
use crate::tags::STRINGS;
use anyhow::{Result, ensure};
use lru::LruCache;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

pub const ROW_BYTES: usize = 74;
pub const COLUMNS: [(&str, &str, usize, usize); 14] = [
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

pub struct Dictionaries<'a> {
    db: &'a Connection,
    cache: LruCache<(usize, String), i32>,
    counts: [i64; 5],
}

impl<'a> Dictionaries<'a> {
    pub fn new(db: &'a Connection) -> Result<Self> {
        db.execute_batch(
            "CREATE TABLE strings(field INTEGER, code INTEGER, value TEXT,
                          PRIMARY KEY(field, code), UNIQUE(field, value));",
        )?;
        Ok(Self {
            db,
            cache: LruCache::new(NonZeroUsize::new(8192).unwrap()),
            counts: [0; 5],
        })
    }

    pub fn code(&mut self, field: usize, value: Option<&str>) -> Result<i32> {
        let Some(value) = value else { return Ok(-1) };
        let key = (field, value.to_owned());
        if let Some(&code) = self.cache.get(&key) {
            return Ok(code);
        }
        let existing: Option<i32> = self
            .db
            .prepare_cached("SELECT code FROM strings WHERE field=? AND value=?")?
            .query_row(params![field as i64, value], |r| r.get(0))
            .optional()?;
        let code = match existing {
            Some(code) => code,
            None => {
                let code = i32::try_from(self.counts[field])?;
                self.db
                    .prepare_cached("INSERT INTO strings VALUES (?,?,?)")?
                    .execute(params![field as i64, code, value])?;
                self.counts[field] += 1;
                code
            }
        };
        self.cache.put(key, code);
        Ok(code)
    }

    pub fn finish(&self, out: &Path) -> Result<Value> {
        fs::create_dir(out.join("dictionaries"))?;
        let mut tables = serde_json::Map::new();
        for (field, name) in STRINGS.iter().enumerate() {
            let size: i64 = self.db.query_row(
                "SELECT coalesce(sum(length(cast(value AS BLOB))),0) FROM strings WHERE field=?",
                [field as i64],
                |r| r.get(0),
            )?;
            let data_path = format!("dictionaries/{name}.data.npy");
            let offsets_path = format!("dictionaries/{name}.offsets.npy");
            let mut data = npy(&out.join(&data_path), "|u1", &[size as u64])?;
            let mut offsets = npy(
                &out.join(&offsets_path),
                "<i8",
                &[self.counts[field] as u64 + 1],
            )?;
            let mut position = 0i64;
            offsets.write_all(&position.to_le_bytes())?;
            let mut stmt = self
                .db
                .prepare("SELECT value FROM strings WHERE field=? ORDER BY code")?;
            let mut rows = stmt.query([field as i64])?;
            while let Some(row) = rows.next()? {
                let value: String = row.get(0)?;
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
                    "null_code": -1, "count": self.counts[field],
                }),
            );
        }
        Ok(Value::Object(tables))
    }
}

#[derive(Serialize, Deserialize)]
struct TileMeta {
    id: String,
    path: String,
    edges: u64,
    vertices: u64,
    bbox: [f64; 4],
    grid_bbox: [f64; 4],
}

struct Tile {
    meta: TileMeta,
    rows: BufWriter<File>,
    coords: BufWriter<File>,
}

pub struct Spools<'a> {
    db: &'a Connection,
    scratch: PathBuf,
    degrees: f64,
    active: LruCache<String, Tile>,
    pub edges: u64,
    pub vertices: u64,
}

impl<'a> Spools<'a> {
    pub fn new(db: &'a Connection, scratch: &Path, degrees: f64) -> Result<Self> {
        db.execute_batch(
            "CREATE TABLE tiles(id TEXT PRIMARY KEY, x INTEGER, y INTEGER, metadata TEXT)",
        )?;
        Ok(Self {
            db,
            scratch: scratch.to_owned(),
            degrees,
            active: LruCache::new(NonZeroUsize::new(32).unwrap()),
            edges: 0,
            vertices: 0,
        })
    }

    fn save(&self, mut tile: Tile) -> Result<()> {
        tile.rows.flush()?;
        tile.coords.flush()?;
        self.db
            .prepare_cached("UPDATE tiles SET metadata=? WHERE id=?")?
            .execute(params![serde_json::to_string(&tile.meta)?, tile.meta.id])?;
        Ok(())
    }

    pub fn close(&mut self) -> Result<()> {
        while let Some((_, tile)) = self.active.pop_lru() {
            self.save(tile)?;
        }
        Ok(())
    }

    fn tile(&mut self, first: [f64; 2]) -> Result<&mut Tile> {
        let size = self.degrees;
        let x = (((first[0] + 180.0) / size).floor() as i64).min((360.0 / size).ceil() as i64 - 1);
        let y = (((first[1] + 90.0) / size).floor() as i64).min((180.0 / size).ceil() as i64 - 1);
        let id = format!("{x}_{y}");
        if !self.active.contains(&id) {
            if self.active.len() == self.active.cap().get() {
                let (_, old) = self.active.pop_lru().unwrap();
                self.save(old)?;
            }
            let existing: Option<String> = self
                .db
                .prepare_cached("SELECT metadata FROM tiles WHERE id=?")?
                .query_row([&id], |r| r.get(0))
                .optional()?;
            let meta = match existing {
                Some(raw) => serde_json::from_str(&raw)?,
                None => {
                    let meta = TileMeta {
                        id: id.clone(),
                        path: format!("tiles/{id}"),
                        edges: 0,
                        vertices: 0,
                        bbox: [180.0, 90.0, -180.0, -90.0],
                        grid_bbox: [
                            x as f64 * size - 180.0,
                            y as f64 * size - 90.0,
                            ((x + 1) as f64 * size - 180.0).min(180.0),
                            ((y + 1) as f64 * size - 90.0).min(90.0),
                        ],
                    };
                    self.db
                        .prepare_cached("INSERT INTO tiles VALUES (?,?,?,?)")?
                        .execute(params![id, x, y, serde_json::to_string(&meta)?])?;
                    meta
                }
            };
            let rows = OpenOptions::new()
                .append(true)
                .create(true)
                .open(self.scratch.join(format!("{id}.rows")))?;
            let coords = OpenOptions::new()
                .append(true)
                .create(true)
                .open(self.scratch.join(format!("{id}.coords")))?;
            self.active.put(
                id.clone(),
                Tile {
                    meta,
                    rows: BufWriter::with_capacity(65536, rows),
                    coords: BufWriter::with_capacity(65536, coords),
                },
            );
        }
        Ok(self.active.get_mut(&id).unwrap())
    }

    pub fn append(
        &mut self,
        u: i64,
        v: i64,
        geometry: &[[f64; 2]],
        reverse: bool,
        attributes: &[u8; 50],
        bbox: [f64; 4],
    ) -> Result<()> {
        let first = if reverse {
            *geometry.last().unwrap()
        } else {
            geometry[0]
        };
        let tile = self.tile(first)?;
        let mut row = [0u8; ROW_BYTES];
        row[..8].copy_from_slice(&u.to_le_bytes());
        row[8..16].copy_from_slice(&v.to_le_bytes());
        row[16..66].copy_from_slice(attributes);
        row[66..].copy_from_slice(&(geometry.len() as i64).to_le_bytes());
        tile.rows.write_all(&row)?;
        for i in 0..geometry.len() {
            let point = geometry[if reverse { geometry.len() - i - 1 } else { i }];
            tile.coords.write_all(&point[0].to_le_bytes())?;
            tile.coords.write_all(&point[1].to_le_bytes())?;
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
        Ok(())
    }

    pub fn finish(&mut self, out: &Path, metadata: &Value) -> Result<u64> {
        self.close()?;
        let mut manifest = writer(&out.join("manifest.json"))?;
        let header = serde_json::to_string_pretty(metadata)?;
        manifest.write_all(header.trim_end_matches('}').trim_end().as_bytes())?;
        manifest.write_all(b",\n  \"tiles\": [\n")?;
        let mut count = 0u64;
        let mut stmt = self.db.prepare("SELECT metadata FROM tiles ORDER BY x,y")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let meta: TileMeta = serde_json::from_str(&row.get::<_, String>(0)?)?;
            self.finish_tile(out, &meta)?;
            if count > 0 {
                manifest.write_all(b",\n")?;
            }
            serde_json::to_writer(&mut manifest, &meta)?;
            count += 1;
        }
        manifest.write_all(b"\n  ]\n}\n")?;
        manifest.flush()?;
        Ok(count)
    }

    fn finish_tile(&self, out: &Path, meta: &TileMeta) -> Result<()> {
        let dest = out.join(&meta.path);
        fs::create_dir_all(&dest)?;
        let mut columns = COLUMNS
            .iter()
            .map(|(name, dtype, _, _)| npy(&dest.join(format!("{name}.npy")), dtype, &[meta.edges]))
            .collect::<Result<Vec<_>>>()?;
        let mut offsets = npy(&dest.join("offsets.npy"), "<i8", &[meta.edges + 1])?;
        let mut position = 0i64;
        offsets.write_all(&position.to_le_bytes())?;
        let rows_path = self.scratch.join(format!("{}.rows", meta.id));
        let coords_path = self.scratch.join(format!("{}.coords", meta.id));
        let mut rows = BufReader::new(File::open(&rows_path)?);
        let mut buffer = vec![0u8; ROW_BYTES * 32768];
        let mut remaining = meta.edges;
        while remaining > 0 {
            let n = remaining.min(32768) as usize;
            let chunk = &mut buffer[..n * ROW_BYTES];
            rows.read_exact(chunk)?;
            for (column, (_, _, start, width)) in columns.iter_mut().zip(COLUMNS) {
                for row in chunk.chunks_exact(ROW_BYTES) {
                    column.write_all(&row[start..start + width])?;
                }
            }
            for row in chunk.chunks_exact(ROW_BYTES) {
                position += i64::from_le_bytes(row[66..].try_into().unwrap());
                offsets.write_all(&position.to_le_bytes())?;
            }
            remaining -= n as u64;
        }
        ensure!(
            position as u64 == meta.vertices,
            "Vertex spool count mismatch"
        );
        for column in &mut columns {
            column.flush()?;
        }
        offsets.flush()?;
        let mut coords = npy(&dest.join("vertices.npy"), "<f8", &[meta.vertices, 2])?;
        let mut source = File::open(&coords_path)?;
        let bytes = std::io::copy(&mut source, &mut coords)?;
        ensure!(bytes == meta.vertices * 16, "Truncated vertex spool");
        coords.flush()?;
        drop(rows);
        drop(source);
        fs::remove_file(rows_path)?;
        fs::remove_file(coords_path)?;
        Ok(())
    }
}
