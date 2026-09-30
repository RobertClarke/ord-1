//! Shared data model.
//!
//! These types are the contract between the block sources (`source`), the
//! tracker (`tracker`), persistence (`store`) and the HTTP API (`server`).
//! Changing them is a cross-module change: update docs/DESIGN.md and every
//! user in the same commit.

use {
  bitcoin::{BlockHash, OutPoint, ScriptBuf, TxOut, Txid, block::Header},
  serde::{Deserialize, Serialize},
};

/// A block whose every input carries the value of the output it spends.
///
/// Produced by both block sources (fast: raw block + in-memory UTXO value
/// map; live: `getblock <hash> 3`). The tracker never talks to bitcoind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBlock {
  pub height: u32,
  pub hash: BlockHash,
  pub header: Header,
  /// Transactions in block order; `txs[0]` is the coinbase.
  pub txs: Vec<ResolvedTx>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTx {
  pub txid: Txid,
  /// In input order. Empty for the coinbase.
  pub inputs: Vec<ResolvedInput>,
  /// In output order, including OP_RETURN and zero-value outputs.
  pub outputs: Vec<TxOut>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedInput {
  pub previous_output: OutPoint,
  /// Value, in sats, of the output being spent.
  pub value: u64,
}

/// A run of consecutive tracked sats at consecutive offsets: sat `start + i`
/// sits at `offset + i` for `i < len`.
///
/// `offset` is relative to whatever holds the piece: an output (the usual
/// case), a transaction's fee (`Move::fee`), the coinbase input stream
/// (`Move::mined`) or ord's lost-sats stream (`Move::lost`, `BlockDelta::lost`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Piece {
  pub offset: u64,
  pub start: u64,
  /// Always ≥ 1.
  pub len: u64,
}

impl Piece {
  /// One past the last sat.
  pub fn end(&self) -> u64 {
    self.start + self.len
  }

  /// One past the last offset.
  pub fn offset_end(&self) -> u64 {
    self.offset + self.len
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UtxoKind {
  /// A normal output: its sats move when it is spent.
  Spendable,
  /// Script starts with OP_RETURN or is longer than 10,000 bytes (Core's
  /// `IsUnspendable`): the sats are burned there for good.
  Unspendable,
}

/// An output that holds at least one tracked sat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedUtxo {
  pub value: u64,
  pub script_pubkey: ScriptBuf,
  /// Height of the block that created the output.
  pub height: u32,
  pub kind: UtxoKind,
  /// Sorted by offset, non-overlapping, merged where offsets and sats are
  /// both contiguous.
  pub pieces: Vec<Piece>,
}

/// A tracked UTXO a transaction spent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveInput {
  pub vin: u32,
  pub outpoint: OutPoint,
  /// The UTXO as it was before it was spent.
  pub utxo: TrackedUtxo,
}

/// An output that received tracked sats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveOutput {
  pub vout: u32,
  pub value: u64,
  pub script_pubkey: ScriptBuf,
  pub kind: UtxoKind,
  /// Offsets are within this output.
  pub pieces: Vec<Piece>,
}

/// One transaction that moved tracked sats, or a coinbase that minted or
/// received them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Move {
  pub txid: Txid,
  pub tx_index: u32,
  pub coinbase: bool,
  /// Σ spent values; 0 for the coinbase.
  pub input_value: u64,
  pub output_value: u64,
  /// Tracked inputs only.
  pub inputs: Vec<MoveInput>,
  /// Outputs that received tracked sats only.
  pub outputs: Vec<MoveOutput>,
  /// Non-coinbase: tracked sats paid as fee; offset within this
  /// transaction's fee (`input_value - output_value`).
  pub fee: Vec<Piece>,
  /// Coinbase: tracked sats newly mined by the subsidy; offset within the
  /// coinbase input stream (subsidy first, then fees).
  pub mined: Vec<Piece>,
  /// Coinbase: tracked sats the coinbase did not claim; offset is ord's
  /// lost-sats offset (running total of all lost sats).
  pub lost: Vec<Piece>,
}

/// Index of a row in `SatCatalog::rows()`; stable for a given index.
pub type RowId = u16;

/// Sparse per-row sat counts, sorted by `RowId`, no zero entries.
pub type RowCounts = Vec<(RowId, u64)>;

/// A row's circulating-supply counters at one height (see DESIGN.md §5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowStats {
  /// Sats of the row created by coinbases so far.
  pub mined: u64,
  /// In spendable outputs.
  pub located: u64,
  /// In unspendable outputs.
  pub burned: u64,
  /// Unclaimed by coinbases.
  pub lost: u64,
  /// In outputs replaced by BIP30 duplicate coinbases.
  pub overwritten: u64,
  /// Located in extracted outputs (the old gist's `n_seq`).
  pub extracted: u64,
  /// Distinct script_pubkeys holding ≥1 located sat of the row.
  pub holders: u64,
  /// Distinct script_pubkeys holding ≥1 extracted sat of the row.
  pub extracted_holders: u64,
}

/// A row's counters before and after one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowDelta {
  pub row: RowId,
  pub before: RowStats,
  pub after: RowStats,
}

/// Everything one block changed: enough to persist it and to undo it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockDelta {
  pub height: u32,
  pub hash: BlockHash,
  pub header: Header,
  pub moves: Vec<Move>,
  /// Tracked UTXOs that existed before the block and were spent in it.
  pub spent: Vec<(OutPoint, TrackedUtxo)>,
  /// Tracked UTXOs created in the block that still exist after it.
  pub created: Vec<(OutPoint, TrackedUtxo)>,
  /// Tracked UTXOs replaced by a BIP30 duplicate output in the block.
  pub overwritten: Vec<(OutPoint, TrackedUtxo)>,
  /// Lost pieces added by the block (ord lost offsets).
  pub lost: Vec<Piece>,
  /// All sats (tracked or not) lost before and after the block.
  pub lost_sats_before: u64,
  pub lost_sats_after: u64,
  /// Rows whose counters the block changed.
  pub rows: Vec<RowDelta>,
}

/// Where a tracked sat is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationKind {
  Unspent,
  Burned,
  Lost,
  Overwritten,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatLocation {
  pub kind: LocationKind,
  /// ord's null outpoint for `Lost`.
  pub outpoint: OutPoint,
  /// Offset of the sat itself within the outpoint (or lost stream).
  pub offset: u64,
  /// The piece containing the sat.
  pub piece: Piece,
}

/// A whole sat range tracked in full (DESIGN.md §1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedRange {
  pub id: String,
  /// First sat.
  pub start: u64,
  /// One past the last sat.
  pub end: u64,
}

/// Index-defining configuration: a different value needs a reindex, so the
/// store records it and refuses to open an index built with another one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackerConfig {
  pub ranges: Vec<TrackedRange>,
  /// Outputs worth at most this many sats count as extracted (10,000).
  pub extracted_max_value: u64,
  /// Outputs created by blocks with header time ≥ this count as extracted
  /// (1_670_976_000 = 2022-12-14T00:00:00Z).
  pub extracted_since_time: u32,
}

impl Default for TrackerConfig {
  fn default() -> Self {
    Self {
      ranges: vec![TrackedRange {
        id: "block9".into(),
        start: 45_000_000_000,
        end: 50_000_000_000,
      }],
      extracted_max_value: 10_000,
      extracted_since_time: 1_670_976_000,
    }
  }
}

/// A tracker's full state, as persisted by the store and restored on startup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackerSnapshot {
  /// Last applied block; `None` for an empty index.
  pub height: Option<u32>,
  pub hash: Option<BlockHash>,
  /// Header times for heights `0..=height`.
  pub times: Vec<u32>,
  /// All sats (tracked or not) lost so far.
  pub lost_sats: u64,
  /// Every tracked UTXO, spendable or not.
  pub utxos: Vec<(OutPoint, TrackedUtxo)>,
  /// Tracked lost pieces (ord lost offsets).
  pub lost: Vec<Piece>,
  /// Tracked UTXOs replaced by BIP30 duplicates, with the replacing height.
  pub overwritten: Vec<(OutPoint, u32, TrackedUtxo)>,
  /// `RowStats::mined` per row (the only counter not derivable from the
  /// UTXOs above), indexed by `RowId`.
  pub mined: Vec<u64>,
}

/// One holder of a row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
  pub script_pubkey: ScriptBuf,
  /// Sats of the row it holds (located; extracted ones only for the
  /// extracted list).
  pub count: u64,
  /// Outputs holding them.
  pub utxos: u64,
  /// Latest creation height among those outputs.
  pub last_received_height: u32,
}

/// A row's supply at the tracker's tip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowSupply {
  pub row: RowId,
  pub stats: RowStats,
  /// Non-extracted located sats in outputs created within the last N years
  /// of the tip's time, for N in `ACTIVE_YEARS`.
  pub active_non_extracted: Vec<(u32, u64)>,
  pub top_extracted_holders: Vec<Holder>,
  pub top_non_extracted_holders: Vec<Holder>,
}

/// Year windows reported by `RowSupply::active_non_extracted`.
pub const ACTIVE_YEARS: [u32; 6] = [1, 2, 3, 4, 5, 7];

/// All rows' supply at one tip, published by the indexer after each commit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupplySnapshot {
  pub height: Option<u32>,
  pub hash: Option<BlockHash>,
  pub time: Option<u32>,
  pub rows: Vec<RowSupply>,
}
