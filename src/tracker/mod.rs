//! The tracker: in-memory, authoritative state of every tracked sat, moved
//! block by block following ordinal theory (docs/DESIGN.md §4–§5).
//! Pure and deterministic; no I/O.
//!
//! OWNER: sats+tracker agent. The public API below is the contract used by
//! the indexer, store and server; keep the signatures.
//!
//! # State
//!
//! * Tracked UTXOs live in a slab of `Slot`s, found by outpoint through a
//!   hash map that hashes only the txid's first 8 bytes and the vout. Every
//!   slot caches its per-row counts, so spending it never recomputes them;
//!   an output that receives an input's pieces intact takes over the input's
//!   slot, so those pieces keep their locator entries.
//! * The sat locator maps each piece's first sat to its holder (a slot, a
//!   lost piece or an overwritten UTXO); pieces longer than one sat are also
//!   kept in an ordered map so a sat inside one is found by predecessor search.
//! * Row aggregates (`RowStats`) are updated as UTXOs are attached and
//!   detached; holders are counted per script from a per-script row tally.
//! * Non-extracted located counts sit in one Fenwick tree per row over
//!   64-height buckets of the creation height, plus a per-bucket list of
//!   slots so the partial boundary bucket is summed exactly.
//!
//! # Time → height
//!
//! "Created within N years of the tip" means created at a height `h` whose
//! running-max header time `max(time(0..=h))` is ≥ tip time − N·365 days.
//! Header times are not monotone (only above the median of the previous 11),
//! the running max is, so the window is a height suffix; it can differ from
//! the raw header time test only for blocks within about two hours of the
//! window's start.
//!
//! # Hot path
//!
//! A transaction without a tracked input costs one hash lookup per input
//! until the first hit, and allocates nothing: only its fee is added to the
//! running fee total that places fee sats in the coinbase stream.

mod apply;
mod supply;
mod undo;

#[cfg(test)]
mod tests;

use {
  crate::{
    model::{
      BlockDelta, LocationKind, Piece, ResolvedBlock, RowCounts, RowId, RowStats, SatLocation,
      SupplySnapshot, TrackedUtxo, TrackerConfig, TrackerSnapshot, UtxoKind,
    },
    sats::SatCatalog,
  },
  anyhow::{Context, bail, ensure},
  bitcoin::{BlockHash, OutPoint, ScriptBuf},
  rustc_hash::FxHashMap,
  std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
    sync::Arc,
  },
  supply::Fenwick,
};

/// Heights per Fenwick bucket (as a shift).
const BUCKET_SHIFT: u32 = 6;

const NO_SCRIPT: u32 = u32::MAX;

/// Outpoint key hashed by the txid's first 8 bytes and the vout: txids are
/// uniformly distributed, so this is as good as hashing all 36 bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
struct OutKey(OutPoint);

impl Hash for OutKey {
  fn hash<H: Hasher>(&self, state: &mut H) {
    let txid: &[u8; 32] = self.0.txid.as_ref();
    let prefix = u64::from_le_bytes(txid[..8].try_into().unwrap());
    state.write_u64(prefix ^ u64::from(self.0.vout).rotate_left(32));
  }
}

/// Where a piece is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Loc {
  Slot(u32),
  Lost(u32),
  Overwritten(u32),
}

struct Slot {
  outpoint: OutPoint,
  utxo: TrackedUtxo,
  rows: RowCounts,
  /// Script entry (spendable UTXOs only).
  script: u32,
  script_pos: u32,
  bucket_pos: u32,
  /// The block (`Tracker::block_seq`) that created the UTXO.
  created_seq: u64,
  live: bool,
}

struct ScriptEntry {
  slots: Vec<u32>,
  /// Per row: (located sats, extracted sats).
  rows: Vec<(RowId, u64, u64)>,
}

struct Overwritten {
  outpoint: OutPoint,
  height: u32,
  utxo: TrackedUtxo,
  rows: RowCounts,
}

/// Dense per-row accumulator.
struct RowAcc {
  counts: Vec<i128>,
  touched: Vec<RowId>,
}

impl RowAcc {
  fn new(rows: usize) -> Self {
    Self {
      counts: vec![0; rows],
      touched: Vec::new(),
    }
  }

  fn add(&mut self, row: RowId, n: i128) {
    let count = &mut self.counts[usize::from(row)];
    if *count == 0 {
      self.touched.push(row);
    }
    *count += n;
  }

  fn add_counts(&mut self, counts: &RowCounts, sign: i128) {
    for &(row, n) in counts {
      self.add(row, sign * i128::from(n));
    }
  }

  fn add_piece(&mut self, catalog: &SatCatalog, piece: &Piece, sign: i128) {
    catalog.add_row_counts(piece.start, piece.len, |row, n| {
      self.add(row, sign * i128::from(n))
    });
  }

  fn take(&mut self) -> anyhow::Result<RowCounts> {
    self.touched.sort_unstable();
    self.touched.dedup();
    let mut out = Vec::with_capacity(self.touched.len());
    let mut negative = false;
    for &row in &self.touched {
      let count = std::mem::take(&mut self.counts[usize::from(row)]);
      if count > 0 {
        out.push((row, u64::try_from(count).unwrap_or(u64::MAX)));
      } else if count < 0 {
        negative = true;
      }
    }
    self.touched.clear();
    ensure!(
      !negative,
      "negative row count (tracker state is inconsistent)"
    );
    Ok(out)
  }
}

fn is_unspendable(script: &bitcoin::Script) -> bool {
  script.is_op_return() || script.len() > 10_000
}

/// Sorted by offset, non-overlapping, merged, inside the value, non-empty.
fn check_pieces(value: u64, pieces: &[Piece]) -> anyhow::Result<()> {
  ensure!(!pieces.is_empty(), "tracked UTXO without pieces");
  for (i, piece) in pieces.iter().enumerate() {
    ensure!(piece.len > 0, "empty piece {piece:?}");
    ensure!(
      piece
        .offset
        .checked_add(piece.len)
        .is_some_and(|end| end <= value),
      "piece {piece:?} outside a {value}-sat output"
    );
    ensure!(
      piece.start.checked_add(piece.len).is_some(),
      "piece {piece:?} overflows"
    );
    if let Some(prev) = i.checked_sub(1).map(|i| pieces[i]) {
      ensure!(
        prev.offset_end() <= piece.offset,
        "pieces {prev:?} and {piece:?} overlap or are unsorted"
      );
      ensure!(
        !(prev.offset_end() == piece.offset && prev.end() == piece.start),
        "pieces {prev:?} and {piece:?} are not merged"
      );
    }
  }
  Ok(())
}

/// Appends `piece`, merging it into the last piece when both its offsets and
/// sats continue it.
fn push_merged(pieces: &mut Vec<Piece>, piece: Piece) {
  if let Some(last) = pieces.last_mut()
    && last.offset_end() == piece.offset
    && last.end() == piece.start
  {
    last.len += piece.len;
    return;
  }
  pieces.push(piece);
}

pub struct Tracker {
  catalog: Arc<SatCatalog>,
  config: TrackerConfig,
  height: Option<u32>,
  hash: Option<BlockHash>,
  times: Vec<u32>,
  /// Running max of `times`.
  max_times: Vec<u32>,
  lost_sats: u64,

  index: FxHashMap<OutKey, u32>,
  slots: Vec<Slot>,
  free_slots: Vec<u32>,
  utxo_count: usize,
  piece_count: usize,

  locator: FxHashMap<u64, Loc>,
  /// Pieces longer than one sat: first sat → one past the last.
  long_pieces: BTreeMap<u64, u64>,
  lost: Vec<Piece>,
  overwritten: Vec<Overwritten>,

  stats: Vec<RowStats>,
  fenwick: Vec<Fenwick>,
  buckets: Vec<Vec<u32>>,
  scripts: FxHashMap<ScriptBuf, u32>,
  script_entries: Vec<ScriptEntry>,
  free_scripts: Vec<u32>,

  /// Incremented per applied, undone or rolled-back block.
  block_seq: u64,
  touched_seq: Vec<u64>,
  before: Vec<RowStats>,
  touched: Vec<RowId>,
  acc: RowAcc,
}

impl Tracker {
  /// Empty state, before the genesis block.
  pub fn new(catalog: Arc<SatCatalog>, config: TrackerConfig) -> Self {
    debug_assert_eq!(
      catalog.config().ranges,
      config.ranges,
      "catalog built for other ranges"
    );
    let rows = catalog.rows().len();
    Self {
      catalog,
      config,
      height: None,
      hash: None,
      times: Vec::new(),
      max_times: Vec::new(),
      lost_sats: 0,
      index: FxHashMap::default(),
      slots: Vec::new(),
      free_slots: Vec::new(),
      utxo_count: 0,
      piece_count: 0,
      locator: FxHashMap::default(),
      long_pieces: BTreeMap::new(),
      lost: Vec::new(),
      overwritten: Vec::new(),
      stats: vec![RowStats::default(); rows],
      fenwick: (0..rows).map(|_| Fenwick::default()).collect(),
      buckets: Vec::new(),
      scripts: FxHashMap::default(),
      script_entries: Vec::new(),
      free_scripts: Vec::new(),
      block_seq: 0,
      touched_seq: vec![0; rows],
      before: vec![RowStats::default(); rows],
      touched: Vec::new(),
      acc: RowAcc::new(rows),
    }
  }

  /// Rebuilds the in-memory state (including row aggregates) from a
  /// persisted snapshot.
  pub fn restore(
    catalog: Arc<SatCatalog>,
    config: TrackerConfig,
    snapshot: TrackerSnapshot,
  ) -> anyhow::Result<Self> {
    ensure!(
      catalog.config().ranges == config.ranges,
      "the catalog was built for other tracked ranges"
    );
    let mut tracker = Self::new(catalog, config);
    let rows = tracker.stats.len();
    ensure!(
      snapshot.mined.len() == rows,
      "snapshot has {} mined counters for {rows} rows",
      snapshot.mined.len()
    );
    let expected_times = snapshot.height.map_or(0, |h| h as usize + 1);
    ensure!(
      snapshot.times.len() == expected_times,
      "snapshot has {} header times for height {:?}",
      snapshot.times.len(),
      snapshot.height
    );
    ensure!(
      snapshot.height.is_some() == snapshot.hash.is_some(),
      "snapshot height and hash disagree"
    );
    tracker.height = snapshot.height;
    tracker.hash = snapshot.hash;
    for &time in &snapshot.times {
      tracker.push_time(time);
    }
    tracker.lost_sats = snapshot.lost_sats;
    let tip = snapshot.height;
    let within_tip = |height: u32| tip.is_some_and(|tip| height <= tip);
    // Every piece must hold sats already mined, or a later coinbase would
    // mint them a second time.
    let mined_end = tip.map_or(0, |tip| crate::sats::math::first_sat(u64::from(tip) + 1));
    let check_mined = |pieces: &[Piece]| -> anyhow::Result<()> {
      for piece in pieces {
        ensure!(
          piece.end() <= mined_end,
          "piece {piece:?} holds sats not mined by height {tip:?}"
        );
      }
      Ok(())
    };

    for (outpoint, utxo) in snapshot.utxos {
      ensure!(
        within_tip(utxo.height),
        "UTXO {outpoint} created above the tip"
      );
      check_pieces(utxo.value, &utxo.pieces).with_context(|| format!("UTXO {outpoint}"))?;
      check_mined(&utxo.pieces)?;
      ensure!(
        (utxo.kind == UtxoKind::Unspendable) == is_unspendable(&utxo.script_pubkey),
        "UTXO {outpoint} has the wrong kind"
      );
      ensure!(
        !tracker.index.contains_key(&OutKey(outpoint)),
        "duplicate UTXO {outpoint}"
      );
      let rows = tracker.piece_rows(&utxo.pieces)?;
      let slot = tracker.alloc_slot(outpoint, utxo, rows, 0);
      tracker.locate_slot(slot)?;
      tracker.attach(slot);
    }

    check_mined(&snapshot.lost)?;
    for piece in snapshot.lost {
      tracker.push_lost(piece)?;
    }

    for (outpoint, height, utxo) in snapshot.overwritten {
      ensure!(
        within_tip(height) && utxo.height <= height,
        "overwritten UTXO {outpoint} has bad heights"
      );
      check_pieces(utxo.value, &utxo.pieces).with_context(|| format!("UTXO {outpoint}"))?;
      check_mined(&utxo.pieces)?;
      let rows = tracker.piece_rows(&utxo.pieces)?;
      tracker.push_overwritten(Overwritten {
        outpoint,
        height,
        utxo,
        rows,
      })?;
    }

    for (row, mined) in snapshot.mined.into_iter().enumerate() {
      tracker.stats[row].mined = mined;
    }
    Ok(tracker)
  }

  /// The full state; `utxos` sorted by outpoint, `lost` and `overwritten`
  /// in the order they happened.
  pub fn snapshot(&self) -> TrackerSnapshot {
    let mut utxos: Vec<(OutPoint, TrackedUtxo)> = self
      .slots
      .iter()
      .filter(|slot| slot.live)
      .map(|slot| (slot.outpoint, slot.utxo.clone()))
      .collect();
    utxos.sort_unstable_by_key(|(outpoint, _)| *outpoint);
    TrackerSnapshot {
      height: self.height,
      hash: self.hash,
      times: self.times.clone(),
      lost_sats: self.lost_sats,
      utxos,
      lost: self.lost.clone(),
      overwritten: self
        .overwritten
        .iter()
        .map(|o| (o.outpoint, o.height, o.utxo.clone()))
        .collect(),
      mined: self.stats.iter().map(|stats| stats.mined).collect(),
    }
  }

  pub fn height(&self) -> Option<u32> {
    self.height
  }

  pub fn hash(&self) -> Option<BlockHash> {
    self.hash
  }

  /// Applies the next block (`height == self.height() + 1`, or 0 when
  /// empty; `header.prev_blockhash` must equal `self.hash()`).
  ///
  /// Inconsistent input (wrong height, parent or hash, outputs exceeding
  /// inputs, a tracked input with another value, spending an unspendable
  /// output, value overflow) is an error, and the tracker is left exactly
  /// as it was.
  pub fn apply_block(&mut self, block: &ResolvedBlock) -> anyhow::Result<BlockDelta> {
    self.apply(block)
  }

  /// Reverts the tip block; `delta` is what `apply_block` returned for it.
  /// A delta that does not match the tip is an error, with nothing changed
  /// (it is validated up front; an error after that point, like a row not
  /// landing on the delta's `before` stats, means the state was already
  /// inconsistent).
  pub fn undo_block(&mut self, delta: &BlockDelta) -> anyhow::Result<()> {
    self.undo(delta)
  }

  pub fn utxo(&self, outpoint: &OutPoint) -> Option<&TrackedUtxo> {
    self
      .index
      .get(&OutKey(*outpoint))
      .map(|&slot| &self.slots[slot as usize].utxo)
  }

  pub fn locate(&self, sat: u64) -> Option<SatLocation> {
    let (start, loc) = match self.locator.get(&sat) {
      Some(&loc) => (sat, loc),
      None => {
        let (&start, &end) = self.long_pieces.range(..=sat).next_back()?;
        if sat >= end {
          return None;
        }
        (start, *self.locator.get(&start)?)
      }
    };
    let find = |pieces: &[Piece]| pieces.iter().copied().find(|p| p.start == start);
    let (kind, outpoint, piece) = match loc {
      Loc::Slot(slot) => {
        let slot = &self.slots[slot as usize];
        let kind = match slot.utxo.kind {
          UtxoKind::Spendable => LocationKind::Unspent,
          UtxoKind::Unspendable => LocationKind::Burned,
        };
        (kind, slot.outpoint, find(&slot.utxo.pieces)?)
      }
      Loc::Lost(i) => (LocationKind::Lost, OutPoint::null(), self.lost[i as usize]),
      Loc::Overwritten(i) => {
        let o = &self.overwritten[i as usize];
        (LocationKind::Overwritten, o.outpoint, find(&o.utxo.pieces)?)
      }
    };
    Some(SatLocation {
      kind,
      outpoint,
      offset: piece.offset + (sat - piece.start),
      piece,
    })
  }

  pub fn row_stats(&self, row: RowId) -> RowStats {
    self
      .stats
      .get(usize::from(row))
      .copied()
      .unwrap_or_default()
  }

  /// Every row's supply at the tip, with the top `top` holders per list.
  pub fn supply(&self, top: usize) -> SupplySnapshot {
    self.supply_snapshot(top)
  }

  pub fn utxo_count(&self) -> usize {
    self.utxo_count
  }

  pub fn piece_count(&self) -> usize {
    self.piece_count
  }

  /// `RowStats::mined` per row, indexed by `RowId` (what `Store::commit`
  /// persists next to the deltas).
  pub fn mined(&self) -> Vec<u64> {
    self.stats.iter().map(|stats| stats.mined).collect()
  }

  /// All sats (tracked or not) lost so far.
  pub fn lost_sats(&self) -> u64 {
    self.lost_sats
  }

  /// Header time of an applied block.
  pub fn time(&self, height: u32) -> Option<u32> {
    self.times.get(height as usize).copied()
  }

  pub fn catalog(&self) -> &Arc<SatCatalog> {
    &self.catalog
  }

  pub fn config(&self) -> &TrackerConfig {
    &self.config
  }

  /// Tracked lost pieces, ord lost offsets.
  pub fn lost_pieces(&self) -> &[Piece] {
    &self.lost
  }

  // ---------------------------------------------------------------------
  // Internals shared by apply, undo and restore.

  fn push_time(&mut self, time: u32) {
    let max = self.max_times.last().copied().unwrap_or(0).max(time);
    self.times.push(time);
    self.max_times.push(max);
  }

  fn pop_time(&mut self) {
    self.times.pop();
    self.max_times.pop();
  }

  fn touch(&mut self, row: RowId) -> &mut RowStats {
    let r = usize::from(row);
    if self.touched_seq[r] != self.block_seq {
      self.touched_seq[r] = self.block_seq;
      self.before[r] = self.stats[r];
      self.touched.push(row);
    }
    &mut self.stats[r]
  }

  fn piece_rows(&mut self, pieces: &[Piece]) -> anyhow::Result<RowCounts> {
    for piece in pieces {
      self.acc.add_piece(&self.catalog, piece, 1);
    }
    self.acc.take()
  }

  fn alloc_slot(
    &mut self,
    outpoint: OutPoint,
    utxo: TrackedUtxo,
    rows: RowCounts,
    seq: u64,
  ) -> u32 {
    let slot = Slot {
      outpoint,
      utxo,
      rows,
      script: NO_SCRIPT,
      script_pos: 0,
      bucket_pos: 0,
      created_seq: seq,
      live: true,
    };
    let id = match self.free_slots.pop() {
      Some(id) => {
        self.slots[id as usize] = slot;
        id
      }
      None => {
        self.slots.push(slot);
        (self.slots.len() - 1) as u32
      }
    };
    self.index.insert(OutKey(outpoint), id);
    self.utxo_count += 1;
    id
  }

  /// Removes a detached slot from the index and frees it.
  fn free_slot(&mut self, id: u32) {
    let slot = &mut self.slots[id as usize];
    debug_assert!(slot.live && slot.script == NO_SCRIPT);
    slot.live = false;
    slot.utxo.pieces = Vec::new();
    slot.rows = Vec::new();
    let outpoint = slot.outpoint;
    if self.index.get(&OutKey(outpoint)) == Some(&id) {
      self.index.remove(&OutKey(outpoint));
    }
    self.utxo_count -= 1;
    self.free_slots.push(id);
  }

  fn locator_insert(&mut self, piece: &Piece, loc: Loc) -> anyhow::Result<()> {
    if let Some(old) = self.locator.insert(piece.start, loc) {
      bail!("sat {} is tracked twice ({old:?} and {loc:?})", piece.start);
    }
    if piece.len > 1 {
      self.long_pieces.insert(piece.start, piece.end());
    }
    Ok(())
  }

  fn locator_remove(&mut self, piece: &Piece) {
    self.locator.remove(&piece.start);
    if piece.len > 1 {
      self.long_pieces.remove(&piece.start);
    }
  }

  fn locate_slot(&mut self, id: u32) -> anyhow::Result<()> {
    let pieces = std::mem::take(&mut self.slots[id as usize].utxo.pieces);
    let result = pieces
      .iter()
      .try_for_each(|piece| self.locator_insert(piece, Loc::Slot(id)));
    self.slots[id as usize].utxo.pieces = pieces;
    result
  }

  fn unlocate_slot(&mut self, id: u32) {
    let pieces = std::mem::take(&mut self.slots[id as usize].utxo.pieces);
    for piece in &pieces {
      self.locator_remove(piece);
    }
    self.slots[id as usize].utxo.pieces = pieces;
  }

  fn extracted(&self, utxo: &TrackedUtxo, count: u64) -> bool {
    self.times[utxo.height as usize] >= self.config.extracted_since_time
      && (utxo.value <= self.config.extracted_max_value || count == utxo.value)
  }

  /// Adds a slot's UTXO to the row aggregates, the script index and the
  /// height buckets.
  fn attach(&mut self, id: u32) {
    let rows = std::mem::take(&mut self.slots[id as usize].rows);
    self.piece_count += self.slots[id as usize].utxo.pieces.len();
    match self.slots[id as usize].utxo.kind {
      UtxoKind::Unspendable => {
        for &(row, n) in &rows {
          self.touch(row).burned += n;
        }
      }
      UtxoKind::Spendable => {
        let script = self.script_entry(id);
        let bucket = (self.slots[id as usize].utxo.height >> BUCKET_SHIFT) as usize;
        if self.buckets.len() <= bucket {
          self.buckets.resize_with(bucket + 1, Vec::new);
        }
        self.slots[id as usize].bucket_pos = self.buckets[bucket].len() as u32;
        self.buckets[bucket].push(id);
        for &(row, n) in &rows {
          let extracted = self.extracted(&self.slots[id as usize].utxo, n);
          let stats = self.touch(row);
          stats.located += n;
          if extracted {
            stats.extracted += n;
          }
          let entry = &mut self.script_entries[script as usize];
          let tally = match entry.rows.iter_mut().find(|(r, _, _)| *r == row) {
            Some(tally) => tally,
            None => {
              entry.rows.push((row, 0, 0));
              entry.rows.last_mut().unwrap()
            }
          };
          let new_holder = tally.1 == 0;
          tally.1 += n;
          let new_extracted_holder = extracted && tally.2 == 0;
          if extracted {
            tally.2 += n;
          }
          let stats = &mut self.stats[usize::from(row)];
          stats.holders += u64::from(new_holder);
          stats.extracted_holders += u64::from(new_extracted_holder);
          if !extracted {
            self.fenwick[usize::from(row)].add(bucket, n);
          }
        }
      }
    }
    self.slots[id as usize].rows = rows;
  }

  /// Inverse of `attach`.
  fn detach(&mut self, id: u32) {
    let rows = std::mem::take(&mut self.slots[id as usize].rows);
    self.piece_count -= self.slots[id as usize].utxo.pieces.len();
    match self.slots[id as usize].utxo.kind {
      UtxoKind::Unspendable => {
        for &(row, n) in &rows {
          self.touch(row).burned -= n;
        }
      }
      UtxoKind::Spendable => {
        let script = self.slots[id as usize].script;
        let bucket = (self.slots[id as usize].utxo.height >> BUCKET_SHIFT) as usize;
        let pos = self.slots[id as usize].bucket_pos as usize;
        let list = &mut self.buckets[bucket];
        list.swap_remove(pos);
        if let Some(&moved) = list.get(pos) {
          self.slots[moved as usize].bucket_pos = pos as u32;
        }
        for &(row, n) in &rows {
          let extracted = self.extracted(&self.slots[id as usize].utxo, n);
          let stats = self.touch(row);
          stats.located -= n;
          if extracted {
            stats.extracted -= n;
          }
          let entry = &mut self.script_entries[script as usize];
          let index = entry
            .rows
            .iter()
            .position(|(r, _, _)| *r == row)
            .expect("script tally for an attached row");
          let tally = &mut entry.rows[index];
          tally.1 -= n;
          if extracted {
            tally.2 -= n;
          }
          let lost_holder = tally.1 == 0;
          let lost_extracted_holder = extracted && tally.2 == 0;
          if tally.1 == 0 {
            entry.rows.swap_remove(index);
          }
          let stats = &mut self.stats[usize::from(row)];
          stats.holders -= u64::from(lost_holder);
          stats.extracted_holders -= u64::from(lost_extracted_holder);
          if !extracted {
            self.fenwick[usize::from(row)].sub(bucket, n);
          }
        }
        self.release_script(id);
      }
    }
    self.slots[id as usize].rows = rows;
  }

  /// Links a spendable slot into its script's entry, creating it if needed.
  fn script_entry(&mut self, id: u32) -> u32 {
    let script = &self.slots[id as usize].utxo.script_pubkey;
    let entry = match self.scripts.get(script.as_script()) {
      Some(&entry) => entry,
      None => {
        let entry = match self.free_scripts.pop() {
          Some(entry) => entry,
          None => {
            self.script_entries.push(ScriptEntry {
              slots: Vec::new(),
              rows: Vec::new(),
            });
            (self.script_entries.len() - 1) as u32
          }
        };
        self.scripts.insert(script.clone(), entry);
        entry
      }
    };
    let slots = &mut self.script_entries[entry as usize].slots;
    self.slots[id as usize].script = entry;
    self.slots[id as usize].script_pos = slots.len() as u32;
    slots.push(id);
    entry
  }

  fn release_script(&mut self, id: u32) {
    let entry = self.slots[id as usize].script;
    let pos = self.slots[id as usize].script_pos as usize;
    let slots = &mut self.script_entries[entry as usize].slots;
    slots.swap_remove(pos);
    if let Some(&moved) = slots.get(pos) {
      self.slots[moved as usize].script_pos = pos as u32;
    }
    if self.script_entries[entry as usize].slots.is_empty() {
      debug_assert!(self.script_entries[entry as usize].rows.is_empty());
      self
        .scripts
        .remove(self.slots[id as usize].utxo.script_pubkey.as_script());
      self.free_scripts.push(entry);
    }
    self.slots[id as usize].script = NO_SCRIPT;
  }

  fn push_lost(&mut self, piece: Piece) -> anyhow::Result<()> {
    ensure!(piece.len > 0, "empty lost piece");
    if let Some(last) = self.lost.last() {
      ensure!(
        last.offset_end() <= piece.offset,
        "lost pieces out of order at {piece:?}"
      );
    }
    let index = self.lost.len() as u32;
    self.locator_insert(&piece, Loc::Lost(index))?;
    self.acc.add_piece(&self.catalog, &piece, 1);
    for (row, n) in self.acc.take()? {
      self.touch(row).lost += n;
    }
    self.lost.push(piece);
    Ok(())
  }

  fn pop_lost(&mut self) -> anyhow::Result<Piece> {
    let piece = self.lost.pop().context("no lost piece to remove")?;
    self.locator_remove(&piece);
    self.acc.add_piece(&self.catalog, &piece, 1);
    for (row, n) in self.acc.take()? {
      self.touch(row).lost -= n;
    }
    Ok(piece)
  }

  fn push_overwritten(&mut self, overwritten: Overwritten) -> anyhow::Result<()> {
    let index = self.overwritten.len() as u32;
    for piece in &overwritten.utxo.pieces {
      self.locator_insert(piece, Loc::Overwritten(index))?;
    }
    for &(row, n) in &overwritten.rows {
      self.touch(row).overwritten += n;
    }
    self.overwritten.push(overwritten);
    Ok(())
  }

  fn pop_overwritten(&mut self) -> anyhow::Result<Overwritten> {
    let overwritten = self.overwritten.pop().context("nothing overwritten")?;
    for piece in &overwritten.utxo.pieces {
      self.locator_remove(piece);
    }
    for &(row, n) in &overwritten.rows {
      self.touch(row).overwritten -= n;
    }
    Ok(overwritten)
  }

  /// Starts a batch of row changes (a block, an undo or a rollback).
  fn begin_rows(&mut self) {
    self.block_seq += 1;
    self.touched.clear();
  }

  /// Consistency check of the whole state against a from-scratch rebuild:
  /// index, locator, pieces, row aggregates, holders and Fenwick trees.
  /// O(state); for tests and debugging.
  pub fn verify(&self) -> anyhow::Result<()> {
    let rebuilt = Tracker::restore(self.catalog.clone(), self.config.clone(), self.snapshot())
      .context("state does not restore")?;
    ensure!(
      rebuilt.stats == self.stats,
      "row stats drifted from a rebuild:\n{}",
      self
        .stats
        .iter()
        .zip(&rebuilt.stats)
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(row, (a, b))| format!(
          "  {}: kept {a:?} rebuilt {b:?}",
          self.catalog.rows()[row].id
        ))
        .collect::<Vec<_>>()
        .join("\n")
    );
    ensure!(rebuilt.utxo_count == self.utxo_count, "UTXO count drifted");
    ensure!(
      rebuilt.piece_count == self.piece_count,
      "piece count drifted"
    );
    // Every piece has its locator entry, and nothing else has one.
    let mut pieces = 0;
    let mut check = |piece: &Piece, loc: Loc| -> anyhow::Result<()> {
      pieces += 1;
      ensure!(
        self.locator.get(&piece.start) == Some(&loc),
        "locator entry for {piece:?} is {:?}, not {loc:?}",
        self.locator.get(&piece.start)
      );
      ensure!(
        (piece.len > 1) == (self.long_pieces.get(&piece.start) == Some(&piece.end())),
        "long piece index drifted at {piece:?}"
      );
      Ok(())
    };
    for (id, slot) in self.slots.iter().enumerate() {
      if slot.live {
        for piece in &slot.utxo.pieces {
          check(piece, Loc::Slot(id as u32))?;
        }
      }
    }
    for (i, piece) in self.lost.iter().enumerate() {
      check(piece, Loc::Lost(i as u32))?;
    }
    for (i, overwritten) in self.overwritten.iter().enumerate() {
      for piece in &overwritten.utxo.pieces {
        check(piece, Loc::Overwritten(i as u32))?;
      }
    }
    ensure!(
      pieces == self.locator.len()
        && rebuilt.locator.len() == self.locator.len()
        && rebuilt.long_pieces == self.long_pieces,
      "locator has stale entries"
    );
    for (row, fenwick) in self.fenwick.iter().enumerate() {
      ensure!(
        fenwick.total() == rebuilt.fenwick[row].total(),
        "Fenwick total drifted for row {row}"
      );
      for bucket in 0..self.buckets.len().max(rebuilt.buckets.len()) {
        ensure!(
          fenwick.prefix(bucket) == rebuilt.fenwick[row].prefix(bucket),
          "Fenwick prefix drifted for row {row} at bucket {bucket}"
        );
      }
    }
    let live = self.slots.iter().filter(|s| s.live).count();
    ensure!(
      live == self.utxo_count && self.index.len() == live,
      "slab drifted"
    );
    for (id, slot) in self.slots.iter().enumerate() {
      if !slot.live {
        continue;
      }
      ensure!(
        self.index.get(&OutKey(slot.outpoint)) == Some(&(id as u32)),
        "index drifted for {}",
        slot.outpoint
      );
      check_pieces(slot.utxo.value, &slot.utxo.pieces)?;
      let mut acc = RowAcc::new(self.stats.len());
      for piece in &slot.utxo.pieces {
        acc.add_piece(&self.catalog, piece, 1);
      }
      ensure!(
        acc.take()? == slot.rows,
        "cached rows drifted for {}",
        slot.outpoint
      );
    }
    ensure!(
      self.scripts.len() == rebuilt.scripts.len(),
      "script index drifted"
    );
    Ok(())
  }
}
