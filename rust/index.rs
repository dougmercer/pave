use anyhow::{Context, Result, ensure};
use memmap2::Mmap;
use rusqlite::Connection;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

pub fn database(path: &Path, cache_mb: usize) -> Result<Connection> {
    let db = Connection::open(path)?;
    db.execute_batch(&format!(
        "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA temp_store=FILE;
         PRAGMA mmap_size=0; PRAGMA cache_size=-{};",
        cache_mb * 1024
    ))?;
    Ok(db)
}

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
            let key = i64::from_le_bytes(data[start..start + 8].try_into().unwrap());
            match key.cmp(&id) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(&data[start..start + self.stride]),
            }
        }
        None
    }

    pub fn contains(&self, id: i64) -> bool {
        self.find(id).is_some()
    }

    pub fn location(&self, id: i64) -> Option<[f64; 2]> {
        let record = self.find(id)?;
        Some([
            f64::from_le_bytes(record[8..16].try_into().unwrap()),
            f64::from_le_bytes(record[16..24].try_into().unwrap()),
        ])
    }
}

pub struct NodeWriter {
    file: BufWriter<File>,
    last: Option<i64>,
}

impl NodeWriter {
    pub fn new(path: &Path) -> Result<Self> {
        Ok(Self {
            file: writer(path)?,
            last: None,
        })
    }

    pub fn add(&mut self, id: i64, lon: f64, lat: f64) -> Result<()> {
        ensure!(
            self.last.is_none_or(|last| id > last),
            "Nodes must have unique ascending IDs; use a sorted snapshot extract"
        );
        self.last = Some(id);
        // Validate coordinates when a drivable way uses them, not for unrelated nodes.
        self.file.write_all(&id.to_le_bytes())?;
        self.file.write_all(&lon.to_le_bytes())?;
        self.file.write_all(&lat.to_le_bytes())?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        self.file.flush().context("Flush node location index")
    }
}

pub fn export_junctions(db: &Connection, path: &Path) -> Result<()> {
    let mut file = writer(path)?;
    let mut stmt = db.prepare("SELECT id FROM nodes WHERE uses=2 OR endpoint=1 ORDER BY id")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        file.write_all(&row.get::<_, i64>(0)?.to_le_bytes())?;
    }
    file.flush()?;
    Ok(())
}
