//! The overlay's state machine: one `poll` brings it in line with
//! bitcoind's mempool and the index's tip. bitcoind and the index are
//! reached through the `MempoolNode` and `TrackedIndex` traits (tests use
//! fakes). See the module docs of `mempool`.

use {
  super::{
    MempoolEntry, MempoolNode, PendingTx, RawMempool, Removed, RemovedStatus, Snapshot, Stats,
    TrackedIndex,
    classify::{self, Classified},
  },
  crate::{
    history::HistoryKind,
    market::MarketContext,
    model::{MoveInput, ResolvedInput, ResolvedTx, TrackedUtxo, UtxoKind, script_hash},
    source::{input_envelopes, input_sighash},
  },
  bitcoin::{BlockHash, OutPoint, Transaction, Txid},
  std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
  },
};

/// New transactions fetched per poll at most; the rest wait for the next.
pub const MAX_NEW_PER_POLL: usize = 200_000;
/// Removed pending transactions are reported this long (seconds)…
pub const REMOVED_TTL: u64 = 3600;
/// …and this many at most.
pub const REMOVED_MAX: usize = 1000;
/// Blocks looked at to tell a confirmed transaction from a dropped one.
const CONFIRM_LOOKBACK: u32 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollOutcome {
  /// The index has no block yet.
  NotReady,
  /// bitcoind's tip is not the index's: nothing changed, try again later.
  Behind,
  /// The mempool and the tip did not change.
  Unchanged,
  /// Polled; `changed` says whether the pending set changed (`seq` bumped).
  Polled { changed: bool },
}

/// A candidate before classification: its transaction and move.
struct Candidate {
  tx: ResolvedTx,
  mv: crate::model::Move,
  depends: Vec<Txid>,
}

pub struct Overlay {
  /// Every transaction in the mempool (as of the last poll): its inputs.
  known: HashMap<Txid, Box<[OutPoint]>>,
  /// Raw transactions of the pending set (needed to reclassify them).
  raw: HashMap<Txid, Transaction>,
  /// The pending set: transactions that spend tracked sats.
  pending: HashMap<Txid, Arc<PendingTx>>,
  /// Moves of the pending set, before classification.
  candidates: HashMap<Txid, Candidate>,
  /// Values and script hashes of pending transactions' untracked inputs.
  prevouts: HashMap<OutPoint, (u64, u32)>,
  entries: HashMap<Txid, MempoolEntry>,
  /// Transactions to examine again (inputs or entry not resolved).
  retry: HashSet<Txid>,
  removed: VecDeque<Removed>,
  seq: u64,
  tip: Option<(u32, BlockHash)>,
  mempool_sequence: Option<u64>,
  ready: bool,
  max_new_per_poll: usize,
  pub stats: Stats,
}

impl Default for Overlay {
  fn default() -> Self {
    Self::new(MAX_NEW_PER_POLL)
  }
}

impl Overlay {
  pub fn new(max_new_per_poll: usize) -> Self {
    Self {
      known: HashMap::new(),
      raw: HashMap::new(),
      pending: HashMap::new(),
      candidates: HashMap::new(),
      prevouts: HashMap::new(),
      entries: HashMap::new(),
      retry: HashSet::new(),
      removed: VecDeque::new(),
      seq: 0,
      tip: None,
      mempool_sequence: None,
      ready: false,
      max_new_per_poll: max_new_per_poll.max(1),
      stats: Stats::default(),
    }
  }

  pub fn seq(&self) -> u64 {
    self.seq
  }

  pub fn known_len(&self) -> usize {
    self.known.len()
  }

  /// One poll. `now` is unix seconds.
  pub fn poll(
    &mut self,
    node: &dyn MempoolNode,
    index: &dyn TrackedIndex,
    ctx: &MarketContext,
    now: u64,
  ) -> anyhow::Result<PollOutcome> {
    let Some((height, hash)) = index.tip()? else {
      return Ok(PollOutcome::NotReady);
    };
    if node.best_block_hash()? != hash {
      return Ok(PollOutcome::Behind);
    }
    let RawMempool { txids, sequence } = node.raw_mempool()?;
    if node.best_block_hash()? != hash {
      return Ok(PollOutcome::Behind);
    }
    self.stats.mempool_txs = txids.len();
    self.stats.mempool_sequence = sequence;
    let tip_changed = self.tip != Some((height, hash));
    if !tip_changed
      && self.ready
      && sequence.is_some()
      && sequence == self.mempool_sequence
      && self.retry.is_empty()
    {
      return Ok(PollOutcome::Unchanged);
    }

    // Transactions that left the mempool.
    let current: HashSet<Txid> = txids.iter().copied().collect();
    let gone: Vec<Txid> = self
      .known
      .keys()
      .filter(|txid| !current.contains(*txid))
      .copied()
      .collect();
    let mut gone_pending = Vec::new();
    for txid in &gone {
      self.known.remove(txid);
      self.raw.remove(txid);
      self.entries.remove(txid);
      self.retry.remove(txid);
      self.candidates.remove(txid);
      if let Some(pending) = self.pending.remove(txid) {
        gone_pending.push(pending);
      }
    }

    // New transactions.
    let added: Vec<Txid> = txids
      .iter()
      .filter(|txid| !self.known.contains_key(*txid))
      .take(self.max_new_per_poll)
      .copied()
      .collect();
    let mut new_txs: HashMap<Txid, Transaction> = HashMap::with_capacity(added.len());
    if !added.is_empty() {
      for (txid, tx) in added.iter().zip(node.transactions(&added)?) {
        let Some(tx) = tx else { continue };
        self.known.insert(
          *txid,
          tx.input.iter().map(|input| input.previous_output).collect(),
        );
        new_txs.insert(*txid, tx);
      }
    }
    self.stats.scanned_txs = self.known.len();

    // Which transactions to examine: everything on a new tip (confirmed
    // parents now come from the index; a reorg may track new inputs), else
    // the new ones and those to retry.
    let examine: Vec<Txid> = if tip_changed {
      self.candidates.clear();
      self.known.keys().copied().collect()
    } else {
      new_txs
        .keys()
        .copied()
        .chain(self.retry.iter().copied())
        .filter(|txid| self.known.contains_key(txid))
        .collect::<HashSet<Txid>>()
        .into_iter()
        .collect()
    };
    self.retry.clear();

    // Tracked outputs of the pending set (kept ones, then new ones as they
    // are found, parents first).
    let mut outputs: HashMap<OutPoint, TrackedUtxo> = HashMap::new();
    let next_height = height + 1;
    for candidate in self.candidates.values() {
      add_outputs(&mut outputs, candidate, next_height);
    }

    for txid in topological(&examine, &self.known) {
      let inputs = self.known[&txid].clone();
      let mut tracked = Vec::new();
      let mut depends = Vec::new();
      for (vin, outpoint) in inputs.iter().enumerate() {
        if let Some(utxo) = outputs.get(outpoint) {
          if utxo.kind == UtxoKind::Spendable {
            tracked.push(MoveInput {
              vin: vin as u32,
              outpoint: *outpoint,
              utxo: utxo.clone(),
            });
            if !depends.contains(&outpoint.txid) {
              depends.push(outpoint.txid);
            }
          }
        } else if let Some(utxo) = index.utxo(*outpoint)?
          && utxo.kind == UtxoKind::Spendable
        {
          tracked.push(MoveInput {
            vin: vin as u32,
            outpoint: *outpoint,
            utxo,
          });
        }
      }
      if tracked.is_empty() {
        continue;
      }
      let tx = match new_txs
        .remove(&txid)
        .or_else(|| self.raw.get(&txid).cloned())
      {
        Some(tx) => tx,
        None => match node.transactions(&[txid])?.pop().flatten() {
          Some(tx) => tx,
          None => continue,
        },
      };
      let Some(resolved) = self.resolve(node, &tx, &tracked)? else {
        log::debug!("mempool: inputs of {txid} not resolved yet");
        self.retry.insert(txid);
        continue;
      };
      let mv = match classify::tx_move(&resolved, tracked) {
        Ok(mv) => mv,
        Err(err) => {
          log::warn!("mempool: skipping {txid}: {err:#}");
          continue;
        }
      };
      let candidate = Candidate {
        tx: resolved,
        mv,
        depends,
      };
      add_outputs(&mut outputs, &candidate, next_height);
      self.raw.insert(txid, tx);
      self.candidates.insert(txid, candidate);
    }
    // Raw transactions of former candidates (a reorg) are not needed.
    self
      .raw
      .retain(|txid, _| self.candidates.contains_key(txid));

    // Mempool entries (first seen, fee, vsize) of new candidates.
    let missing: Vec<Txid> = self
      .candidates
      .keys()
      .filter(|txid| !self.entries.contains_key(*txid))
      .copied()
      .collect();
    if !missing.is_empty() {
      for (txid, entry) in missing.iter().zip(node.entries(&missing)?) {
        match entry {
          Some(entry) => {
            self.entries.insert(*txid, entry);
          }
          None => {
            self.retry.insert(*txid);
          }
        }
      }
    }

    // Classify the pending set: a sale settled by a second transaction is
    // attributed through its spender, so a parent may change with a child.
    let mut spenders: HashMap<Txid, Vec<Txid>> = HashMap::new();
    for (txid, candidate) in &self.candidates {
      for input in &candidate.tx.inputs {
        if self.candidates.contains_key(&input.previous_output.txid) {
          let list = spenders.entry(input.previous_output.txid).or_default();
          if !list.contains(txid) {
            list.push(*txid);
          }
        }
      }
    }
    let mut changed = false;
    let mut next = HashMap::with_capacity(self.candidates.len());
    for (txid, candidate) in &self.candidates {
      let Some(entry) = self.entries.get(txid) else {
        continue;
      };
      let first_seen = u32::try_from(entry.time).unwrap_or(u32::MAX);
      let spender_txs: Vec<&ResolvedTx> = spenders
        .get(txid)
        .map(|list| {
          let mut list = list.clone();
          list.sort();
          list
            .iter()
            .map(|child| &self.candidates[child].tx)
            .collect()
        })
        .unwrap_or_default();
      let time_of = |h: u32| {
        if h > height {
          Some(first_seen)
        } else {
          index.time(h).ok().flatten()
        }
      };
      let classified: Classified = classify::classify(
        &candidate.tx,
        candidate.mv.clone(),
        &spender_txs,
        next_height,
        first_seen,
        ctx,
        &time_of,
      );
      let pending = PendingTx {
        txid: *txid,
        tx: candidate.tx.clone(),
        first_seen: entry.time,
        fee: entry.fee,
        vsize: entry.vsize,
        depends: candidate.depends.clone(),
        height: next_height,
        classified,
      };
      let pending = match self.pending.get(txid) {
        Some(old) if **old == pending => old.clone(),
        _ => {
          changed = true;
          Arc::new(pending)
        }
      };
      next.insert(*txid, pending);
    }
    if self.pending.keys().any(|txid| !next.contains_key(txid)) {
      changed = true;
    }
    // Pending transactions no longer pending while still in the mempool
    // (a reorg changed what they spend) are dropped without a tombstone.
    self.pending = next;

    // Tombstones for the pending transactions that left the mempool.
    if !gone_pending.is_empty() {
      changed = true;
      self.seq += 1;
      let statuses = self.removal_statuses(&gone_pending, index, height, tip_changed)?;
      for (pending, (status, at_height, replaced_by)) in gone_pending.iter().zip(statuses) {
        self.removed.push_front(Removed {
          txid: pending.txid,
          status,
          height: at_height,
          replaced_by,
          at: now,
          seq: self.seq,
        });
      }
    } else if changed {
      self.seq += 1;
    }
    while self
      .removed
      .back()
      .is_some_and(|removed| removed.at + REMOVED_TTL < now)
      || self.removed.len() > REMOVED_MAX
    {
      self.removed.pop_back();
    }

    // Forget input values no pending transaction needs.
    let needed: HashSet<OutPoint> = self
      .candidates
      .values()
      .flat_map(|candidate| {
        candidate
          .tx
          .inputs
          .iter()
          .map(|input| input.previous_output)
      })
      .collect();
    self
      .prevouts
      .retain(|outpoint, _| needed.contains(outpoint));

    self.tip = Some((height, hash));
    self.mempool_sequence = sequence;
    self.ready = true;
    if changed {
      self.stats.last_change_at = Some(now);
    }
    Ok(PollOutcome::Polled { changed })
  }

  /// `tx` with every input's value and script hash, or `None` when some
  /// could not be resolved (yet).
  fn resolve(
    &mut self,
    node: &dyn MempoolNode,
    tx: &Transaction,
    tracked: &[MoveInput],
  ) -> anyhow::Result<Option<ResolvedTx>> {
    let mut known: HashMap<OutPoint, (u64, u32)> = HashMap::new();
    for input in tracked {
      known.insert(
        input.outpoint,
        (input.utxo.value, script_hash(&input.utxo.script_pubkey)),
      );
    }
    let missing: Vec<OutPoint> = tx
      .input
      .iter()
      .map(|input| input.previous_output)
      .filter(|outpoint| !known.contains_key(outpoint) && !self.prevouts.contains_key(outpoint))
      .collect();
    if !missing.is_empty() {
      for (outpoint, output) in missing.iter().zip(node.prevouts(&missing)?) {
        if let Some(output) = output {
          self.prevouts.insert(
            *outpoint,
            (output.value.to_sat(), script_hash(&output.script_pubkey)),
          );
        }
      }
    }
    let mut inputs = Vec::with_capacity(tx.input.len());
    for input in &tx.input {
      let outpoint = input.previous_output;
      let Some(&(value, script_hash)) = known
        .get(&outpoint)
        .or_else(|| self.prevouts.get(&outpoint))
      else {
        return Ok(None);
      };
      inputs.push(ResolvedInput {
        previous_output: outpoint,
        value,
        script_hash,
        sighash: input_sighash(&input.script_sig, &input.witness),
        envelopes: input_envelopes(&input.witness),
      });
    }
    Ok(Some(ResolvedTx {
      txid: tx.compute_txid(),
      inputs,
      outputs: tx.output.clone(),
    }))
  }

  /// Why each of `gone` (pending transactions that left the mempool, in
  /// any order) left: confirmed in a block indexed since the last poll,
  /// conflicted by one, replaced by a transaction in the mempool now, or
  /// evicted. A transaction whose pending parent left unconfirmed shares
  /// its parent's fate.
  fn removal_statuses(
    &self,
    gone: &[Arc<PendingTx>],
    index: &dyn TrackedIndex,
    height: u32,
    tip_changed: bool,
  ) -> anyhow::Result<Vec<Fate>> {
    let mut confirmed: HashMap<Txid, u32> = HashMap::new();
    let mut block_spent: HashSet<OutPoint> = HashSet::new();
    if tip_changed {
      let from = match self.tip {
        Some((old, _)) if old < height => (old + 1).max(height.saturating_sub(CONFIRM_LOOKBACK)),
        // A reorg (or the first poll): the tip block only.
        _ => height,
      };
      for h in from..=height {
        for (txid, spent) in index.block_moves(h)?.unwrap_or_default() {
          confirmed.insert(txid, h);
          block_spent.extend(spent);
        }
      }
    }

    // Spenders, in today's mempool, of the inputs of the removed ones.
    let wanted: HashSet<OutPoint> = gone
      .iter()
      .flat_map(|pending| pending.tx.inputs.iter().map(|input| input.previous_output))
      .collect();
    let mut replacements: HashMap<OutPoint, Txid> = HashMap::new();
    for (txid, inputs) in &self.known {
      for outpoint in inputs.iter() {
        if wanted.contains(outpoint) {
          replacements.insert(*outpoint, *txid);
        }
      }
    }

    let mut fates = Fates {
      gone,
      by_txid: gone
        .iter()
        .enumerate()
        .map(|(i, pending)| (pending.txid, i))
        .collect(),
      confirmed,
      block_spent,
      replacements,
      fates: vec![None; gone.len()],
    };
    Ok((0..gone.len()).map(|i| fates.fate(i, 0)).collect())
  }

  /// The published view of the overlay.
  pub fn snapshot(&self, enabled: bool) -> Snapshot {
    let mut txs: Vec<Arc<PendingTx>> = self.pending.values().cloned().collect();
    txs.sort_by(|a, b| b.first_seen.cmp(&a.first_seen).then(a.txid.cmp(&b.txid)));
    let mut stats = self.stats.clone();
    stats.pending_sales = txs.iter().map(|tx| tx.classified.market.sales.len()).sum();
    stats.pending_extractions = txs
      .iter()
      .filter(|tx| tx.has_kind(HistoryKind::Extraction))
      .count();
    Snapshot::new(
      enabled,
      self.ready,
      self.seq,
      self.tip,
      txs,
      self.removed.iter().cloned().collect(),
      stats,
    )
  }
}

fn add_outputs(outputs: &mut HashMap<OutPoint, TrackedUtxo>, candidate: &Candidate, height: u32) {
  for output in &candidate.mv.outputs {
    outputs.insert(
      OutPoint::new(candidate.mv.txid, output.vout),
      TrackedUtxo {
        value: output.value,
        script_pubkey: output.script_pubkey.clone(),
        height,
        kind: output.kind,
        pieces: output.pieces.clone(),
      },
    );
  }
}

/// `txids` ordered so that a transaction comes after its parents among
/// them (any order otherwise; a cycle, impossible in a mempool, is broken
/// arbitrarily).
fn topological(txids: &[Txid], known: &HashMap<Txid, Box<[OutPoint]>>) -> Vec<Txid> {
  let set: HashSet<Txid> = txids.iter().copied().collect();
  let mut indegree: HashMap<Txid, usize> = HashMap::with_capacity(txids.len());
  let mut children: HashMap<Txid, Vec<Txid>> = HashMap::new();
  for txid in txids {
    let mut parents: Vec<Txid> = known
      .get(txid)
      .map(|inputs| {
        inputs
          .iter()
          .map(|outpoint| outpoint.txid)
          .filter(|parent| set.contains(parent) && parent != txid)
          .collect()
      })
      .unwrap_or_default();
    parents.sort();
    parents.dedup();
    indegree.insert(*txid, parents.len());
    for parent in parents {
      children.entry(parent).or_default().push(*txid);
    }
  }
  let mut queue: VecDeque<Txid> = txids
    .iter()
    .filter(|txid| indegree[*txid] == 0)
    .copied()
    .collect();
  let mut order = Vec::with_capacity(txids.len());
  let mut done: HashSet<Txid> = HashSet::with_capacity(txids.len());
  loop {
    while let Some(txid) = queue.pop_front() {
      if !done.insert(txid) {
        continue;
      }
      order.push(txid);
      for child in children.get(&txid).into_iter().flatten() {
        let n = indegree.get_mut(child).unwrap();
        *n -= 1;
        if *n == 0 {
          queue.push_back(*child);
        }
      }
    }
    match txids.iter().find(|txid| !done.contains(*txid)) {
      Some(txid) => queue.push_back(*txid),
      None => break,
    }
  }
  order
}

/// Why a pending transaction left: its status, the confirming height, the
/// replacing transaction.
type Fate = (RemovedStatus, Option<u32>, Option<Txid>);

/// Works out the fates of the pending transactions that left the mempool.
struct Fates<'a> {
  gone: &'a [Arc<PendingTx>],
  by_txid: HashMap<Txid, usize>,
  /// Transactions of the blocks indexed since the last poll, by height.
  confirmed: HashMap<Txid, u32>,
  /// Tracked outpoints those blocks spent.
  block_spent: HashSet<OutPoint>,
  /// Today's mempool spenders of the gone ones' inputs.
  replacements: HashMap<OutPoint, Txid>,
  fates: Vec<Option<Fate>>,
}

impl Fates<'_> {
  fn fate(&mut self, i: usize, depth: usize) -> Fate {
    if let Some(fate) = self.fates[i] {
      return fate;
    }
    let pending = self.gone[i].clone();
    let fate = if let Some(&height) = self.confirmed.get(&pending.txid) {
      (RemovedStatus::Confirmed, Some(height), None)
    } else if pending
      .classified
      .mv
      .inputs
      .iter()
      .any(|input| self.block_spent.contains(&input.outpoint))
    {
      (RemovedStatus::Conflicted, None, None)
    } else if let Some(by) = pending
      .tx
      .inputs
      .iter()
      .find_map(|input| self.replacements.get(&input.previous_output))
    {
      (RemovedStatus::Replaced, None, Some(*by))
    } else {
      // A parent that left unconfirmed takes its children with it.
      let mut inherited = None;
      if depth < 32 {
        for parent in &pending.depends {
          if let Some(&j) = self.by_txid.get(parent) {
            let fate = self.fate(j, depth + 1);
            if fate.0 != RemovedStatus::Confirmed {
              inherited = Some(fate);
              break;
            }
          }
        }
      }
      inherited.unwrap_or((RemovedStatus::Evicted, None, None))
    };
    self.fates[i] = Some(fate);
    fate
  }
}
