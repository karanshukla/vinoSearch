//! Every chunk's embedding in memory, quantized, and kept in step with
//! the database through its change log.

use super::Kind;
use crate::ml::embed::DIM;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};

/// Every chunk's embedding, held as int8 with a per-vector scale: a quarter
/// of the memory, and a quarter of the bytes each query reads, which is what
/// bounds brute-force search. On a 113k-chunk index the top result scored at
/// most 0.001 below f32's (mean 0.00001), and 97% of the top 40 matched.
pub struct Vectors {
    pub(super) ids: Vec<i64>,
    pub(super) data: Vec<i8>,
    pub(super) scales: Vec<f32>,
    pub(super) kinds: Vec<Kind>,
    /// Where each id is in `ids`, for removing it.
    pub(super) pos: HashMap<i64, usize>,
    /// Database state when loaded, to notice the index changing.
    stamp: (i64, u64),
    /// The last `chunk_log` entry these vectors include.
    seq: i64,
}

/// Changes when anything is written: `data_version` for other connections'
/// commits, `total_changes` for this one's. Both are O(1); counting chunks
/// scanned the whole table on every query.
fn stamp(conn: &Connection) -> Result<(i64, u64)> {
    let version = conn.query_row("PRAGMA data_version", [], |r| r.get(0))?;
    Ok((version, conn.total_changes()))
}

/// The newest `chunk_log` seq ever assigned (0 for none), from
/// sqlite_sequence so it survives the log being pruned empty.
fn log_end(conn: &Connection) -> Result<i64> {
    Ok(conn
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'chunk_log'",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

impl Vectors {
    fn empty() -> Self {
        Self {
            ids: Vec::new(),
            data: Vec::new(),
            scales: Vec::new(),
            kinds: Vec::new(),
            pos: HashMap::new(),
            stamp: (0, 0),
            seq: 0,
        }
    }

    /// Append one chunk's embedding, if the blob is a DIM-long f32 vector.
    fn push(&mut self, id: i64, blob: &[u8], kind: Kind) {
        if blob.len() != DIM * 4 {
            return;
        }
        let mut v = [0f32; DIM];
        for (x, c) in v.iter_mut().zip(blob.as_chunks::<4>().0) {
            *x = f32::from_le_bytes(*c);
        }
        let (q, scale) = quantize(&v);
        self.pos.insert(id, self.ids.len());
        self.ids.push(id);
        self.data.extend_from_slice(&q);
        self.scales.push(scale);
        self.kinds.push(kind);
    }

    /// Drop `id` by moving the last vector into its slot.
    fn remove(&mut self, id: i64) {
        let Some(p) = self.pos.remove(&id) else {
            return;
        };
        let last = self.ids.len() - 1;
        if p != last {
            self.data.copy_within(last * DIM..(last + 1) * DIM, p * DIM);
            self.ids[p] = self.ids[last];
            self.scales[p] = self.scales[last];
            self.kinds[p] = self.kinds[last];
            self.pos.insert(self.ids[p], p);
        }
        self.ids.truncate(last);
        self.data.truncate(last * DIM);
        self.scales.truncate(last);
        self.kinds.truncate(last);
    }

    pub fn load(conn: &Connection) -> Result<Self> {
        // One read transaction, so the log position matches the rows read.
        let tx = conn.unchecked_transaction()?;
        let n: i64 = tx.query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))?;
        let n = n as usize;
        let mut out = Self {
            ids: Vec::with_capacity(n),
            data: Vec::with_capacity(n * DIM),
            scales: Vec::with_capacity(n),
            kinds: Vec::with_capacity(n),
            pos: HashMap::with_capacity(n),
            stamp: stamp(&tx)?,
            seq: log_end(&tx)?,
        };
        let mut stmt = tx.prepare("SELECT id, embedding, kind FROM chunks")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            // Borrowed, so no copy of each blob is made on the way in.
            if let rusqlite::types::ValueRef::Blob(blob) = row.get_ref(1)? {
                out.push(row.get(0)?, blob, Kind::from_db(row.get(2)?));
            }
        }
        Ok(out)
    }

    /// Catch up with the index if it changed since loading: only the chunks
    /// inserted or deleted since, from `chunk_log`. A full reload when the
    /// log no longer reaches back far enough, or when most of the index
    /// changed; the old copy is dropped first, so a reload never holds two.
    pub fn refresh(&mut self, conn: &Connection) -> Result<()> {
        if stamp(conn)? == self.stamp {
            return Ok(());
        }
        if !self.catch_up(conn)? {
            *self = Self::empty();
            *self = Self::load(conn)?;
        }
        Ok(())
    }

    /// Ok(false) when a full reload is needed instead.
    pub(super) fn catch_up(&mut self, conn: &Connection) -> Result<bool> {
        let tx = conn.unchecked_transaction()?;
        let end = log_end(&tx)?;
        if end == self.seq {
            self.stamp = stamp(&tx)?;
            return Ok(true);
        }
        let first: Option<i64> = tx.query_row(
            "SELECT min(seq) FROM chunk_log WHERE seq > ?1",
            [self.seq],
            |r| r.get(0),
        )?;
        // Pruned past us (or a database from before the log, or rebuilt).
        if end < self.seq || first != Some(self.seq + 1) {
            return Ok(false);
        }
        let touched: HashSet<i64> = {
            let mut stmt = tx.prepare("SELECT chunk_id FROM chunk_log WHERE seq > ?1")?;
            stmt.query_map([self.seq], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        if touched.len() > self.ids.len() / 2 {
            return Ok(false);
        }
        // Whatever happened to an id in between, its row now (or its
        // absence) is what counts: drop the old vector, read the current one.
        for id in &touched {
            self.remove(*id);
        }
        let mut stmt = tx.prepare("SELECT embedding, kind FROM chunks WHERE id = ?1")?;
        let mut added = Vec::new();
        for id in &touched {
            let mut rows = stmt.query([id])?;
            if let Some(row) = rows.next()? {
                added.push((*id, row.get::<_, Vec<u8>>(0)?, Kind::from_db(row.get(1)?)));
            }
        }
        // Exact, so a few new chunks do not double a 100 MB buffer.
        self.ids.reserve_exact(added.len());
        self.scales.reserve_exact(added.len());
        self.kinds.reserve_exact(added.len());
        self.data.reserve_exact(added.len() * DIM);
        for (id, blob, kind) in &added {
            self.push(*id, blob, *kind);
        }
        self.seq = end;
        self.stamp = stamp(&tx)?;
        Ok(true)
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }
}

/// Symmetric int8: `v ≈ q * scale`, with the largest component at ±127.
pub(super) fn quantize(v: &[f32; DIM]) -> ([i8; DIM], f32) {
    let max = v.iter().fold(0f32, |m, x| m.max(x.abs()));
    if max == 0.0 {
        return ([0; DIM], 0.0);
    }
    let scale = max / 127.0;
    (std::array::from_fn(|i| (v[i] / scale).round() as i8), scale)
}

/// Lanes summed independently, so the compiler can use SIMD.
const LANES: usize = 16;

pub(super) fn dot(a: &[i8; DIM], b: &[i8; DIM]) -> i32 {
    let mut acc = [0i32; LANES];
    for (x, y) in a
        .as_chunks::<LANES>()
        .0
        .iter()
        .zip(b.as_chunks::<LANES>().0)
    {
        for i in 0..LANES {
            acc[i] += i32::from(x[i]) * i32::from(y[i]);
        }
    }
    acc.iter().sum()
}
