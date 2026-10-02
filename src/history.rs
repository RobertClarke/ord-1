//! Per-address history of tracked-sat activity (docs/API.md "Address
//! history"): for every transaction that moved tracked sats, one entry per
//! script (address) that sent or received them. Pure: derived from a
//! block's `BlockDelta` and `MarketDelta`; the store keeps the entries
//! forever (whatever `--moves-retention`) and reverts them with their block.
//!
//! Definitions (one transaction, one party = one script):
//! * **received**: the tracked sats of the party's spendable outputs;
//!   **sent**: the tracked sats of the tracked UTXOs it spent. Values are the
//!   values of those outputs and UTXOs (outputs and inputs without tracked
//!   sats are not counted). Unspendable outputs (OP_RETURN, oversized
//!   scripts) are not parties: their sats are `burned`, credited to the
//!   parties whose inputs held them.
//! * **direction**: `in` (received only), `out` (sent only), `self` (both).
//! * **isolated**: received sats now in an extracted output
//!   (`model::output_extracted`) that came from an input that was not
//!   extracted (traced by sat number), or from no tracked input. This is the
//!   dashboard's extraction notion (bitcoin-monitoring
//!   `supply/lib/notable.js`, `isolatedRuns`) without its notability
//!   thresholds: sats moved between extracted outputs were isolated
//!   already, so consolidating extracted outputs isolates nothing.
//! * **kind**, the first that applies:
//!   1. `mined`: the coinbase (block subsidy and fee sats);
//!   2. `sale`: the party is the seller (`Sale::seller`) or the buyer
//!      (`Sale::buyer_script`) of a sale detected in the transaction;
//!   3. `extraction`: it isolated tracked sats (`isolated > 0`);
//!   4. `burn`: tracked sats of its inputs went to unspendable outputs;
//!   5. `consolidation`: it spent at least two tracked UTXOs into exactly
//!      one tracked output of its own, and sent no tracked sat to anyone
//!      else (fees aside);
//!   6. `transfer`: anything else.

use {
  crate::{
    model::{
      BlockDelta, MarketDelta, Move, Piece, RowCounts, RowId, TrackerConfig, UtxoKind,
      output_extracted,
    },
    sats::SatCatalog,
  },
  bitcoin::{Script, ScriptBuf, Txid, hashes::Hash},
  serde::{Deserialize, Serialize},
  std::collections::HashMap,
};

/// What the history needs from the sat catalog (`SatCatalog`; a trait so
/// the server tests can use their small fake catalog).
pub trait HistoryCatalog: Sync {
  fn config(&self) -> &TrackerConfig;
  /// Per-row counts of the tracked run `[start, start + len)`.
  fn row_counts(&self, start: u64, len: u64) -> RowCounts;
  /// 0 for the row with the fewest sats that will ever exist.
  fn row_rank(&self, row: RowId) -> usize;
  /// The rows a tracked sat counts toward.
  fn sat_rows(&self, sat: u64) -> Vec<RowId>;
  /// Sats of `[start, end)` special on their own (not merely in a tracked
  /// range), ascending, at most `cap`.
  fn individual_sats(&self, start: u64, end: u64, cap: usize) -> Vec<u64>;
}

impl HistoryCatalog for SatCatalog {
  fn config(&self) -> &TrackerConfig {
    SatCatalog::config(self)
  }

  fn row_counts(&self, start: u64, len: u64) -> RowCounts {
    SatCatalog::row_counts(self, start, len)
  }

  fn row_rank(&self, row: RowId) -> usize {
    SatCatalog::row_rank(self, row)
  }

  fn sat_rows(&self, sat: u64) -> Vec<RowId> {
    SatCatalog::sat_rows(self, sat)
  }

  fn individual_sats(&self, start: u64, end: u64, cap: usize) -> Vec<u64> {
    SatCatalog::individual_sats(self, start, end, cap)
  }
}

/// The 64-bit key of a script in the history: the first 8 bytes of its
/// SHA-256, big-endian. Fixed (it is stored); a collision between two
/// scripts would mix their histories, at 2^-64 odds per pair.
pub fn history_key(script: &Script) -> u64 {
  let hash = bitcoin::hashes::sha256::Hash::hash(script.as_bytes());
  u64::from_be_bytes(hash.as_byte_array()[..8].try_into().unwrap())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
  In,
  Out,
  #[serde(rename = "self")]
  SelfTransfer,
}

impl Direction {
  pub fn code(self) -> u8 {
    match self {
      Self::In => 1,
      Self::Out => 2,
      Self::SelfTransfer => 3,
    }
  }

  pub fn from_code(code: u8) -> Option<Self> {
    Some(match code {
      1 => Self::In,
      2 => Self::Out,
      3 => Self::SelfTransfer,
      _ => return None,
    })
  }

  pub fn name(self) -> &'static str {
    match self {
      Self::In => "in",
      Self::Out => "out",
      Self::SelfTransfer => "self",
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryKind {
  Transfer,
  Consolidation,
  Burn,
  Extraction,
  Sale,
  Mined,
}

impl HistoryKind {
  pub const ALL: [Self; 6] = [
    Self::Transfer,
    Self::Consolidation,
    Self::Burn,
    Self::Extraction,
    Self::Sale,
    Self::Mined,
  ];

  pub fn code(self) -> u8 {
    match self {
      Self::Transfer => 0,
      Self::Consolidation => 1,
      Self::Burn => 2,
      Self::Extraction => 3,
      Self::Sale => 4,
      Self::Mined => 5,
    }
  }

  pub fn from_code(code: u8) -> Option<Self> {
    Self::ALL.get(usize::from(code)).copied()
  }

  pub fn name(self) -> &'static str {
    match self {
      Self::Transfer => "transfer",
      Self::Consolidation => "consolidation",
      Self::Burn => "burn",
      Self::Extraction => "extraction",
      Self::Sale => "sale",
      Self::Mined => "mined",
    }
  }

  pub fn from_name(name: &str) -> Option<Self> {
    Self::ALL.into_iter().find(|kind| kind.name() == name)
  }
}

/// What a party received or sent in one transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Side {
  /// Tracked sats.
  pub sats: u64,
  /// Σ values of the outputs (received) or spent UTXOs (sent).
  pub value: u64,
  /// Number of those outputs or spent UTXOs.
  pub count: u32,
  /// Per-row counts of the tracked sats, sorted by row, no zeros.
  pub rows: RowCounts,
}

impl Side {
  pub fn is_empty(&self) -> bool {
    self.count == 0
  }
}

/// One script's part in one transaction that moved tracked sats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
  /// `history_key` of the script.
  pub key: u64,
  pub height: u32,
  pub tx_index: u32,
  pub txid: Txid,
  pub kind: HistoryKind,
  pub direction: Direction,
  pub received: Side,
  pub sent: Side,
  /// Some output it received is extracted.
  pub extracted_in: bool,
  /// Some UTXO it spent was extracted.
  pub extracted_out: bool,
  /// Tracked sats received into its extracted outputs.
  pub extracted_sats: u64,
  /// Of those, the sats it isolated (see the module docs).
  pub isolated: u64,
  /// Tracked sats of its inputs that went to unspendable outputs.
  pub burned: u64,
  /// It sold (`Sale::seller`) in this transaction.
  pub seller: bool,
  /// It bought (`Sale::buyer_script`) in this transaction.
  pub buyer: bool,
  /// Other scripts it exchanged tracked sats with: the senders of what it
  /// received and the receivers (spendable outputs) of what it sent.
  pub counterparties: u32,
  /// The main one: for `in` the script that sent it the most tracked sats,
  /// for `out` the one it sent the most to (ties: the lowest vin or vout),
  /// for `self` the main receiver, else the main sender. `None` when there
  /// is none (e.g. the coinbase, or sats it sent only to itself).
  pub counterparty: Option<ScriptBuf>,
  /// Its rarest tracked sat in the transaction (received or sent): one of
  /// the row with the fewest sats that will ever exist.
  pub lead_sat: Option<u64>,
}

/// A party's accumulators.
#[derive(Default)]
struct Party {
  received: Side,
  sent: Side,
  received_counts: Vec<u64>,
  sent_counts: Vec<u64>,
  extracted_in: bool,
  extracted_out: bool,
  extracted_sats: u64,
  isolated: u64,
  burned: u64,
  /// Tracked sats of its inputs that went to another script's output.
  sent_to_others: u64,
  /// Other scripts that sent it tracked sats: (key, sats, first vin).
  senders: Vec<(u64, u64, u32)>,
  /// Other scripts it sent tracked sats to: (key, sats, first vout).
  receivers: Vec<(u64, u64, u32)>,
  /// (rank, sat) of the best lead candidate so far.
  lead: Option<(usize, u64)>,
}

/// Adds `sats` from or to `key` at `position` (a vin or vout).
fn credit(list: &mut Vec<(u64, u64, u32)>, key: u64, sats: u64, position: u32) {
  match list.iter_mut().find(|(other, _, _)| *other == key) {
    Some(entry) => {
      entry.1 += sats;
      entry.2 = entry.2.min(position);
    }
    None => list.push((key, sats, position)),
  }
}

/// The counterparty that exchanged the most sats (ties: the lowest vin or
/// vout).
fn main_counterparty(list: &[(u64, u64, u32)]) -> Option<u64> {
  list
    .iter()
    .max_by(|a, b| a.1.cmp(&b.1).then(b.2.cmp(&a.2)))
    .map(|(key, _, _)| *key)
}

fn add_counts(counts: &mut Vec<u64>, row_counts: &RowCounts) {
  for &(row, n) in row_counts {
    let row = usize::from(row);
    if counts.len() <= row {
      counts.resize(row + 1, 0);
    }
    counts[row] += n;
  }
}

fn sparse(counts: &[u64]) -> RowCounts {
  counts
    .iter()
    .enumerate()
    .filter(|(_, n)| **n > 0)
    .map(|(row, n)| (row as RowId, *n))
    .collect()
}

/// The rarest sat of `piece` (whose row counts are `counts`), with its
/// rank, if it beats `best`.
fn lead_of(
  catalog: &dyn HistoryCatalog,
  piece: &Piece,
  counts: &RowCounts,
  best: Option<(usize, u64)>,
) -> Option<(usize, u64)> {
  let rank = counts.iter().map(|&(row, _)| catalog.row_rank(row)).min()?;
  if best.is_some_and(|(best, _)| best <= rank) {
    return None;
  }
  let sat_rank = |sat: u64| {
    catalog
      .sat_rows(sat)
      .into_iter()
      .map(|row| catalog.row_rank(row))
      .min()
  };
  let found = if piece.len <= 64 {
    (piece.start..piece.end()).find(|&sat| sat_rank(sat) == Some(rank))
  } else {
    std::iter::once(piece.start)
      .chain(catalog.individual_sats(piece.start, piece.end(), 256))
      .find(|&sat| sat_rank(sat) == Some(rank))
  };
  Some((rank, found.unwrap_or(piece.start)))
}

/// A tracked input run: sats `[start, end)` of input `vin`, held by the
/// party at `party`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Run {
  start: u64,
  end: u64,
  party: usize,
  vin: u32,
  extracted: bool,
}

/// Calls `f(len, run)` for each part of `[start, end)` with the input run it
/// came from (`None`: from no tracked input). `runs` are sorted and
/// disjoint (sats are unique).
fn trace(runs: &[Run], start: u64, end: u64, mut f: impl FnMut(u64, Option<&Run>)) {
  let mut cursor = start;
  let first = runs.partition_point(|run| run.end <= start);
  for run in &runs[first..] {
    if run.start >= end {
      break;
    }
    if run.start > cursor {
      f(run.start - cursor, None);
    }
    let a = run.start.max(cursor);
    let b = run.end.min(end);
    f(b - a, Some(run));
    cursor = b;
  }
  if cursor < end {
    f(end - cursor, None);
  }
}

fn party_index<'a>(
  index: &mut HashMap<u64, usize>,
  parties: &mut Vec<(u64, &'a Script, Party)>,
  script: &'a Script,
) -> usize {
  let key = history_key(script);
  *index.entry(key).or_insert_with(|| {
    parties.push((key, script, Party::default()));
    parties.len() - 1
  })
}

/// The history entries of one move, one per party, in no particular
/// order. `time_of` gives header times (for the spent UTXOs' extraction
/// verdict).
pub fn move_entries(
  m: &Move,
  height: u32,
  block_time: u32,
  market: &MarketDelta,
  catalog: &dyn HistoryCatalog,
  time_of: &dyn Fn(u32) -> Option<u32>,
) -> Vec<HistoryEntry> {
  let config = catalog.config();
  let mut index = HashMap::<u64, usize>::new();
  let mut parties = Vec::<(u64, &Script, Party)>::new();

  let mut runs = Vec::new();
  for input in &m.inputs {
    let i = party_index(&mut index, &mut parties, &input.utxo.script_pubkey);
    let extracted = time_of(input.utxo.height).is_some_and(|time| {
      output_extracted(
        input.utxo.kind,
        input.utxo.value,
        &input.utxo.pieces,
        time,
        config,
      )
    });
    let p = &mut parties[i].2;
    p.sent.count += 1;
    p.sent.value += input.utxo.value;
    p.extracted_out |= extracted;
    for piece in &input.utxo.pieces {
      p.sent.sats += piece.len;
      let counts = catalog.row_counts(piece.start, piece.len);
      add_counts(&mut p.sent_counts, &counts);
      if let Some(lead) = lead_of(catalog, piece, &counts, p.lead) {
        p.lead = Some(lead);
      }
      runs.push(Run {
        start: piece.start,
        end: piece.end(),
        party: i,
        vin: input.vin,
        extracted,
      });
    }
  }
  runs.sort_unstable();

  for output in &m.outputs {
    if output.kind != UtxoKind::Spendable {
      for piece in &output.pieces {
        trace(&runs, piece.start, piece.end(), |len, run| {
          if let Some(run) = run {
            parties[run.party].2.burned += len;
          }
        });
      }
      continue;
    }
    let i = party_index(&mut index, &mut parties, &output.script_pubkey);
    let key = parties[i].0;
    let extracted = output_extracted(
      output.kind,
      output.value,
      &output.pieces,
      block_time,
      config,
    );
    {
      let p = &mut parties[i].2;
      p.received.count += 1;
      p.received.value += output.value;
      p.extracted_in |= extracted;
    }
    for piece in &output.pieces {
      let counts = catalog.row_counts(piece.start, piece.len);
      {
        let p = &mut parties[i].2;
        p.received.sats += piece.len;
        if extracted {
          p.extracted_sats += piece.len;
        }
        add_counts(&mut p.received_counts, &counts);
        if let Some(lead) = lead_of(catalog, piece, &counts, p.lead) {
          p.lead = Some(lead);
        }
      }
      let mut isolated = 0;
      let mut from = Vec::new();
      trace(&runs, piece.start, piece.end(), |len, run| {
        if extracted && run.is_none_or(|run| !run.extracted) {
          isolated += len;
        }
        if let Some(run) = run
          && run.party != i
        {
          from.push((run.party, run.vin, len));
        }
      });
      parties[i].2.isolated += isolated;
      for (source, vin, len) in from {
        let source_key = parties[source].0;
        let s = &mut parties[source].2;
        s.sent_to_others += len;
        credit(&mut s.receivers, key, len, output.vout);
        credit(&mut parties[i].2.senders, source_key, len, vin);
      }
    }
  }

  let scripts = parties
    .iter()
    .map(|(key, script, _)| (*key, *script))
    .collect::<HashMap<u64, &Script>>();

  parties
    .into_iter()
    .map(|(key, _, mut p)| {
      p.received.rows = sparse(&p.received_counts);
      p.sent.rows = sparse(&p.sent_counts);
      let direction = match (p.received.is_empty(), p.sent.is_empty()) {
        (false, true) => Direction::In,
        (true, false) => Direction::Out,
        _ => Direction::SelfTransfer,
      };
      let seller = market
        .sales
        .iter()
        .any(|sale| sale.txid == m.txid && history_key(&sale.seller.script_pubkey) == key);
      let buyer = market.sales.iter().any(|sale| {
        sale.txid == m.txid
          && sale
            .buyer_script
            .as_ref()
            .is_some_and(|script| history_key(script) == key)
      });
      let consolidation = direction == Direction::SelfTransfer
        && p.sent.count >= 2
        && p.received.count == 1
        && p.sent_to_others == 0;
      let kind = if m.coinbase {
        HistoryKind::Mined
      } else if seller || buyer {
        HistoryKind::Sale
      } else if p.isolated > 0 {
        HistoryKind::Extraction
      } else if p.burned > 0 {
        HistoryKind::Burn
      } else if consolidation {
        HistoryKind::Consolidation
      } else {
        HistoryKind::Transfer
      };
      let counterparty = match direction {
        Direction::In => main_counterparty(&p.senders),
        Direction::Out => main_counterparty(&p.receivers),
        Direction::SelfTransfer => {
          main_counterparty(&p.receivers).or_else(|| main_counterparty(&p.senders))
        }
      };
      let mut others = p
        .senders
        .iter()
        .map(|(key, _, _)| *key)
        .collect::<Vec<u64>>();
      for (receiver, _, _) in &p.receivers {
        if !others.contains(receiver) {
          others.push(*receiver);
        }
      }
      HistoryEntry {
        key,
        height,
        tx_index: m.tx_index,
        txid: m.txid,
        kind,
        direction,
        received: p.received,
        sent: p.sent,
        extracted_in: p.extracted_in,
        extracted_out: p.extracted_out,
        extracted_sats: p.extracted_sats,
        isolated: if m.coinbase { 0 } else { p.isolated },
        burned: p.burned,
        seller,
        buyer,
        counterparties: others.len() as u32,
        counterparty: counterparty.map(|key| scripts[&key].to_owned()),
        lead_sat: p.lead.map(|(_, sat)| sat),
      }
    })
    .collect()
}

/// Every history entry of a block, sorted by (key, tx_index).
pub fn block_entries(
  delta: &BlockDelta,
  market: &MarketDelta,
  catalog: &dyn HistoryCatalog,
  time_of: &(dyn Fn(u32) -> Option<u32> + Sync),
) -> Vec<HistoryEntry> {
  use rayon::prelude::*;
  let time = delta.header.time;
  let mut entries = if delta.moves.len() >= 64 {
    delta
      .moves
      .par_iter()
      .flat_map_iter(|m| move_entries(m, delta.height, time, market, catalog, time_of))
      .collect::<Vec<_>>()
  } else {
    delta
      .moves
      .iter()
      .flat_map(|m| move_entries(m, delta.height, time, market, catalog, time_of))
      .collect()
  };
  entries.sort_unstable_by_key(|entry| (entry.key, entry.tx_index));
  entries
}

/// The (key, tx_index) of every entry `block_entries` makes for a block:
/// one per move and script of a tracked input or spendable output, whatever
/// the catalog. Used to revert a block from its undo record.
pub fn block_keys(delta: &BlockDelta) -> Vec<(u64, u32)> {
  let mut keys = Vec::new();
  for m in &delta.moves {
    for input in &m.inputs {
      keys.push((history_key(&input.utxo.script_pubkey), m.tx_index));
    }
    for output in &m.outputs {
      if output.kind == UtxoKind::Spendable {
        keys.push((history_key(&output.script_pubkey), m.tx_index));
      }
    }
  }
  keys.sort_unstable();
  keys.dedup();
  keys
}

#[cfg(test)]
mod tests;
