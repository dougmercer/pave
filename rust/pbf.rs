//! The parts of the OSM PBF format that preprocessing reads. Blocks are walked where they
//! lie, because copying every element out of them dominated the cost of decoding a file.
use anyhow::{Context, Result, anyhow, bail, ensure};
use flate2::{Decompress, FlushDecompress};
use std::cell::RefCell;
use std::fs::File;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::Path;

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BLOB_BYTES: usize = 32 * 1024 * 1024;
const TRUNCATED: &str = "Truncated data in PBF block";

const GROUP_NODE: u64 = 1;
const GROUP_DENSE: u64 = 2;
const GROUP_WAY: u64 = 3;
const GROUP_RELATION: u64 = 4;

#[inline]
fn varint(data: &mut &[u8]) -> Option<u64> {
    let mut value = 0;
    for (i, &byte) in data.iter().enumerate().take(10) {
        value |= u64::from(byte & 0x7f) << (i * 7);
        if byte < 0x80 {
            *data = &data[i + 1..];
            return Some(value);
        }
    }
    None
}

#[inline]
fn signed(value: u64) -> i64 {
    (value >> 1) as i64 ^ -((value & 1) as i64)
}

struct Field<'a> {
    number: u64,
    value: u64,
    bytes: &'a [u8],
}

/// The fields of one protobuf message, whichever it is.
struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    fn field(&mut self) -> Option<Field<'a>> {
        let tag = varint(&mut self.0)?;
        let (mut value, mut bytes) = (0, &self.0[..0]);
        let skip = match tag & 7 {
            0 => {
                value = varint(&mut self.0)?;
                0
            }
            1 => 8,
            2 => {
                let length = varint(&mut self.0)? as usize;
                bytes = self.0.get(..length)?;
                length
            }
            5 => 4,
            _ => return None,
        };
        self.0 = self.0.get(skip..)?;
        Some(Field {
            number: tag >> 3,
            value,
            bytes,
        })
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<Field<'a>>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        (!self.0.is_empty()).then(|| self.field().context(TRUNCATED))
    }
}

/// Where a blob's message sits in the file.
#[derive(Clone, Copy)]
pub struct Blob {
    pub offset: u64,
    size: usize,
}

pub struct Header {
    pub required_features: Vec<String>,
    pub optional_features: Vec<String>,
    pub writing_program: Option<String>,
    pub replication_timestamp: Option<i64>,
}

pub struct Pbf {
    file: File,
}

impl Pbf {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            file: File::open(path)?,
        })
    }

    /// Find the first header blob and every data blob, without reading their contents.
    pub fn blobs(&self) -> Result<(Option<Blob>, Vec<Blob>)> {
        let (mut header, mut data) = (None, Vec::new());
        let (end, mut offset) = (self.file.metadata()?.len(), 0);
        while offset < end {
            let mut size = [0; 4];
            self.file
                .read_exact_at(&mut size, offset)
                .context("Truncated PBF file")?;
            let size = u32::from_be_bytes(size) as usize;
            ensure!(size <= MAX_HEADER_BYTES, "PBF blob header is too large");
            let mut message = vec![0; size];
            self.file
                .read_exact_at(&mut message, offset + 4)
                .context("Truncated PBF file")?;
            let (mut kind, mut blob_size) = (&message[..0], 0);
            for field in Fields(&message) {
                let field = field?;
                match field.number {
                    1 => kind = field.bytes,
                    3 => blob_size = field.value as usize,
                    _ => (),
                }
            }
            let blob = Blob {
                offset: offset + 4 + size as u64,
                size: blob_size,
            };
            offset = blob.offset + blob_size as u64;
            ensure!(offset <= end, "Truncated PBF file");
            match kind {
                b"OSMHeader" => header = header.or(Some(blob)),
                b"OSMData" => data.push(blob),
                _ => (),
            }
        }
        Ok((header, data))
    }

    fn read(&self, blob: Blob) -> Result<Vec<u8>> {
        ensure!(blob.size <= MAX_BLOB_BYTES, "PBF blob is too large");
        let mut message = vec![0; blob.size];
        self.file.read_exact_at(&mut message, blob.offset)?;
        let (mut raw, mut size, mut zlib) = (None, 0, None);
        for field in Fields(&message) {
            let field = field?;
            match field.number {
                1 => raw = Some(field.bytes),
                2 => size = field.value as usize,
                3 => zlib = Some(field.bytes),
                4..=7 => bail!("Unsupported PBF blob compression"),
                _ => (),
            }
        }
        if let Some(raw) = raw {
            return Ok(raw.to_vec());
        }
        let zlib = zlib.context("Empty PBF blob")?;
        ensure!(size <= MAX_BLOB_BYTES, "PBF blob is too large");
        thread_local! {
            static INFLATE: RefCell<Decompress> = RefCell::new(Decompress::new(true));
        }
        let mut data = vec![0; size];
        let written = INFLATE.with_borrow_mut(|inflate| -> Result<usize> {
            inflate.reset(true);
            inflate
                .decompress(zlib, &mut data, FlushDecompress::Finish)
                .map_err(|error| anyhow!("Invalid compressed PBF blob: {error}"))?;
            Ok(inflate.total_out() as usize)
        })?;
        ensure!(written == size, "PBF blob is shorter than it claims");
        Ok(data)
    }

    pub fn header(&self, blob: Blob) -> Result<Header> {
        let data = self.read(blob)?;
        let mut header = Header {
            required_features: Vec::new(),
            optional_features: Vec::new(),
            writing_program: None,
            replication_timestamp: None,
        };
        for field in Fields(&data) {
            let field = field?;
            let text = || String::from_utf8_lossy(field.bytes).into_owned();
            match field.number {
                4 => header.required_features.push(text()),
                5 => header.optional_features.push(text()),
                16 => header.writing_program = Some(text()),
                32 => header.replication_timestamp = Some(field.value as i64),
                _ => (),
            }
        }
        Ok(header)
    }

    pub fn block(&self, blob: Blob) -> Result<Block> {
        Block::parse(self.read(blob)?)
    }
}

pub struct Block {
    data: Vec<u8>,
    strings: Vec<Range<u32>>,
    groups: Vec<Range<u32>>,
    granularity: i64,
    lat_offset: i64,
    lon_offset: i64,
}

impl Block {
    fn parse(data: Vec<u8>) -> Result<Self> {
        let within = |bytes: &[u8]| {
            let start = (bytes.as_ptr() as usize - data.as_ptr() as usize) as u32;
            start..start + bytes.len() as u32
        };
        let (mut strings, mut groups) = (Vec::new(), Vec::new());
        let (mut granularity, mut lat_offset, mut lon_offset) = (100, 0, 0);
        for field in Fields(&data) {
            let field = field?;
            match field.number {
                1 => {
                    for string in Fields(field.bytes) {
                        strings.push(within(string?.bytes));
                    }
                }
                2 => groups.push(within(field.bytes)),
                17 => granularity = field.value as i64,
                19 => lat_offset = field.value as i64,
                20 => lon_offset = field.value as i64,
                _ => (),
            }
        }
        Ok(Self {
            data,
            strings,
            groups,
            granularity,
            lat_offset,
            lon_offset,
        })
    }

    fn string(&self, index: u64) -> Option<&[u8]> {
        let range = self.strings.get(index as usize)?;
        Some(&self.data[range.start as usize..range.end as usize])
    }

    /// Every element of every group, as its still encoded message.
    fn elements(&self) -> impl Iterator<Item = Result<Field<'_>>> {
        let groups = self.groups.iter();
        groups.flat_map(|group| Fields(&self.data[group.start as usize..group.end as usize]))
    }

    pub fn has_ways_or_relations(&self) -> Result<bool> {
        for element in self.elements() {
            if matches!(element?.number, GROUP_WAY | GROUP_RELATION) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn ways(&self) -> impl Iterator<Item = Result<Way<'_>>> {
        self.elements().filter_map(|element| match element {
            Ok(element) if element.number == GROUP_WAY => Some(self.way(element.bytes)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
    }

    fn way<'a>(&'a self, message: &'a [u8]) -> Result<Way<'a>> {
        let empty = &message[..0];
        let mut way = Way {
            block: self,
            id: 0,
            keys: empty,
            values: empty,
            refs: empty,
        };
        for field in Fields(message) {
            let field = field?;
            match field.number {
                1 => way.id = field.value as i64,
                2 => way.keys = field.bytes,
                3 => way.values = field.bytes,
                8 => way.refs = field.bytes,
                _ => (),
            }
        }
        Ok(way)
    }

    /// Call `visit` with each node in the block, dense or not, in file order.
    pub fn nodes(&self, mut visit: impl FnMut(Node<'_>) -> Result<()>) -> Result<()> {
        for element in self.elements() {
            let element = element?;
            match element.number {
                GROUP_NODE => visit(self.node(element.bytes)?)?,
                GROUP_DENSE => self.dense_nodes(element.bytes, &mut visit)?,
                _ => (),
            }
        }
        Ok(())
    }

    fn located<'a>(&'a self, id: i64, lon: i64, lat: i64, tags: RawTags<'a>) -> Node<'a> {
        Node {
            id,
            lon: self.lon_offset + self.granularity * lon,
            lat: self.lat_offset + self.granularity * lat,
            tags,
        }
    }

    fn node<'a>(&'a self, message: &'a [u8]) -> Result<Node<'a>> {
        let (mut id, mut lon, mut lat) = (0, 0, 0);
        let (mut keys, mut values) = (&message[..0], &message[..0]);
        for field in Fields(message) {
            let field = field?;
            match field.number {
                1 => id = signed(field.value),
                2 => keys = field.bytes,
                3 => values = field.bytes,
                8 => lat = signed(field.value),
                9 => lon = signed(field.value),
                _ => (),
            }
        }
        let tags = RawTags {
            block: self,
            keys,
            values: Some(values),
        };
        Ok(self.located(id, lon, lat, tags))
    }

    fn dense_nodes<'a>(
        &'a self,
        message: &'a [u8],
        visit: &mut impl FnMut(Node<'_>) -> Result<()>,
    ) -> Result<()> {
        let empty = &message[..0];
        let (mut ids, mut lats, mut lons, mut keys_values) = (empty, empty, empty, empty);
        for field in Fields(message) {
            let field = field?;
            match field.number {
                1 => ids = field.bytes,
                8 => lats = field.bytes,
                9 => lons = field.bytes,
                10 => keys_values = field.bytes,
                _ => (),
            }
        }
        let (mut id, mut lat, mut lon) = (0, 0, 0);
        while !ids.is_empty() {
            id += signed(varint(&mut ids).context(TRUNCATED)?);
            lat += signed(varint(&mut lats).context(TRUNCATED)?);
            lon += signed(varint(&mut lons).context(TRUNCATED)?);
            // Each node's tags end in a zero, and are absent if no node in the block has any.
            let start = keys_values;
            while varint(&mut keys_values).is_some_and(|key| key != 0) {
                varint(&mut keys_values).context(TRUNCATED)?;
            }
            let length = (start.len() - keys_values.len()).saturating_sub(1);
            let tags = RawTags {
                block: self,
                keys: &start[..length],
                values: None,
            };
            visit(self.located(id, lon, lat, tags))?;
        }
        Ok(())
    }
}

pub struct Way<'a> {
    block: &'a Block,
    pub id: i64,
    keys: &'a [u8],
    values: &'a [u8],
    refs: &'a [u8],
}

impl<'a> Way<'a> {
    pub fn tags(&self) -> RawTags<'a> {
        RawTags {
            block: self.block,
            keys: self.keys,
            values: Some(self.values),
        }
    }

    /// Replace `refs` with this way's node references.
    pub fn read_refs(&self, refs: &mut Vec<i64>) -> Result<()> {
        refs.clear();
        let (mut data, mut id) = (self.refs, 0);
        while !data.is_empty() {
            id += signed(varint(&mut data).context(TRUNCATED)?);
            refs.push(id);
        }
        Ok(())
    }
}

pub struct Node<'a> {
    pub id: i64,
    /// Nanodegrees.
    pub lon: i64,
    pub lat: i64,
    pub tags: RawTags<'a>,
}

/// Tags as bytes. Dense nodes hold keys and values alternately in `keys`.
pub struct RawTags<'a> {
    block: &'a Block,
    keys: &'a [u8],
    values: Option<&'a [u8]>,
}

impl<'a> RawTags<'a> {
    /// The tags as text, ending at any that are not valid UTF-8.
    pub fn text(self) -> impl Iterator<Item = (&'a str, &'a str)> {
        self.map_while(|(key, value)| {
            Some((
                std::str::from_utf8(key).ok()?,
                std::str::from_utf8(value).ok()?,
            ))
        })
    }
}

impl<'a> Iterator for RawTags<'a> {
    type Item = (&'a [u8], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let key = varint(&mut self.keys)?;
        let value = varint(match &mut self.values {
            Some(values) => values,
            None => &mut self.keys,
        })?;
        Some((self.block.string(key)?, self.block.string(value)?))
    }
}
