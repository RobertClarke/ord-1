//! Shared data model.
//!
//! These types are the contract between the block sources (`source`), the
//! tracker (`tracker`), persistence (`store`) and the HTTP API (`server`).
//! Changing them is a cross-module change: update docs/DESIGN.md and every
//! user in the same commit.

use {
  bitcoin::{
    BlockHash, OutPoint, Script, ScriptBuf, TxOut, Txid, block::Header, hashes::Hash as _,
  },
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

/// The mint year of `RowId`-space year index 0 (DESIGN.md §5.1).
pub const FIRST_YEAR: u16 = 2009;

/// Mint years the tracker breaks every row down by: `FIRST_YEAR` to 2136,
/// past the last block with a subsidy. A sat of a later year counts in the
/// last one.
pub const YEAR_SLOTS: usize = 128;

/// Bit `y` set for year index `y` (year `FIRST_YEAR + y`).
pub type YearMask = u128;

/// The tracker's id of row `row` restricted to the sats mined in year index
/// `year` (DESIGN.md §5.1): ids `0..rows` are the catalog's rows, then one
/// block of `YEAR_SLOTS` ids per catalog row.
pub fn year_row(rows: usize, row: RowId, year: usize) -> RowId {
  debug_assert!(usize::from(row) < rows && year < YEAR_SLOTS);
  (rows + usize::from(row) * YEAR_SLOTS + year) as RowId
}

/// The catalog row and year index of a year row id; `None` for a catalog
/// row (`id < rows`) or a luck row.
pub fn split_year_row(rows: usize, id: RowId) -> Option<(RowId, usize)> {
  match split_row(rows, id) {
    TrackedRow::Year(row, year) => Some((row, year)),
    _ => None,
  }
}

/// Luck values the tracker breaks every row down by (DESIGN.md §5.2):
/// `0..LUCK_SLOTS`, a block of higher luck counting in the last one (the
/// luckiest mainnet block so far has 21).
pub const LUCK_SLOTS: usize = 32;

/// Bit `luck` set for luck `luck`.
pub type LuckMask = u32;

/// The tracker's id of row `row` restricted to the sats mined in blocks of
/// luck `luck` (DESIGN.md §5.2): after the year rows, one block of
/// `LUCK_SLOTS` ids per catalog row.
pub fn luck_row(rows: usize, row: RowId, luck: usize) -> RowId {
  debug_assert!(usize::from(row) < rows && luck < LUCK_SLOTS);
  (rows * (1 + YEAR_SLOTS) + usize::from(row) * LUCK_SLOTS + luck) as RowId
}

/// The catalog row and luck of a luck row id; `None` for any other id.
pub fn split_luck_row(rows: usize, id: RowId) -> Option<(RowId, usize)> {
  match split_row(rows, id) {
    TrackedRow::Luck(row, luck) => Some((row, luck)),
    _ => None,
  }
}

/// Mint months the tracker breaks every row down by (DESIGN.md §5.3):
/// `FIRST_YEAR`-01 to 2058-12 (50 years), a sat of a later month counting
/// in the last one. Bounded by `RowId`: every catalog row takes this many
/// ids next to its year and luck rows, so 600 leaves room for 86 rows.
pub const MONTH_SLOTS: usize = 600;

/// The tracker's id of row `row` restricted to the sats mined in month
/// index `month` (`(year - FIRST_YEAR) * 12 + month - 1`, DESIGN.md §5.3):
/// after the luck rows, one block of `MONTH_SLOTS` ids per catalog row.
pub fn month_row(rows: usize, row: RowId, month: usize) -> RowId {
  debug_assert!(usize::from(row) < rows && month < MONTH_SLOTS);
  (rows * (1 + YEAR_SLOTS + LUCK_SLOTS) + usize::from(row) * MONTH_SLOTS + month) as RowId
}

/// The catalog row and month index of a month row id; `None` for any other
/// id.
pub fn split_month_row(rows: usize, id: RowId) -> Option<(RowId, usize)> {
  match split_row(rows, id) {
    TrackedRow::Month(row, month) => Some((row, month)),
    _ => None,
  }
}

/// The first tracker row id of the month rows: every id from it on is one.
pub fn first_month_row(rows: usize) -> usize {
  rows * (1 + YEAR_SLOTS + LUCK_SLOTS)
}

/// What a tracker row id counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackedRow {
  /// A catalog row.
  Catalog(RowId),
  /// A catalog row's sats of one mint year (index).
  Year(RowId, usize),
  /// A catalog row's sats of one luck.
  Luck(RowId, usize),
  /// A catalog row's sats of one mint month (index).
  Month(RowId, usize),
}

/// Splits a tracker row id (`id < tracked_row_ids(rows)`).
pub fn split_row(rows: usize, id: RowId) -> TrackedRow {
  let id = usize::from(id);
  if id < rows {
    return TrackedRow::Catalog(id as RowId);
  }
  let index = id - rows;
  if index < rows * YEAR_SLOTS {
    return TrackedRow::Year((index / YEAR_SLOTS) as RowId, index % YEAR_SLOTS);
  }
  let index = index - rows * YEAR_SLOTS;
  if index < rows * LUCK_SLOTS {
    return TrackedRow::Luck((index / LUCK_SLOTS) as RowId, index % LUCK_SLOTS);
  }
  let index = index - rows * LUCK_SLOTS;
  TrackedRow::Month((index / MONTH_SLOTS) as RowId, index % MONTH_SLOTS)
}

/// Ids the tracker keeps counters for, given the catalog's row count.
pub fn tracked_row_ids(rows: usize) -> usize {
  rows * (1 + YEAR_SLOTS + LUCK_SLOTS + MONTH_SLOTS)
}

/// Leading zero bits of a 256-bit number given big-endian.
fn leading_zero_bits(bytes: impl IntoIterator<Item = u8>) -> u32 {
  let mut zeros = 0;
  for byte in bytes {
    if byte != 0 {
      return zeros + byte.leading_zeros();
    }
    zeros += 8;
  }
  zeros
}

/// Leading zero bits (of 256) of the target compact `bits` encode, as
/// bitcoin-monitoring's `targetHexFromBits` reads them (sign bit dropped;
/// a target wider than 256 bits has none).
fn target_zero_bits(bits: u32) -> u32 {
  let exponent = bits >> 24;
  let mantissa = bits & 0x007f_ffff;
  let width = if exponent <= 3 {
    let shifted = mantissa.checked_shr(8 * (3 - exponent)).unwrap_or(0);
    32 - shifted.leading_zeros()
  } else if mantissa == 0 {
    0
  } else {
    32 - mantissa.leading_zeros() + 8 * (exponent - 3)
  };
  256u32.saturating_sub(width)
}

/// A block's **luck** (DESIGN.md §5.2): the leading zero bits of its hash
/// (as shown, big-endian) less those of the target its header's `bits`
/// encode; `None` when negative (a hash above its target: no valid block).
/// The same definition and values as bitcoin-monitoring's
/// `calculateLuckFromHashAndTarget`.
pub fn block_luck(hash: &BlockHash, bits: bitcoin::CompactTarget) -> Option<u32> {
  let bytes: [u8; 32] = hash.to_byte_array();
  let hash_zeros = leading_zero_bits(bytes.into_iter().rev());
  hash_zeros.checked_sub(target_zero_bits(bits.to_consensus()))
}

/// The luck the tracker counts a block's sats under: `block_luck`, a
/// block with none (only on chains without valid proof of work, as in
/// tests) as luck 0, capped at `LUCK_SLOTS - 1`.
pub fn luck_slot(hash: &BlockHash, bits: bitcoin::CompactTarget) -> u8 {
  block_luck(hash, bits).map_or(0, |luck| luck.min(LUCK_SLOTS as u32 - 1) as u8)
}

/// The UTC calendar year of a unix time.
pub fn year_of_time(time: u32) -> u32 {
  year_month_of_time(time).0
}

/// The year index (`year - FIRST_YEAR`, within `0..YEAR_SLOTS`) of a time.
pub fn year_index_of_time(time: u32) -> usize {
  (year_of_time(time).saturating_sub(u32::from(FIRST_YEAR)) as usize).min(YEAR_SLOTS - 1)
}

/// The UTC calendar year and month (1–12) of a unix time.
pub fn year_month_of_time(time: u32) -> (u32, u32) {
  // Howard Hinnant's civil_from_days, for days ≥ 0.
  let days = time / 86_400 + 719_468;
  let era = days / 146_097;
  let doe = days - era * 146_097;
  let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let month = if mp < 10 { mp + 3 } else { mp - 9 };
  (yoe + era * 400 + u32::from(month <= 2), month)
}

/// The month index (`(year - FIRST_YEAR) * 12 + month - 1`, within
/// `0..MONTH_SLOTS`) of a time.
pub fn month_index_of_time(time: u32) -> usize {
  let (year, month) = year_month_of_time(time);
  let index = (year.saturating_sub(u32::from(FIRST_YEAR)) as usize) * 12 + month as usize - 1;
  index.min(MONTH_SLOTS - 1)
}

/// `(year, month)` (1–12) of a month index.
pub fn month_of_index(month: usize) -> (u16, u8) {
  (FIRST_YEAR + (month / 12) as u16, (month % 12) as u8 + 1)
}

/// `2017-12` for a month index.
pub fn month_name(month: usize) -> String {
  let (year, month) = month_of_index(month);
  format!("{year}-{month:02}")
}

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
  /// Where the address history starts (docs/API.md "Address history").
  #[serde(default)]
  pub address_history_from: HistoryFrom,
}

/// The first block the address history covers (`--address-history-from`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum HistoryFrom {
  /// The first block whose header time is at least `extracted_since_time`
  /// (2022-12-14, the ordinals era), and every block after it: every
  /// extraction, and the sales of that era.
  #[default]
  Ordinals,
  /// Every block from genesis (an order of magnitude bigger on mainnet).
  All,
  /// Every block from this height.
  Height(u32),
}

impl HistoryFrom {
  /// Whether the block at `height` is covered, given `max_time`, the
  /// highest header time of blocks `0..=height` (header times are not
  /// monotone; the running maximum is).
  pub fn covers(self, height: u32, max_time: u32, config: &TrackerConfig) -> bool {
    match self {
      Self::Ordinals => max_time >= config.extracted_since_time,
      Self::All => true,
      Self::Height(from) => height >= from,
    }
  }
}

impl std::fmt::Display for HistoryFrom {
  fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
    match self {
      Self::Ordinals => write!(f, "ordinals"),
      Self::All => write!(f, "all"),
      Self::Height(height) => write!(f, "{height}"),
    }
  }
}

impl std::str::FromStr for HistoryFrom {
  type Err = String;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "ordinals" => Ok(Self::Ordinals),
      "all" => Ok(Self::All),
      _ => s
        .parse()
        .map(Self::Height)
        .map_err(|_| format!("expected a height, `ordinals` or `all`, got `{s}`")),
    }
  }
}

impl Serialize for HistoryFrom {
  fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(self)
  }
}

impl<'de> Deserialize<'de> for HistoryFrom {
  fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    String::deserialize(deserializer)?
      .parse()
      .map_err(serde::de::Error::custom)
  }
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
      address_history_from: HistoryFrom::Ordinals,
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
  /// Luck slots (`luck_slot`) for heights `0..=height`, from the headers.
  pub lucks: Vec<u8>,
  /// All sats (tracked or not) lost so far.
  pub lost_sats: u64,
  /// Every tracked UTXO, spendable or not.
  pub utxos: Vec<(OutPoint, TrackedUtxo)>,
  /// Tracked lost pieces (ord lost offsets).
  pub lost: Vec<Piece>,
  /// Tracked UTXOs replaced by BIP30 duplicates, with the replacing height.
  pub overwritten: Vec<(OutPoint, u32, TrackedUtxo)>,
  /// `RowStats::mined` per row (the only counter not derivable from the
  /// UTXOs above), indexed by `RowId`: the catalog's rows, then their year
  /// rows (`tracked_row_ids`).
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
  /// The same broken down by mint year (DESIGN.md §5.1).
  pub years: YearSupply,
  /// The same broken down by block luck (DESIGN.md §5.2).
  pub lucks: LuckSupply,
  /// The same broken down by mint month (DESIGN.md §5.3).
  pub months: MonthSupply,
}

/// Every row's supply by the month its sats were mined.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonthSupply {
  /// First height of each month index, from `FIRST_YEAR`-01 to the tip's
  /// month (a month no block was mined in starts where the next one does).
  pub starts: Vec<u32>,
  /// One per catalog row, in `RowId` order.
  pub rows: Vec<RowMonths>,
}

/// One catalog row's supply by mint month: the months it has mined sats
/// in, ascending, each month's supply under its month row id
/// (`month_row`), holders exact (one month), top holders at most
/// `YEAR_TOP_HOLDERS` per list. Months are not combined, so no holder
/// masks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowMonths {
  pub row: RowId,
  pub cells: Vec<(usize, RowSupply)>,
}

/// Every row's supply by the luck of the blocks its sats were mined in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LuckSupply {
  /// Each block's luck slot (`luck_slot`), by height up to the tip.
  pub heights: Vec<u8>,
  /// One per catalog row, in `RowId` order.
  pub rows: Vec<RowLucks>,
}

/// One catalog row's supply by luck.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowLucks {
  pub row: RowId,
  /// The lucks the row has mined sats of, ascending: each luck's supply
  /// under its luck row id (`luck_row`), without top holders.
  pub cells: Vec<(usize, RowSupply)>,
  /// Holders by the set of lucks they hold located sats of: (luck mask,
  /// scripts); the holders of any set of lucks are exact.
  pub holder_masks: Vec<(LuckMask, u64)>,
  /// The same for extracted sats.
  pub extracted_holder_masks: Vec<(LuckMask, u64)>,
}

impl RowLucks {
  /// Scripts holding located (`extracted`: extracted) sats of the row mined
  /// in blocks of any luck in `lucks`.
  pub fn holders(&self, lucks: LuckMask, extracted: bool) -> u64 {
    let masks = if extracted {
      &self.extracted_holder_masks
    } else {
      &self.holder_masks
    };
    masks
      .iter()
      .filter(|(mask, _)| mask & lucks != 0)
      .map(|(_, count)| count)
      .sum()
  }
}

/// Every row's supply by the year its sats were mined.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct YearSupply {
  /// First height of each year, from `FIRST_YEAR` to the tip's year (a
  /// year no block was mined in starts where the next one does).
  pub starts: Vec<u32>,
  /// One per catalog row, in `RowId` order.
  pub rows: Vec<RowYears>,
}

/// One catalog row's supply by mint year.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowYears {
  pub row: RowId,
  /// The years the row has mined sats in, ascending: each year's supply
  /// under its year row id (`year_row`); top holders at most
  /// `YEAR_TOP_HOLDERS` per list.
  pub cells: Vec<(usize, RowSupply)>,
  /// Holders by the set of years they hold located sats of: (year mask,
  /// scripts), so the holders of any set of years are exact (the scripts
  /// whose mask meets it).
  pub holder_masks: Vec<(YearMask, u64)>,
  /// The same for extracted sats.
  pub extracted_holder_masks: Vec<(YearMask, u64)>,
}

impl RowYears {
  /// Scripts holding located (`extracted`: extracted) sats of the row mined
  /// in any year of `years`.
  pub fn holders(&self, years: YearMask, extracted: bool) -> u64 {
    let masks = if extracted {
      &self.extracted_holder_masks
    } else {
      &self.holder_masks
    };
    masks
      .iter()
      .filter(|(mask, _)| mask & years != 0)
      .map(|(_, count)| count)
      .sum()
  }
}

/// Top holders per list kept for a year or month row (the catalog rows keep the
/// indexer's `supply_top`).
pub const YEAR_TOP_HOLDERS: usize = 100;

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
  fn years_of_times() {
    for (time, year) in [
      (0, 1970),
      (1_231_006_505, 2009), // genesis
      (1_262_303_999, 2009), // 2009-12-31T23:59:59Z
      (1_262_304_000, 2010),
      (1_330_473_600, 2012), // 2012-02-29
      (1_767_225_599, 2025),
      (1_767_225_600, 2026),
      (4_102_444_800, 2100),
      (u32::MAX, 2106),
    ] {
      assert_eq!(year_of_time(time), year, "{time}");
    }
    assert_eq!(year_index_of_time(0), 0);
    assert_eq!(year_index_of_time(1_262_304_000), 1);
    assert_eq!(year_index_of_time(u32::MAX), 97);
  }

  #[test]
  fn months_of_times() {
    for (time, (year, month), index) in [
      (1_231_006_505, (2009, 1), 0), // genesis
      (1_233_446_399, (2009, 1), 0), // 2009-01-31T23:59:59Z
      (1_233_446_400, (2009, 2), 1),
      (1_262_303_999, (2009, 12), 11),
      (1_262_304_000, (2010, 1), 12),
      (1_330_473_600, (2012, 2), 37), // 2012-02-29
      (1_330_559_999, (2012, 2), 37),
      (1_330_560_000, (2012, 3), 38),
      (1_512_086_400, (2017, 12), 107), // 2017-12-01
      (1_788_220_800, (2026, 9), 212),  // 2026-09-01
      (2_871_763_200, (2061, 1), MONTH_SLOTS - 1),
    ] {
      assert_eq!(year_month_of_time(time), (year, month), "{time}");
      assert_eq!(month_index_of_time(time), index, "{time}");
      if index < MONTH_SLOTS - 1 {
        assert_eq!(month_of_index(index), (year as u16, month as u8));
      }
    }
    assert_eq!(month_index_of_time(0), 0);
    assert_eq!(month_name(0), "2009-01");
    assert_eq!(month_name(107), "2017-12");
    assert_eq!(month_name(MONTH_SLOTS - 1), "2058-12");
    // A month index's year is its year index.
    for time in (1_231_006_505..2_000_000_000u32).step_by(86_400 * 7) {
      assert_eq!(month_index_of_time(time) / 12, year_index_of_time(time));
    }
  }

  #[test]
  fn year_row_ids() {
    let rows = 72;
    assert_eq!(tracked_row_ids(rows), 72 * 761);
    assert!(tracked_row_ids(rows) <= usize::from(RowId::MAX));
    assert_eq!(split_year_row(rows, 71), None);
    assert_eq!(split_row(rows, 71), TrackedRow::Catalog(71));
    for (row, year) in [(0, 0), (0, 127), (5, 17), (71, 127)] {
      let id = year_row(rows, row, year);
      assert!(usize::from(id) >= rows && usize::from(id) < tracked_row_ids(rows));
      assert_eq!(split_year_row(rows, id), Some((row, year)));
      assert_eq!(split_luck_row(rows, id), None);
    }
    for (row, luck) in [(0, 0), (0, 31), (5, 17), (71, 31)] {
      let id = luck_row(rows, row, luck);
      assert!(usize::from(id) >= rows * 129 && usize::from(id) < tracked_row_ids(rows));
      assert_eq!(split_luck_row(rows, id), Some((row, luck)));
      assert_eq!(split_year_row(rows, id), None);
    }
    assert_eq!(
      usize::from(luck_row(rows, 71, LUCK_SLOTS - 1)) + 1,
      first_month_row(rows)
    );
    for (row, month) in [(0, 0), (0, 599), (5, 107), (71, 599)] {
      let id = month_row(rows, row, month);
      assert!(usize::from(id) >= first_month_row(rows) && usize::from(id) < tracked_row_ids(rows));
      assert_eq!(split_month_row(rows, id), Some((row, month)));
      assert_eq!(split_luck_row(rows, id), None);
      assert_eq!(split_year_row(rows, id), None);
    }
    assert_eq!(
      usize::from(month_row(rows, 71, MONTH_SLOTS - 1)),
      tracked_row_ids(rows) - 1
    );
    // Room for 86 catalog rows.
    assert!(tracked_row_ids(86) <= usize::from(RowId::MAX));
    assert!(tracked_row_ids(87) > usize::from(RowId::MAX));
  }

  #[test]
  fn block_lucks() {
    use bitcoin::CompactTarget;
    let hash = |hex: &str| hex.parse::<BlockHash>().unwrap();
    let bits = CompactTarget::from_consensus;
    // Real mainnet blocks: genesis (luck 11), block 1 and 840,000.
    assert_eq!(
      block_luck(
        &hash("000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"),
        bits(0x1d00_ffff)
      ),
      Some(11)
    );
    assert_eq!(
      block_luck(
        &hash("00000000839a8e6886ab5951d76f411475428afc90947ee320161bbf18eb6048"),
        bits(0x1d00_ffff)
      ),
      Some(0)
    );
    assert_eq!(
      block_luck(
        &hash("0000000000000000000320283a032748cef8227873ff4872689bf23f1cda83a5"),
        bits(0x1703_4219)
      ),
      Some(0)
    );
    // The target's leading zeros, as targetHexFromBits reads them.
    assert_eq!(target_zero_bits(0x1d00_ffff), 32);
    assert_eq!(target_zero_bits(0x207f_ffff), 1);
    assert_eq!(target_zero_bits(0x1703_4219), 78);
    assert_eq!(target_zero_bits(0x0300_0001), 255);
    assert_eq!(target_zero_bits(0x0100_8000), 256);
    assert_eq!(target_zero_bits(0x2200_ffff), 0);
    assert_eq!(target_zero_bits(0x1d80_ffff), 32);
    // A hash above its target has no luck; the tracker counts it as 0, and
    // caps the luckiest at the last slot.
    let high = hash("00ff000000000000000000000000000000000000000000000000000000000000");
    assert_eq!(block_luck(&high, bits(0x1d00_ffff)), None);
    assert_eq!(luck_slot(&high, bits(0x1d00_ffff)), 0);
    let lucky = hash("0000000000000000000000000000000000000000000000000000000000000001");
    assert_eq!(block_luck(&lucky, bits(0x1d00_ffff)), Some(255 - 32));
    assert_eq!(
      usize::from(luck_slot(&lucky, bits(0x1d00_ffff))),
      LUCK_SLOTS - 1
    );
  }

  #[test]
  fn holders_of_luck_sets() {
    let lucks = RowLucks {
      holder_masks: vec![(0b0001, 10), (0b0011, 5), (0b1100, 2)],
      extracted_holder_masks: vec![(0b0010, 3)],
      ..RowLucks::default()
    };
    // Luck ≥ N: the masks with a bit at N or above.
    let at_least = |n: u32| LuckMask::MAX << n;
    assert_eq!(lucks.holders(at_least(0), false), 17);
    assert_eq!(lucks.holders(at_least(1), false), 7);
    assert_eq!(lucks.holders(at_least(2), false), 2);
    assert_eq!(lucks.holders(at_least(4), false), 0);
    assert_eq!(lucks.holders(0b0010, false), 5);
    assert_eq!(lucks.holders(at_least(1), true), 3);
    assert_eq!(lucks.holders(at_least(2), true), 0);
  }

  #[test]
  fn holders_of_year_sets() {
    let years = RowYears {
      holder_masks: vec![(0b001, 10), (0b011, 5), (0b100, 2)],
      extracted_holder_masks: vec![(0b010, 3)],
      ..RowYears::default()
    };
    assert_eq!(years.holders(0b001, false), 15);
    assert_eq!(years.holders(0b010, false), 5);
    assert_eq!(years.holders(0b011, false), 15);
    assert_eq!(years.holders(0b111, false), 17);
    assert_eq!(years.holders(0b101, true), 0);
    assert_eq!(years.holders(0b010, true), 3);
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
