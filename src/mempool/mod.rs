//! The mempool overlay: unconfirmed transactions that move tracked sats,
//! classified exactly as a block would classify them (sat flow, sales,
//! extractions, address history), held in memory next to the index and
//! never written to it (docs/DESIGN.md §6.2, docs/API.md "Mempool").
//!
//! A thread (`run`) polls bitcoind every few seconds, and right after each
//! index commit:
//!
//! 1. It goes on only while bitcoind's best block is the index's tip, so a
//!    transaction that just confirmed is never reported as dropped before
//!    its block is indexed.
//! 2. `getrawmempool false true` lists the mempool. New transactions are
//!    fetched raw (`getrawtransaction`, batched); the overlay keeps every
//!    mempool transaction's input outpoints (~9 MB at 100k transactions).
//! 3. A transaction is **pending** (a candidate) when one of its inputs is a
//!    tracked UTXO of the index or a tracked output of another pending
//!    transaction (chains of any depth, parents first). On a new tip every
//!    mempool transaction is checked again.
//! 4. A pending transaction's untracked inputs are valued with `gettxout`
//!    (confirmed) or the parent's raw transaction (unconfirmed);
//!    `getmempoolentry` gives its fee, vsize and first-seen time. Then
//!    `classify` derives its `Move`, sales and history entries as at the
//!    next height.
//! 5. One that leaves the mempool leaves a tombstone (`Removed`): confirmed,
//!    conflicted, replaced or evicted.
//!
//! The server reads only the published `Snapshot`.

pub mod classify;
pub mod flow;
mod node;
pub mod overlay;
#[cfg(test)]
mod tests;

pub use {
  node::{RpcMempoolNode, StoreIndex},
  overlay::{Overlay, PollOutcome},
};

use {
  crate::{
    history::HistoryKind,
    indexer::Published,
    market::MarketContext,
    model::{Piece, TrackedUtxo, UtxoKind},
    store::Store,
  },
  bitcoin::{BlockHash, OutPoint, Transaction, TxOut, Txid},
  serde::Serialize,
  std::{
    collections::HashMap,
    sync::{
      Arc,
      atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
  },
};

pub use classify::Classified;

/// `getrawmempool false true`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawMempool {
  pub txids: Vec<Txid>,
  /// bitcoind's `mempool_sequence`.
  pub sequence: Option<u64>,
}

/// What the overlay needs from `getmempoolentry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MempoolEntry {
  /// Unix seconds the transaction entered bitcoind's mempool.
  pub time: u64,
  /// Sats (`fees.base`).
  pub fee: u64,
  pub vsize: u64,
}

/// bitcoind, as the overlay uses it. Missing transactions (e.g. removed
/// since the listing) come back as `None`, not as errors.
pub trait MempoolNode {
  fn best_block_hash(&self) -> anyhow::Result<BlockHash>;
  fn raw_mempool(&self) -> anyhow::Result<RawMempool>;
  fn transactions(&self, txids: &[Txid]) -> anyhow::Result<Vec<Option<Transaction>>>;
  fn entries(&self, txids: &[Txid]) -> anyhow::Result<Vec<Option<MempoolEntry>>>;
  /// The outputs `outpoints` spend: unspent in the chainstate, or outputs of
  /// mempool transactions.
  fn prevouts(&self, outpoints: &[OutPoint]) -> anyhow::Result<Vec<Option<TxOut>>>;
}

/// A block's transactions that moved tracked sats, with the tracked
/// outpoints each spent.
pub type BlockMoves = Vec<(Txid, Vec<OutPoint>)>;

/// The index, as the overlay reads it.
pub trait TrackedIndex {
  /// The committed tip.
  fn tip(&self) -> anyhow::Result<Option<(u32, BlockHash)>>;
  /// A tracked UTXO (spendable or not).
  fn utxo(&self, outpoint: OutPoint) -> anyhow::Result<Option<TrackedUtxo>>;
  /// Header time of an indexed block.
  fn time(&self, height: u32) -> anyhow::Result<Option<u32>>;
  /// The transactions of block `height` that moved tracked sats, with the
  /// tracked outpoints each spent (`None`: moves not retained).
  fn block_moves(&self, height: u32) -> anyhow::Result<Option<BlockMoves>>;
}

/// A pending transaction: it spends tracked sats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTx {
  pub txid: Txid,
  pub tx: crate::model::ResolvedTx,
  /// bitcoind's mempool entry time, unix seconds.
  pub first_seen: u64,
  pub fee: u64,
  pub vsize: u64,
  /// Pending parents whose tracked outputs it spends.
  pub depends: Vec<Txid>,
  /// The height it was classified for (the index tip + 1).
  pub height: u32,
  pub classified: Classified,
}

impl PendingTx {
  /// sat/vB, one decimal.
  pub fn fee_rate(&self) -> f64 {
    if self.vsize == 0 {
      return 0.0;
    }
    (self.fee as f64 / self.vsize as f64 * 10.0).round() / 10.0
  }

  pub fn has_kind(&self, kind: HistoryKind) -> bool {
    self
      .classified
      .entries
      .iter()
      .any(|entry| entry.kind == kind)
  }

  /// Its entries' kinds, distinct, in the history's precedence order.
  pub fn kinds(&self) -> Vec<HistoryKind> {
    [
      HistoryKind::Mined,
      HistoryKind::Sale,
      HistoryKind::Extraction,
      HistoryKind::Burn,
      HistoryKind::Consolidation,
      HistoryKind::Transfer,
    ]
    .into_iter()
    .filter(|kind| self.has_kind(*kind))
    .collect()
  }

  /// Every pending sat piece the transaction moved (its tracked inputs).
  pub fn input_pieces(&self) -> impl Iterator<Item = &Piece> {
    self
      .classified
      .mv
      .inputs
      .iter()
      .flat_map(|input| input.utxo.pieces.iter())
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovedStatus {
  Confirmed,
  Replaced,
  Conflicted,
  Evicted,
}

/// A pending transaction that left the mempool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Removed {
  pub txid: Txid,
  pub status: RemovedStatus,
  /// The block that confirmed it.
  pub height: Option<u32>,
  /// The transaction that replaced it (or its pending parent).
  pub replaced_by: Option<Txid>,
  /// Unix seconds it was seen gone.
  pub at: u64,
  /// The `seq` that removed it.
  pub seq: u64,
}

/// Poll counters, for `/mempool/status`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
  pub mempool_sequence: Option<u64>,
  pub mempool_txs: usize,
  pub scanned_txs: usize,
  pub pending_sales: usize,
  pub pending_extractions: usize,
  pub interval_ms: u64,
  pub last_poll_at: Option<u64>,
  pub last_poll_ms: Option<u64>,
  pub last_change_at: Option<u64>,
  pub error: Option<String>,
}

/// Where a pending transaction spends a tracked output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spend {
  /// Index into `Snapshot::txs`.
  pub tx: usize,
  pub vin: u32,
}

/// The overlay as published for the server: immutable.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
  pub enabled: bool,
  /// The first poll completed.
  pub ready: bool,
  pub seq: u64,
  /// The index tip the pending set is consistent with.
  pub tip: Option<(u32, BlockHash)>,
  /// Newest `first_seen` first.
  pub txs: Vec<Arc<PendingTx>>,
  /// Newest first.
  pub removed: Vec<Removed>,
  pub stats: Stats,
  by_txid: HashMap<Txid, usize>,
  /// Tracked outpoints (confirmed or pending) spent by pending transactions.
  spends: HashMap<OutPoint, Spend>,
  /// Tracked outputs of pending transactions: (tx, index in its move's outputs).
  outputs: HashMap<OutPoint, (usize, usize)>,
  /// Sold outpoints of pending sales.
  sold: HashMap<OutPoint, usize>,
}

/// A sat's latest pending location (the end of a pending chain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingLocation {
  pub tx: usize,
  pub kind: PendingLocationKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingLocationKind {
  /// In output `vout` at `offset`.
  Output {
    vout: u32,
    offset: u64,
    burned: bool,
  },
  /// Paid as fee: to a coinbase not mined yet.
  Fee,
}

impl Snapshot {
  pub fn new(
    enabled: bool,
    ready: bool,
    seq: u64,
    tip: Option<(u32, BlockHash)>,
    txs: Vec<Arc<PendingTx>>,
    removed: Vec<Removed>,
    stats: Stats,
  ) -> Self {
    let mut by_txid = HashMap::with_capacity(txs.len());
    let mut spends = HashMap::new();
    let mut outputs = HashMap::new();
    let mut sold = HashMap::new();
    for (i, tx) in txs.iter().enumerate() {
      by_txid.insert(tx.txid, i);
      for input in &tx.classified.mv.inputs {
        spends.insert(
          input.outpoint,
          Spend {
            tx: i,
            vin: input.vin,
          },
        );
      }
      for (j, output) in tx.classified.mv.outputs.iter().enumerate() {
        outputs.insert(OutPoint::new(tx.txid, output.vout), (i, j));
      }
      for sale in &tx.classified.market.sales {
        sold.insert(sale.seller_outpoint, i);
      }
    }
    Self {
      enabled,
      ready,
      seq,
      tip,
      txs,
      removed,
      stats,
      by_txid,
      spends,
      outputs,
      sold,
    }
  }

  /// A disabled overlay (`--no-mempool`, `--no-sync`).
  pub fn disabled() -> Self {
    Self::default()
  }

  pub fn tx(&self, txid: &Txid) -> Option<&Arc<PendingTx>> {
    self.by_txid.get(txid).map(|&i| &self.txs[i])
  }

  pub fn removed(&self, txid: &Txid) -> Option<&Removed> {
    self.removed.iter().find(|removed| removed.txid == *txid)
  }

  /// The pending transaction spending a tracked outpoint.
  pub fn spend(&self, outpoint: &OutPoint) -> Option<Spend> {
    self.spends.get(outpoint).copied()
  }

  /// Whether the pending spend of `outpoint` sells it.
  pub fn sold(&self, outpoint: &OutPoint) -> bool {
    self.sold.contains_key(outpoint)
  }

  /// A tracked output a pending transaction creates, as a UTXO (created at
  /// the pending height), with the transaction's index.
  pub fn output(&self, outpoint: &OutPoint) -> Option<(usize, TrackedUtxo)> {
    let &(i, j) = self.outputs.get(outpoint)?;
    let tx = &self.txs[i];
    let output = &tx.classified.mv.outputs[j];
    Some((
      i,
      TrackedUtxo {
        value: output.value,
        script_pubkey: output.script_pubkey.clone(),
        height: tx.height,
        kind: output.kind,
        pieces: output.pieces.clone(),
      },
    ))
  }

  /// Pending outputs paying `script` (spendable, tracked), with their
  /// transactions' indexes, newest first.
  pub fn outputs_to(&self, script: &bitcoin::Script) -> Vec<(OutPoint, usize, TrackedUtxo)> {
    let mut list = Vec::new();
    for (i, tx) in self.txs.iter().enumerate() {
      for output in &tx.classified.mv.outputs {
        if output.kind == UtxoKind::Spendable && output.script_pubkey.as_script() == script {
          let outpoint = OutPoint::new(tx.txid, output.vout);
          if let Some((_, utxo)) = self.output(&outpoint) {
            list.push((outpoint, i, utxo));
          }
        }
      }
    }
    list
  }

  /// Where a pending transaction last moved `sat`, if any: the location no
  /// other pending transaction moves it from.
  pub fn locate(&self, sat: u64) -> Option<PendingLocation> {
    let contains = |pieces: &[Piece]| {
      pieces
        .iter()
        .find(|piece| piece.start <= sat && sat < piece.end())
        .map(|piece| piece.offset + (sat - piece.start))
    };
    for (i, tx) in self.txs.iter().enumerate() {
      let mv = &tx.classified.mv;
      for output in &mv.outputs {
        if let Some(offset) = contains(&output.pieces) {
          let outpoint = OutPoint::new(tx.txid, output.vout);
          if self.spends.contains_key(&outpoint) {
            break;
          }
          return Some(PendingLocation {
            tx: i,
            kind: PendingLocationKind::Output {
              vout: output.vout,
              offset,
              burned: output.kind == UtxoKind::Unspendable,
            },
          });
        }
      }
      if contains(&mv.fee).is_some() {
        return Some(PendingLocation {
          tx: i,
          kind: PendingLocationKind::Fee,
        });
      }
    }
    None
  }
}

/// How the overlay thread runs.
#[derive(Debug, Clone, Copy)]
pub struct MempoolOptions {
  pub interval: Duration,
  pub max_new_per_poll: usize,
}

impl Default for MempoolOptions {
  fn default() -> Self {
    Self {
      interval: Duration::from_secs(5),
      max_new_per_poll: overlay::MAX_NEW_PER_POLL,
    }
  }
}

fn unix_now() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|duration| duration.as_secs())
    .unwrap_or_default()
}

/// The overlay thread's loop: polls until `shutdown`, publishing a snapshot
/// after every poll. Polls every `options.interval`, and as soon as the
/// published index tip changes.
pub fn run(
  store: Arc<Store>,
  node: &dyn MempoolNode,
  ctx: &MarketContext,
  published: &Published,
  options: MempoolOptions,
  shutdown: &AtomicBool,
) {
  let mut overlay = Overlay::new(options.max_new_per_poll);
  overlay.stats.interval_ms = options.interval.as_millis() as u64;
  published.set_mempool(overlay.snapshot(true));
  let mut last_hash = None;
  let mut last_poll: Option<Instant> = None;
  while !shutdown.load(Ordering::Relaxed) {
    let status = published.status();
    if status.mode != crate::indexer::Mode::Live {
      // Starting, syncing or catching up: the tip moves too fast to follow.
      thread::sleep(Duration::from_millis(500));
      continue;
    }
    let hash = status.hash;
    let due = last_poll.is_none_or(|at| at.elapsed() >= options.interval);
    if !due && hash == last_hash {
      thread::sleep(Duration::from_millis(250));
      continue;
    }
    last_hash = hash;
    last_poll = Some(Instant::now());
    let started = Instant::now();
    let result = store
      .reader()
      .and_then(|reader| overlay.poll(node, &StoreIndex(reader), ctx, unix_now()));
    let now = unix_now();
    match result {
      Ok(outcome) => {
        overlay.stats.error = None;
        if matches!(outcome, PollOutcome::Polled { .. } | PollOutcome::Unchanged) {
          overlay.stats.last_poll_at = Some(now);
          overlay.stats.last_poll_ms = Some(started.elapsed().as_millis() as u64);
        }
        if let PollOutcome::Polled { changed: true } = outcome {
          log::debug!(
            "mempool: seq {}, {} pending of {} ({} ms)",
            overlay.seq(),
            overlay.snapshot(true).txs.len(),
            overlay.known_len(),
            started.elapsed().as_millis()
          );
        }
        if outcome == PollOutcome::Behind {
          // The indexer is catching up: poll again soon.
          last_poll = None;
          last_hash = None;
          thread::sleep(Duration::from_millis(500));
        }
      }
      Err(err) => {
        log::warn!("mempool poll failed: {err:#}");
        overlay.stats.error = Some(format!("{err:#}"));
      }
    }
    published.set_mempool(overlay.snapshot(true));
  }
}
