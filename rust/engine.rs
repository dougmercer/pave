use crate::index::{
    LOCATION_BYTES, NodeIds, NodeIndex, NodeTagIndex, NodeTags, location_record, writer,
};
use crate::pbf::{Blob, Block, Header, Node, Pbf, Way};
use crate::storage::{Attributes, COLUMNS, Dictionaries, STRING_SLOTS, Spools};
use crate::tags::{self, HIGHWAYS, STRINGS, WAY_STRINGS};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, SecondsFormat};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::Instant;

const UNSORTED_NODES: &str = "Nodes must have unique ascending IDs; use a sorted snapshot extract";
const UNSORTED_FILE: &str = "PBF header claims Sort.Type_then_ID but elements are not sorted";
const UNSORTED_WAYS: &str = "Ways must have unique ascending IDs; use a sorted snapshot extract";

pub struct Config {
    pub input: PathBuf,
    pub output: PathBuf,
    pub scratch: PathBuf,
    pub degrees: f64,
    /// Edge rows and vertices buffered across all tiles before spilling to scratch.
    pub spool_mb: usize,
    pub threads: usize,
    pub quiet: bool,
    /// Polled between blocks on the calling thread, so Ctrl-C is not deferred to the end.
    pub interrupted: Box<dyn Fn() -> Result<()> + Send + Sync>,
}

/// Run `work` on the decode pool, feeding results to `consume` in source order.
/// Blocks are read, decoded, transformed and dropped by workers; the caller only does ordered I/O.
fn pipeline<T: Send>(
    pbf: &Pbf,
    config: &Config,
    pool: &rayon::ThreadPool,
    blobs: &[Blob],
    work: impl Fn(Blob, Block) -> Result<T> + Sync,
    mut consume: impl FnMut(T) -> Result<()>,
) -> Result<()> {
    // One result slot per block in flight, so a slow block never idles the other workers.
    let (sender, receiver) = sync_channel::<Receiver<Result<T>>>(config.threads * 2);
    let work = &work;
    std::thread::scope(|scope| {
        let producer = scope.spawn(move || {
            pool.in_place_scope(|tasks| {
                for &blob in blobs {
                    let (result, slot) = sync_channel(1);
                    if sender.send(slot).is_err() {
                        break;
                    }
                    tasks.spawn(move |_| {
                        let block = pbf.block(blob);
                        let _ = result.send(block.and_then(|block| work(blob, block)));
                    });
                }
            })
        });
        let consumed = (|| {
            for slot in receiver {
                (config.interrupted)()?;
                // A closed slot means its worker panicked, which joining the producer reports.
                let Ok(item) = slot.recv() else { break };
                consume(item?)?;
            }
            Ok(())
        })();
        producer.join().expect("decode thread panicked");
        consumed
    })
}

/// Stream blob headers, read the OSM header, and find where node blocks end.
fn layout(pbf: &Pbf, path: &Path) -> Result<(Value, Vec<Blob>, usize)> {
    let (header, data) = pbf.blobs()?;
    let header = pbf.header(header.context("No OSM header in input PBF")?)?;
    let source = source_header(&header, path)?;
    let sorted = header
        .optional_features
        .iter()
        .any(|f| f == "Sort.Type_then_ID");
    // Sorted files keep nodes first, so the node/way boundary can be found by bisection.
    let mut boundary = 0;
    if sorted {
        let mut high = data.len();
        while boundary < high {
            let mid = boundary + (high - boundary) / 2;
            if pbf.block(data[mid])?.has_ways_or_relations()? {
                high = mid;
            } else {
                boundary = mid + 1;
            }
        }
    }
    Ok((source, data, boundary))
}

fn source_header(header: &Header, path: &Path) -> Result<Value> {
    for feature in &header.required_features {
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
        .replication_timestamp
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
        "generator": header.writing_program,
    }))
}

/// The drivable class of a way, if it has one.
fn highway(way: &Way<'_>) -> Option<u8> {
    way.tags().find_map(|(key, value)| {
        if key == b"highway" {
            tags::highway_code(value)
        } else {
            None
        }
    })
}

struct WayScan {
    blob: Blob,
    ways: u64,
    range: Option<(i64, i64)>,
}

fn scan_ways(blob: Blob, block: Block, ids: &NodeIds) -> Result<WayScan> {
    let mut scan = WayScan {
        blob,
        ways: 0,
        range: None,
    };
    let mut refs = Vec::new();
    for way in block.ways() {
        let way = way?;
        scan.range = match scan.range {
            None => Some((way.id, way.id)),
            Some((first, last)) => {
                ensure!(way.id > last, UNSORTED_WAYS);
                Some((first, way.id))
            }
        };
        if highway(&way).is_none() {
            continue;
        }
        scan.ways += 1;
        way.read_refs(&mut refs)?;
        if refs.is_empty() {
            continue;
        }
        let first = refs[0];
        let last = *refs.last().unwrap();
        refs.sort_unstable();
        refs.dedup(); // Distinct ways, not repeated occurrences within a way.
        for &id in &refs {
            ids.mark(id, id == first || id == last);
        }
    }
    Ok(scan)
}

#[derive(Default)]
struct NodeScan {
    /// Location records in runs, each with its byte offset in the location file.
    runs: Vec<(usize, Vec<u8>)>,
    tagged: Vec<(i64, Option<String>, [f32; 3])>,
    range: Option<(i64, i64)>,
}

/// The barrier and limits on a node, if it has any.
fn read_node_tags<'a>(
    node_tags: impl Iterator<Item = (&'a str, &'a str)>,
) -> Option<(Option<String>, [f32; 3])> {
    let (mut barrier, mut limits) = (None, [f32::NAN; 3]);
    for (key, value) in node_tags {
        match key {
            "barrier" => barrier = Some(value.to_owned()),
            "maxheight" => limits[0] = tags::limit(Some(value), "length"),
            "maxweight" => limits[1] = tags::limit(Some(value), "weight"),
            "maxwidth" => limits[2] = tags::limit(Some(value), "length"),
            _ => (),
        }
    }
    (barrier.is_some() || limits.iter().any(|limit| !limit.is_nan())).then_some((barrier, limits))
}

impl NodeScan {
    fn visit(&mut self, ids: &NodeIds, node: Node<'_>) -> Result<()> {
        let id = node.id;
        self.range = match self.range {
            None => Some((id, id)),
            Some((first, last)) => {
                ensure!(id > last, UNSORTED_NODES);
                Some((first, id))
            }
        };
        let Some((rank, _)) = ids.rank(id) else {
            return Ok(());
        };
        let found = read_node_tags(node.tags.text());
        let record = location_record(node.lon, node.lat, found.is_some());
        if let Some((barrier, limits)) = found {
            self.tagged.push((id, barrier, limits));
        }
        // Only a missing node can break a run, because IDs ascend.
        let start = rank * LOCATION_BYTES;
        match self.runs.last_mut() {
            Some((offset, bytes)) if *offset + bytes.len() == start => {
                bytes.extend_from_slice(&record)
            }
            _ => self.runs.push((start, record.to_vec())),
        }
        Ok(())
    }
}

fn scan_nodes(block: Block, ids: &NodeIds, nodes_only: bool) -> Result<NodeScan> {
    // A file that lies about its sort order must not silently lose ways.
    ensure!(
        !nodes_only || !block.has_ways_or_relations()?,
        UNSORTED_FILE
    );
    let mut scan = NodeScan::default();
    block.nodes(|node| scan.visit(ids, node))?;
    Ok(scan)
}

/// The lowest and highest node IDs. Sorted files keep them in the blocks at each end of
/// their nodes; other files can have them anywhere.
fn node_id_range(
    pbf: &Pbf,
    config: &Config,
    pool: &rayon::ThreadPool,
    data: &[Blob],
    boundary: usize,
) -> Result<Option<(i64, i64)>> {
    let blobs: Vec<Blob> = if boundary == 0 {
        data.to_vec()
    } else {
        let ends = [0, boundary - 1, boundary];
        ends.iter().filter_map(|&i| data.get(i).copied()).collect()
    };
    let widen = |range: Option<(i64, i64)>, low: i64, high: i64| {
        Some(range.map_or((low, high), |(a, b)| (a.min(low), b.max(high))))
    };
    let mut range = None;
    pipeline(
        pbf,
        config,
        pool,
        &blobs,
        |_, block| {
            let mut range = None;
            block.nodes(|node| {
                range = widen(range, node.id, node.id);
                Ok(())
            })?;
            Ok(range)
        },
        |found| {
            if let Some((low, high)) = found {
                range = widen(range, low, high);
            }
            Ok(())
        },
    )?;
    Ok(range)
}

fn topology(
    pbf: &Pbf,
    config: &Config,
    pool: &rayon::ThreadPool,
    data: &[Blob],
    boundary: usize,
) -> Result<(Vec<Blob>, u64, NodeIds, Vec<String>)> {
    let (first, last) = node_id_range(pbf, config, pool, data, boundary)?.unwrap_or((0, 0));
    let mut ids = NodeIds::new(first, last)?;
    let (mut way_blocks, mut ways, mut last_way) = (Vec::new(), 0, None);
    pipeline(
        pbf,
        config,
        pool,
        &data[boundary..],
        |blob, block| scan_ways(blob, block, &ids),
        |scan| {
            if let Some((first, last)) = scan.range {
                ensure!(last_way.is_none_or(|id| first > id), UNSORTED_WAYS);
                last_way = Some(last);
                way_blocks.push(scan.blob);
            }
            ways += scan.ways;
            Ok(())
        },
    )?;
    let used = ids.count(pool);
    let locations = File::create(config.scratch.join("locations.idx"))?;
    locations.set_len((used * LOCATION_BYTES) as u64)?;
    let mut node_tags = writer(&config.scratch.join("node_tags.idx"))?;
    // Distinct barrier values are few, so workers can share them as a table in RAM.
    let (mut barriers, mut barrier_codes) = (Vec::new(), HashMap::new());
    let mut last_node = None;
    let node_end = if boundary == 0 {
        data.len()
    } else {
        (boundary + 1).min(data.len())
    };
    pipeline(
        pbf,
        config,
        pool,
        &data[..node_end],
        |blob, block| {
            let nodes_only = boundary > 0
                && data
                    .get(boundary)
                    .is_none_or(|end| blob.offset < end.offset);
            scan_nodes(block, &ids, nodes_only)
        },
        |scan| {
            if let Some((first, last)) = scan.range {
                ensure!(last_node.is_none_or(|id| first > id), UNSORTED_NODES);
                last_node = Some(last);
            }
            for (offset, bytes) in &scan.runs {
                locations.write_all_at(bytes, *offset as u64)?;
            }
            for (id, barrier, limits) in scan.tagged {
                let barrier = barrier.map(|value| {
                    *barrier_codes.entry(value.clone()).or_insert_with(|| {
                        barriers.push(value);
                        barriers.len() as u32 - 1
                    })
                });
                node_tags.write_all(&NodeTags { barrier, limits }.record(id))?;
            }
            Ok(())
        },
    )?;
    node_tags.flush()?;
    Ok((way_blocks, ways, ids, barriers))
}

struct PreparedEdge {
    u: i64,
    v: i64,
    points: Range<usize>,
    reverse: bool,
    bbox: [f64; 4],
    attributes: Attributes,
    barrier: Option<String>,
    barrier_at_v: Option<String>,
}

struct PreparedWay {
    strings: [Option<String>; WAY_STRINGS],
    edges: Range<usize>,
}

/// Read-only indexes the workers share while building edges.
struct Lookups<'a> {
    nodes: &'a NodeIndex,
    node_tags: &'a NodeTagIndex,
    barriers: &'a [String],
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

fn prepare_way(way: Way<'_>, lookups: &Lookups<'_>, out: &mut Prepared) -> Result<()> {
    let Some(highway) = highway(&way) else {
        return Ok(());
    };
    let mut refs = Vec::new();
    way.read_refs(&mut refs)?;
    if refs.len() < 2 {
        return Ok(());
    }
    let base = out.points.len();
    let mut junction = Vec::with_capacity(refs.len());
    let mut node_tags = Vec::with_capacity(refs.len());
    for &id in &refs {
        let location = lookups.nodes.location(id).with_context(|| {
            format!(
                "Drivable way {} references missing node {id}; use a complete extract",
                way.id
            )
        })?;
        let point = location.point;
        ensure!(
            point[0].is_finite()
                && point[1].is_finite()
                && point[0].abs() <= 180.0
                && point[1].abs() <= 90.0,
            "Drivable way {} has invalid WGS84 coordinates at node {id}",
            way.id
        );
        out.points.push(point);
        junction.push(location.junction);
        node_tags.push(if location.tagged {
            lookups.node_tags.node_tags(id)
        } else {
            None
        });
    }
    let tags: Vec<_> = way.tags().text().collect();
    let tags = tags.as_slice();
    let mut attributes: Attributes = [0; size_of::<Attributes>()];
    attributes[..8].copy_from_slice(&way.id.to_le_bytes());
    attributes[8..16].copy_from_slice(&tags::width(tags).to_le_bytes());
    attributes[16] = highway;
    for (offset, name, kind) in [
        (21, "maxweight", "weight"),
        (25, "maxheight", "length"),
        (54, "maxwidth", "length"),
        (58, "maxlength", "length"),
    ] {
        attributes[offset..offset + 4]
            .copy_from_slice(&tags::limit(tags::get(tags, name), kind).to_le_bytes());
    }
    let speed = |key| tags::limit(tags::get(tags, key), "speed");
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
    attributes[29] = tags::flags(tags);
    let strings = std::array::from_fn(|i| tags::get(tags, STRINGS[i]).map(str::to_owned));
    let limit = |name, kind| tags::limit(tags::get(tags, name), kind);
    let way_limits = [
        limit("maxheight", "length"),
        limit("maxweight", "weight"),
        limit("maxwidth", "length"),
    ];
    let direction = tags::direction(tags);
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
        let interior = &node_tags[start + 1..end];
        for (reverse, u, v, speed, arrival) in [
            (false, refs[start], refs[end], forward_speed, node_tags[end]),
            (
                true,
                refs[end],
                refs[start],
                backward_speed,
                node_tags[start],
            ),
        ] {
            if (reverse && direction > 0) || (!reverse && direction < 0) {
                continue;
            }
            let mut limits = way_limits;
            for node in interior.iter().flatten().chain(&arrival) {
                for (limit, value) in limits.iter_mut().zip(node.limits) {
                    *limit = limit.min(value); // f32::min ignores a missing (NaN) limit.
                }
            }
            for (offset, value) in [
                (25, limits[0]),
                (21, limits[1]),
                (54, limits[2]),
                (17, speed),
            ] {
                attributes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            }
            let barrier_name = |code: u32| lookups.barriers[code as usize].as_str();
            let mut barriers: Vec<_> = interior
                .iter()
                .flatten()
                .filter_map(|node| node.barrier.map(barrier_name))
                .collect();
            if reverse {
                barriers.reverse();
            }
            out.edges.push(PreparedEdge {
                u,
                v,
                points: points.clone(),
                reverse,
                bbox,
                attributes,
                barrier: (!barriers.is_empty()).then(|| barriers.join(";")),
                barrier_at_v: arrival
                    .and_then(|node| node.barrier)
                    .map(|code| barrier_name(code).to_owned()),
            });
        }
        start = end;
    }
    let edges = first_edge..out.edges.len();
    out.ways.push(PreparedWay { strings, edges });
    Ok(())
}

fn prepare_block(block: Block, lookups: &Lookups<'_>) -> Result<Prepared> {
    let mut prepared = Prepared::default();
    for way in block.ways() {
        prepare_way(way?, lookups, &mut prepared)?;
    }
    Ok(prepared)
}

pub fn run(config: Config) -> Result<String> {
    ensure!(
        config.degrees.is_finite() && config.degrees > 0.0 && config.degrees <= 180.0,
        "tile_degrees must be finite and in (0, 180]"
    );
    ensure!(
        config.threads > 0 && config.spool_mb > 0,
        "threads and spool_mb must be positive"
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
    let pbf = Pbf::open(&config.input)?;
    let (source, data, boundary) = layout(&pbf, &config.input)?;
    let (way_blocks, ways, ids, barriers) = topology(&pbf, &config, &pool, &data, boundary)?;
    let topology_seconds = phase.elapsed().as_secs_f64();
    let phase = Instant::now();
    log("Pass 2/2: splitting ways and spooling directed edges (Rust)");
    let locations_path = config.scratch.join("locations.idx");
    let node_tags_path = config.scratch.join("node_tags.idx");
    let nodes = NodeIndex::open(ids, &locations_path)?;
    let node_tags = NodeTagIndex::open(&node_tags_path)?;
    let lookups = Lookups {
        nodes: &nodes,
        node_tags: &node_tags,
        barriers: &barriers,
    };
    let mut dictionaries = Dictionaries::default();
    let mut spools = Spools::new(&config.scratch, config.degrees, config.spool_mb << 20)?;
    // Strings are coded in block order here; a second thread does the slower spooling.
    let (sender, receiver) = sync_channel::<Prepared>(2);
    std::thread::scope(|scope| {
        let spooled = scope.spawn(|| -> Result<()> {
            for prepared in receiver {
                for edge in &prepared.edges {
                    let segment = &prepared.points[edge.points.clone()];
                    spools.append(
                        edge.u,
                        edge.v,
                        segment,
                        edge.reverse,
                        &edge.attributes,
                        edge.bbox,
                    )?;
                }
            }
            spools.close()
        });
        let coded = pipeline(
            &pbf,
            &config,
            &pool,
            &way_blocks,
            |_, block| prepare_block(block, &lookups),
            |mut prepared| {
                for way in &prepared.ways {
                    let mut codes = [0; STRINGS.len()];
                    for (i, value) in way.strings.iter().enumerate() {
                        codes[i] = dictionaries.code(i, value.as_deref())?;
                    }
                    for edge in &mut prepared.edges[way.edges.clone()] {
                        codes[WAY_STRINGS] =
                            dictionaries.code(WAY_STRINGS, edge.barrier.as_deref())?;
                        codes[WAY_STRINGS + 1] =
                            dictionaries.code(WAY_STRINGS + 1, edge.barrier_at_v.as_deref())?;
                        for (slot, code) in STRING_SLOTS.iter().zip(codes) {
                            edge.attributes[*slot..slot + 4].copy_from_slice(&code.to_le_bytes());
                        }
                    }
                }
                // A closed channel means spooling failed, which joining the thread reports.
                let _ = sender.send(prepared);
                Ok(())
            },
        );
        drop(sender);
        let spooled = spooled.join().expect("spool thread panicked");
        spooled.and(coded)
    })?;
    drop((nodes, node_tags));
    fs::remove_file(locations_path)?;
    fs::remove_file(node_tags_path)?;
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
    let tiles = spools.finish(&config.output, &metadata, &pool)?;
    Ok(serde_json::to_string(&json!({
        "edges": spools.edges, "vertices": spools.vertices, "tiles": tiles, "drivable_ways": ways,
        "stages": {"topology_seconds": topology_seconds, "edges_seconds": edges_seconds,
                   "columns_seconds": phase.elapsed().as_secs_f64()}, "source": source,
        "settings": {"tile_degrees": config.degrees, "spool_mb": config.spool_mb,
                     "threads": config.threads},
        "buffers": {"spool_flushes": spools.flushes},
        "implementation": "rust",
    }))?)
}
