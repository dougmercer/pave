use crate::index::{DiskIndex, RefSorter, database, writer};
use crate::storage::{Attributes, COLUMNS, Dictionaries, STRING_SLOTS, Spools};
use crate::tags::{self, HIGHWAYS, STRINGS, Tags};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, SecondsFormat};
use osmpbf::{BlobReader, BlobType, ByteOffset, Element, HeaderBlock, PrimitiveBlock, Way};
use rayon::prelude::*;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::time::Instant;

const UNSORTED_NODES: &str = "Nodes must have unique ascending IDs; use a sorted snapshot extract";
const UNSORTED_FILE: &str = "PBF header claims Sort.Type_then_ID but elements are not sorted";
const UNSORTED_WAYS: &str = "Ways must have unique ascending IDs; use a sorted snapshot extract";

pub struct Config {
    pub input: PathBuf,
    pub output: PathBuf,
    pub scratch: PathBuf,
    pub degrees: f64,
    /// SQLite page cache for the string dictionaries.
    pub cache_mb: usize,
    /// In-memory run size for the external sort of node references.
    pub sort_mb: usize,
    /// Edge rows and vertices buffered across all tiles before spilling to scratch.
    pub spool_mb: usize,
    pub threads: usize,
    pub quiet: bool,
    /// Polled between batches on the calling thread, so Ctrl-C is not deferred to the end.
    pub interrupted: Box<dyn Fn() -> Result<()> + Send + Sync>,
}

/// Run `work` on the decode pool in batches, feeding results to `consume` in source order.
/// Blocks are decoded, transformed and dropped by workers; the caller only does ordered I/O.
fn pipeline<T: Send>(
    config: &Config,
    pool: &rayon::ThreadPool,
    offsets: &[u64],
    work: impl Fn(u64, PrimitiveBlock) -> Result<T> + Sync,
    mut consume: impl FnMut(T) -> Result<()>,
) -> Result<()> {
    let (sender, receiver) = sync_channel::<Vec<T>>(2);
    let work = &work;
    std::thread::scope(|scope| {
        let producer = scope.spawn(move || -> Result<()> {
            let mut reader = BlobReader::seekable_from_path(&config.input)?;
            for batch in offsets.chunks(config.threads * 2) {
                let blobs = batch
                    .iter()
                    .map(|&offset| reader.blob_from_offset(ByteOffset(offset)))
                    .collect::<osmpbf::Result<Vec<_>>>()?;
                let results = pool.install(|| {
                    blobs
                        .par_iter()
                        .map(|blob| {
                            let offset = blob.offset().context("PBF reader has no byte offset")?.0;
                            work(offset, blob.to_primitiveblock()?)
                        })
                        .collect::<Result<Vec<_>>>()
                })?;
                if sender.send(results).is_err() {
                    break;
                }
            }
            Ok(())
        });
        let consumed = (|| {
            for batch in receiver {
                (config.interrupted)()?;
                for item in batch {
                    consume(item)?;
                }
            }
            Ok(())
        })();
        let produced = producer.join().expect("decode thread panicked");
        consumed.and(produced)
    })
}

/// Stream blob headers, read the OSM header, and find where node blocks end.
fn layout(config: &Config) -> Result<(Value, Vec<u64>, usize)> {
    let mut reader = BlobReader::seekable_from_path(&config.input)?;
    let (mut header, mut data) = (None, Vec::new());
    while let Some(entry) = reader.next_header_skip_blob() {
        let (blob_header, offset) = entry?;
        let offset = offset.context("PBF reader has no byte offset")?.0;
        match blob_header.blob_type() {
            BlobType::OsmHeader => header = header.or(Some(offset)),
            BlobType::OsmData => data.push(offset),
            BlobType::Unknown(_) => (),
        }
    }
    let header = reader
        .blob_from_offset(ByteOffset(header.context("No OSM header in input PBF")?))?
        .to_headerblock()?;
    let source = source_header(&header, &config.input)?;
    let sorted = header
        .optional_features()
        .iter()
        .any(|f| f == "Sort.Type_then_ID");
    // Sorted files keep nodes first, so the node/way boundary can be found by bisection.
    let mut boundary = 0;
    if sorted {
        let mut high = data.len();
        while boundary < high {
            let mid = boundary + (high - boundary) / 2;
            let block = reader
                .blob_from_offset(ByteOffset(data[mid]))?
                .to_primitiveblock()?;
            let only_nodes = !block
                .groups()
                .any(|group| group.ways().next().is_some() || group.relations().next().is_some());
            if only_nodes {
                boundary = mid + 1;
            } else {
                high = mid;
            }
        }
    }
    Ok((source, data, boundary))
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

struct WayScan {
    offset: u64,
    keys: Vec<i64>,
    ways: u64,
    range: Option<(i64, i64)>,
}

fn scan_ways(offset: u64, block: PrimitiveBlock) -> Result<WayScan> {
    let mut scan = WayScan {
        offset,
        keys: Vec::new(),
        ways: 0,
        range: None,
    };
    let mut refs = Vec::new();
    for group in block.groups() {
        for way in group.ways() {
            scan.range = match scan.range {
                None => Some((way.id(), way.id())),
                Some((first, last)) => {
                    ensure!(way.id() > last, UNSORTED_WAYS);
                    Some((first, way.id()))
                }
            };
            if !way
                .tags()
                .any(|(key, value)| key == "highway" && tags::highway_code(value).is_some())
            {
                continue;
            }
            scan.ways += 1;
            refs.clear();
            refs.extend(way.refs());
            if refs.is_empty() {
                continue;
            }
            let first = refs[0];
            let last = *refs.last().unwrap();
            refs.sort_unstable();
            refs.dedup(); // Distinct ways, not repeated occurrences within a way.
            scan.keys.extend(
                refs.iter()
                    .map(|&id| (id << 1) | i64::from(id == first || id == last)),
            );
        }
    }
    Ok(scan)
}

struct NodeScan {
    records: Vec<u8>,
    range: Option<(i64, i64)>,
}

fn scan_nodes(block: PrimitiveBlock, used: &DiskIndex, nodes_only: bool) -> Result<NodeScan> {
    let mut scan = NodeScan {
        records: Vec::new(),
        range: None,
    };
    let mut position = 0;
    let mut visit = |id: i64, lon: i64, lat: i64| -> Result<()> {
        scan.range = match scan.range {
            None => {
                position = used.lower_bound(id);
                Some((id, id))
            }
            Some((first, last)) => {
                ensure!(id > last, UNSORTED_NODES);
                Some((first, id))
            }
        };
        while position < used.len() && used.key(position) >> 1 < id {
            position += 1;
        }
        if position < used.len() && used.key(position) >> 1 == id {
            scan.records
                .extend_from_slice(&used.key(position).to_le_bytes());
            scan.records
                .extend_from_slice(&(lon as f64 / 1e9).to_le_bytes());
            scan.records
                .extend_from_slice(&(lat as f64 / 1e9).to_le_bytes());
            position += 1;
        }
        Ok(())
    };
    for element in block.elements() {
        match element {
            Element::Node(node) => visit(node.id(), node.nano_lon(), node.nano_lat())?,
            Element::DenseNode(node) => visit(node.id(), node.nano_lon(), node.nano_lat())?,
            // A file that lies about its sort order must not silently lose ways.
            Element::Way(_) | Element::Relation(_) => ensure!(!nodes_only, UNSORTED_FILE),
        }
    }
    Ok(scan)
}

fn topology(
    config: &Config,
    pool: &rayon::ThreadPool,
    data: &[u64],
    boundary: usize,
) -> Result<(Vec<u64>, u64, usize)> {
    let mut sorter = RefSorter::new(&config.scratch, config.sort_mb << 20);
    let (mut way_blocks, mut ways, mut last_way) = (Vec::new(), 0, None);
    pipeline(config, pool, &data[boundary..], scan_ways, |scan| {
        if let Some((first, last)) = scan.range {
            ensure!(last_way.is_none_or(|id| first > id), UNSORTED_WAYS);
            last_way = Some(last);
            way_blocks.push(scan.offset);
        }
        ways += scan.ways;
        sorter.extend(&scan.keys, pool)
    })?;
    let used_path = config.scratch.join("used.idx");
    let sort_runs = sorter.export_used(&used_path, pool)?;
    let used = DiskIndex::open(&used_path, 8)?;
    let mut locations = writer(&config.scratch.join("locations.idx"))?;
    let mut last_node = None;
    let node_end = if boundary == 0 {
        data.len()
    } else {
        (boundary + 1).min(data.len())
    };
    pipeline(
        config,
        pool,
        &data[..node_end],
        |offset, block| {
            let nodes_only = boundary > 0 && data.get(boundary).is_none_or(|&end| offset < end);
            scan_nodes(block, &used, nodes_only)
        },
        |scan| {
            if let Some((first, last)) = scan.range {
                ensure!(last_node.is_none_or(|id| first > id), UNSORTED_NODES);
                last_node = Some(last);
            }
            locations.write_all(&scan.records)?;
            Ok(())
        },
    )?;
    locations.flush()?;
    drop(used);
    fs::remove_file(used_path)?;
    Ok((way_blocks, ways, sort_runs))
}

struct PreparedEdge {
    u: i64,
    v: i64,
    points: Range<usize>,
    reverse: bool,
    bbox: [f64; 4],
    attributes: Attributes,
}

struct PreparedWay {
    strings: [Option<String>; STRINGS.len()],
    edges: Range<usize>,
}

#[derive(Default)]
struct Prepared {
    ways: Vec<PreparedWay>,
    edges: Vec<PreparedEdge>,
    points: Vec<[f64; 2]>,
}

/// Great-circle metres along a polyline, on the IUGG mean Earth radius.
fn length_m(points: &[[f64; 2]]) -> f64 {
    points
        .windows(2)
        .map(|pair| {
            let [lon1, lat1] = pair[0].map(f64::to_radians);
            let [lon2, lat2] = pair[1].map(f64::to_radians);
            let chord = ((lat2 - lat1) / 2.0).sin().powi(2)
                + lat1.cos() * lat2.cos() * ((lon2 - lon1) / 2.0).sin().powi(2);
            2.0 * 6_371_008.8 * chord.sqrt().asin()
        })
        .sum()
}

fn prepare_way(way: Way<'_>, nodes: &DiskIndex, out: &mut Prepared) -> Result<()> {
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
    let base = out.points.len();
    let mut junction = Vec::with_capacity(refs.len());
    for &id in &refs {
        let (point, is_junction) = nodes.location(id).with_context(|| {
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
        out.points.push(point);
        junction.push(is_junction);
    }
    let tags: Tags = way
        .tags()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    let mut attributes: Attributes = [0; size_of::<Attributes>()];
    attributes[..8].copy_from_slice(&way.id().to_le_bytes());
    attributes[8..16].copy_from_slice(&tags::width(&tags).to_le_bytes());
    attributes[16] = highway;
    for (offset, name, kind) in [
        (21, "maxweight", "weight"),
        (25, "maxheight", "length"),
        (54, "maxwidth", "length"),
        (58, "maxlength", "length"),
    ] {
        attributes[offset..offset + 4]
            .copy_from_slice(&tags::limit(tags::get(&tags, name), kind).to_le_bytes());
    }
    let speed = |key| tags::limit(tags::get(&tags, key), "speed");
    let directed_speed = |key| {
        let value = speed(key);
        if value.is_nan() {
            speed("maxspeed")
        } else {
            value
        }
    };
    let forward_speed = directed_speed("maxspeed:forward");
    let backward_speed = directed_speed("maxspeed:backward");
    attributes[29] = tags::flags(&tags);
    let strings = STRINGS.map(|name| tags::get(&tags, name).map(str::to_owned));
    let direction = tags::direction(&tags);
    let first_edge = out.edges.len();
    let mut start = 0;
    for end in 1..refs.len() {
        if !junction[end] {
            continue;
        }
        let points = base + start..base + end + 1;
        let segment = &out.points[points.clone()];
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
        attributes[50..54].copy_from_slice(&(length_m(segment) as f32).to_le_bytes());
        if direction >= 0 {
            let (u, v) = (refs[start], refs[end]);
            attributes[17..21].copy_from_slice(&forward_speed.to_le_bytes());
            out.edges.push(PreparedEdge {
                u,
                v,
                points: points.clone(),
                reverse: false,
                bbox,
                attributes,
            });
        }
        if direction <= 0 {
            let (u, v) = (refs[end], refs[start]);
            attributes[17..21].copy_from_slice(&backward_speed.to_le_bytes());
            out.edges.push(PreparedEdge {
                u,
                v,
                points: points.clone(),
                reverse: true,
                bbox,
                attributes,
            });
        }
        start = end;
    }
    let edges = first_edge..out.edges.len();
    out.ways.push(PreparedWay { strings, edges });
    Ok(())
}

fn prepare_block(block: PrimitiveBlock, nodes: &DiskIndex) -> Result<Prepared> {
    let mut prepared = Prepared::default();
    for group in block.groups() {
        for way in group.ways() {
            prepare_way(way, nodes, &mut prepared)?;
        }
    }
    Ok(prepared)
}

pub fn run(config: Config) -> Result<String> {
    ensure!(
        config.degrees.is_finite() && config.degrees > 0.0 && config.degrees <= 180.0,
        "tile_degrees must be finite and in (0, 180]"
    );
    ensure!(
        config.threads > 0 && config.cache_mb > 0 && config.sort_mb > 0 && config.spool_mb > 0,
        "threads, cache_mb, sort_mb and spool_mb must be positive"
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
    let (source, data, boundary) = layout(&config)?;
    let (way_blocks, ways, sort_runs) = topology(&config, &pool, &data, boundary)?;
    let topology_seconds = phase.elapsed().as_secs_f64();
    let phase = Instant::now();
    log("Pass 2/2: splitting ways and spooling directed edges (Rust)");
    let locations_path = config.scratch.join("locations.idx");
    let nodes = DiskIndex::open(&locations_path, 24)?;
    let db = database(&config.scratch.join("output.sqlite"), config.cache_mb)?;
    let mut dictionaries = Dictionaries::new(&db)?;
    let mut spools = Spools::new(&config.scratch, config.degrees, config.spool_mb << 20)?;
    db.execute_batch("BEGIN")?;
    pipeline(
        &config,
        &pool,
        &way_blocks,
        |_, block| prepare_block(block, &nodes),
        |prepared| {
            for way in &prepared.ways {
                let mut codes = [0; STRINGS.len()];
                for (i, value) in way.strings.iter().enumerate() {
                    codes[i] = dictionaries.code(i, value.as_deref())?;
                }
                for edge in &prepared.edges[way.edges.clone()] {
                    let mut attributes = edge.attributes;
                    for (slot, code) in STRING_SLOTS.iter().zip(codes) {
                        attributes[*slot..slot + 4].copy_from_slice(&code.to_le_bytes());
                    }
                    let segment = &prepared.points[edge.points.clone()];
                    spools.append(
                        edge.u,
                        edge.v,
                        segment,
                        edge.reverse,
                        &attributes,
                        edge.bbox,
                    )?;
                }
            }
            Ok(())
        },
    )?;
    spools.close()?;
    db.execute_batch("COMMIT")?;
    drop(nodes);
    fs::remove_file(locations_path)?;
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
        "format": "osm-road-prep", "version": 2, "crs": "EPSG:4326",
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
        "settings": {"tile_degrees": config.degrees, "cache_mb": config.cache_mb, "sort_mb": config.sort_mb,
                     "spool_mb": config.spool_mb, "threads": config.threads},
        "buffers": {"sort_runs": sort_runs, "spool_flushes": spools.flushes},
        "implementation": "rust",
    }))?)
}
