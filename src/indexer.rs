//! Orchestration: fresh index → fast sync; then the live loop (poll, reorg
//! handling, per-block commit); publishes the supply snapshot for the server.
//!
//! OWNER: store+server+indexer agent.
//!
//! The tracker and bitcoind are reached through the `Engine` and `Node`
//! traits (implemented by `TrackerEngine` and `RpcNode`), so the loop can be
//! tested with fakes.

use {
  crate::{
    model::{BlockDelta, ResolvedBlock, RowId, SupplySnapshot, TrackerConfig, TrackerSnapshot},
    sats::SatCatalog,
    settings::{Chain, FastSync, Settings},
    source::{FastSource, FastSourceStats, FastSyncOptions, LiveSource, Rpc, RpcConfig},
    store::{CommitOptions, CommitStats, IndexMeta, SCHEMA_VERSION, Store, UNDO_DEPTH},
    tracker::Tracker,
  },
  anyhow::{Context, anyhow, ensure},
  bitcoin::BlockHash,
  serde::Serialize,
  std::{
    sync::{
      Arc, RwLock,
      atomic::{AtomicBool, Ordering},
      mpsc::{SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
  },
};

/// The fast sync stops this many blocks below bitcoind's tip.
pub const FAST_SYNC_DEPTH: u32 = 12;

/// An interrupted fast sync is restarted from genesis when more than this
/// many blocks are left; otherwise the live source finishes it.
pub const FAST_SYNC_RESTART_THRESHOLD: u32 = 150_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
  #[default]
  Starting,
  FastSync,
  CatchUp,
  Live,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Timings {
  /// Blocks applied per second between the last two commits.
  pub blocks_per_second: f64,
  pub last_commit_blocks: u64,
  pub last_commit_seconds: f64,
  /// Unix time of the last commit.
  pub last_commit_at: Option<u64>,
  pub commits: u64,
  pub commit_seconds_total: f64,
  /// Time the tracker took to compute the last supply snapshot.
  pub supply_seconds: f64,
  pub fast_sync_seconds: Option<f64>,
  /// Seconds to load the snapshot and restore the tracker at startup.
  pub restore_seconds: Option<f64>,
}

/// The indexer's progress, published for `/status`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Status {
  pub mode: Mode,
  pub height: Option<u32>,
  pub hash: Option<BlockHash>,
  pub time: Option<u32>,
  pub bitcoind_height: Option<u32>,
  pub fast_sync_complete: bool,
  pub tracked_utxos: u64,
  pub tracked_pieces: u64,
  pub lost_sats: u64,
  pub timings: Timings,
  /// The last indexing error, cleared by the next successful commit.
  pub error: Option<String>,
}

/// What the indexer publishes for the server after each commit.
#[derive(Default)]
pub struct Published {
  status: RwLock<Status>,
  supply: RwLock<Arc<SupplySnapshot>>,
}

impl Published {
  pub fn status(&self) -> Status {
    self.status.read().unwrap().clone()
  }

  pub fn supply(&self) -> Arc<SupplySnapshot> {
    self.supply.read().unwrap().clone()
  }

  pub fn update_status(&self, update: impl FnOnce(&mut Status)) {
    update(&mut self.status.write().unwrap());
  }

  pub fn set_supply(&self, supply: SupplySnapshot) {
    *self.supply.write().unwrap() = Arc::new(supply);
  }
}

/// bitcoind, as the indexer uses it.
pub trait Node: Send + Sync {
  /// `getblockchaininfo.chain`.
  fn chain(&self) -> anyhow::Result<String>;
  fn block_count(&self) -> anyhow::Result<u32>;
  fn best_block_hash(&self) -> anyhow::Result<BlockHash>;
  fn block_hash(&self, height: u32) -> anyhow::Result<BlockHash>;
  /// Heights `from..=to` in order, prefetched `threads` at a time.
  fn blocks(
    &self,
    from: u32,
    to: u32,
    threads: usize,
  ) -> Box<dyn Iterator<Item = anyhow::Result<ResolvedBlock>> + Send + '_>;
  /// Genesis to `options.stop_height`.
  fn fast_blocks(&self, options: FastSyncOptions) -> anyhow::Result<Box<dyn FastBlocks>>;
}

pub trait FastBlocks {
  fn next_block(&mut self) -> Option<anyhow::Result<ResolvedBlock>>;
  fn stats(&self) -> FastSourceStats;
}

impl FastBlocks for FastSource {
  fn next_block(&mut self) -> Option<anyhow::Result<ResolvedBlock>> {
    FastSource::next_block(self)
  }

  fn stats(&self) -> FastSourceStats {
    FastSource::stats(self)
  }
}

pub struct RpcNode {
  rpc: Rpc,
  live: LiveSource,
}

impl RpcNode {
  pub fn new(config: RpcConfig) -> anyhow::Result<Self> {
    let rpc = Rpc::new(config)?;
    Ok(Self {
      live: LiveSource::new(rpc.clone()),
      rpc,
    })
  }
}

impl Node for RpcNode {
  fn chain(&self) -> anyhow::Result<String> {
    self.rpc.chain()
  }

  fn block_count(&self) -> anyhow::Result<u32> {
    self.rpc.block_count()
  }

  fn best_block_hash(&self) -> anyhow::Result<BlockHash> {
    self.rpc.best_block_hash()
  }

  fn block_hash(&self, height: u32) -> anyhow::Result<BlockHash> {
    self.rpc.block_hash(height)
  }

  fn blocks(
    &self,
    from: u32,
    to: u32,
    threads: usize,
  ) -> Box<dyn Iterator<Item = anyhow::Result<ResolvedBlock>> + Send + '_> {
    self.live.blocks(from, to, threads)
  }

  fn fast_blocks(&self, options: FastSyncOptions) -> anyhow::Result<Box<dyn FastBlocks>> {
    Ok(Box::new(FastSource::start(self.rpc.clone(), options)?))
  }
}

/// The tracker, as the indexer drives it (only on the indexer's thread).
pub trait Engine {
  fn height(&self) -> Option<u32>;
  fn hash(&self) -> Option<BlockHash>;
  fn apply_block(&mut self, block: &ResolvedBlock) -> anyhow::Result<BlockDelta>;
  fn undo_block(&mut self, delta: &BlockDelta) -> anyhow::Result<()>;
  /// Per-row mined counters, indexed by `RowId`.
  fn mined(&self) -> Vec<u64>;
  fn supply(&self, top: usize) -> SupplySnapshot;
  fn utxo_count(&self) -> usize;
  fn piece_count(&self) -> usize;
}

pub trait EngineFactory: Send + Sync {
  /// `SatCatalog::fingerprint`, recorded in the index meta.
  fn fingerprint(&self) -> String;
  fn fresh(&self) -> anyhow::Result<Box<dyn Engine>>;
  fn restore(&self, snapshot: TrackerSnapshot) -> anyhow::Result<Box<dyn Engine>>;
}

pub struct TrackerEngine {
  tracker: Tracker,
  rows: usize,
}

impl Engine for TrackerEngine {
  fn height(&self) -> Option<u32> {
    self.tracker.height()
  }

  fn hash(&self) -> Option<BlockHash> {
    self.tracker.hash()
  }

  fn apply_block(&mut self, block: &ResolvedBlock) -> anyhow::Result<BlockDelta> {
    self.tracker.apply_block(block)
  }

  fn undo_block(&mut self, delta: &BlockDelta) -> anyhow::Result<()> {
    self.tracker.undo_block(delta)
  }

  fn mined(&self) -> Vec<u64> {
    (0..self.rows)
      .map(|row| self.tracker.row_stats(row as RowId).mined)
      .collect()
  }

  fn supply(&self, top: usize) -> SupplySnapshot {
    self.tracker.supply(top)
  }

  fn utxo_count(&self) -> usize {
    self.tracker.utxo_count()
  }

  fn piece_count(&self) -> usize {
    self.tracker.piece_count()
  }
}

pub struct TrackerFactory {
  catalog: Arc<SatCatalog>,
  config: TrackerConfig,
}

impl TrackerFactory {
  pub fn new(catalog: Arc<SatCatalog>, config: TrackerConfig) -> Self {
    Self { catalog, config }
  }
}

impl EngineFactory for TrackerFactory {
  fn fingerprint(&self) -> String {
    self.catalog.fingerprint()
  }

  fn fresh(&self) -> anyhow::Result<Box<dyn Engine>> {
    Ok(Box::new(TrackerEngine {
      tracker: Tracker::new(self.catalog.clone(), self.config.clone()),
      rows: self.catalog.rows().len(),
    }))
  }

  fn restore(&self, snapshot: TrackerSnapshot) -> anyhow::Result<Box<dyn Engine>> {
    Ok(Box::new(TrackerEngine {
      tracker: Tracker::restore(self.catalog.clone(), self.config.clone(), snapshot)?,
      rows: self.catalog.rows().len(),
    }))
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
  /// `ord index update`: index to bitcoind's tip, then return.
  UntilCaughtUp,
  /// `ord server`: follow the tip until shut down.
  Forever,
}

#[derive(Debug, Clone)]
pub struct IndexerOptions {
  pub chain: Chain,
  pub config: TrackerConfig,
  /// Commit the fast sync and catch-up every this many blocks...
  pub commit_interval: u32,
  /// ...or this often.
  pub commit_every: Duration,
  pub fetch_threads: usize,
  pub fast_sync: bool,
  pub fast_sync_restart_threshold: u32,
  pub poll_interval: Duration,
  /// Top holders per list in the published supply (live).
  pub supply_top: usize,
  /// The same during the fast sync, where commits are frequent.
  pub fast_sync_supply_top: usize,
  /// First wait after a bitcoind error (doubling up to a minute).
  pub retry_interval: Duration,
  /// Give up after this many consecutive bitcoind errors (`None`: never).
  pub max_retries: Option<u32>,
  /// Initial capacity of the fast sync's UTXO value map.
  pub utxo_capacity: usize,
  /// Index only blocks below this height.
  pub height_limit: Option<u32>,
}

impl IndexerOptions {
  pub fn new(settings: &Settings) -> Self {
    Self {
      chain: settings.chain,
      config: settings.config.clone(),
      commit_interval: settings.commit_interval,
      commit_every: Duration::from_secs(30),
      fetch_threads: settings.fetch_threads,
      fast_sync: settings.fast_sync == FastSync::Auto,
      fast_sync_restart_threshold: FAST_SYNC_RESTART_THRESHOLD,
      poll_interval: Duration::from_secs(2),
      supply_top: 1000,
      fast_sync_supply_top: 100,
      retry_interval: Duration::from_secs(1),
      max_retries: None,
      height_limit: settings.height_limit,
      utxo_capacity: match settings.chain {
        Chain::Mainnet => 190_000_000,
        Chain::Testnet4 | Chain::Signet => 10_000_000,
        Chain::Regtest => 100_000,
      },
    }
  }
}

/// The in-memory side of the index while the indexer runs.
struct State {
  engine: Box<dyn Engine>,
  fast_sync_complete: bool,
  /// Height at which the initial sync counts as complete (bitcoind's tip
  /// minus `FAST_SYNC_DEPTH` at startup).
  initial_target: Option<u32>,
  last_commit: Instant,
}

enum Step {
  CaughtUp,
  /// A bitcoind error: pending blocks were committed, try again later.
  Retry(anyhow::Error),
  Shutdown,
}

enum FastSyncEnd {
  Done,
  Shutdown,
  SourceFailed(anyhow::Error),
}

pub struct Indexer {
  store: Arc<Store>,
  node: Arc<dyn Node>,
  factory: Arc<dyn EngineFactory>,
  published: Arc<Published>,
  shutdown: Arc<AtomicBool>,
  options: IndexerOptions,
}

impl Indexer {
  pub fn new(
    store: Arc<Store>,
    node: Arc<dyn Node>,
    factory: Arc<dyn EngineFactory>,
    published: Arc<Published>,
    shutdown: Arc<AtomicBool>,
    options: IndexerOptions,
  ) -> Self {
    Self {
      store,
      node,
      factory,
      published,
      shutdown,
      options,
    }
  }

  pub fn expected_meta(&self) -> IndexMeta {
    expected_meta(
      self.options.chain,
      &self.options.config,
      self.factory.fingerprint(),
    )
  }

  pub fn run(&self, mode: RunMode) -> anyhow::Result<()> {
    let result = self.run_inner(mode);
    if let Err(err) = &result {
      self
        .published
        .update_status(|status| status.error = Some(format!("{err:#}")));
    }
    result
  }

  fn stopping(&self) -> bool {
    self.shutdown.load(Ordering::Relaxed)
  }

  /// Sleeps unless shutting down; returns whether shutdown was requested.
  fn sleep(&self, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while !self.stopping() {
      let now = Instant::now();
      if now >= deadline {
        return false;
      }
      thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
    true
  }

  /// Retries `f` on error with backoff; `None` when shutting down.
  fn retry<T>(
    &self,
    what: &str,
    mut f: impl FnMut() -> anyhow::Result<T>,
  ) -> anyhow::Result<Option<T>> {
    let mut failures = 0;
    loop {
      if self.stopping() {
        return Ok(None);
      }
      match f() {
        Ok(value) => return Ok(Some(value)),
        Err(err) => {
          failures += 1;
          if self.options.max_retries.is_some_and(|max| failures > max) {
            return Err(err.context(format!("{what} failed {failures} times")));
          }
          log::warn!("{what} failed (attempt {failures}): {err:#}");
          self.published.update_status(|status| {
            status.error = Some(format!("{what}: {err:#}"));
          });
          if self.sleep(self.backoff(failures)) {
            return Ok(None);
          }
        }
      }
    }
  }

  fn backoff(&self, failures: u32) -> Duration {
    (self.options.retry_interval * 2u32.saturating_pow(failures.saturating_sub(1)))
      .min(Duration::from_secs(60))
  }

  fn set_mode(&self, mode: Mode) {
    self.published.update_status(|status| status.mode = mode);
  }

  fn run_inner(&self, mode: RunMode) -> anyhow::Result<()> {
    self.set_mode(Mode::Starting);

    let Some(chain) = self.retry("getblockchaininfo", || self.node.chain())? else {
      return Ok(());
    };
    ensure!(
      chain == self.options.chain.bitcoind_name(),
      "bitcoind is on chain `{chain}` but ord was started with --chain {}",
      self.options.chain
    );

    let expected = self.expected_meta();
    let meta = match self.store.meta()? {
      Some(meta) => {
        meta
          .check_compatible(&expected)
          .with_context(|| format!("cannot use the index at {}", self.store.path().display()))?;
        meta
      }
      None => {
        self.store.init(&expected)?;
        expected
      }
    };

    let mut state = self.restore(&meta)?;

    if self.initial_sync(&mut state)? {
      return Ok(());
    }

    let mut failures = 0;
    while !self.stopping() {
      match self.sync_to_tip(&mut state)? {
        Step::CaughtUp => {
          failures = 0;
          self.published.update_status(|status| status.error = None);
          if mode == RunMode::UntilCaughtUp || self.sleep(self.options.poll_interval) {
            break;
          }
        }
        Step::Retry(err) => {
          failures += 1;
          if self.options.max_retries.is_some_and(|max| failures > max) {
            return Err(err.context(format!("bitcoind failed {failures} times")));
          }
          log::warn!("indexing failed (attempt {failures}), retrying: {err:#}");
          self
            .published
            .update_status(|status| status.error = Some(format!("{err:#}")));
          if self.sleep(self.backoff(failures)) {
            break;
          }
        }
        Step::Shutdown => break,
      }
    }

    log::info!("indexer stopped at height {:?}", state.engine.height());

    Ok(())
  }

  fn restore(&self, meta: &IndexMeta) -> anyhow::Result<State> {
    let engine = restore(
      &self.store,
      &*self.factory,
      &self.published,
      meta,
      self.options.supply_top,
    )?;
    Ok(State {
      engine,
      fast_sync_complete: meta.fast_sync_complete,
      initial_target: None,
      last_commit: Instant::now(),
    })
  }

  /// Runs the fast sync when it applies. Returns whether shutdown was
  /// requested.
  fn initial_sync(&self, state: &mut State) -> anyhow::Result<bool> {
    let mut attempts = 0;
    loop {
      let Some(tip) = self.retry("getblockcount", || self.node.block_count())? else {
        return Ok(true);
      };
      self
        .published
        .update_status(|status| status.bitcoind_height = Some(tip));

      let target = match self.options.height_limit {
        Some(limit) if limit <= tip => limit.checked_sub(1),
        _ => tip.checked_sub(FAST_SYNC_DEPTH),
      };
      state.initial_target = target;

      if state.fast_sync_complete || !self.options.fast_sync {
        return Ok(false);
      }

      let Some(stop) = target else {
        return Ok(false);
      };

      if let Some(height) = state.engine.height() {
        let remaining = stop.saturating_sub(height);
        if remaining <= self.options.fast_sync_restart_threshold {
          if remaining > 0 {
            log::info!(
              "the fast sync stopped at {height}; finishing the {remaining} blocks to {stop} \
               with the live source"
            );
          }
          return Ok(false);
        }
        log::warn!(
          "the fast sync stopped at {height} with {remaining} blocks left to {stop}; \
           restarting it from genesis"
        );
        self.store.wipe()?;
        self.store.init(&self.expected_meta())?;
        state.engine = self.factory.fresh()?;
        state.fast_sync_complete = false;
        self.published.update_status(|status| {
          status.height = None;
          status.hash = None;
          status.time = None;
          status.tracked_utxos = 0;
          status.tracked_pieces = 0;
          status.lost_sats = 0;
        });
        self.published.set_supply(SupplySnapshot::default());
      }

      match self.fast_sync(state, stop)? {
        FastSyncEnd::Done => return Ok(false),
        FastSyncEnd::Shutdown => return Ok(true),
        FastSyncEnd::SourceFailed(err) => {
          attempts += 1;
          if attempts > 3 {
            return Err(err.context("the fast sync failed 3 times"));
          }
          log::warn!(
            "the fast sync failed at {:?}: {err:#}",
            state.engine.height()
          );
          self
            .published
            .update_status(|status| status.error = Some(format!("fast sync: {err:#}")));
          if self.sleep(self.backoff(attempts)) {
            return Ok(true);
          }
        }
      }
    }
  }

  fn fast_sync(&self, state: &mut State, stop: u32) -> anyhow::Result<FastSyncEnd> {
    let start = Instant::now();
    let from = state.engine.height().map_or(0, |height| height + 1);
    ensure!(from == 0, "the fast sync only starts from genesis");
    log::info!("fast sync from genesis to {stop}");
    self.set_mode(Mode::FastSync);

    let mut source = match self.node.fast_blocks(FastSyncOptions {
      stop_height: stop,
      fetch_threads: self.options.fetch_threads,
      buffer_blocks: (self.options.fetch_threads * 4).max(64),
      utxo_capacity: self.options.utxo_capacity,
    }) {
      Ok(source) => source,
      Err(err) => return Ok(FastSyncEnd::SourceFailed(err)),
    };

    let committer = Committer::spawn(self.store.clone(), self.published.clone());

    let mut batch = Vec::new();
    let mut end = FastSyncEnd::Done;
    let mut result = Ok(());

    while !self.stopping() {
      let Some(block) = source.next_block() else {
        break;
      };
      let block = match block {
        Ok(block) => block,
        Err(err) => {
          end = FastSyncEnd::SourceFailed(err);
          break;
        }
      };
      let height = block.height;
      match state.engine.apply_block(&block) {
        Ok(delta) => batch.push(delta),
        Err(err) => {
          result = Err(err.context(format!("failed to apply block {height}")));
          break;
        }
      }
      if (batch.len() >= self.options.commit_interval as usize
        || state.last_commit.elapsed() >= self.options.commit_every
        || height >= stop)
        && let Err(err) = self.submit(&committer, state, &mut batch, stop, &*source)
      {
        result = Err(err);
        break;
      }
    }

    if self.stopping() && result.is_ok() {
      end = FastSyncEnd::Shutdown;
    }

    // Whatever stopped the loop, the applied blocks are whole: commit them.
    let flushed = self.submit(&committer, state, &mut batch, stop, &*source);
    let finished = committer.finish();
    drop(source);
    result?;
    flushed?;
    finished?;

    let seconds = start.elapsed().as_secs_f64();
    if matches!(end, FastSyncEnd::Done) {
      ensure!(
        state.engine.height() == Some(stop),
        "the fast source ended at {:?} instead of {stop}",
        state.engine.height()
      );
      log::info!("fast sync to {stop} done in {seconds:.0}s");
      self
        .published
        .update_status(|status| status.timings.fast_sync_seconds = Some(seconds));
    }

    Ok(end)
  }

  fn submit(
    &self,
    committer: &Committer,
    state: &mut State,
    batch: &mut Vec<BlockDelta>,
    stop: u32,
    source: &dyn FastBlocks,
  ) -> anyhow::Result<()> {
    let Some(last) = batch.last() else {
      return Ok(());
    };
    let complete = last.height >= stop;
    state.fast_sync_complete = complete;
    let supply_start = Instant::now();
    let supply = state.engine.supply(self.options.fast_sync_supply_top);
    let supply_seconds = supply_start.elapsed().as_secs_f64();
    let stats = source.stats();
    log::info!(
      "fast sync: committing {} blocks to {} ({} UTXOs in the value map, {:.1} GB fetched)",
      batch.len(),
      last.height,
      stats.utxos,
      stats.bytes_fetched as f64 / 1e9
    );
    committer.submit(Batch {
      deltas: std::mem::take(batch),
      mined: state.engine.mined(),
      fast_sync_complete: complete,
      target: Some(stop),
      supply,
      supply_seconds,
      utxos: state.engine.utxo_count() as u64,
      pieces: state.engine.piece_count() as u64,
      applied_seconds: state.last_commit.elapsed().as_secs_f64(),
    })?;
    state.last_commit = Instant::now();
    Ok(())
  }

  /// Follows bitcoind to its tip, handling reorgs. `Err` is fatal.
  fn sync_to_tip(&self, state: &mut State) -> anyhow::Result<Step> {
    loop {
      if self.stopping() {
        return Ok(Step::Shutdown);
      }

      let best = match self.node.best_block_hash() {
        Ok(best) => best,
        Err(err) => return Ok(Step::Retry(err)),
      };

      let count = match self.node.block_count() {
        Ok(count) => count,
        Err(err) => return Ok(Step::Retry(err)),
      };
      self
        .published
        .update_status(|status| status.bitcoind_height = Some(count));

      // With --height-limit, bitcoind's chain ends below the limit for us.
      let count = match self.options.height_limit {
        Some(0) => return Ok(Step::CaughtUp),
        Some(limit) => count.min(limit - 1),
        None => count,
      };

      if Some(best) == state.engine.hash() {
        self.set_mode(Mode::Live);
        return Ok(Step::CaughtUp);
      }

      if let (Some(height), Some(hash)) = (state.engine.height(), state.engine.hash()) {
        if count >= height {
          match self.node.block_hash(height) {
            Ok(theirs) if theirs == hash => {}
            Ok(theirs) => {
              log::warn!("reorg: bitcoind has {theirs} at {height}, the index {hash}");
              self.rollback(state)?;
              continue;
            }
            Err(err) => return Ok(Step::Retry(err)),
          }
        } else {
          // bitcoind is below the index: still syncing (its tip is one of
          // our blocks: wait) or on a shorter fork (roll back).
          let theirs = match self.node.block_hash(count) {
            Ok(theirs) => theirs,
            Err(err) => return Ok(Step::Retry(err)),
          };
          if Some(theirs) == self.store.block_hash(count)? {
            log::info!("bitcoind is at {count}, below the index at {height}; waiting");
            return Ok(Step::CaughtUp);
          }
          log::warn!("reorg: bitcoind's tip {theirs} at {count} is not in the index");
          self.rollback(state)?;
          continue;
        }
      }

      let from = state.engine.height().map_or(0, |height| height + 1);
      if from > count {
        return Ok(Step::CaughtUp);
      }

      self.set_mode(if count - from >= 1 {
        Mode::CatchUp
      } else {
        Mode::Live
      });

      let mut pending = Vec::new();
      let mut error = None;
      for block in self.node.blocks(from, count, self.options.fetch_threads) {
        if self.stopping() {
          break;
        }
        let block = match block {
          Ok(block) => block,
          Err(err) => {
            error = Some(err);
            break;
          }
        };
        if state
          .engine
          .hash()
          .is_some_and(|hash| block.header.prev_blockhash != hash)
        {
          log::warn!(
            "block {} {} does not extend the index tip; rechecking bitcoind's chain",
            block.height,
            block.hash
          );
          break;
        }
        let height = block.height;
        pending.push(
          state
            .engine
            .apply_block(&block)
            .with_context(|| format!("failed to apply block {height}"))?,
        );
        if pending.len() >= self.options.commit_interval as usize
          || state.last_commit.elapsed() >= self.options.commit_every
        {
          self.commit(state, &mut pending)?;
        }
      }
      self.commit(state, &mut pending)?;

      if let Some(err) = error {
        return Ok(Step::Retry(err));
      }
    }
  }

  fn commit(&self, state: &mut State, pending: &mut Vec<BlockDelta>) -> anyhow::Result<()> {
    let Some(last) = pending.last() else {
      return Ok(());
    };
    state.fast_sync_complete |= state
      .initial_target
      .is_none_or(|target| last.height >= target);
    let supply_start = Instant::now();
    let supply = state.engine.supply(self.options.supply_top);
    let supply_seconds = supply_start.elapsed().as_secs_f64();
    let batch = Batch {
      deltas: std::mem::take(pending),
      mined: state.engine.mined(),
      fast_sync_complete: state.fast_sync_complete,
      target: None,
      supply,
      supply_seconds,
      utxos: state.engine.utxo_count() as u64,
      pieces: state.engine.piece_count() as u64,
      applied_seconds: state.last_commit.elapsed().as_secs_f64(),
    };
    batch.commit(&self.store, &self.published)?;
    state.last_commit = Instant::now();
    Ok(())
  }

  /// Reverts the tip block in the store and the tracker.
  fn rollback(&self, state: &mut State) -> anyhow::Result<()> {
    let height = state.engine.height();
    let delta = self.store.rollback_tip()?.ok_or_else(|| {
      anyhow!(
        "reorg deeper than the index's {UNDO_DEPTH} blocks of undo records (at height \
         {height:?}); restart with --reindex"
      )
    })?;
    ensure!(
      Some(delta.height) == height && Some(delta.hash) == state.engine.hash(),
      "the index tip {} {} is not the tracker's {height:?} {:?}",
      delta.height,
      delta.hash,
      state.engine.hash()
    );
    state.engine.undo_block(&delta)?;
    log::warn!("reorg: rolled back block {} {}", delta.height, delta.hash);

    let reader = self.store.reader()?;
    let time = match state.engine.height() {
      Some(height) => reader.header(height)?.map(|header| header.time),
      None => None,
    };
    drop(reader);

    self.published.update_status(|status| {
      status.height = state.engine.height();
      status.hash = state.engine.hash();
      status.time = time;
      status.tracked_utxos = state.engine.utxo_count() as u64;
      status.tracked_pieces = state.engine.piece_count() as u64;
      status.lost_sats = delta.lost_sats_before;
    });
    self
      .published
      .set_supply(state.engine.supply(self.options.supply_top));
    Ok(())
  }
}

/// The meta of an index this process would build.
pub fn expected_meta(chain: Chain, config: &TrackerConfig, fingerprint: String) -> IndexMeta {
  IndexMeta {
    schema_version: SCHEMA_VERSION,
    chain: chain.name().into(),
    config: config.clone(),
    catalog_fingerprint: fingerprint,
    height: None,
    hash: None,
    fast_sync_complete: false,
  }
}

/// Rebuilds the tracker from the index (`meta` is its meta) and publishes
/// the index's status and the tracker's supply.
pub fn restore(
  store: &Store,
  factory: &dyn EngineFactory,
  published: &Published,
  meta: &IndexMeta,
  supply_top: usize,
) -> anyhow::Result<Box<dyn Engine>> {
  let start = Instant::now();
  let engine = if meta.height.is_some() {
    let snapshot = store.load_snapshot()?;
    log::info!(
      "restoring the tracker at height {:?}: {} tracked UTXOs",
      snapshot.height,
      snapshot.utxos.len()
    );
    factory.restore(snapshot)?
  } else {
    factory.fresh()?
  };
  ensure!(
    engine.height() == meta.height && engine.hash() == meta.hash,
    "the restored tracker is at {:?} {:?} but the index at {:?} {:?}",
    engine.height(),
    engine.hash(),
    meta.height,
    meta.hash
  );
  let restore_seconds = start.elapsed().as_secs_f64();

  let reader = store.reader()?;
  let time = match meta.height {
    Some(height) => reader.header(height)?.map(|header| header.time),
    None => None,
  };
  let lost_sats = reader.lost_sats()?;
  drop(reader);

  let supply_start = Instant::now();
  let supply = engine.supply(supply_top);
  let supply_seconds = supply_start.elapsed().as_secs_f64();

  published.update_status(|status| {
    status.height = meta.height;
    status.hash = meta.hash;
    status.time = time;
    status.fast_sync_complete = meta.fast_sync_complete;
    status.tracked_utxos = engine.utxo_count() as u64;
    status.tracked_pieces = engine.piece_count() as u64;
    status.lost_sats = lost_sats;
    status.timings.restore_seconds = Some(restore_seconds);
    status.timings.supply_seconds = supply_seconds;
  });
  published.set_supply(supply);

  Ok(engine)
}

/// Blocks to commit and what to publish once they are.
struct Batch {
  deltas: Vec<BlockDelta>,
  mined: Vec<u64>,
  fast_sync_complete: bool,
  target: Option<u32>,
  supply: SupplySnapshot,
  supply_seconds: f64,
  utxos: u64,
  pieces: u64,
  /// Time spent applying these blocks since the previous commit.
  applied_seconds: f64,
}

impl Batch {
  fn commit(self, store: &Store, published: &Published) -> anyhow::Result<CommitStats> {
    let start = Instant::now();
    let stats = store.commit_with(
      &self.deltas,
      &self.mined,
      self.fast_sync_complete,
      &CommitOptions {
        durable: true,
        target_height: self.target,
      },
    )?;
    let seconds = start.elapsed().as_secs_f64();

    let last = self.deltas.last();
    let blocks = self.deltas.len() as u64;
    let now = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map(|duration| duration.as_secs())
      .unwrap_or_default();

    if let Some(last) = last {
      log::debug!(
        "committed {blocks} blocks to {} in {seconds:.2}s ({} utxo, {} locator, {} script \
         writes, {} moves)",
        last.height,
        stats.utxo_writes,
        stats.locator_writes,
        stats.script_writes,
        stats.moves
      );
    }

    published.update_status(|status| {
      if let Some(last) = last {
        status.height = Some(last.height);
        status.hash = Some(last.hash);
        status.time = Some(last.header.time);
        status.lost_sats = last.lost_sats_after;
      }
      status.fast_sync_complete = self.fast_sync_complete;
      status.tracked_utxos = self.utxos;
      status.tracked_pieces = self.pieces;
      status.error = None;
      let timings = &mut status.timings;
      timings.blocks_per_second = if self.applied_seconds > 0.0 {
        blocks as f64 / self.applied_seconds
      } else {
        0.0
      };
      timings.last_commit_blocks = blocks;
      timings.last_commit_seconds = seconds;
      timings.last_commit_at = Some(now);
      timings.commits += 1;
      timings.commit_seconds_total += seconds;
      timings.supply_seconds = self.supply_seconds;
    });
    published.set_supply(self.supply);

    Ok(stats)
  }
}

/// Commits fast-sync batches on a background thread while the tracker
/// applies the next blocks. At most one batch waits while another commits.
struct Committer {
  sender: Option<SyncSender<Batch>>,
  thread: Option<JoinHandle<anyhow::Result<()>>>,
}

impl Committer {
  fn spawn(store: Arc<Store>, published: Arc<Published>) -> Self {
    let (sender, receiver) = sync_channel::<Batch>(1);
    let thread = thread::Builder::new()
      .name("commit".into())
      .spawn(move || -> anyhow::Result<()> {
        for batch in receiver {
          batch.commit(&store, &published)?;
        }
        Ok(())
      })
      .expect("failed to spawn the commit thread");
    Self {
      sender: Some(sender),
      thread: Some(thread),
    }
  }

  fn submit(&self, batch: Batch) -> anyhow::Result<()> {
    self
      .sender
      .as_ref()
      .expect("committer finished")
      .send(batch)
      .map_err(|_| anyhow!("the commit thread stopped"))
  }

  /// Waits for every submitted batch; the first commit error, if any.
  fn finish(mut self) -> anyhow::Result<()> {
    self.sender.take();
    self
      .thread
      .take()
      .expect("committer finished")
      .join()
      .map_err(|_| anyhow!("the commit thread panicked"))?
  }
}

impl Drop for Committer {
  fn drop(&mut self) {
    self.sender.take();
    if let Some(thread) = self.thread.take() {
      let _ = thread.join();
    }
  }
}

#[cfg(test)]
pub(crate) mod tests;
