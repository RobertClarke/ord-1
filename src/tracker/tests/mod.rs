//! Tracker tests: hand-written scenarios, a differential test against a naive
//! full sat-range model, undo/snapshot round trips and a benchmark.

mod bench;
mod differential;
mod lucks;
mod mainnet;
mod scenarios;
mod years;

use {
  super::*,
  crate::{
    model::{ResolvedInput, ResolvedTx},
    sats::math::{first_sat, subsidy},
  },
  bitcoin::{
    Amount, CompactTarget, ScriptBuf, TxMerkleNode, TxOut, Txid,
    block::{Header, Version},
    hashes::Hash,
  },
  std::sync::OnceLock,
};

pub(crate) fn catalog_for(config: &TrackerConfig) -> Arc<SatCatalog> {
  static DEFAULT: OnceLock<Arc<SatCatalog>> = OnceLock::new();
  static RANGELESS: OnceLock<Arc<SatCatalog>> = OnceLock::new();
  static CUSTOM: OnceLock<Arc<SatCatalog>> = OnceLock::new();
  let cell = if *config == TrackerConfig::default() {
    &DEFAULT
  } else if config.ranges.is_empty() {
    &RANGELESS
  } else {
    &CUSTOM
  };
  let catalog = cell.get_or_init(|| Arc::new(SatCatalog::new(config).unwrap()));
  assert_eq!(
    catalog.config(),
    config,
    "one custom config per test binary"
  );
  catalog.clone()
}

pub(crate) fn rangeless_config() -> TrackerConfig {
  TrackerConfig {
    ranges: Vec::new(),
    ..TrackerConfig::default()
  }
}

/// A small range straddling the boundary of blocks 11 and 12.
pub(crate) fn custom_config() -> TrackerConfig {
  TrackerConfig {
    ranges: vec![crate::model::TrackedRange {
      id: "custom".into(),
      start: 60_000_000_000 - 1_500_000_000,
      end: 60_000_000_000 + 700_000_000,
    }],
    ..TrackerConfig::default()
  }
}

/// An empty tracker whose tip is `height` (so the next block is `height + 1`).
pub(crate) fn tracker_at(config: &TrackerConfig, height: Option<u32>) -> (Tracker, Chain) {
  let catalog = catalog_for(config);
  let rows = catalog.rows().len();
  let chain = Chain::at(height);
  let snapshot = TrackerSnapshot {
    height,
    hash: height.map(|_| chain.prev),
    times: height.map_or(Vec::new(), |h| (0..=h).map(|i| chain.time_of(i)).collect()),
    lucks: height.map_or(Vec::new(), |h| vec![0; h as usize + 1]),
    lost_sats: 0,
    utxos: Vec::new(),
    lost: Vec::new(),
    overwritten: Vec::new(),
    mined: vec![0; crate::model::tracked_row_ids(rows)],
  };
  (
    Tracker::restore(catalog, config.clone(), snapshot).unwrap(),
    chain,
  )
}

pub(crate) fn txid(n: u64) -> Txid {
  let mut bytes = [0u8; 32];
  bytes[..8].copy_from_slice(&n.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes());
  bytes[8..16].copy_from_slice(&n.to_le_bytes());
  Txid::from_byte_array(bytes)
}

/// A P2WPKH-shaped script unique to `n`.
pub(crate) fn script(n: u8) -> ScriptBuf {
  let mut bytes = vec![0x00, 0x14];
  bytes.extend([n; 20]);
  ScriptBuf::from_bytes(bytes)
}

pub(crate) fn out(value: u64) -> TxOut {
  out_to(value, 1)
}

pub(crate) fn out_to(value: u64, owner: u8) -> TxOut {
  TxOut {
    value: Amount::from_sat(value),
    script_pubkey: script(owner),
  }
}

pub(crate) fn op_return(value: u64) -> TxOut {
  TxOut {
    value: Amount::from_sat(value),
    script_pubkey: ScriptBuf::from_bytes(vec![0x6a, 0x04, 1, 2, 3, 4]),
  }
}

/// Unspendable because it is longer than 10,000 bytes.
pub(crate) fn huge_script(value: u64) -> TxOut {
  TxOut {
    value: Amount::from_sat(value),
    script_pubkey: ScriptBuf::from_bytes(vec![0x51; 10_001]),
  }
}

pub(crate) fn coinbase(txid: Txid, outputs: Vec<TxOut>) -> ResolvedTx {
  ResolvedTx {
    txid,
    inputs: Vec::new(),
    outputs,
  }
}

pub(crate) fn tx(txid: Txid, inputs: &[(OutPoint, u64)], outputs: Vec<TxOut>) -> ResolvedTx {
  ResolvedTx {
    txid,
    inputs: inputs
      .iter()
      .map(|&(previous_output, value)| ResolvedInput {
        previous_output,
        value,
        script_hash: 0,
        sighash: None,
        envelopes: Vec::new(),
      })
      .collect(),
    outputs,
  }
}

pub(crate) fn op(txid: Txid, vout: u32) -> OutPoint {
  OutPoint { txid, vout }
}

/// Builds consecutive blocks with consistent heights, hashes and times.
pub(crate) struct Chain {
  pub next: u32,
  pub prev: BlockHash,
  /// Overrides for the next block's time.
  pub next_time: Option<u32>,
  /// The next block's luck slot: its nonce is ground until it has it.
  pub next_luck: Option<u8>,
}

impl Chain {
  /// Continues after `height` (a synthetic parent hash stands in for it).
  pub fn at(height: Option<u32>) -> Self {
    match height {
      None => Self {
        next: 0,
        prev: BlockHash::all_zeros(),
        next_time: None,
        next_luck: None,
      },
      Some(h) => Self {
        next: h + 1,
        prev: BlockHash::from_byte_array([0xab; 32]),
        next_time: None,
        next_luck: None,
      },
    }
  }

  /// Ten minutes per block from the genesis time.
  pub fn time_of(&self, height: u32) -> u32 {
    1_231_006_505 + height * 600
  }

  pub fn block(&mut self, txs: Vec<ResolvedTx>) -> ResolvedBlock {
    let height = self.next;
    let time = self.next_time.take().unwrap_or(self.time_of(height));
    let mut merkle = [0u8; 32];
    merkle[..4].copy_from_slice(&height.to_le_bytes());
    merkle[4..12].copy_from_slice(&(txs.len() as u64).to_le_bytes());
    if let Some(first) = txs.first() {
      merkle[12..].copy_from_slice(&first.txid.as_byte_array()[..20]);
    }
    let mut header = Header {
      version: Version::ONE,
      prev_blockhash: self.prev,
      merkle_root: TxMerkleNode::from_byte_array(merkle),
      time,
      bits: CompactTarget::from_consensus(0x207f_ffff),
      nonce: height,
    };
    if let Some(luck) = self.next_luck.take() {
      while crate::model::luck_slot(&header.block_hash(), header.bits) != luck {
        header.nonce = header.nonce.wrapping_add(0x9e37_79b9);
      }
    }
    let hash = header.block_hash();
    self.prev = hash;
    self.next += 1;
    ResolvedBlock {
      height,
      hash,
      header,
      txs,
    }
  }
}

/// The year rows add up to their catalog rows (DESIGN.md §5.1): every
/// counter but the holders is the sum of the years', the holders of all
/// years (from the year masks) are the row's, and the active windows add up.
pub(crate) fn check_years(tracker: &Tracker) {
  let supply = tracker.supply(5);
  let rows = tracker.catalog().rows().len();
  assert_eq!(supply.rows.len(), rows);
  assert_eq!(supply.years.rows.len(), rows);
  let all: crate::model::YearMask = (1 << supply.years.starts.len()) - 1;
  for (row, years) in supply.rows.iter().zip(&supply.years.rows) {
    assert_eq!(row.row, years.row);
    let name = &tracker.catalog().rows()[usize::from(row.row)].id;
    let mut sum = RowStats::default();
    let mut active = vec![0u64; row.active_non_extracted.len()];
    let mut last_year = None;
    for (year, cell) in &years.cells {
      assert!(last_year < Some(*year), "{name}: years out of order");
      last_year = Some(*year);
      assert!(
        *year < supply.years.starts.len(),
        "{name}: year {year} past the tip"
      );
      assert_eq!(
        cell.row,
        crate::model::year_row(rows, row.row, *year),
        "{name}"
      );
      assert_eq!(cell.stats, tracker.year_stats(row.row, *year), "{name}");
      let s = cell.stats;
      sum.mined += s.mined;
      sum.located += s.located;
      sum.burned += s.burned;
      sum.lost += s.lost;
      sum.overwritten += s.overwritten;
      sum.extracted += s.extracted;
      assert!(s.holders <= row.stats.holders, "{name} {year}");
      assert!(
        s.extracted_holders <= row.stats.extracted_holders,
        "{name} {year}"
      );
      assert_eq!(years.holders(1 << year, false), s.holders, "{name} {year}");
      assert_eq!(
        years.holders(1 << year, true),
        s.extracted_holders,
        "{name} {year}"
      );
      for (total, (_, n)) in active.iter_mut().zip(&cell.active_non_extracted) {
        *total += n;
      }
    }
    sum.holders = years.holders(all, false);
    sum.extracted_holders = years.holders(all, true);
    assert_eq!(sum, row.stats, "{name}: the years do not add up");
    assert_eq!(
      active,
      row
        .active_non_extracted
        .iter()
        .map(|(_, n)| *n)
        .collect::<Vec<_>>(),
      "{name}: active by year"
    );
  }
}

/// The month rows add up to their catalog rows and to their year rows
/// (DESIGN.md §5.3): every counter but the holders, and the active windows;
/// each month's holders are at most its year's; every month cell's row id
/// is its month row, and the months start where their years do.
pub(crate) fn check_months(tracker: &Tracker) {
  use crate::model::{RowSupply, month_row};
  let supply = tracker.supply(5);
  let rows = tracker.catalog().rows().len();
  assert_eq!(supply.months.rows.len(), rows);
  assert_eq!(supply.months.starts, tracker.month_starts());
  for (year, &start) in supply.years.starts.iter().enumerate() {
    if let Some(&month_start) = supply.months.starts.get(year * 12) {
      assert_eq!(month_start, start, "year {year} starts with its January");
    }
  }
  assert!(supply.months.starts.len() <= supply.years.starts.len() * 12);
  let additive = |sum: &mut RowStats, s: &RowStats| {
    sum.mined += s.mined;
    sum.located += s.located;
    sum.burned += s.burned;
    sum.lost += s.lost;
    sum.overwritten += s.overwritten;
    sum.extracted += s.extracted;
  };
  let active = |cells: &mut Vec<u64>, cell: &RowSupply| {
    cells.resize(cell.active_non_extracted.len(), 0);
    for (total, (_, n)) in cells.iter_mut().zip(&cell.active_non_extracted) {
      *total += n;
    }
  };
  for ((row, months), years) in supply
    .rows
    .iter()
    .zip(&supply.months.rows)
    .zip(&supply.years.rows)
  {
    assert_eq!(row.row, months.row);
    let name = &tracker.catalog().rows()[usize::from(row.row)].id;
    let mut sum = RowStats::default();
    let mut sum_active = Vec::new();
    // Per year index: (stats, active).
    let mut by_year: BTreeMap<usize, (RowStats, Vec<u64>)> = BTreeMap::new();
    let mut last = None;
    for (month, cell) in &months.cells {
      assert!(last < Some(*month), "{name}: months out of order");
      last = Some(*month);
      assert!(
        *month < supply.months.starts.len(),
        "{name}: month past the tip"
      );
      assert_eq!(cell.row, month_row(rows, row.row, *month), "{name}");
      assert_eq!(cell.stats, tracker.month_stats(row.row, *month), "{name}");
      assert!(cell.stats.holders <= row.stats.holders, "{name} {month}");
      assert!(cell.top_non_extracted_holders.len() <= 5);
      additive(&mut sum, &cell.stats);
      active(&mut sum_active, cell);
      let year = by_year.entry(month / 12).or_default();
      additive(&mut year.0, &cell.stats);
      active(&mut year.1, cell);
      let year_cell = years
        .cells
        .iter()
        .find(|(y, _)| *y == month / 12)
        .map(|(_, cell)| cell.stats)
        .unwrap_or_default();
      assert!(cell.stats.holders <= year_cell.holders, "{name} {month}");
      assert!(
        cell.stats.extracted_holders <= year_cell.extracted_holders,
        "{name} {month}"
      );
    }
    sum.holders = row.stats.holders;
    sum.extracted_holders = row.stats.extracted_holders;
    assert_eq!(sum, row.stats, "{name}: the months do not add up");
    if !months.cells.is_empty() {
      assert_eq!(
        sum_active,
        row
          .active_non_extracted
          .iter()
          .map(|(_, n)| *n)
          .collect::<Vec<_>>(),
        "{name}: active by month"
      );
    }
    for (year, cell) in &years.cells {
      let (mut stats, active) = by_year.remove(year).unwrap_or_default();
      stats.holders = cell.stats.holders;
      stats.extracted_holders = cell.stats.extracted_holders;
      assert_eq!(stats, cell.stats, "{name} {year}: the months do not add up");
      if !active.is_empty() {
        assert_eq!(
          active,
          cell
            .active_non_extracted
            .iter()
            .map(|(_, n)| *n)
            .collect::<Vec<_>>(),
          "{name} {year}: active by month"
        );
      }
    }
    assert!(
      by_year.is_empty(),
      "{name}: months of a year without a cell"
    );
  }
}

/// The luck rows add up to their catalog rows (DESIGN.md §5.2), as the
/// year rows do (`check_years`); every luck cell's row id is its luck row.
pub(crate) fn check_lucks(tracker: &Tracker) {
  use crate::model::{LUCK_SLOTS, LuckMask, luck_row};
  let supply = tracker.supply(5);
  let rows = tracker.catalog().rows().len();
  assert_eq!(supply.lucks.rows.len(), rows);
  assert_eq!(supply.lucks.heights, tracker.lucks());
  let all: LuckMask = LuckMask::MAX;
  for (row, lucks) in supply.rows.iter().zip(&supply.lucks.rows) {
    assert_eq!(row.row, lucks.row);
    let name = &tracker.catalog().rows()[usize::from(row.row)].id;
    let mut sum = RowStats::default();
    let mut active = vec![0u64; row.active_non_extracted.len()];
    let mut last = None;
    for (luck, cell) in &lucks.cells {
      assert!(last < Some(*luck), "{name}: lucks out of order");
      last = Some(*luck);
      assert!(*luck < LUCK_SLOTS);
      assert_eq!(cell.row, luck_row(rows, row.row, *luck), "{name}");
      assert_eq!(cell.stats, tracker.luck_stats(row.row, *luck), "{name}");
      assert!(cell.top_extracted_holders.is_empty() && cell.top_non_extracted_holders.is_empty());
      let s = cell.stats;
      sum.mined += s.mined;
      sum.located += s.located;
      sum.burned += s.burned;
      sum.lost += s.lost;
      sum.overwritten += s.overwritten;
      sum.extracted += s.extracted;
      assert_eq!(lucks.holders(1 << luck, false), s.holders, "{name} {luck}");
      assert_eq!(
        lucks.holders(1 << luck, true),
        s.extracted_holders,
        "{name} {luck}"
      );
      for (total, (_, n)) in active.iter_mut().zip(&cell.active_non_extracted) {
        *total += n;
      }
    }
    sum.holders = lucks.holders(all, false);
    sum.extracted_holders = lucks.holders(all, true);
    assert_eq!(sum, row.stats, "{name}: the lucks do not add up");
    assert_eq!(
      active,
      row
        .active_non_extracted
        .iter()
        .map(|(_, n)| *n)
        .collect::<Vec<_>>(),
      "{name}: active by luck"
    );
  }
}
