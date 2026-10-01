//! Shared data model.
//!
//! These types are the contract between the block sources (`source`), the
//! tracker (`tracker`), persistence (`store`) and the HTTP API (`server`).
//! Changing them is a cross-module change: update docs/DESIGN.md and every
//! user in the same commit.

use {
  bitcoin::{BlockHash, OutPoint, Script, ScriptBuf, TxOut, Txid, block::Header},
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInput {
  pub previous_output: OutPoint,
  /// Value, in sats, of the output being spent.
  pub value: u64,
  /// `script_hash` of the spent output's scriptPubKey: compare inputs'
  /// scripts with each other, and with outputs' (hash them with
  /// `script_hash`), without carrying scripts around. Equal scripts hash
  /// equal; two different scripts collide with probability 2^-32 by
  /// chance, but anyone can grind a collision, so never treat a match as
  /// proof.
  pub script_hash: u32,
  /// Sighash flag of the input's (first) signature, as signed: `0x00` taproot
  /// default, `0x01` ALL, `0x02` NONE, `0x03` SINGLE, `| 0x80` ANYONECANPAY.
  /// `None` when no signature was recognized (or the source does not read
  /// witnesses). Marketplace listings sign SINGLE|ANYONECANPAY (`0x83`).
  pub sighash: Option<u8>,
  /// Inscription envelopes this input reveals (ord's envelope in a taproot
  /// script-path spend), in envelope order. Usually empty.
  pub envelopes: Vec<Envelope>,
}

/// The 32-bit script hash `ResolvedInput::script_hash` carries: the low 32
/// bits of SipHash-1-3 with key (0, 0) over the script's bytes (no length
/// prefix). Implemented here, not taken from a dependency, so it never
/// changes and may be stored.
pub fn script_hash(script: &Script) -> u32 {
  siphash13(script.as_bytes()) as u32
}

/// SipHash-1-3 with key (0, 0).
fn siphash13(bytes: &[u8]) -> u64 {
  #[inline(always)]
  fn round(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13) ^ v[0];
    v[0] = v[0].rotate_left(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16) ^ v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21) ^ v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17) ^ v[2];
    v[2] = v[2].rotate_left(32);
  }

  let mut v = [
    0x736f_6d65_7073_6575,
    0x646f_7261_6e64_6f6d,
    0x6c79_6765_6e65_7261,
    0x7465_6462_7974_6573,
  ];

  let (chunks, tail) = bytes.as_chunks::<8>();
  for chunk in chunks {
    let m = u64::from_le_bytes(*chunk);
    v[3] ^= m;
    round(&mut v);
    v[0] ^= m;
  }

  let mut last = [0u8; 8];
  last[..tail.len()].copy_from_slice(tail);
  let m = u64::from_le_bytes(last) | (bytes.len() as u64) << 56;
  v[3] ^= m;
  round(&mut v);
  v[0] ^= m;

  v[2] ^= 0xff;
  round(&mut v);
  round(&mut v);
  round(&mut v);

  v[0] ^ v[1] ^ v[2] ^ v[3]
}

/// One inscription envelope revealed by an input. Per ord, it inscribes the
/// sat at `pointer` (an offset into the transaction's input sats) when the
/// pointer tag is present and points inside the outputs, otherwise the first
/// sat of the input that reveals it, unless the inscription is unbound (see
/// `unrecognized_even_field`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Envelope {
  /// ord's `Inscription::pointer()`: the first value of tag 2 before the
  /// body, little-endian; `None` if there is none or a byte past the eighth
  /// is not zero.
  pub pointer: Option<u64>,
  /// ord's `unrecognized_even_field`. ord makes an inscription **unbound**
  /// (credited to no sat, filed under its unbound outpoint) exactly when
  /// this is set or the revealing input spends a 0-value output
  /// (`ResolvedInput::value == 0`); nothing else unbinds one (other curses
  /// only affect numbering).
  ///
  /// Set when, of the complete (tag, value) pairs before the body (an
  /// incomplete trailing tag is ignored), any is left after ord takes the
  /// tags it knows (the first value of each of 1, 2, 7, 9, 11, 13, 19; every
  /// value of 3, 5, 17) and that tag's first byte is even: an unknown even
  /// tag (`[0]`, 4, 66 "unbound", even `OP_PUSHNUM`s such as OP_4 used as
  /// tags, multi-byte tags like `[2, 0]`) or a repeated pointer tag 2.
  pub unrecognized_even_field: bool,
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
  /// Extraction allowance (10,000): an output counts as extracted when its
  /// value is at most this many sats per individually tracked sat, plus the
  /// length of each run of range sats and this much per run
  /// (`is_extracted`, DESIGN.md §5).
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

/// Sats of `[start, end)` inside the union of `ranges` (which may overlap or
/// come in any order), and the number of maximal runs they form.
fn inside_ranges(start: u64, end: u64, ranges: &[TrackedRange]) -> (u64, u64) {
  let clip = |range: &TrackedRange| {
    let (a, b) = (range.start.max(start), range.end.min(end));
    (a < b).then_some((a, b))
  };
  match ranges {
    [] => (0, 0),
    [range] => clip(range).map_or((0, 0), |(a, b)| (b - a, 1)),
    _ => {
      let mut clipped: Vec<(u64, u64)> = ranges.iter().filter_map(clip).collect();
      clipped.sort_unstable();
      let (mut inside, mut runs) = (0, 0);
      let mut run: Option<(u64, u64)> = None;
      for (a, b) in clipped {
        match &mut run {
          Some((_, run_end)) if a <= *run_end => *run_end = (*run_end).max(b),
          _ => {
            if let Some((run_start, run_end)) = run {
              inside += run_end - run_start;
              runs += 1;
            }
            run = Some((a, b));
          }
        }
      }
      if let Some((run_start, run_end)) = run {
        inside += run_end - run_start;
        runs += 1;
      }
      (inside, runs)
    }
  }
}

/// A piece's share of an output's extraction budget: every sat of the piece
/// outside the config ranges allows `extracted_max_value` sats, and every
/// maximal run of it inside them allows its length plus
/// `extracted_max_value`.
pub fn piece_extraction_budget(piece: &Piece, config: &TrackerConfig) -> u128 {
  let (inside, runs) = inside_ranges(piece.start, piece.end(), &config.ranges);
  let outside = piece.len - inside;
  u128::from(inside)
    + u128::from(config.extracted_max_value) * (u128::from(runs) + u128::from(outside))
}

/// The most an output holding `pieces` may be worth and still count as
/// extracted (DESIGN.md §5): the sum of `piece_extraction_budget`.
pub fn extraction_budget(pieces: &[Piece], config: &TrackerConfig) -> u128 {
  pieces
    .iter()
    .map(|piece| piece_extraction_budget(piece, config))
    .sum()
}

/// Whether a tracked output is extracted (DESIGN.md §5), identically for
/// every row it holds: spendable, created by a block whose header time is at
/// least `extracted_since_time`, and worth at most its
/// `extraction_budget`. With the defaults: one tracked sat in ≤ 10,000
/// sats, 200 rare sats in 66,000 (≤ 2,000,000), 100,000 block 9 sats in
/// 100,438 (≤ 110,000), but not 2 uncommons in a whole BTC (≤ 20,000).
pub fn is_extracted(utxo: &TrackedUtxo, created_time: u32, config: &TrackerConfig) -> bool {
  output_extracted(utxo.kind, utxo.value, &utxo.pieces, created_time, config)
}

/// `is_extracted` for an output given by its parts (a move's output).
pub fn output_extracted(
  kind: UtxoKind,
  value: u64,
  pieces: &[Piece],
  created_time: u32,
  config: &TrackerConfig,
) -> bool {
  if kind != UtxoKind::Spendable || created_time < config.extracted_since_time {
    return false;
  }
  let value = u128::from(value);
  let mut budget = 0u128;
  for piece in pieces {
    budget += piece_extraction_budget(piece, config);
    if budget >= value {
      return true;
    }
  }
  budget >= value
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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn script_hash_is_stable() {
    // Pinned: stored hashes depend on these never changing. (SipHash-1-3,
    // key (0, 0); checked against std's DefaultHasher when written.)
    let script = |prefix: &[u8], fill: u8, len: usize, suffix: &[u8]| {
      let mut bytes = prefix.to_vec();
      bytes.extend(vec![fill; len]);
      bytes.extend(suffix);
      ScriptBuf::from_bytes(bytes)
    };
    for (script, hash) in [
      (ScriptBuf::new(), 0x150c_532c),
      (script(&[0x00, 0x14], 0xab, 20, &[]), 0x0a27_86ec),
      (script(&[0x51, 0x20], 0xcd, 32, &[]), 0x916c_fc6d),
      (
        script(&[0x76, 0xa9, 0x14], 0x11, 20, &[0x88, 0xac]),
        0x90c9_c6af,
      ),
      (script(&[0x6a, 0x01, 0x00], 0, 0, &[]), 0x6baf_f564),
    ] {
      assert_eq!(script_hash(&script), hash, "{script:?}");
    }
  }

  #[test]
  fn script_hash_spreads() {
    // Scripts differing in any one bit (and in length alone) hash apart.
    let base = vec![0x51u8; 34];
    let mut hashes = std::collections::BTreeSet::new();
    hashes.insert(script_hash(Script::from_bytes(&base)));
    for byte in 0..base.len() {
      for bit in 0..8 {
        let mut bytes = base.clone();
        bytes[byte] ^= 1 << bit;
        assert!(hashes.insert(script_hash(Script::from_bytes(&bytes))));
      }
    }
    for len in 0..base.len() {
      assert!(hashes.insert(script_hash(Script::from_bytes(&base[..len]))));
    }
  }

  const B9: u64 = 45_000_000_000;
  const SINCE: u32 = 1_670_976_000;

  fn utxo(value: u64, pieces: &[(u64, u64)]) -> TrackedUtxo {
    let mut offset = 0;
    TrackedUtxo {
      value,
      script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
      height: 900_000,
      kind: UtxoKind::Spendable,
      pieces: pieces
        .iter()
        .map(|&(start, len)| {
          let piece = Piece { offset, start, len };
          offset += len + 330;
          piece
        })
        .collect(),
    }
  }

  /// `n` individually tracked sats, far from block 9.
  fn singles(n: u64) -> Vec<(u64, u64)> {
    (0..n)
      .map(|i| (1_000_000_000_000_000 + i * 1000, 1))
      .collect()
  }

  #[test]
  fn one_tracked_sat_allows_ten_thousand() {
    let config = TrackerConfig::default();
    assert!(is_extracted(&utxo(10_000, &singles(1)), SINCE, &config));
    assert!(!is_extracted(&utxo(10_001, &singles(1)), SINCE, &config));
    assert_eq!(
      extraction_budget(&utxo(1, &singles(1)).pieces, &config),
      10_000
    );
  }

  #[test]
  fn collector_consolidations_are_extracted() {
    // A collector's consolidations: ~330 sats of padding per rare sat.
    let config = TrackerConfig::default();
    for (sats, value) in [(100, 33_000), (200, 66_000), (5_000, 1_924_843)] {
      let consolidation = utxo(value, &singles(sats));
      assert_eq!(
        extraction_budget(&consolidation.pieces, &config),
        u128::from(sats) * 10_000
      );
      assert!(
        is_extracted(&consolidation, SINCE, &config),
        "{sats} in {value}"
      );
    }
  }

  #[test]
  fn range_runs_allow_their_length_plus_ten_thousand() {
    let config = TrackerConfig::default();
    // Near-pure block 9: 100,000 tracked sats in 100,438.
    let chunk = utxo(100_438, &[(B9 + 7, 100_000)]);
    assert_eq!(extraction_budget(&chunk.pieces, &config), 110_000);
    assert!(is_extracted(&chunk, SINCE, &config));
    assert!(!is_extracted(
      &utxo(110_001, &[(B9 + 7, 100_000)]),
      SINCE,
      &config
    ));
    // A pure 5e9 block 9 output.
    assert!(is_extracted(
      &utxo(5_000_000_000, &[(B9, 5_000_000_000)]),
      SINCE,
      &config
    ));
    // Two runs each get their own allowance.
    assert_eq!(
      extraction_budget(&utxo(1, &[(B9, 10), (B9 + 100, 10)]).pieces, &config),
      20 + 20_000
    );
  }

  #[test]
  fn exchange_outputs_with_a_few_uncommons_are_not() {
    let config = TrackerConfig::default();
    let exchange = utxo(100_000_000, &singles(2));
    assert_eq!(extraction_budget(&exchange.pieces, &config), 20_000);
    assert!(!is_extracted(&exchange, SINCE, &config));
  }

  #[test]
  fn pieces_straddling_the_range_edge() {
    // The black of block 9 (in the range) and the uncommon of block 10.
    let config = TrackerConfig::default();
    let edge = utxo(20_001, &[(50_000_000_000 - 1, 2)]);
    assert_eq!(
      extraction_budget(&edge.pieces, &config),
      1 + 10_000 + 10_000
    );
    assert!(is_extracted(&edge, SINCE, &config));
    assert!(!is_extracted(
      &utxo(20_002, &[(50_000_000_000 - 1, 2)]),
      SINCE,
      &config
    ));
    // Without ranges every sat is individual.
    let rangeless = TrackerConfig {
      ranges: Vec::new(),
      ..TrackerConfig::default()
    };
    assert_eq!(extraction_budget(&edge.pieces, &rangeless), 20_000);
  }

  #[test]
  fn unspendable_and_early_outputs_are_never_extracted() {
    let config = TrackerConfig::default();
    let mut burned = utxo(1, &singles(1));
    burned.kind = UtxoKind::Unspendable;
    assert!(!is_extracted(&burned, SINCE, &config));
    assert!(!is_extracted(&utxo(1, &singles(1)), SINCE - 1, &config));
    assert!(!output_extracted(
      UtxoKind::Spendable,
      1,
      &[],
      SINCE,
      &config
    ));
    assert!(output_extracted(
      UtxoKind::Spendable,
      0,
      &[],
      SINCE,
      &config
    ));
  }

  #[test]
  fn overlapping_and_adjacent_ranges_count_as_their_union() {
    let range = |start, end| TrackedRange {
      id: "r".into(),
      start,
      end,
    };
    let config = TrackerConfig {
      ranges: vec![
        range(150, 200),
        range(100, 160),
        range(200, 220),
        range(300, 400),
      ],
      ..TrackerConfig::default()
    };
    // [100, 220) and [300, 400) inside, [220, 300) and [400, 450) outside.
    let pieces = [Piece {
      offset: 0,
      start: 90,
      len: 360,
    }];
    assert_eq!(
      extraction_budget(&pieces, &config),
      120 + 100 + 2 * 10_000 + (10 + 80 + 50) * 10_000
    );
  }

  #[test]
  fn budgets_do_not_overflow() {
    let config = TrackerConfig {
      extracted_max_value: u64::MAX,
      ..TrackerConfig::default()
    };
    let huge = utxo(u64::MAX, &[(0, u64::MAX / 2), (B9, 5_000_000_000)]);
    assert!(extraction_budget(&huge.pieces, &config) > u128::from(u64::MAX));
    assert!(is_extracted(&huge, SINCE, &config));
  }
}

/// How a sale was made (docs/API.md "Sales and inscriptions").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaleKind {
  /// The seller pre-signed a listing (SINGLE|ANYONECANPAY) that a buyer completed.
  Listing,
  /// The seller accepted a buyer's offer.
  Offer,
  /// A sale recognized only by a marketplace fee output (e.g. legacy Magic Eden).
  Unknown,
}

/// One sale of tracked sats: a seller's input that carried them, paid for in
/// the same transaction. Self-contained, because sales are kept forever while
/// moves are pruned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sale {
  pub txid: Txid,
  pub tx_index: u32,
  /// The seller's input.
  pub vin: u32,
  pub kind: SaleKind,
  /// Marketplace id (data/marketplaces.json), or "unknown".
  pub marketplace: String,
  /// The UTXO the seller sold, as it was (value, script, creation, pieces).
  pub seller_outpoint: OutPoint,
  pub seller: TrackedUtxo,
  /// The output paying the seller, when identified.
  pub payment_vout: Option<u32>,
  /// What the buyer paid for this listing: seller proceeds plus the
  /// marketplace fees attributable to it.
  pub price: u64,
  pub seller_proceeds: u64,
  pub fees: u64,
  /// The output that received the sold sats (the buyer's), if one did.
  pub buyer_vout: Option<u32>,
  pub buyer_script: Option<ScriptBuf>,
}

/// An inscription envelope that landed on a tracked sat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InscriptionEvent {
  pub sat: u64,
  pub txid: Txid,
  /// The input that revealed the envelope.
  pub vin: u32,
  /// The transaction's index in its block (orders it against sales).
  pub tx_index: u32,
}

/// What one block did in the market: sales of tracked sats and inscriptions
/// on tracked sats. Derived from the block and the tracker's delta by
/// `market::analyze`; persisted and undone with the block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketDelta {
  pub sales: Vec<Sale>,
  pub inscriptions: Vec<InscriptionEvent>,
}
