//! The tracker: in-memory, authoritative state of every tracked sat, moved
//! block by block following ordinal theory (docs/DESIGN.md §4–§5).
//! Pure and deterministic; no I/O.
//!
//! OWNER: sats+tracker agent. The public API below is the contract used by
//! the indexer, store and server; keep the signatures.

use {
  crate::{
    model::{
      BlockDelta, ResolvedBlock, RowId, RowStats, SatLocation, SupplySnapshot, TrackedUtxo,
      TrackerConfig, TrackerSnapshot,
    },
    sats::SatCatalog,
  },
  bitcoin::{BlockHash, OutPoint},
  std::sync::Arc,
};

pub struct Tracker {}

impl Tracker {
  /// Empty state, before the genesis block.
  pub fn new(_catalog: Arc<SatCatalog>, _config: TrackerConfig) -> Self {
    unimplemented!()
  }

  /// Rebuilds the in-memory state (including row aggregates) from a
  /// persisted snapshot.
  pub fn restore(
    _catalog: Arc<SatCatalog>,
    _config: TrackerConfig,
    _snapshot: TrackerSnapshot,
  ) -> anyhow::Result<Self> {
    unimplemented!()
  }

  pub fn snapshot(&self) -> TrackerSnapshot {
    unimplemented!()
  }

  pub fn height(&self) -> Option<u32> {
    unimplemented!()
  }

  pub fn hash(&self) -> Option<BlockHash> {
    unimplemented!()
  }

  /// Applies the next block (`height == self.height() + 1`, or 0 when
  /// empty; `header.prev_blockhash` must equal `self.hash()`).
  pub fn apply_block(&mut self, _block: &ResolvedBlock) -> anyhow::Result<BlockDelta> {
    unimplemented!()
  }

  /// Reverts the tip block; `delta` is what `apply_block` returned for it.
  pub fn undo_block(&mut self, _delta: &BlockDelta) -> anyhow::Result<()> {
    unimplemented!()
  }

  pub fn utxo(&self, _outpoint: &OutPoint) -> Option<&TrackedUtxo> {
    unimplemented!()
  }

  pub fn locate(&self, _sat: u64) -> Option<SatLocation> {
    unimplemented!()
  }

  pub fn row_stats(&self, _row: RowId) -> RowStats {
    unimplemented!()
  }

  /// Every row's supply at the tip, with the top `top` holders per list.
  pub fn supply(&self, _top: usize) -> SupplySnapshot {
    unimplemented!()
  }

  pub fn utxo_count(&self) -> usize {
    unimplemented!()
  }

  pub fn piece_count(&self) -> usize {
    unimplemented!()
  }
}
