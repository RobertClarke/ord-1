//! Differential test: random chains through the tracker and through a naive
//! model of ord's full sat-range flow (every UTXO holds its complete list of
//! sat ranges; the coinbase takes the subsidy range, then the fee ranges;
//! unclaimed ranges are lost at ord's running offset). After every block,
//! every tracked piece's location, the per-row counters, the deltas and the
//! lost stream must agree.

use {
  super::*,
  crate::model::{ACTIVE_YEARS, BlockDelta, LocationKind},
  std::collections::{BTreeMap, HashMap, HashSet, VecDeque},
};

/// xorshift64*: deterministic, dependency-free.
struct Rng(u64);

impl Rng {
  fn next(&mut self) -> u64 {
    self.0 ^= self.0 >> 12;
    self.0 ^= self.0 << 25;
    self.0 ^= self.0 >> 27;
    self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
  }

  fn below(&mut self, n: u64) -> u64 {
    if n == 0 { 0 } else { self.next() % n }
  }

  fn chance(&mut self, percent: u64) -> bool {
    self.below(100) < percent
  }
}

#[derive(Clone)]
struct NaiveUtxo {
  value: u64,
  script: ScriptBuf,
  kind: UtxoKind,
  height: u32,
  /// Complete sat ranges in offset order.
  ranges: Vec<(u64, u64)>,
  /// The tracked pieces the tracker must hold.
  pieces: Vec<Piece>,
  rows: RowCounts,
}

impl NaiveUtxo {
  fn tracked(&self) -> TrackedUtxo {
    TrackedUtxo {
      value: self.value,
      script_pubkey: self.script.clone(),
      height: self.height,
      kind: self.kind,
      pieces: self.pieces.clone(),
    }
  }
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
struct Counters {
  mined: u64,
  located: u64,
  burned: u64,
  lost: u64,
  overwritten: u64,
}

struct Naive {
  catalog: Arc<SatCatalog>,
  /// Row counts of single-sat pieces, memoized.
  sat_rows: std::cell::RefCell<HashMap<u64, RowCounts>>,
  config: TrackerConfig,
  /// Tracked runs of every coinbase so far: start → end.
  runs: BTreeMap<u64, u64>,
  utxos: HashMap<OutPoint, NaiveUtxo>,
  tracked_utxos: usize,
  lost: Vec<Piece>,
  lost_sats: u64,
  overwritten: Vec<(OutPoint, u32, NaiveUtxo)>,
  counters: Vec<Counters>,
  times: Vec<u32>,
  coinbases: Vec<Txid>,
}

/// Per script and row: (located, extracted, extracted UTXOs, non-extracted
/// UTXOs, last extracted height, last non-extracted height).
type Tally = (u64, u64, u64, u64, u32, u32);

/// What the naive model saw one block do.
#[derive(Default)]
struct NaiveBlock {
  spent: Vec<(OutPoint, TrackedUtxo)>,
  /// Every output created, with its expected pieces at creation.
  outputs: Vec<(OutPoint, Vec<Piece>)>,
  created: HashSet<OutPoint>,
  lost: Vec<Piece>,
  overwritten: Vec<(OutPoint, TrackedUtxo)>,
}

fn take(ranges: &mut VecDeque<(u64, u64)>, mut n: u64) -> Vec<(u64, u64)> {
  let mut out = Vec::new();
  while n > 0 {
    let (start, end) = ranges.pop_front().expect("naive: not enough sats");
    let len = end - start;
    if len <= n {
      out.push((start, end));
      n -= len;
    } else {
      out.push((start, start + n));
      ranges.push_front((start + n, end));
      n = 0;
    }
  }
  out
}

impl Naive {
  fn new(catalog: Arc<SatCatalog>, config: TrackerConfig, times: Vec<u32>) -> Self {
    let rows = catalog.rows().len();
    Self {
      catalog,
      sat_rows: Default::default(),
      config,
      runs: BTreeMap::new(),
      utxos: HashMap::new(),
      tracked_utxos: 0,
      lost: Vec::new(),
      lost_sats: 0,
      overwritten: Vec::new(),
      counters: vec![Counters::default(); rows],
      times,
      coinbases: Vec::new(),
    }
  }

  /// Tracked pieces of consecutive sat ranges starting at `offset`.
  fn pieces(&self, ranges: &[(u64, u64)], mut offset: u64) -> Vec<Piece> {
    let mut pieces = Vec::new();
    for &(s, e) in ranges {
      let mut runs = Vec::new();
      if let Some((&rs, &re)) = self.runs.range(..=s).next_back()
        && re > s
      {
        runs.push((rs, re));
      }
      runs.extend(self.runs.range(s + 1..e).map(|(&a, &b)| (a, b)));
      for (rs, re) in runs {
        let (a, b) = (rs.max(s), re.min(e));
        if a < b {
          push_merged(
            &mut pieces,
            Piece {
              offset: offset + (a - s),
              start: a,
              len: b - a,
            },
          );
        }
      }
      offset += e - s;
    }
    pieces
  }

  fn rows(&self, pieces: &[Piece]) -> RowCounts {
    let mut counts = vec![0u64; self.catalog.rows().len()];
    let mut memo = self.sat_rows.borrow_mut();
    for piece in pieces {
      let rows = if piece.len == 1 {
        memo
          .entry(piece.start)
          .or_insert_with(|| self.catalog.row_counts(piece.start, 1))
          .clone()
      } else {
        self.catalog.row_counts(piece.start, piece.len)
      };
      for (row, n) in rows {
        counts[usize::from(row)] += n;
      }
    }
    counts
      .into_iter()
      .enumerate()
      .filter(|&(_, n)| n > 0)
      .map(|(row, n)| (row as RowId, n))
      .collect()
  }

  fn add(&mut self, rows: &RowCounts, f: impl Fn(&mut Counters) -> &mut u64, sign: i64) {
    for &(row, n) in rows {
      let counter = f(&mut self.counters[usize::from(row)]);
      *counter = if sign > 0 { *counter + n } else { *counter - n };
    }
  }

  fn create(
    &mut self,
    outpoint: OutPoint,
    output: &TxOut,
    height: u32,
    ranges: Vec<(u64, u64)>,
    seen: &mut NaiveBlock,
  ) {
    let pieces = self.pieces(&ranges, 0);
    let rows = self.rows(&pieces);
    let kind = if is_unspendable(&output.script_pubkey) {
      UtxoKind::Unspendable
    } else {
      UtxoKind::Spendable
    };
    match kind {
      UtxoKind::Spendable => self.add(&rows, |c| &mut c.located, 1),
      UtxoKind::Unspendable => self.add(&rows, |c| &mut c.burned, 1),
    }
    if !pieces.is_empty() {
      self.tracked_utxos += 1;
      seen.created.insert(outpoint);
    }
    seen.outputs.push((outpoint, pieces.clone()));
    let utxo = NaiveUtxo {
      value: output.value.to_sat(),
      script: output.script_pubkey.clone(),
      kind,
      height,
      ranges,
      pieces,
      rows,
    };
    assert!(self.utxos.insert(outpoint, utxo).is_none());
  }

  fn remove(&mut self, outpoint: &OutPoint) -> NaiveUtxo {
    let utxo = self
      .utxos
      .remove(outpoint)
      .expect("naive: unknown outpoint");
    match utxo.kind {
      UtxoKind::Spendable => self.add(&utxo.rows, |c| &mut c.located, -1),
      UtxoKind::Unspendable => self.add(&utxo.rows, |c| &mut c.burned, -1),
    }
    if !utxo.pieces.is_empty() {
      self.tracked_utxos -= 1;
    }
    utxo
  }

  fn apply(&mut self, block: &ResolvedBlock) -> NaiveBlock {
    let mut seen = NaiveBlock::default();
    let height = block.height;
    self.times.push(block.header.time);
    let first = first_sat(height.into());
    let sub = subsidy(height.into());
    for (start, len) in self.catalog.tracked_runs(first, first + sub) {
      self.runs.insert(start, start + len);
      let rows = self.rows(&[Piece {
        offset: 0,
        start,
        len,
      }]);
      self.add(&rows, |c| &mut c.mined, 1);
    }
    let mut coinbase: VecDeque<(u64, u64)> = VecDeque::new();
    if sub > 0 {
      coinbase.push_back((first, first + sub));
    }
    for tx in &block.txs[1..] {
      let mut ranges = VecDeque::new();
      for input in &tx.inputs {
        let existed_before = !seen.created.contains(&input.previous_output)
          && !seen
            .outputs
            .iter()
            .any(|(o, _)| *o == input.previous_output);
        let utxo = self.remove(&input.previous_output);
        assert_eq!(utxo.value, input.value);
        if existed_before && !utxo.pieces.is_empty() {
          seen.spent.push((input.previous_output, utxo.tracked()));
        }
        seen.created.remove(&input.previous_output);
        ranges.extend(utxo.ranges);
      }
      for (vout, output) in tx.outputs.iter().enumerate() {
        let taken = take(&mut ranges, output.value.to_sat());
        self.create(op(tx.txid, vout as u32), output, height, taken, &mut seen);
      }
      coinbase.extend(ranges);
    }
    let cb = &block.txs[0];
    for (vout, output) in cb.outputs.iter().enumerate() {
      let outpoint = op(cb.txid, vout as u32);
      if self.utxos.contains_key(&outpoint) {
        let old = self.remove(&outpoint);
        if !old.pieces.is_empty() {
          self.add(&old.rows, |c| &mut c.overwritten, 1);
          seen.overwritten.push((outpoint, old.tracked()));
          self.overwritten.push((outpoint, height, old));
        }
      }
      let taken = take(&mut coinbase, output.value.to_sat());
      self.create(outpoint, output, height, taken, &mut seen);
    }
    let rest: Vec<(u64, u64)> = coinbase.into_iter().collect();
    let lost = self.pieces(&rest, self.lost_sats);
    let rows = self.rows(&lost);
    self.add(&rows, |c| &mut c.lost, 1);
    self.lost.extend(lost.iter().copied());
    seen.lost = lost;
    self.lost_sats += rest.iter().map(|(s, e)| e - s).sum::<u64>();
    self.coinbases.push(cb.txid);
    seen
  }

  fn max_times(&self) -> Vec<u32> {
    let mut max = 0;
    self
      .times
      .iter()
      .map(|&t| {
        max = max.max(t);
        max
      })
      .collect()
  }

  /// DESIGN.md §5, restated: every tracked sat outside the (single) config
  /// range allows `extracted_max_value` sats of value, every piece's run
  /// inside it allows its length plus `extracted_max_value`.
  fn extracted(&self, utxo: &NaiveUtxo) -> bool {
    assert!(self.config.ranges.len() <= 1);
    let max = u128::from(self.config.extracted_max_value);
    let mut budget = 0u128;
    for piece in &utxo.pieces {
      let inside = self.config.ranges.first().map_or(0, |range| {
        piece
          .end()
          .min(range.end)
          .saturating_sub(piece.start.max(range.start))
      });
      budget += u128::from(inside) + max * u128::from(inside > 0);
      budget += max * u128::from(piece.len - inside);
    }
    utxo.kind == UtxoKind::Spendable
      && self.times[utxo.height as usize] >= self.config.extracted_since_time
      && u128::from(utxo.value) <= budget
  }

  /// Holder statistics and the active windows, recomputed from scratch.
  fn check_supply(&self, tracker: &Tracker) {
    let rows = self.catalog.rows().len();
    let supply = tracker.supply(5);
    let tip = *self.times.last().unwrap();
    let max_times = self.max_times();
    // Per row: script → (located, extracted, utxos_ext, utxos_non, last_ext, last_non)
    let mut per_row: Vec<HashMap<&ScriptBuf, Tally>> = vec![HashMap::new(); rows];
    let mut extracted = vec![0u64; rows];
    let mut active = vec![vec![0u64; ACTIVE_YEARS.len()]; rows];
    for utxo in self.utxos.values() {
      if utxo.kind != UtxoKind::Spendable {
        continue;
      }
      let ext = self.extracted(utxo);
      for &(row, n) in &utxo.rows {
        let tally = per_row[usize::from(row)].entry(&utxo.script).or_default();
        tally.0 += n;
        if ext {
          tally.1 += n;
          tally.2 += 1;
          tally.4 = tally.4.max(utxo.height);
          extracted[usize::from(row)] += n;
        } else {
          tally.3 += 1;
          tally.5 = tally.5.max(utxo.height);
          for (w, &years) in ACTIVE_YEARS.iter().enumerate() {
            let since = tip.saturating_sub(years * 365 * 86_400);
            if max_times[utxo.height as usize] >= since {
              active[usize::from(row)][w] += n;
            }
          }
        }
      }
    }
    for row in 0..rows {
      let s = &supply.rows[row];
      let tallies = &per_row[row];
      assert_eq!(s.stats.extracted, extracted[row], "row {row} extracted");
      assert_eq!(
        s.stats.holders,
        tallies.values().filter(|t| t.0 > 0).count() as u64,
        "row {row} holders"
      );
      assert_eq!(
        s.stats.extracted_holders,
        tallies.values().filter(|t| t.1 > 0).count() as u64,
        "row {row} extracted holders"
      );
      assert_eq!(
        s.active_non_extracted,
        ACTIVE_YEARS
          .iter()
          .copied()
          .zip(active[row].iter().copied())
          .collect::<Vec<_>>(),
        "row {row} active"
      );
      let top = |extracted: bool| -> Vec<Holder> {
        let mut list: Vec<Holder> = tallies
          .iter()
          .filter_map(|(script, t)| {
            let (count, utxos, last) = if extracted {
              (t.1, t.2, t.4)
            } else {
              (t.0 - t.1, t.3, t.5)
            };
            (count > 0).then(|| Holder {
              script_pubkey: (*script).clone(),
              count,
              utxos,
              last_received_height: last,
            })
          })
          .collect();
        list.sort_by(|a, b| {
          b.count
            .cmp(&a.count)
            .then_with(|| a.script_pubkey.cmp(&b.script_pubkey))
        });
        list.truncate(5);
        list
      };
      assert_eq!(
        s.top_extracted_holders,
        top(true),
        "row {row} top extracted"
      );
      assert_eq!(
        s.top_non_extracted_holders,
        top(false),
        "row {row} top non-extracted"
      );
    }
  }
}

use crate::model::Holder;

fn merged(pieces: &[Piece]) -> Vec<Piece> {
  let mut out = Vec::new();
  for &piece in pieces {
    push_merged(&mut out, piece);
  }
  out
}

fn check_location(
  tracker: &Tracker,
  sat: u64,
  kind: LocationKind,
  outpoint: OutPoint,
  offset: u64,
) {
  let location = tracker
    .locate(sat)
    .unwrap_or_else(|| panic!("sat {sat} not located, expected {kind:?} {outpoint}:{offset}"));
  assert_eq!(
    (location.kind, location.outpoint, location.offset),
    (kind, outpoint, offset),
    "sat {sat}"
  );
}

/// Locates the first, last and a random sat of every piece; for UTXOs with
/// many pieces (block 9 as 50k individual palindromes when it is not a
/// tracked range), of the first and last and 30 random pieces, as `locate`
/// scans a UTXO's pieces.
fn check_piece_locations(
  tracker: &Tracker,
  rng: &mut Rng,
  pieces: &[Piece],
  kind: LocationKind,
  outpoint: OutPoint,
) {
  let mut check = |piece: &Piece| {
    for d in [0, piece.len - 1, rng.below(piece.len)] {
      check_location(tracker, piece.start + d, kind, outpoint, piece.offset + d);
    }
  };
  if pieces.len() <= 64 {
    pieces.iter().for_each(&mut check);
  } else {
    let mut sample = vec![pieces[0], pieces[pieces.len() - 1]];
    let mut pick = Rng(pieces.len() as u64 ^ pieces[0].start);
    for _ in 0..30 {
      sample.push(pieces[pick.below(pieces.len() as u64) as usize]);
    }
    sample.iter().for_each(check);
  }
}

fn check_block(
  tracker: &Tracker,
  naive: &Naive,
  seen: &NaiveBlock,
  delta: &BlockDelta,
  rng: &mut Rng,
) {
  // Delta: net spent/created, overwritten, lost.
  let mut spent = delta.spent.clone();
  let mut expected_spent = seen.spent.clone();
  spent.sort_by_key(|(o, _)| *o);
  expected_spent.sort_by_key(|(o, _)| *o);
  assert_eq!(spent, expected_spent, "spent");
  let created: HashSet<OutPoint> = delta.created.iter().map(|(o, _)| *o).collect();
  assert_eq!(created, seen.created, "created");
  for (outpoint, utxo) in &delta.created {
    assert_eq!(*utxo, naive.utxos[outpoint].tracked(), "created {outpoint}");
  }
  assert_eq!(delta.overwritten, seen.overwritten, "overwritten");
  assert_eq!(merged(&delta.lost), merged(&seen.lost), "lost");
  assert_eq!(delta.lost_sats_after, naive.lost_sats);
  assert_eq!(tracker.lost_sats(), naive.lost_sats);
  // Moves carry the same output pieces.
  for m in &delta.moves {
    for output in &m.outputs {
      let expected = &seen
        .outputs
        .iter()
        .find(|(o, _)| *o == op(m.txid, output.vout))
        .unwrap()
        .1;
      assert_eq!(
        &output.pieces, expected,
        "move output {}:{}",
        m.txid, output.vout
      );
    }
  }

  // State: every tracked UTXO created in the block, and a sample of others.
  assert_eq!(
    tracker.utxo_count(),
    naive.tracked_utxos,
    "tracked UTXO count"
  );
  for (outpoint, _) in &seen.outputs {
    match naive.utxos.get(outpoint) {
      Some(utxo) if !utxo.pieces.is_empty() => {
        assert_eq!(tracker.utxo(outpoint), Some(&utxo.tracked()), "{outpoint}");
        let kind = match utxo.kind {
          UtxoKind::Spendable => LocationKind::Unspent,
          UtxoKind::Unspendable => LocationKind::Burned,
        };
        check_piece_locations(tracker, rng, &utxo.pieces, kind, *outpoint);
      }
      _ => assert!(
        tracker.utxo(outpoint).is_none(),
        "{outpoint} is not tracked"
      ),
    }
  }
  check_piece_locations(
    tracker,
    rng,
    &seen.lost,
    LocationKind::Lost,
    OutPoint::null(),
  );
  for (outpoint, utxo) in &seen.overwritten {
    check_piece_locations(
      tracker,
      rng,
      &utxo.pieces,
      LocationKind::Overwritten,
      *outpoint,
    );
  }
  if rng.chance(20) {
    for (outpoint, utxo) in naive
      .utxos
      .iter()
      .filter(|(_, u)| !u.pieces.is_empty())
      .take(20)
    {
      assert_eq!(tracker.utxo(outpoint), Some(&utxo.tracked()));
    }
  }

  // Row counters.
  for (row, counters) in naive.counters.iter().enumerate() {
    let stats = tracker.row_stats(row as RowId);
    assert_eq!(
      Counters {
        mined: stats.mined,
        located: stats.located,
        burned: stats.burned,
        lost: stats.lost,
        overwritten: stats.overwritten,
      },
      *counters,
      "row {}",
      naive.catalog.rows()[row].id
    );
  }
}

/// Everything after a full scan: every naive piece located, lost stream and
/// overwritten list identical.
fn check_full(tracker: &Tracker, naive: &Naive, rng: &mut Rng) {
  let snapshot = tracker.snapshot();
  let mut expected: Vec<(OutPoint, TrackedUtxo)> = naive
    .utxos
    .iter()
    .filter(|(_, u)| !u.pieces.is_empty())
    .map(|(o, u)| (*o, u.tracked()))
    .collect();
  expected.sort_by_key(|(o, _)| *o);
  assert_eq!(snapshot.utxos, expected);
  assert_eq!(merged(&snapshot.lost), merged(&naive.lost));
  assert_eq!(
    snapshot.overwritten,
    naive
      .overwritten
      .iter()
      .map(|(o, h, u)| (*o, *h, u.tracked()))
      .collect::<Vec<_>>()
  );
  for (outpoint, utxo) in &expected {
    let kind = match utxo.kind {
      UtxoKind::Spendable => LocationKind::Unspent,
      UtxoKind::Unspendable => LocationKind::Burned,
    };
    check_piece_locations(tracker, rng, &utxo.pieces, kind, *outpoint);
  }
  check_piece_locations(
    tracker,
    rng,
    &naive.lost,
    LocationKind::Lost,
    OutPoint::null(),
  );
  naive.check_supply(tracker);
  super::check_years(tracker);
  tracker.verify().unwrap();
}

struct Generator {
  rng: Rng,
  next_txid: u64,
  time: u32,
}

impl Generator {
  fn txid(&mut self) -> Txid {
    self.next_txid += 1;
    txid(self.next_txid)
  }

  fn script(&mut self) -> ScriptBuf {
    match self.rng.below(100) {
      0..=7 => ScriptBuf::from_bytes(vec![0x6a, self.rng.below(256) as u8]),
      8 => ScriptBuf::from_bytes(vec![0x51; 10_001]),
      _ => script(self.rng.below(12) as u8),
    }
  }

  /// Splits `total` into `n` values, with zeros and dust now and then.
  fn split(&mut self, total: u64, n: usize) -> Vec<u64> {
    let mut values = vec![0u64; n];
    let mut left = total;
    for (i, value) in values.iter_mut().enumerate() {
      if i + 1 == n {
        *value = left;
        break;
      }
      *value = match self.rng.below(10) {
        0 => 0,
        1 => left.min([1, 546, 10_000, 10_001][self.rng.below(4) as usize]),
        // Room for a few tracked sats: extracted only by their budget.
        2 => left.min(10_001 + self.rng.below(40_000)),
        _ => self.rng.below(left + 1),
      };
      left -= *value;
    }
    // Rotate so the remainder is not always last.
    let r = self.rng.below(n as u64) as usize;
    values.rotate_left(r);
    values
  }

  fn outputs(&mut self, total: u64, max: u64) -> Vec<TxOut> {
    let n = 1 + self.rng.below(max) as usize;
    self
      .split(total, n)
      .into_iter()
      .map(|value| TxOut {
        value: Amount::from_sat(value),
        script_pubkey: self.script(),
      })
      .collect()
  }

  fn block(&mut self, naive: &Naive, chain: &mut Chain) -> ResolvedBlock {
    let mut tracked: Vec<(OutPoint, u64)> = Vec::new();
    let mut untracked: Vec<(OutPoint, u64)> = Vec::new();
    let mut keys: Vec<&OutPoint> = naive.utxos.keys().collect();
    keys.sort();
    for outpoint in keys {
      let utxo = &naive.utxos[outpoint];
      if utxo.kind == UtxoKind::Spendable {
        if utxo.pieces.is_empty() {
          untracked.push((*outpoint, utxo.value));
        } else {
          tracked.push((*outpoint, utxo.value));
        }
      }
    }
    let mut txs = Vec::new();
    let mut fees = 0u64;
    for _ in 0..self.rng.below(7) {
      let mut inputs = Vec::new();
      for _ in 0..1 + self.rng.below(3) {
        let list = if !tracked.is_empty() && (untracked.is_empty() || self.rng.chance(60)) {
          &mut tracked
        } else {
          &mut untracked
        };
        if list.is_empty() {
          break;
        }
        let i = self.rng.below(list.len() as u64) as usize;
        inputs.push(list.swap_remove(i));
      }
      if inputs.is_empty() {
        break;
      }
      let total: u64 = inputs.iter().map(|(_, v)| v).sum();
      let fee = match self.rng.below(10) {
        0..=4 => 0,
        5..=7 => self.rng.below(total.min(1000) + 1),
        8 => self.rng.below(total / 2 + 1),
        _ => total,
      };
      let txid = self.txid();
      let outputs = self.outputs(total - fee, 4);
      // Outputs are spendable later in the same block.
      for (vout, output) in outputs.iter().enumerate() {
        if !is_unspendable(&output.script_pubkey) {
          untracked.push((op(txid, vout as u32), output.value.to_sat()));
        }
      }
      fees += fee;
      txs.push(tx(txid, &inputs, outputs));
    }
    // Outputs of this block's transactions are "untracked" candidates above,
    // though they may hold tracked sats: that is what makes intra-block
    // chains of tracked sats.
    let claim = subsidy(chain.next.into()) + fees;
    let claimed = if self.rng.chance(15) {
      self.rng.below(claim + 1)
    } else {
      claim
    };
    let coinbase_txid = if self.rng.chance(2) && !naive.coinbases.is_empty() {
      // BIP30-style duplicate of an earlier coinbase.
      naive.coinbases[self.rng.below(naive.coinbases.len() as u64) as usize]
    } else {
      self.txid()
    };
    let mut outputs = self.outputs(claimed, 3);
    if self.rng.chance(30) {
      outputs.push(op_return(0));
    }
    let mut all = vec![coinbase(coinbase_txid, outputs)];
    all.extend(txs);
    self.time = match self.rng.below(10) {
      0 => self.time.saturating_sub(self.rng.below(7200) as u32),
      _ => self.time + self.rng.below(3 * 86_400) as u32,
    };
    chain.next_time = Some(self.time);
    chain.block(all)
  }
}

/// Runs `blocks` random blocks after `start` (None: from genesis).
fn run(config: &TrackerConfig, start: Option<u32>, blocks: u32, seed: u64) {
  let (mut tracker, mut chain) = tracker_at(config, start);
  let history = tracker.snapshot().times;
  // Start a little before 2022-12-14 so extraction switches on mid-run.
  let time = history.last().copied().unwrap_or(0).max(1_660_000_000);
  let mut naive = Naive::new(tracker.catalog().clone(), config.clone(), history);
  let mut generator = Generator {
    rng: Rng(seed),
    next_txid: seed << 20,
    time,
  };
  let mut check_rng = Rng(seed ^ 0xdead_beef);
  // The last few blocks, deltas and pre-block snapshots, to test undo/redo.
  let mut recent: VecDeque<(ResolvedBlock, BlockDelta, TrackerSnapshot)> = VecDeque::new();
  let mut touched_tracked = 0usize;
  for i in 0..blocks {
    let block = generator.block(&naive, &mut chain);
    let before = tracker.snapshot();
    let delta = tracker
      .apply_block(&block)
      .unwrap_or_else(|err| panic!("block {}: {err:#}", block.height));
    let seen = naive.apply(&block);
    touched_tracked += delta.moves.iter().filter(|m| !m.coinbase).count();
    check_block(&tracker, &naive, &seen, &delta, &mut check_rng);

    recent.push_back((block, delta, before));
    if recent.len() > 5 {
      recent.pop_front();
    }
    if i % 37 == 36 {
      // Undo the last few blocks, compare, then redo them.
      let k = 1 + check_rng.below(recent.len() as u64) as usize;
      let after = tracker.snapshot();
      for (_, delta, before) in recent.iter().rev().take(k) {
        tracker.undo_block(delta).unwrap();
        assert_eq!(&tracker.snapshot(), before, "undo to {}", delta.height - 1);
      }
      tracker.verify().unwrap();
      for (block, delta, _) in recent.iter().skip(recent.len() - k) {
        assert_eq!(
          &tracker.apply_block(block).unwrap(),
          delta,
          "redo {}",
          block.height
        );
      }
      assert_eq!(tracker.snapshot(), after);
    }
    if i % 97 == 96 || i + 1 == blocks {
      check_full(&tracker, &naive, &mut check_rng);
      let restored = Tracker::restore(
        tracker.catalog().clone(),
        config.clone(),
        tracker.snapshot(),
      )
      .unwrap();
      assert_eq!(restored.snapshot(), tracker.snapshot());
      assert_eq!(restored.supply(5), tracker.supply(5));
      tracker = restored;
    }
  }
  assert!(
    touched_tracked > blocks as usize / 2,
    "too few tracked moves: {touched_tracked}"
  );
}

fn blocks(debug: u32, release: u32) -> u32 {
  if cfg!(debug_assertions) {
    debug
  } else {
    release
  }
}

#[test]
fn differential_rangeless() {
  let config = rangeless_config();
  run(&config, None, blocks(700, 3000), 1);
  run(&config, Some(209_700), blocks(300, 1000), 2);
}

#[test]
fn differential_block9() {
  let config = TrackerConfig::default();
  run(&config, None, blocks(700, 3000), 3);
  run(&config, Some(24_000), blocks(300, 1000), 4);
}

#[test]
fn differential_custom_range() {
  let config = custom_config();
  run(&config, None, blocks(700, 3000), 5);
  run(&config, Some(840_000 - 150), blocks(300, 1000), 6);
}

/// The start of the 300-block window whose coinbases mint the most tracked
/// sats beyond their two boundary sats, among windows starting in `heights`.
fn densest_window(heights: std::ops::Range<u32>) -> u32 {
  let catalog = catalog_for(&TrackerConfig::default());
  heights
    .step_by(300)
    .max_by_key(|&h| {
      let runs = catalog.tracked_runs(first_sat(h.into()), first_sat(u64::from(h) + 300));
      runs.len()
    })
    .unwrap()
}

#[test]
fn differential_silk_road_and_pizza_blocks() {
  // Start where the coinbases hold Silk Road, pizza and hitman combination
  // sats (alphas, omegas, palindromes inside the ranges).
  let sr = densest_window(150_000..290_000);
  let catalog = catalog_for(&TrackerConfig::default());
  let runs = catalog.tracked_runs(first_sat(sr.into()), first_sat(u64::from(sr) + 300));
  assert!(runs.len() > 700, "{sr}: {}", runs.len());
  run(
    &TrackerConfig::default(),
    Some(sr - 1),
    blocks(300, 1500),
    7,
  );
  let pizza = densest_window(24_000..57_000);
  run(
    &TrackerConfig::default(),
    Some(pizza - 1),
    blocks(300, 1000),
    8,
  );
}
