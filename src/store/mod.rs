//! Persistence (redb): tracked UTXOs, the sat locator, the script index,
//! per-block moves, per-row history, headers and undo records for reorgs.
//! See docs/DESIGN.md §6.
//!
//! OWNER: store+server+indexer agent. The public API below is used by the
//! indexer and server; extend freely, keep the listed signatures.

use {
  crate::model::{
    BlockDelta, Move, RowId, RowStats, SatLocation, TrackedUtxo, TrackerConfig, TrackerSnapshot,
  },
  bitcoin::{BlockHash, OutPoint, Script, block::Header},
  serde::{Deserialize, Serialize},
  std::path::Path,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexMeta {
  pub schema_version: u32,
  pub chain: String,
  pub config: TrackerConfig,
  pub catalog_fingerprint: String,
  pub height: Option<u32>,
  pub hash: Option<BlockHash>,
  /// The fast initial sync reached its stop height.
  pub fast_sync_complete: bool,
}

pub struct Store {}

impl Store {
  pub fn open(_path: &Path) -> anyhow::Result<Self> {
    unimplemented!()
  }

  pub fn meta(&self) -> anyhow::Result<Option<IndexMeta>> {
    unimplemented!()
  }

  pub fn init(&self, _meta: &IndexMeta) -> anyhow::Result<()> {
    unimplemented!()
  }

  pub fn load_snapshot(&self) -> anyhow::Result<TrackerSnapshot> {
    unimplemented!()
  }

  /// Persists consecutive blocks atomically. `mined` is the tracker's
  /// per-row mined counters after the last delta.
  pub fn commit(
    &self,
    _deltas: &[BlockDelta],
    _mined: &[u64],
    _fast_sync_complete: bool,
  ) -> anyhow::Result<()> {
    unimplemented!()
  }

  /// Reverts the tip block in the database and returns its delta so the
  /// tracker can undo it too. `None` when no undo record is left.
  pub fn rollback_tip(&self) -> anyhow::Result<Option<BlockDelta>> {
    unimplemented!()
  }

  pub fn block_hash(&self, _height: u32) -> anyhow::Result<Option<BlockHash>> {
    unimplemented!()
  }

  pub fn header(&self, _height: u32) -> anyhow::Result<Option<Header>> {
    unimplemented!()
  }

  pub fn utxo(&self, _outpoint: OutPoint) -> anyhow::Result<Option<TrackedUtxo>> {
    unimplemented!()
  }

  pub fn locate(&self, _sat: u64) -> anyhow::Result<Option<SatLocation>> {
    unimplemented!()
  }

  pub fn utxos_for_script(
    &self,
    _script_pubkey: &Script,
  ) -> anyhow::Result<Vec<(OutPoint, TrackedUtxo)>> {
    unimplemented!()
  }

  pub fn moves(&self, _height: u32) -> anyhow::Result<Option<Vec<Move>>> {
    unimplemented!()
  }

  pub fn row_history(
    &self,
    _row: RowId,
    _from: u32,
    _to: u32,
  ) -> anyhow::Result<Vec<(u32, RowStats)>> {
    unimplemented!()
  }
}
