use anyhow::{Result, ensure};
use memmap2::Mmap;
use rayon::prelude::*;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Node IDs per leaf is `1 << LEAF_BITS`. A small extract's nodes are scattered across the
/// whole ID space, so leaves must be small for it to allocate little.
const LEAF_BITS: u32 = 12;
const LEAF_WORDS: usize = 1 << (LEAF_BITS - 6);
/// Words a rank has to count at most, which keeps lookups in `used` short.
const RANK_WORDS: usize = 8;
/// Keeps the table of leaves small enough to allocate; OSM IDs reach about 2^34.
const MAX_ID_SPAN: u64 = 1 << 40;

pub const LOCATION_BYTES: usize = 16;
/// A location record holds latitude above these flags. Unwritten records read as zero.
const FLAG_BITS: u32 = 2;
const PRESENT: i64 = 1;
const TAGGED: i64 = 2;

pub fn writer(path: &Path) -> Result<BufWriter<File>> {
    Ok(BufWriter::with_capacity(256 * 1024, File::create(path)?))
}

/// One location record, from nanodegrees. Clamping keeps a wild latitude out of range
/// rather than letting the flags overflow it.
pub fn location_record(lon: i64, lat: i64, tagged: bool) -> [u8; LOCATION_BYTES] {
    let lat =
        (lat.clamp(-1 << 60, 1 << 60) << FLAG_BITS) | PRESENT | if tagged { TAGGED } else { 0 };
    let mut record = [0u8; LOCATION_BYTES];
    record[..8].copy_from_slice(&lon.to_le_bytes());
    record[8..].copy_from_slice(&lat.to_le_bytes());
    record
}

struct Leaf {
    used: [AtomicU64; LEAF_WORDS],
    junction: [AtomicU64; LEAF_WORDS],
}

impl Default for Leaf {
    fn default() -> Self {
        Self {
            used: std::array::from_fn(|_| AtomicU64::new(0)),
            junction: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

fn used_in(words: &[AtomicU64]) -> u16 {
    words
        .iter()
        .map(|word| word.load(Relaxed).count_ones() as u16)
        .sum()
}

/// Used nodes before a leaf, and within it before each `RANK_WORDS` of `used`. These sit
/// beside the leaves because a leaf one byte larger takes a much larger allocation.
#[derive(Clone, Copy)]
struct LeafRank {
    before: u64,
    within: [u16; LEAF_WORDS / RANK_WORDS],
}

/// One bit per node ID for use by a drivable way and one for being a junction, so ways can
/// mark nodes from any thread. A node's rank among the used IDs then locates its record.
pub struct NodeIds {
    first: i64,
    span: u64,
    leaves: Vec<OnceLock<Box<Leaf>>>,
    /// One per leaf, filled in by `count`.
    ranks: Vec<LeafRank>,
}

impl NodeIds {
    pub fn new(first: i64, last: i64) -> Result<Self> {
        let span = last.abs_diff(first);
        ensure!(
            span < MAX_ID_SPAN,
            "Node IDs from {first} to {last} span too wide a range"
        );
        let leaves = (span >> LEAF_BITS) as usize + 1;
        Ok(Self {
            first,
            span,
            leaves: (0..leaves).map(|_| OnceLock::new()).collect(),
            ranks: Vec::new(),
        })
    }

    /// Where an ID's bit lives, or None outside the IDs that exist.
    fn position(&self, id: i64) -> Option<(usize, usize, u64)> {
        let offset = id.wrapping_sub(self.first) as u64;
        (offset <= self.span).then(|| {
            let word = (offset >> 6) as usize & (LEAF_WORDS - 1);
            ((offset >> LEAF_BITS) as usize, word, 1 << (offset & 63))
        })
    }

    /// Record one way's use of a node, which is a junction if another way used it first or
    /// it ends this one. An ID that does not exist is reported when its way is built.
    pub fn mark(&self, id: i64, endpoint: bool) {
        let Some((leaf, word, bit)) = self.position(id) else {
            return;
        };
        let leaf = self.leaves[leaf].get_or_init(Box::default);
        let seen = leaf.used[word].fetch_or(bit, Relaxed) & bit != 0;
        if seen || endpoint {
            leaf.junction[word].fetch_or(bit, Relaxed);
        }
    }

    /// Finish marking, returning how many nodes are used.
    pub fn count(&mut self, pool: &rayon::ThreadPool) -> usize {
        let count = |leaf: &OnceLock<Box<Leaf>>| {
            let mut rank = LeafRank {
                before: 0,
                within: [0; LEAF_WORDS / RANK_WORDS],
            };
            if let Some(leaf) = leaf.get() {
                let mut total = 0;
                let groups = rank.within.iter_mut().zip(leaf.used.chunks(RANK_WORDS));
                for (within, words) in groups {
                    *within = total;
                    total += used_in(words);
                }
                rank.before = u64::from(total);
            }
            rank
        };
        self.ranks = pool.install(|| self.leaves.par_iter().map(count).collect());
        let mut total = 0;
        for rank in &mut self.ranks {
            total += std::mem::replace(&mut rank.before, total);
        }
        total as usize
    }

    /// A used node's position in ID order and whether it is a junction.
    pub fn rank(&self, id: i64) -> Option<(usize, bool)> {
        let (index, word, bit) = self.position(id)?;
        let leaf = self.leaves[index].get()?;
        let used = leaf.used[word].load(Relaxed);
        if used & bit == 0 {
            return None;
        }
        let (rank, group) = (&self.ranks[index], word / RANK_WORDS);
        let within = rank.within[group] + used_in(&leaf.used[group * RANK_WORDS..word]);
        let before = rank.before + u64::from(within) + u64::from((used & (bit - 1)).count_ones());
        let junction = leaf.junction[word].load(Relaxed) & bit != 0;
        Some((before as usize, junction))
    }
}

/// Node coordinates by rank, in a private scratch file.
pub struct NodeIndex {
    ids: NodeIds,
    map: Option<Mmap>,
}

impl NodeIndex {
    pub fn open(ids: NodeIds, path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let map = if file.metadata()?.len() == 0 {
            None
        } else {
            // SAFETY: we exclusively own the private scratch directory. Writers have
            // flushed and closed, and no file is mutated/truncated while mapped.
            Some(unsafe { Mmap::map(&file)? })
        };
        Ok(Self { ids, map })
    }

    /// Location and flags, or None for a node the extract does not contain.
    pub fn location(&self, id: i64) -> Option<Location> {
        let (rank, junction) = self.ids.rank(id)?;
        let record = &self.map.as_ref()?[rank * LOCATION_BYTES..][..LOCATION_BYTES];
        let lon = i64::from_le_bytes(record[..8].try_into().unwrap());
        let lat = i64::from_le_bytes(record[8..].try_into().unwrap());
        (lat & PRESENT != 0).then(|| Location {
            point: [lon as f64 / 1e9, (lat >> FLAG_BITS) as f64 / 1e9],
            junction,
            tagged: lat & TAGGED != 0,
        })
    }
}

pub struct Location {
    pub point: [f64; 2],
    pub junction: bool,
    pub tagged: bool,
}

/// What a node's own tags add to the edges using it. Limits are height, weight, width.
#[derive(Clone, Copy)]
pub struct NodeTags {
    pub barrier: Option<u32>,
    pub limits: [f32; 3],
}

impl NodeTags {
    pub const RECORD_BYTES: usize = 24;

    pub fn record(&self, id: i64) -> [u8; Self::RECORD_BYTES] {
        let mut record = [0u8; Self::RECORD_BYTES];
        record[..8].copy_from_slice(&id.to_le_bytes());
        record[8..12].copy_from_slice(&self.barrier.unwrap_or(u32::MAX).to_le_bytes());
        for (i, limit) in self.limits.iter().enumerate() {
            record[12 + i * 4..16 + i * 4].copy_from_slice(&limit.to_le_bytes());
        }
        record
    }
}

/// The few nodes with tags that matter, sorted by ID in a private scratch file.
pub struct NodeTagIndex {
    map: Option<Mmap>,
}

impl NodeTagIndex {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        ensure!(
            size % NodeTags::RECORD_BYTES as u64 == 0,
            "Truncated index: {}",
            path.display()
        );
        let map = if size == 0 {
            None
        } else {
            // SAFETY: as for `NodeIndex`.
            Some(unsafe { Mmap::map(&file)? })
        };
        Ok(Self { map })
    }

    pub fn node_tags(&self, id: i64) -> Option<NodeTags> {
        let data = self.map.as_ref()?;
        let stride = NodeTags::RECORD_BYTES;
        let (mut lo, mut hi) = (0, data.len() / stride);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let record = &data[mid * stride..][..stride];
            let key = i64::from_le_bytes(record[..8].try_into().unwrap());
            match key.cmp(&id) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let float = |start: usize| {
                        f32::from_le_bytes(record[start..start + 4].try_into().unwrap())
                    };
                    let barrier = u32::from_le_bytes(record[8..12].try_into().unwrap());
                    return Some(NodeTags {
                        barrier: (barrier != u32::MAX).then_some(barrier),
                        limits: [float(12), float(16), float(20)],
                    });
                }
            }
        }
        None
    }
}
