use crate::index::{DiskIndex, NodeWriter, database, export_junctions, writer};
use crate::storage::{COLUMNS, Dictionaries, Spools};
use crate::tags::{self, HIGHWAYS, STRINGS, Tags};
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, SecondsFormat};
use osmpbf::{Blob, BlobReader, BlobType, ByteOffset, Element, HeaderBlock, PrimitiveBlock, Way};
use rayon::prelude::*;
use rusqlite::params;
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct Config {
    pub input: PathBuf,
    pub output: PathBuf,
    pub scratch: PathBuf,
    pub degrees: f64,
    pub cache_mb: usize,
    pub threads: usize,
    pub quiet: bool,
}

enum Block {
    Header(HeaderBlock),
    Data(PrimitiveBlock),
    Other,
}

fn decode(blobs: &[Blob], pool: &rayon::ThreadPool) -> Result<Vec<Block>> {
    // Batches are bounded by --threads and consumed in source order.
    pool.install(|| {
        blobs
            .par_iter()
            .map(|blob| {
                Ok(match blob.get_type() {
                    BlobType::OsmHeader => Block::Header(blob.to_headerblock()?),
                    BlobType::OsmData => Block::Data(blob.to_primitiveblock()?),
                    BlobType::Unknown(_) => Block::Other,
                })
            })
            .collect()
    })
}

fn source_header(header: &HeaderBlock, path: &Path) -> Result<Value> {
    for feature in header.required_features() {
        ensure!(
            feature != "HistoricalInformation",
            "History PBF is unsupported; provide a snapshot extract"
        );
        ensure!(
            matches!(feature.as_str(), "OsmSchema-V0.6" | "DenseNodes"),
            "Unsupported required PBF feature: {feature}"
        );
    }
    let timestamp = header
        .osmosis_replication_timestamp()
        .map(|seconds| {
            DateTime::from_timestamp(seconds, 0)
                .context("Invalid replication timestamp in PBF header")
                .map(|date| date.to_rfc3339_opts(SecondsFormat::Secs, true))
        })
        .transpose()?;
    Ok(json!({
        "file": path.file_name().unwrap_or_default().to_string_lossy(),
        "size_bytes": fs::metadata(path)?.len(), "timestamp": timestamp,
        "timestamp_source": if timestamp.is_some() { Some("pbf_header") } else { None },
        "generator": header.writing_program(),
    }))
}

fn topology(config: &Config, pool: &rayon::ThreadPool) -> Result<(Value, u64)> {
    let db = database(&config.scratch.join("topology.sqlite"), config.cache_mb)?;
    db.execute_batch(
        "CREATE TABLE nodes(id INTEGER PRIMARY KEY, uses INTEGER, endpoint INTEGER); BEGIN",
    )?;
    let mut upsert = db.prepare(
        "INSERT INTO nodes VALUES (?,1,?) ON CONFLICT(id)
        DO UPDATE SET uses=2, endpoint=max(endpoint,excluded.endpoint)",
    )?;
    let mut locations = NodeWriter::new(&config.scratch.join("locations.idx"))?;
    let mut way_blocks = writer(&config.scratch.join("ways.blocks"))?;
    let mut reader = BlobReader::seekable_from_path(&config.input)?;
    let mut source = None;
    let mut ways = 0;
    let mut last_way = None;
    let mut refs = Vec::new();
    loop {
        let blobs = reader
            .by_ref()
            .take(config.threads)
            .collect::<osmpbf::Result<Vec<_>>>()?;
        if blobs.is_empty() {
            break;
        }
        let blocks = decode(&blobs, pool)?;
        for (blob, block) in blobs.iter().zip(blocks) {
            match block {
                Block::Header(header) => {
                    source = Some(source_header(&header, &config.input)?);
                }
                Block::Data(block) => {
                    let mut contains_ways = false;
                    for element in block.elements() {
                        match element {
                            Element::Node(node) => locations.add(
                                node.id(),
                                node.nano_lon() as f64 / 1e9,
                                node.nano_lat() as f64 / 1e9,
                            )?,
                            Element::DenseNode(node) => locations.add(
                                node.id(),
                                node.nano_lon() as f64 / 1e9,
                                node.nano_lat() as f64 / 1e9,
                            )?,
                            Element::Way(way) => {
                                contains_ways = true;
                                ensure!(
                                    last_way.is_none_or(|id| way.id() > id),
                                    "Ways must have unique ascending IDs; use a sorted snapshot extract"
                                );
                                last_way = Some(way.id());
                                if !way.tags().any(|(key, value)| {
                                    key == "highway" && tags::highway_code(value).is_some()
                                }) {
                                    continue;
                                }
                                ways += 1;
                                refs.clear();
                                refs.extend(way.refs());
                                if refs.is_empty() {
                                    continue;
                                }
                                let first = refs[0];
                                let last = *refs.last().unwrap();
                                refs.sort_unstable();
                                refs.dedup(); // Distinct ways, not repeated occurrences within a way.
                                for &id in &refs {
                                    upsert.execute(params![
                                        id,
                                        i32::from(id == first || id == last)
                                    ])?;
                                }
                            }
                            Element::Relation(_) => (),
                        }
                    }
                    if contains_ways {
                        way_blocks.write_all(
                            &blob
                                .offset()
                                .context("PBF reader has no byte offset")?
                                .0
                                .to_le_bytes(),
                        )?;
                    }
                }
                Block::Other => (),
            }
        }
    }
    drop(upsert);
    db.execute_batch("COMMIT")?;
    locations.finish()?;
    way_blocks.flush()?;
    export_junctions(&db, &config.scratch.join("junctions.idx"))?;
    Ok((source.context("No OSM header in input PBF")?, ways))
}

fn emit_way(
    way: Way<'_>,
    nodes: &DiskIndex,
    junctions: &DiskIndex,
    dictionaries: &mut Dictionaries<'_>,
    spools: &mut Spools<'_>,
) -> Result<()> {
    let Some(highway) = way.tags().find_map(|(key, value)| {
        if key == "highway" {
            tags::highway_code(value)
        } else {
            None
        }
    }) else {
        return Ok(());
    };
    let refs: Vec<_> = way.refs().collect();
    if refs.len() < 2 {
        return Ok(());
    }
    let mut geometry = Vec::with_capacity(refs.len());
    for &id in &refs {
        let point = nodes.location(id).with_context(|| {
            format!(
                "Drivable way {} references missing node {id}; use a complete extract",
                way.id()
            )
        })?;
        ensure!(
            point[0].is_finite()
                && point[1].is_finite()
                && point[0].abs() <= 180.0
                && point[1].abs() <= 90.0,
            "Drivable way {} has invalid WGS84 coordinates at node {id}",
            way.id()
        );
        geometry.push(point);
    }
    let tags: Tags = way
        .tags()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    let mut attributes = [0u8; 50];
    attributes[..8].copy_from_slice(&way.id().to_le_bytes());
    attributes[8..16].copy_from_slice(&tags::width(&tags).to_le_bytes());
    attributes[16] = highway;
    for (i, (name, kind)) in [
        ("maxspeed", "speed"),
        ("maxweight", "weight"),
        ("maxheight", "length"),
    ]
    .iter()
    .enumerate()
    {
        attributes[17 + i * 4..21 + i * 4]
            .copy_from_slice(&tags::limit(tags::get(&tags, name), kind).to_le_bytes());
    }
    attributes[29] = tags::flags(&tags);
    for (i, name) in STRINGS.iter().enumerate() {
        attributes[30 + i * 4..34 + i * 4]
            .copy_from_slice(&dictionaries.code(i, tags::get(&tags, name))?.to_le_bytes());
    }
    let direction = tags::direction(&tags);
    let mut start = 0;
    for end in 1..refs.len() {
        if !junctions.contains(refs[end]) {
            continue;
        }
        let segment = &geometry[start..=end];
        let mut bbox = [180.0f64, 90.0f64, -180.0f64, -90.0f64];
        for point in segment {
            bbox = [
                bbox[0].min(point[0]),
                bbox[1].min(point[1]),
                bbox[2].max(point[0]),
                bbox[3].max(point[1]),
            ];
        }
        if segment
            .windows(2)
            .any(|p| (p[0][0] - p[1][0]).abs() > 180.0)
        {
            // A conservative envelope avoids missing a dateline-crossing edge.
            bbox[0] = -180.0;
            bbox[2] = 180.0;
        }
        if direction >= 0 {
            spools.append(refs[start], refs[end], segment, false, &attributes, bbox)?;
        }
        if direction <= 0 {
            spools.append(refs[end], refs[start], segment, true, &attributes, bbox)?;
        }
        start = end;
    }
    Ok(())
}

pub fn run(config: Config) -> Result<String> {
    ensure!(
        config.degrees.is_finite() && config.degrees > 0.0 && config.degrees <= 180.0,
        "tile_degrees must be finite and in (0, 180]"
    );
    ensure!(
        config.threads > 0 && config.cache_mb > 0,
        "threads and cache_mb must be positive"
    );
    let log = |message| {
        if !config.quiet {
            eprintln!("{message}");
        }
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.threads)
        .build()?;
    let phase = Instant::now();
    log("Pass 1/2: building disk-backed node locations and junction incidence (Rust)");
    let (source, ways) = topology(&config, &pool)?;
    let topology_seconds = phase.elapsed().as_secs_f64();
    let phase = Instant::now();
    log("Pass 2/2: splitting ways and spooling directed edges (Rust)");
    let nodes = DiskIndex::open(&config.scratch.join("locations.idx"), 24)?;
    let junctions = DiskIndex::open(&config.scratch.join("junctions.idx"), 8)?;
    let db = database(&config.scratch.join("output.sqlite"), config.cache_mb)?;
    let mut dictionaries = Dictionaries::new(&db)?;
    let mut spools = Spools::new(&db, &config.scratch, config.degrees)?;
    db.execute_batch("BEGIN")?;
    let mut reader = BlobReader::seekable_from_path(&config.input)?;
    let offsets_file = File::open(config.scratch.join("ways.blocks"))?;
    let mut remaining = offsets_file.metadata()?.len() / 8;
    let mut offsets = BufReader::new(offsets_file);
    while remaining > 0 {
        let count = remaining.min(config.threads as u64);
        let mut blobs = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let mut bytes = [0u8; 8];
            offsets.read_exact(&mut bytes)?;
            blobs.push(reader.blob_from_offset(ByteOffset(u64::from_le_bytes(bytes)))?);
        }
        for block in decode(&blobs, &pool)? {
            let Block::Data(block) = block else {
                bail!("Expected way data block");
            };
            for group in block.groups() {
                for way in group.ways() {
                    emit_way(way, &nodes, &junctions, &mut dictionaries, &mut spools)?;
                }
            }
        }
        remaining -= count;
    }
    spools.close()?;
    db.execute_batch("COMMIT")?;
    drop(nodes);
    drop(junctions);
    let edges_seconds = phase.elapsed().as_secs_f64();
    let phase = Instant::now();
    log("Writing memory-mappable columns and manifest (Rust)");
    let dictionary_tables = dictionaries.finish(&config.output)?;
    let highway_enum = HIGHWAYS
        .iter()
        .enumerate()
        .map(|(i, name)| (i.to_string(), json!(name)))
        .collect::<serde_json::Map<_, _>>();
    let mut columns = COLUMNS
        .iter()
        .map(|(name, dtype, _, _)| (name.to_string(), json!(dtype)))
        .collect::<serde_json::Map<_, _>>();
    columns.insert("vertices".into(), json!("<f8"));
    columns.insert("offsets".into(), json!("<i8"));
    let metadata = json!({
        "format": "osm-road-prep", "version": 1, "crs": "EPSG:4326",
        "coordinate_order": ["lon", "lat"], "source": source,
        "source_pbf_timestamp": source["timestamp"], "tile_degrees": config.degrees,
        "grid_origin": [-180, -90], "tile_assignment": "first_vertex",
        "bbox_order": ["west", "south", "east", "north"], "tile_bbox": "full_geometry_envelope",
        "edges": spools.edges, "vertices": spools.vertices, "drivable_ways": ways,
        "columns": columns, "highway_enum": highway_enum,
        "flag_bits": {"bridge": 1, "tunnel": 2, "bridge_present": 4, "tunnel_present": 8},
        "numeric_null": "NaN", "string_dictionaries": dictionary_tables,
        "attribution": "© OpenStreetMap contributors", "license": "https://www.openstreetmap.org/copyright",
    });
    let tiles = spools.finish(&config.output, &metadata)?;
    Ok(serde_json::to_string(&json!({
        "edges": spools.edges, "vertices": spools.vertices, "tiles": tiles, "drivable_ways": ways,
        "stages": {"topology_seconds": topology_seconds, "edges_seconds": edges_seconds,
                   "columns_seconds": phase.elapsed().as_secs_f64()}, "source": source,
        "settings": {"tile_degrees": config.degrees, "cache_mb": config.cache_mb, "threads": config.threads},
        "implementation": "rust",
    }))?)
}
