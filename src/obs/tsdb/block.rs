//! Block files: one per tier and time window, holding compressed chunks.
//!
//! A window is first an *open* block (`<start>.open`), appended to as heads
//! are cut: each record is
//!
//! ```text
//! u32 series id | i64 last t | u32 len | chunk (len bytes) | u32 crc32
//! ```
//!
//! A torn record at the end (a crash mid-write) fails its CRC and is cut
//! off when the file is opened again. When the window is over the block is
//! *sealed*: rewritten with the records sorted by series and an index at the
//! end, then renamed to `<start>-<end>.blk` (atomically: a crash leaves
//! either the open file or the sealed one; a stale `.open` next to its
//! sealed block is deleted).

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const SEALED_MAGIC: &[u8; 8] = b"LKTSB1\0\0";
const TRAILER_MAGIC: &[u8; 8] = b"LKTSEND\0";
const RECORD_OVERHEAD: usize = 4 + 8 + 4 + 4;
/// footer entry: id u32, offset u64, chunks u32, mint i64, maxt i64
const ENTRY: usize = 4 + 8 + 4 + 8 + 8;

pub fn crc(data: &[&[u8]]) -> u32 {
    let mut c = flate2::Crc::new();
    for d in data {
        c.update(d);
    }
    c.sum()
}

fn record(id: u32, last_t: i64, chunk: &[u8]) -> Vec<u8> {
    let mut rec = Vec::with_capacity(chunk.len() + RECORD_OVERHEAD);
    rec.extend_from_slice(&id.to_le_bytes());
    rec.extend_from_slice(&last_t.to_le_bytes());
    rec.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
    rec.extend_from_slice(chunk);
    let c = crc(&[&rec]);
    rec.extend_from_slice(&c.to_le_bytes());
    rec
}

/// Where one chunk is.
#[derive(Clone, Copy, Debug)]
pub struct ChunkRef {
    pub id: u32,
    pub offset: u64,
    pub len: u32,
    pub mint: i64,
    pub maxt: i64,
}

impl ChunkRef {
    /// Offset of the chunk bytes in the file.
    fn data_offset(&self) -> u64 {
        self.offset + 16
    }
}

/// A block still being written.
pub struct OpenBlock {
    pub start: i64,
    pub end: i64,
    pub path: PathBuf,
    file: File,
    len: u64,
    pub chunks: Vec<ChunkRef>,
    dirty: bool,
}

impl OpenBlock {
    pub fn path_for(dir: &Path, start: i64) -> PathBuf {
        dir.join(format!("{start}.open"))
    }

    /// Open (or create) the open block of the window at `start`; a torn
    /// tail is cut off.
    pub fn open(dir: &Path, start: i64, end: i64) -> Result<Self> {
        let path = Self::path_for(dir, start);
        let mut file = OpenOptions::new().read(true).append(true).create(true).open(&path).with_context(|| format!("failed to open {}", path.display()))?;
        let mut data = Vec::new();
        file.seek(SeekFrom::Start(0))?;
        file.read_to_end(&mut data)?;
        let mut chunks = Vec::new();
        let mut pos = 0usize;
        while pos + RECORD_OVERHEAD <= data.len() {
            let id = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
            let maxt = i64::from_le_bytes(data[pos + 4..pos + 12].try_into().unwrap());
            let len = u32::from_le_bytes(data[pos + 12..pos + 16].try_into().unwrap()) as usize;
            let total = RECORD_OVERHEAD + len;
            if pos + total > data.len() {
                break;
            }
            let body = &data[pos..pos + 16 + len];
            let stored = u32::from_le_bytes(data[pos + 16 + len..pos + total].try_into().unwrap());
            if crc(&[body]) != stored {
                break;
            }
            let mint = super::gorilla::header(&data[pos + 16..pos + 16 + len]).map(|h| h.1).unwrap_or(maxt);
            chunks.push(ChunkRef { id, offset: pos as u64, len: len as u32, mint, maxt });
            pos += total;
        }
        if pos < data.len() {
            eprintln!("obs: {}: cutting off {} bytes of a torn write", path.display(), data.len() - pos);
            file.set_len(pos as u64)?;
        }
        Ok(OpenBlock { start, end, path, file, len: pos as u64, chunks, dirty: false })
    }

    pub fn append(&mut self, id: u32, mint: i64, maxt: i64, chunk: &[u8]) -> Result<()> {
        let rec = record(id, maxt, chunk);
        self.file.write_all(&rec)?;
        self.chunks.push(ChunkRef { id, offset: self.len, len: chunk.len() as u32, mint, maxt });
        self.len += rec.len() as u64;
        self.dirty = true;
        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        if self.dirty {
            self.file.sync_data()?;
            self.dirty = false;
        }
        Ok(())
    }

    pub fn size(&self) -> u64 {
        self.len
    }

    /// The chunks of `ids` overlapping [mint, maxt], as (ref) to read later.
    pub fn refs(&self, ids: &HashSet<u32>, mint: i64, maxt: i64) -> Vec<ChunkRef> {
        self.chunks.iter().filter(|c| ids.contains(&c.id) && c.maxt >= mint && c.mint <= maxt).copied().collect()
    }

    /// Rewrite as a sealed block; returns its path. The open file is removed.
    pub fn seal(mut self, dir: &Path) -> Result<PathBuf> {
        self.sync()?;
        let mut order = self.chunks.clone();
        order.sort_by_key(|c| (c.id, c.mint, c.offset));
        let src = File::open(&self.path)?;
        let sealed = sealed_path(dir, self.start, self.end);
        let tmp = sealed.with_extension("tmp");
        let mut out = std::io::BufWriter::new(File::create(&tmp)?);
        out.write_all(SEALED_MAGIC)?;
        let mut pos = SEALED_MAGIC.len() as u64;
        let mut footer: Vec<u8> = Vec::new();
        let mut entries = 0u32;
        let mut i = 0;
        while i < order.len() {
            let id = order[i].id;
            let first = pos;
            let (mut mint, mut maxt, mut n) = (i64::MAX, i64::MIN, 0u32);
            while i < order.len() && order[i].id == id {
                let c = order[i];
                let mut chunk = vec![0u8; c.len as usize];
                src.read_exact_at(&mut chunk, c.data_offset())?;
                let rec = record(id, c.maxt, &chunk);
                out.write_all(&rec)?;
                pos += rec.len() as u64;
                mint = mint.min(c.mint);
                maxt = maxt.max(c.maxt);
                n += 1;
                i += 1;
            }
            footer.extend_from_slice(&id.to_le_bytes());
            footer.extend_from_slice(&first.to_le_bytes());
            footer.extend_from_slice(&n.to_le_bytes());
            footer.extend_from_slice(&mint.to_le_bytes());
            footer.extend_from_slice(&maxt.to_le_bytes());
            entries += 1;
        }
        out.write_all(&footer)?;
        out.write_all(&pos.to_le_bytes())?;
        out.write_all(&entries.to_le_bytes())?;
        out.write_all(&crc(&[&footer]).to_le_bytes())?;
        out.write_all(TRAILER_MAGIC)?;
        let file = out.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &sealed)?;
        sync_dir(dir);
        let _ = std::fs::remove_file(&self.path);
        sync_dir(dir);
        Ok(sealed)
    }
}

pub fn sealed_path(dir: &Path, start: i64, end: i64) -> PathBuf {
    dir.join(format!("{start}-{end}.blk"))
}

pub fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

/// A sealed block on disk (the index is read when queried).
#[derive(Clone, Debug)]
pub struct SealedBlock {
    pub start: i64,
    pub end: i64,
    pub path: PathBuf,
    pub size: u64,
}

impl SealedBlock {
    /// `<start>-<end>.blk`
    pub fn parse(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?.strip_suffix(".blk")?;
        let (s, e) = name.split_once('-')?;
        let size = std::fs::metadata(path).ok()?.len();
        Some(SealedBlock { start: s.parse().ok()?, end: e.parse().ok()?, path: path.to_path_buf(), size })
    }

    /// (series id, mint, maxt) of every series in the block.
    pub fn index(&self) -> Result<Vec<(u32, i64, i64)>> {
        let file = File::open(&self.path)?;
        let (footer, entries) = Self::footer(&self.path, &file)?;
        Ok((0..entries)
            .map(|i| {
                let e = &footer[i * ENTRY..(i + 1) * ENTRY];
                (
                    u32::from_le_bytes(e[0..4].try_into().unwrap()),
                    i64::from_le_bytes(e[16..24].try_into().unwrap()),
                    i64::from_le_bytes(e[24..32].try_into().unwrap()),
                )
            })
            .collect())
    }

    fn footer(path: &Path, file: &File) -> Result<(Vec<u8>, usize)> {
        let len = file.metadata()?.len();
        if len < (SEALED_MAGIC.len() + 24) as u64 {
            bail!("{}: too short", path.display());
        }
        let mut trailer = [0u8; 24];
        file.read_exact_at(&mut trailer, len - 24)?;
        if &trailer[16..] != TRAILER_MAGIC {
            bail!("{}: bad trailer", path.display());
        }
        let footer_at = u64::from_le_bytes(trailer[0..8].try_into().unwrap());
        let entries = u32::from_le_bytes(trailer[8..12].try_into().unwrap()) as usize;
        let want = u32::from_le_bytes(trailer[12..16].try_into().unwrap());
        let mut footer = vec![0u8; entries * ENTRY];
        file.read_exact_at(&mut footer, footer_at)?;
        if crc(&[&footer]) != want {
            bail!("{}: bad index checksum", path.display());
        }
        Ok((footer, entries))
    }

    /// The chunks of `ids` overlapping [mint, maxt].
    pub fn refs(&self, ids: &HashSet<u32>, mint: i64, maxt: i64) -> Result<(File, Vec<ChunkRef>)> {
        let file = File::open(&self.path)?;
        let (footer, entries) = Self::footer(&self.path, &file)?;
        let mut out = Vec::new();
        let mut wanted: Vec<u32> = ids.iter().copied().collect();
        wanted.sort_unstable();
        let entry = |i: usize| {
            let e = &footer[i * ENTRY..(i + 1) * ENTRY];
            (
                u32::from_le_bytes(e[0..4].try_into().unwrap()),
                u64::from_le_bytes(e[4..12].try_into().unwrap()),
                u32::from_le_bytes(e[12..16].try_into().unwrap()),
                i64::from_le_bytes(e[16..24].try_into().unwrap()),
                i64::from_le_bytes(e[24..32].try_into().unwrap()),
            )
        };
        // Entries are sorted by id: binary search each wanted id.
        let mut lo = 0usize;
        for id in wanted {
            let (mut a, mut b) = (lo, entries);
            while a < b {
                let m = (a + b) / 2;
                if entry(m).0 < id {
                    a = m + 1;
                } else {
                    b = m;
                }
            }
            lo = a;
            if a >= entries {
                break;
            }
            let (eid, offset, n, emint, emaxt) = entry(a);
            if eid != id || emaxt < mint || emint > maxt {
                continue;
            }
            // Walk this series' records.
            let mut pos = offset;
            for _ in 0..n {
                let mut head = [0u8; 16];
                file.read_exact_at(&mut head, pos)?;
                let clen = u32::from_le_bytes(head[12..16].try_into().unwrap());
                let cmaxt = i64::from_le_bytes(head[4..12].try_into().unwrap());
                let mut first = [0u8; 10];
                file.read_exact_at(&mut first, pos + 16)?;
                let cmint = i64::from_le_bytes(first[2..10].try_into().unwrap());
                if cmaxt >= mint && cmint <= maxt {
                    out.push(ChunkRef { id, offset: pos, len: clen, mint: cmint, maxt: cmaxt });
                }
                pos += (RECORD_OVERHEAD as u64) + clen as u64;
            }
        }
        Ok((file, out))
    }

    /// Every chunk in the block (for downsampling), in series order.
    pub fn all(&self) -> Result<(File, Vec<ChunkRef>)> {
        let file = File::open(&self.path)?;
        let len = file.metadata()?.len();
        let mut trailer = [0u8; 24];
        file.read_exact_at(&mut trailer, len.saturating_sub(24))?;
        if &trailer[16..] != TRAILER_MAGIC {
            bail!("{}: bad trailer", self.path.display());
        }
        let footer_at = u64::from_le_bytes(trailer[0..8].try_into().unwrap());
        let mut out = Vec::new();
        let mut pos = SEALED_MAGIC.len() as u64;
        while pos < footer_at {
            let mut head = [0u8; 16];
            file.read_exact_at(&mut head, pos)?;
            let id = u32::from_le_bytes(head[0..4].try_into().unwrap());
            let maxt = i64::from_le_bytes(head[4..12].try_into().unwrap());
            let clen = u32::from_le_bytes(head[12..16].try_into().unwrap());
            out.push(ChunkRef { id, offset: pos, len: clen, mint: i64::MIN, maxt });
            pos += (RECORD_OVERHEAD as u64) + clen as u64;
        }
        Ok((file, out))
    }
}

/// Read one chunk's bytes, checking its CRC.
pub fn read_chunk(file: &File, c: &ChunkRef) -> Result<Vec<u8>> {
    let mut rec = vec![0u8; RECORD_OVERHEAD + c.len as usize];
    file.read_exact_at(&mut rec, c.offset)?;
    let body = 16 + c.len as usize;
    let stored = u32::from_le_bytes(rec[body..body + 4].try_into().unwrap());
    if crc(&[&rec[..body]]) != stored {
        bail!("chunk checksum mismatch at {}", c.offset);
    }
    rec.truncate(body);
    Ok(rec.split_off(16))
}

#[cfg(test)]
mod tests {
    use super::super::gorilla::{Encoder, decode};
    use super::*;

    fn chunk(start: i64, n: i64, v: f64) -> (i64, i64, Vec<u8>) {
        let mut e = Encoder::new();
        for i in 0..n {
            e.push(start + i * 10, v + i as f64);
        }
        (start, start + (n - 1) * 10, e.encode())
    }

    #[test]
    fn open_append_reopen_seal_query() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = OpenBlock::open(dir.path(), 0, 1000).unwrap();
        for (id, start) in [(2u32, 0i64), (1, 0), (2, 100), (1, 100)] {
            let (mint, maxt, c) = chunk(start, 5, id as f64);
            b.append(id, mint, maxt, &c).unwrap();
        }
        b.sync().unwrap();
        drop(b);
        // A torn write at the end is cut off.
        std::fs::OpenOptions::new().append(true).open(OpenBlock::path_for(dir.path(), 0)).unwrap().write_all(&[1, 2, 3]).unwrap();
        let b = OpenBlock::open(dir.path(), 0, 1000).unwrap();
        assert_eq!(b.chunks.len(), 4);
        let ids: HashSet<u32> = [1].into();
        assert_eq!(b.refs(&ids, 0, 1000).len(), 2);
        assert_eq!(b.refs(&ids, 50, 60).len(), 0);
        let path = b.seal(dir.path()).unwrap();
        assert!(!OpenBlock::path_for(dir.path(), 0).exists());
        let s = SealedBlock::parse(&path).unwrap();
        assert_eq!((s.start, s.end), (0, 1000));
        let ids: HashSet<u32> = [1, 2, 7].into();
        let (file, refs) = s.refs(&ids, 0, 1000).unwrap();
        assert_eq!(refs.iter().map(|r| (r.id, r.mint)).collect::<Vec<_>>(), vec![(1, 0), (1, 100), (2, 0), (2, 100)]);
        let mut out = Vec::new();
        decode(&read_chunk(&file, &refs[1]).unwrap(), &mut out);
        assert_eq!(out[0], (100, 1.0));
        let (_, refs) = s.refs(&[2].into(), 105, 1000).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(s.all().unwrap().1.len(), 4);
    }
}
