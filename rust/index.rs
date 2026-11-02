use anyhow::{Result, ensure};
use memmap2::Mmap;
use rayon::prelude::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// Index keys hold a node ID above these flag bits, so they sort by ID.
pub const KEY_BITS: u32 = 2;
const JUNCTION: i64 = 1;
pub const TAGGED: i64 = 2;

pub fn writer(path: &Path) -> Result<BufWriter<File>> {
    Ok(BufWriter::with_capacity(256 * 1024, File::create(path)?))
}

/// Immutable sorted fixed-width records backed by a private scratch file.
pub struct DiskIndex {
    map: Option<Mmap>,
    stride: usize,
}

impl DiskIndex {
    pub fn open(path: &Path, stride: usize) -> Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        ensure!(
            size % stride as u64 == 0,
            "Truncated index: {}",
            path.display()
        );
        let map = if size == 0 {
            None
        } else {
            // SAFETY: we exclusively own the private scratch directory. Writers have
            // flushed and closed, and no file is mutated/truncated while mapped.
            Some(unsafe { Mmap::map(&file)? })
        };
        Ok(Self { map, stride })
    }

    fn find(&self, id: i64) -> Option<&[u8]> {
        let data = self.map.as_ref()?;
        let (mut lo, mut hi) = (0, data.len() / self.stride);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let start = mid * self.stride;
            let key = i64::from_le_bytes(data[start..start + 8].try_into().unwrap()) >> KEY_BITS;
            match key.cmp(&id) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(&data[start..start + self.stride]),
            }
        }
        None
    }

    /// Location and flags, read from a single record.
    pub fn location(&self, id: i64) -> Option<Location> {
        let record = self.find(id)?;
        Some(Location {
            point: [
                f64::from_le_bytes(record[8..16].try_into().unwrap()),
                f64::from_le_bytes(record[16..24].try_into().unwrap()),
            ],
            junction: i64::from(record[0]) & JUNCTION != 0,
            tagged: i64::from(record[0]) & TAGGED != 0,
        })
    }

    pub fn node_tags(&self, id: i64) -> Option<NodeTags> {
        let record = self.find(id)?;
        let float = |start: usize| f32::from_le_bytes(record[start..start + 4].try_into().unwrap());
        let barrier = u32::from_le_bytes(record[8..12].try_into().unwrap());
        Some(NodeTags {
            barrier: (barrier != u32::MAX).then_some(barrier),
            limits: [float(12), float(16), float(20)],
        })
    }

    pub fn len(&self) -> usize {
        self.map.as_ref().map_or(0, |data| data.len() / self.stride)
    }

    pub fn key(&self, index: usize) -> i64 {
        let data = self.map.as_ref().unwrap();
        let start = index * self.stride;
        i64::from_le_bytes(data[start..start + 8].try_into().unwrap())
    }

    pub fn lower_bound(&self, id: i64) -> usize {
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.key(mid) >> KEY_BITS < id {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
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

    pub fn record(&self, key: i64) -> [u8; Self::RECORD_BYTES] {
        let mut record = [0u8; Self::RECORD_BYTES];
        record[..8].copy_from_slice(&key.to_le_bytes());
        record[8..12].copy_from_slice(&self.barrier.unwrap_or(u32::MAX).to_le_bytes());
        for (i, limit) in self.limits.iter().enumerate() {
            record[12 + i * 4..16 + i * 4].copy_from_slice(&limit.to_le_bytes());
        }
        record
    }
}

/// External sort of `(id << KEY_BITS) | endpoint`, one entry per way using the node.
pub struct RefSorter {
    scratch: PathBuf,
    buffer: Vec<i64>,
    limit: usize,
    runs: Vec<PathBuf>,
}

impl RefSorter {
    pub fn new(scratch: &Path, bytes: usize) -> Self {
        Self {
            scratch: scratch.to_owned(),
            buffer: Vec::with_capacity(bytes / 8),
            limit: bytes / 8,
            runs: Vec::new(),
        }
    }

    pub fn extend(&mut self, values: &[i64], pool: &rayon::ThreadPool) -> Result<()> {
        let mut values = values;
        while !values.is_empty() {
            let take = values.len().min(self.limit - self.buffer.len());
            self.buffer.extend_from_slice(&values[..take]);
            values = &values[take..];
            if self.buffer.len() == self.limit {
                self.spill(pool)?;
            }
        }
        Ok(())
    }

    fn spill(&mut self, pool: &rayon::ThreadPool) -> Result<()> {
        pool.install(|| self.buffer.par_sort_unstable());
        let path = self.scratch.join(format!("refs.{}", self.runs.len()));
        let mut file = writer(&path)?;
        for value in &self.buffer {
            file.write_all(&value.to_le_bytes())?;
        }
        file.flush()?;
        self.runs.push(path);
        self.buffer.clear();
        Ok(())
    }

    /// Merge runs into sorted unique keys for every referenced node, flagging junctions.
    /// Returns the number of sorted runs, which shows whether `sort_mb` was exceeded.
    pub fn export_used(mut self, path: &Path, pool: &rayon::ThreadPool) -> Result<usize> {
        if !self.buffer.is_empty() {
            self.spill(pool)?;
        }
        self.buffer = Vec::new();
        let mut runs = self
            .runs
            .iter()
            .map(|run| Ok(BufReader::with_capacity(64 * 1024, File::open(run)?)))
            .collect::<Result<Vec<_>>>()?;
        let mut heap = BinaryHeap::with_capacity(runs.len());
        for (i, run) in runs.iter_mut().enumerate() {
            if let Some(value) = read_i64(run)? {
                heap.push(Reverse((value, i)));
            }
        }
        let mut out = writer(path)?;
        let mut current = None;
        let (mut uses, mut endpoint) = (0u64, false);
        while let Some(Reverse((value, i))) = heap.pop() {
            let id = value >> KEY_BITS;
            if current != Some(id) {
                if let Some(id) = current {
                    let key = (id << KEY_BITS) | i64::from(uses >= 2 || endpoint);
                    out.write_all(&key.to_le_bytes())?;
                }
                current = Some(id);
                (uses, endpoint) = (0, false);
            }
            uses += 1;
            endpoint |= value & JUNCTION != 0;
            if let Some(next) = read_i64(&mut runs[i])? {
                heap.push(Reverse((next, i)));
            }
        }
        if let Some(id) = current {
            let key = (id << KEY_BITS) | i64::from(uses >= 2 || endpoint);
            out.write_all(&key.to_le_bytes())?;
        }
        out.flush()?;
        for run in &self.runs {
            std::fs::remove_file(run)?;
        }
        Ok(self.runs.len())
    }
}

fn read_i64(reader: &mut impl Read) -> Result<Option<i64>> {
    let mut bytes = [0u8; 8];
    match reader.read_exact(&mut bytes) {
        Ok(()) => Ok(Some(i64::from_le_bytes(bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(error) => Err(error.into()),
    }
}
