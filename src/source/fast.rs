//! The fast initial-sync pipeline (docs/DESIGN.md §6).
//!
//! ```text
//!  mapper ──(height, hash)──► N fetchers ──PreparedBlock──► resolver ──ResolvedBlock──► next_block()
//!  batched getblockhash       getblock <hash> 0, decode,    in height order:
//!                             txids, merkle check,          UTXO value map
//!                             intra-block spends
//! ```
//!
//! Every block holds a permit from the moment a fetcher claims its height
//! until `next_block` hands it to the consumer, so at most `buffer_blocks`
//! blocks are in flight, buffered for reordering or waiting for the
//! consumer. The fetchers do all the per-block work that parallelizes
//! (decoding, txids, resolving spends of outputs created in the same block);
//! the resolver only touches the UTXO map, on its own thread, so the
//! consumer's `next_block` is a channel receive.

use {
  super::{RetryPolicy, Rpc, enters_utxo_set, input_envelopes, input_sighash, utxo::UtxoMap},
  crate::model::{ResolvedBlock, ResolvedInput, ResolvedTx, script_hash},
  anyhow::{Context, anyhow, ensure},
  bitcoin::{
    Block, BlockHash, OutPoint, Transaction, TxMerkleNode, Txid, consensus, hashes::Hash,
    merkle_tree,
  },
  crossbeam_channel::{Receiver, Sender, bounded, select, unbounded},
  rustc_hash::FxHashMap,
  std::{
    collections::BTreeMap,
    panic::{self, AssertUnwindSafe},
    sync::{
      Arc,
      atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
  },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastSyncOptions {
  /// Last height to deliver (inclusive).
  pub stop_height: u32,
  /// Parallel `getblock` requests.
  pub fetch_threads: usize,
  /// Decoded blocks buffered ahead of the consumer.
  pub buffer_blocks: usize,
  /// Initial capacity of the UTXO value map (entries).
  pub utxo_capacity: usize,
}

impl FastSyncOptions {
  /// Defaults measured against a mainnet Core 31 node at ~1 ms RTT:
  ///
  /// * 16 fetch threads keep bitcoind busy (~330 MB/s of raw blocks, 660
  ///   MB/s of hex);
  /// * 256 buffered blocks smooth out the consumer (≤ ~0.75 GB);
  /// * 200 M entries sizes the UTXO map at 2^28 buckets (12.25 GiB, 48 + 1
  ///   bytes each), room for 234.9 M entries; mainnet peaks at ~190 M. Any
  ///   value from 117.5 M to 234.9 M gives the same table; above that it
  ///   doubles.
  pub fn recommended(stop_height: u32) -> Self {
    Self {
      stop_height,
      fetch_threads: 16,
      buffer_blocks: 256,
      utxo_capacity: 200_000_000,
    }
  }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FastSourceStats {
  /// Last height delivered by `next_block`.
  pub height: Option<u32>,
  /// UTXO map entries after that block.
  pub utxos: u64,
  /// Raw block bytes fetched so far (the RPC transfer is twice that, hex).
  pub bytes_fetched: u64,
}

/// Where the fast source spends its time, for tuning and benchmarks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FastSourceDiagnostics {
  /// Blocks fetched and prepared by the fetchers.
  pub blocks_fetched: u64,
  /// Seconds the fetchers spent in `getblock`, summed over threads.
  pub fetch_secs: f64,
  /// Seconds the fetchers spent decoding and preparing, summed over threads.
  pub prepare_secs: f64,
  /// Seconds the resolver spent applying blocks to the UTXO map.
  pub resolve_secs: f64,
  /// Seconds the resolver waited for the next block in height order.
  pub resolve_wait_secs: f64,
  /// UTXO map inserts and removes.
  pub map_ops: u64,
  /// Inputs resolved within their own block, without touching the map.
  pub intra_block_spends: u64,
  /// Outputs that replaced an unspent entry (BIP30 duplicate txids).
  pub overwrites: u64,
  /// Resolved blocks waiting for `next_block`.
  pub ready_blocks: usize,
  /// Size of the UTXO map's allocation.
  pub map_bytes: u64,
  /// Entries the UTXO map holds before a shard has to grow.
  pub map_capacity: u64,
  /// Buckets freed by removes that stay unusable for new entries until
  /// their shard is rebuilt (hashbrown tombstones).
  pub map_tombstones: u64,
  /// Shard rebuilds that cleared tombstones.
  pub map_compactions: u64,
  /// Seconds `next_block` waited for the pipeline.
  pub next_block_wait_secs: f64,
}

/// Genesis-to-`stop_height` pipeline: parallel raw-block fetch and decode,
/// in-order delivery, input values resolved from an in-memory UTXO value
/// map that sees every block. Dropping it stops its threads and frees the map.
pub struct FastSource {
  output: Receiver<anyhow::Result<Resolved>>,
  permits: Sender<()>,
  /// Set to stop the threads' RPC retries.
  cancel: Arc<AtomicBool>,
  /// Dropped to stop the threads.
  shutdown: Option<Sender<()>>,
  workers: Vec<JoinHandle<()>>,
  resolver: Option<JoinHandle<()>>,
  shared: Arc<Shared>,
  stop_height: u32,
  next_height: u32,
  finished: bool,
  utxos: u64,
}

struct Resolved {
  block: ResolvedBlock,
  utxos: u64,
}

/// A decoded block whose inputs spending earlier blocks still need values.
struct PreparedBlock {
  block: ResolvedBlock,
  /// UTXO map operations in block order.
  ops: Vec<Op>,
  intra_block_spends: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
  Insert {
    outpoint: OutPoint,
    value: u64,
    script_hash: u32,
  },
  Spend {
    outpoint: OutPoint,
    tx: u32,
    vin: u32,
  },
  /// An insert whose output is spent later in the same block.
  Cancelled,
}

struct Job {
  height: u32,
  hash: anyhow::Result<BlockHash>,
}

#[derive(Default)]
struct Shared {
  bytes_fetched: AtomicU64,
  blocks_fetched: AtomicU64,
  fetch_nanos: AtomicU64,
  prepare_nanos: AtomicU64,
  resolve_nanos: AtomicU64,
  resolve_wait_nanos: AtomicU64,
  map_ops: AtomicU64,
  intra_block_spends: AtomicU64,
  overwrites: AtomicU64,
  map_bytes: AtomicU64,
  map_capacity: AtomicU64,
  map_tombstones: AtomicU64,
  map_compactions: AtomicU64,
  next_block_wait_nanos: AtomicU64,
}

impl Shared {
  fn add(counter: &AtomicU64, value: u64) {
    counter.fetch_add(value, Ordering::Relaxed);
  }

  fn add_elapsed(counter: &AtomicU64, start: Instant) {
    Self::add(counter, start.elapsed().as_nanos() as u64);
  }

  fn secs(counter: &AtomicU64) -> f64 {
    counter.load(Ordering::Relaxed) as f64 / 1e9
  }
}

/// The client the pipeline uses: retrying transient failures until it is
/// cancelled, by `cancel` or by the caller's own flags.
fn pipeline_rpc(rpc: &Rpc, cancel: Arc<AtomicBool>) -> Rpc {
  rpc.with_retries(RetryPolicy::forever()).with_cancel(cancel)
}

/// Heights per `getblockhash` batch.
const HASH_BATCH: u32 = 2_000;

/// How long `drop` waits for fetchers stuck in a slow request.
const JOIN_DEADLINE: Duration = Duration::from_secs(2);

impl FastSource {
  /// Starts the pipeline. A failed fast sync cannot resume (the UTXO value
  /// map is lost), so its requests retry transient failures (connection
  /// errors, HTTP 5xx, RPC -28) with `RetryPolicy::forever()`: through any
  /// bitcoind restart or network outage, until the source is dropped or a
  /// cancel flag of `rpc` (`Rpc::with_cancel`) is set. Other errors end it.
  pub fn start(rpc: Rpc, options: FastSyncOptions) -> anyhow::Result<Self> {
    let cancel = Arc::new(AtomicBool::new(false));
    let rpc = pipeline_rpc(&rpc, cancel.clone());

    let tip = rpc.block_count()?;

    ensure!(
      options.stop_height <= tip,
      "fast sync stop height {} is above bitcoind's tip {tip}",
      options.stop_height
    );

    let fetch_threads = options.fetch_threads.max(1);
    let buffer_blocks = options.buffer_blocks.max(1);
    let shared = Arc::new(Shared::default());

    let (shutdown_tx, shutdown_rx) = bounded::<()>(0);
    let (jobs_tx, jobs_rx) = bounded::<Job>(2 * HASH_BATCH as usize);
    let (permits_tx, permits_rx) = bounded::<()>(buffer_blocks);
    for _ in 0..buffer_blocks {
      permits_tx.send(()).unwrap();
    }
    let (prepared_tx, prepared_rx) = unbounded::<(u32, anyhow::Result<PreparedBlock>)>();
    let (output_tx, output_rx) = unbounded::<anyhow::Result<Resolved>>();

    let mut workers = Vec::with_capacity(fetch_threads + 1);

    workers.push(spawn("fast-sync-hashes", {
      let rpc = rpc.clone();
      let shutdown = shutdown_rx.clone();
      move || map_heights(&rpc, options.stop_height, &jobs_tx, &shutdown)
    })?);

    for i in 0..fetch_threads {
      workers.push(spawn(&format!("fast-sync-fetch-{i}"), {
        let rpc = rpc.clone();
        let permits = permits_rx.clone();
        let jobs = jobs_rx.clone();
        let prepared = prepared_tx.clone();
        let shutdown = shutdown_rx.clone();
        let shared = shared.clone();
        move || fetch(&rpc, &permits, &jobs, &prepared, &shutdown, &shared)
      })?);
    }
    drop(prepared_tx);

    let resolver = spawn("fast-sync-resolve", {
      let shared = shared.clone();
      let capacity = options.utxo_capacity;
      let stop_height = options.stop_height;
      move || {
        Resolver::new(capacity, shared).run(stop_height, &prepared_rx, &output_tx, &shutdown_rx)
      }
    })?;

    Ok(Self {
      output: output_rx,
      permits: permits_tx,
      cancel,
      shutdown: Some(shutdown_tx),
      workers,
      resolver: Some(resolver),
      shared,
      stop_height: options.stop_height,
      next_height: 0,
      finished: false,
      utxos: 0,
    })
  }

  /// The next block in height order, `None` after `stop_height`.
  pub fn next_block(&mut self) -> Option<anyhow::Result<ResolvedBlock>> {
    if self.finished {
      return None;
    }

    let start = Instant::now();
    let received = self.output.recv();
    Shared::add_elapsed(&self.shared.next_block_wait_nanos, start);

    match received {
      Ok(Ok(Resolved { block, utxos })) => {
        // Never blocks: at most `buffer_blocks` permits exist.
        let _ = self.permits.try_send(());
        self.utxos = utxos;
        self.next_height = block.height + 1;
        if block.height == self.stop_height {
          self.finished = true;
          self.stop();
        }
        Some(Ok(block))
      }
      Ok(Err(error)) => {
        self.finished = true;
        self.stop();
        Some(Err(error))
      }
      Err(_) => {
        self.finished = true;
        let panic = self.stop();
        Some(Err(anyhow!(
          "fast sync pipeline stopped before block {}{}",
          self.next_height,
          panic.map(|panic| format!(": {panic}")).unwrap_or_default()
        )))
      }
    }
  }

  pub fn stats(&self) -> FastSourceStats {
    FastSourceStats {
      height: self.next_height.checked_sub(1),
      utxos: self.utxos,
      bytes_fetched: self.shared.bytes_fetched.load(Ordering::Relaxed),
    }
  }

  pub fn diagnostics(&self) -> FastSourceDiagnostics {
    let shared = &self.shared;
    FastSourceDiagnostics {
      blocks_fetched: shared.blocks_fetched.load(Ordering::Relaxed),
      fetch_secs: Shared::secs(&shared.fetch_nanos),
      prepare_secs: Shared::secs(&shared.prepare_nanos),
      resolve_secs: Shared::secs(&shared.resolve_nanos),
      resolve_wait_secs: Shared::secs(&shared.resolve_wait_nanos),
      map_ops: shared.map_ops.load(Ordering::Relaxed),
      intra_block_spends: shared.intra_block_spends.load(Ordering::Relaxed),
      overwrites: shared.overwrites.load(Ordering::Relaxed),
      ready_blocks: self.output.len(),
      map_bytes: shared.map_bytes.load(Ordering::Relaxed),
      map_capacity: shared.map_capacity.load(Ordering::Relaxed),
      map_tombstones: shared.map_tombstones.load(Ordering::Relaxed),
      map_compactions: shared.map_compactions.load(Ordering::Relaxed),
      next_block_wait_secs: Shared::secs(&shared.next_block_wait_nanos),
    }
  }

  /// Signals every thread to stop and joins them; the resolver (which owns
  /// the UTXO map) always, fetchers stuck in a slow request only up to a
  /// deadline. Returns the resolver's panic message, if it panicked.
  fn stop(&mut self) -> Option<String> {
    self.cancel.store(true, Ordering::Relaxed);
    self.shutdown.take();

    let panic = self.resolver.take().and_then(|resolver| {
      resolver.join().err().map(|payload| {
        payload
          .downcast_ref::<String>()
          .cloned()
          .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
          .unwrap_or_else(|| "resolver panicked".into())
      })
    });

    let deadline = Instant::now() + JOIN_DEADLINE;
    while self.workers.iter().any(|worker| !worker.is_finished()) && Instant::now() < deadline {
      thread::sleep(Duration::from_millis(1));
    }
    for worker in self.workers.drain(..) {
      if worker.is_finished() {
        let _ = worker.join();
      }
    }

    panic
  }
}

impl Drop for FastSource {
  fn drop(&mut self) {
    self.stop();
  }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> anyhow::Result<JoinHandle<()>> {
  thread::Builder::new()
    .name(name.into())
    .spawn(f)
    .with_context(|| format!("spawning thread {name}"))
}

/// Feeds `(height, hash)` jobs for heights `0..=stop_height`, in order.
fn map_heights(rpc: &Rpc, stop_height: u32, jobs: &Sender<Job>, shutdown: &Receiver<()>) {
  let end = u64::from(stop_height) + 1;
  let mut height = 0u64;
  while height < end {
    let batch_end = end.min(height + u64::from(HASH_BATCH));
    let (start, stop) = (height as u32, batch_end as u32);
    let jobs_batch = match rpc.block_hashes(start..stop) {
      Ok(hashes) => hashes
        .into_iter()
        .zip(start..)
        .map(|(hash, height)| Job {
          height,
          hash: Ok(hash),
        })
        .collect::<Vec<Job>>(),
      Err(error) => vec![Job {
        height: start,
        hash: Err(error.context(format!("mapping heights {start}..{stop} to block hashes"))),
      }],
    };
    for job in jobs_batch {
      let failed = job.hash.is_err();
      select! {
        send(jobs, job) -> result => if result.is_err() { return },
        recv(shutdown) -> _ => return,
      }
      if failed {
        return;
      }
    }
    height = batch_end;
  }
}

fn fetch(
  rpc: &Rpc,
  permits: &Receiver<()>,
  jobs: &Receiver<Job>,
  prepared: &Sender<(u32, anyhow::Result<PreparedBlock>)>,
  shutdown: &Receiver<()>,
  shared: &Shared,
) {
  let mut scratch = FxHashMap::default();
  loop {
    select! {
      recv(permits) -> permit => if permit.is_err() { return },
      recv(shutdown) -> _ => return,
    }

    let job = select! {
      recv(jobs) -> job => match job {
        Ok(job) => job,
        Err(_) => return,
      },
      recv(shutdown) -> _ => return,
    };

    let height = job.height;
    let result = job.hash.and_then(|hash| {
      let start = Instant::now();
      let raw = rpc
        .block_raw(hash)
        .with_context(|| format!("fetching block {height} ({hash})"))?;
      Shared::add_elapsed(&shared.fetch_nanos, start);
      Shared::add(&shared.bytes_fetched, raw.len() as u64);

      let start = Instant::now();
      let block = panic::catch_unwind(AssertUnwindSafe(|| {
        prepare(height, hash, &raw, &mut scratch)
      }))
      .unwrap_or_else(|_| Err(anyhow!("panicked while preparing block {height} ({hash})")))?;
      Shared::add_elapsed(&shared.prepare_nanos, start);
      Shared::add(&shared.blocks_fetched, 1);
      Ok(block)
    });

    if prepared.send((height, result)).is_err() {
      return;
    }
  }
}

/// Decodes a raw block, checks it against its hash, computes txids and
/// builds the block's UTXO map operations. Inputs spending outputs created
/// earlier in the same block are resolved here; their outputs never reach
/// the map. (Only coinbases can repeat a txid, and coinbase outputs cannot be
/// spent in their own block, so this matches applying every operation in
/// order.)
fn prepare(
  height: u32,
  hash: BlockHash,
  raw: &[u8],
  created: &mut FxHashMap<OutPoint, (u64, u32, usize)>,
) -> anyhow::Result<PreparedBlock> {
  prepare_with(height, hash, raw, created, true)
}

/// Decodes a raw block exactly like the fast source's fetchers, without the
/// UTXO value map: inputs spending outputs of earlier blocks keep value 0
/// and script hash 0.
/// `witness: false` skips reading sighash flags and envelopes. For
/// validation and benchmarks.
#[doc(hidden)]
pub fn decode_block(
  height: u32,
  hash: BlockHash,
  raw: &[u8],
  witness: bool,
) -> anyhow::Result<ResolvedBlock> {
  prepare_with(height, hash, raw, &mut FxHashMap::default(), witness).map(|prepared| prepared.block)
}

fn prepare_with(
  height: u32,
  hash: BlockHash,
  raw: &[u8],
  created: &mut FxHashMap<OutPoint, (u64, u32, usize)>,
  witness: bool,
) -> anyhow::Result<PreparedBlock> {
  let block = consensus::deserialize::<Block>(raw)
    .with_context(|| format!("decoding block {height} ({hash})"))?;

  ensure!(
    block.block_hash() == hash,
    "getblock {hash} (height {height}) returned block {}",
    block.block_hash()
  );

  let txids = block
    .txdata
    .iter()
    .map(Transaction::compute_txid)
    .collect::<Vec<Txid>>();

  ensure!(
    merkle_tree::calculate_root(txids.iter().map(|txid| txid.to_raw_hash()))
      .map(TxMerkleNode::from_raw_hash)
      == Some(block.header.merkle_root),
    "block {height} ({hash}): transactions do not match the header's merkle root"
  );

  ensure!(
    block.txdata[0].is_coinbase(),
    "block {height} ({hash}) does not start with a coinbase"
  );

  let (inputs, outputs) = block.txdata.iter().fold((0, 0), |(inputs, outputs), tx| {
    (inputs + tx.input.len(), outputs + tx.output.len())
  });

  created.clear();
  let mut ops = Vec::with_capacity(inputs + outputs);
  let mut intra_block_spends = 0;
  let mut txs = Vec::with_capacity(block.txdata.len());

  for (index, (tx, txid)) in block.txdata.into_iter().zip(txids).enumerate() {
    let inputs = if index == 0 {
      Vec::new()
    } else {
      tx.input
        .iter()
        .enumerate()
        .map(|(vin, input)| {
          let outpoint = input.previous_output;
          let (value, script_hash) = match created.remove(&outpoint) {
            Some((value, script_hash, op)) => {
              ops[op] = Op::Cancelled;
              intra_block_spends += 1;
              (value, script_hash)
            }
            None => {
              ops.push(Op::Spend {
                outpoint,
                tx: index as u32,
                vin: vin as u32,
              });
              (0, 0)
            }
          };
          ResolvedInput {
            previous_output: outpoint,
            value,
            script_hash,
            sighash: witness
              .then(|| input_sighash(&input.script_sig, &input.witness))
              .flatten(),
            envelopes: if witness {
              input_envelopes(&input.witness)
            } else {
              Vec::new()
            },
          }
        })
        .collect()
    };

    for (vout, output) in tx.output.iter().enumerate() {
      if enters_utxo_set(height, index, output) {
        let outpoint = OutPoint {
          txid,
          vout: vout as u32,
        };
        let value = output.value.to_sat();
        let script_hash = script_hash(&output.script_pubkey);
        created.insert(outpoint, (value, script_hash, ops.len()));
        ops.push(Op::Insert {
          outpoint,
          value,
          script_hash,
        });
      }
    }

    txs.push(ResolvedTx {
      txid,
      inputs,
      outputs: tx.output,
    });
  }

  if intra_block_spends > 0 {
    ops.retain(|op| *op != Op::Cancelled);
  }

  Ok(PreparedBlock {
    block: ResolvedBlock {
      height,
      hash,
      header: block.header,
      txs,
    },
    ops,
    intra_block_spends,
  })
}

/// The in-order stage: owns the UTXO value map.
struct Resolver {
  map: UtxoMap,
  warned_headroom: bool,
  shared: Arc<Shared>,
  prev_hash: BlockHash,
}

impl Resolver {
  fn new(capacity: usize, shared: Arc<Shared>) -> Self {
    let map = UtxoMap::with_capacity(capacity);
    shared
      .map_bytes
      .store(map.allocated_bytes(), Ordering::Relaxed);
    shared
      .map_capacity
      .store(map.capacity() as u64, Ordering::Relaxed);
    Self {
      warned_headroom: false,
      map,
      shared,
      prev_hash: BlockHash::all_zeros(),
    }
  }

  fn run(
    mut self,
    stop_height: u32,
    prepared: &Receiver<(u32, anyhow::Result<PreparedBlock>)>,
    output: &Sender<anyhow::Result<Resolved>>,
    shutdown: &Receiver<()>,
  ) {
    let mut pending = BTreeMap::new();
    let mut height = 0;

    loop {
      let block = match pending.remove(&height) {
        Some(block) => block,
        None => {
          let start = Instant::now();
          select! {
            recv(prepared) -> message => {
              Shared::add_elapsed(&self.shared.resolve_wait_nanos, start);
              match message {
                Ok((height, block)) => {
                  pending.insert(height, block);
                  continue;
                }
                Err(_) => {
                  let _ = output.send(Err(anyhow!("fast sync fetchers stopped before block {height}")));
                  return;
                }
              }
            },
            recv(shutdown) -> _ => return,
          }
        }
      };

      let start = Instant::now();
      let result = block.and_then(|block| self.apply(block));
      Shared::add_elapsed(&self.shared.resolve_nanos, start);

      match result {
        Ok(block) => {
          let resolved = Resolved {
            block,
            utxos: self.map.len() as u64,
          };
          if output.send(Ok(resolved)).is_err() {
            return;
          }
        }
        Err(error) => {
          let _ = output.send(Err(error));
          return;
        }
      }

      if height == stop_height {
        return;
      }
      height += 1;
    }
  }

  fn apply(&mut self, prepared: PreparedBlock) -> anyhow::Result<ResolvedBlock> {
    let PreparedBlock {
      mut block,
      ops,
      intra_block_spends,
    } = prepared;

    ensure!(
      block.header.prev_blockhash == self.prev_hash,
      "block {} ({}) does not build on {}: the chain changed during the fast sync",
      block.height,
      block.hash,
      self.prev_hash,
    );

    for op in &ops {
      match *op {
        Op::Insert {
          outpoint,
          value,
          script_hash,
        } => {
          if self.map.insert(outpoint, value, script_hash).is_some() {
            log::info!(
              "block {}: output {outpoint} replaces an unspent output with the same outpoint (BIP30)",
              block.height
            );
            Shared::add(&self.shared.overwrites, 1);
          }
        }
        Op::Spend { outpoint, tx, vin } => {
          let tx = &mut block.txs[tx as usize];
          let Some((value, script_hash)) = self.map.remove(&outpoint) else {
            return Err(anyhow!(
              "block {} ({}): input {vin} of transaction {} spends {outpoint}, which is not in the UTXO set",
              block.height,
              block.hash,
              tx.txid,
            ));
          };
          let input = &mut tx.inputs[vin as usize];
          input.value = value;
          input.script_hash = script_hash;
        }
        Op::Cancelled => unreachable!(),
      }
    }

    let maintenance = self.map.maintain();
    if maintenance.grown > 0 {
      log::warn!(
        "UTXO value map: {} shard(s) grew at block {} ({} entries); raise utxo_capacity",
        maintenance.grown,
        block.height,
        self.map.len(),
      );
      self
        .shared
        .map_bytes
        .store(self.map.allocated_bytes(), Ordering::Relaxed);
      self
        .shared
        .map_capacity
        .store(self.map.capacity() as u64, Ordering::Relaxed);
    }
    if maintenance.compacted > 0 {
      log::debug!(
        "UTXO value map: rebuilt {} shard(s) to clear tombstones at block {}",
        maintenance.compacted,
        block.height
      );
      Shared::add(&self.shared.map_compactions, maintenance.compacted as u64);
    }
    self
      .shared
      .map_tombstones
      .store(self.map.tombstones() as u64, Ordering::Relaxed);

    let headroom = self.map.capacity().saturating_sub(self.map.len());
    if !self.warned_headroom && headroom < self.map.capacity() / 20 {
      self.warned_headroom = true;
      log::warn!(
        "UTXO value map is nearly full at block {}: {} entries, room for {} more before shards grow; raise utxo_capacity",
        block.height,
        self.map.len(),
        headroom,
      );
    }

    Shared::add(&self.shared.map_ops, ops.len() as u64);
    Shared::add(&self.shared.intra_block_spends, intra_block_spends);
    self.prev_hash = block.hash;

    Ok(block)
  }
}

#[cfg(test)]
mod tests {
  use {
    super::*,
    bitcoin::{
      Amount, CompactTarget, ScriptBuf, Sequence, TxIn, TxOut, Witness,
      absolute::LockTime,
      block::{Header, Version},
      transaction,
    },
  };

  #[test]
  fn the_pipeline_retries_until_cancelled() {
    let caller = Arc::new(AtomicBool::new(false));
    let rpc = Rpc::new(crate::source::RpcConfig {
      url: "http://127.0.0.1:1".into(),
      auth: crate::source::RpcAuth::None,
      timeout_secs: 1,
    })
    .unwrap()
    .with_cancel(caller.clone());
    assert!(!rpc.retry_policy().is_forever());

    let own = Arc::new(AtomicBool::new(false));
    let pipeline = pipeline_rpc(&rpc, own.clone());
    assert!(pipeline.retry_policy().is_forever());

    // The caller's flag (the indexer's shutdown) still cancels it...
    caller.store(true, Ordering::Relaxed);
    let error = pipeline.block_count().unwrap_err();
    assert!(format!("{error:#}").contains("cancelled"), "{error:#}");

    // ...and so does the source's own.
    caller.store(false, Ordering::Relaxed);
    own.store(true, Ordering::Relaxed);
    let error = pipeline.block_count().unwrap_err();
    assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
  }

  fn coinbase(tag: u8, outputs: Vec<TxOut>) -> Transaction {
    Transaction {
      version: transaction::Version::ONE,
      lock_time: LockTime::ZERO,
      input: vec![TxIn {
        previous_output: OutPoint::null(),
        script_sig: ScriptBuf::from_bytes(vec![1, tag]),
        sequence: Sequence::MAX,
        witness: Witness::new(),
      }],
      output: outputs,
    }
  }

  fn spend(inputs: &[OutPoint], outputs: Vec<TxOut>) -> Transaction {
    Transaction {
      version: transaction::Version::TWO,
      lock_time: LockTime::ZERO,
      input: inputs
        .iter()
        .map(|outpoint| TxIn {
          previous_output: *outpoint,
          script_sig: ScriptBuf::new(),
          sequence: Sequence::MAX,
          witness: Witness::from_slice(&[[7u8; 20]]),
        })
        .collect(),
      output: outputs,
    }
  }

  /// An output whose script depends on its value, so script hashes differ.
  fn out(value: u64) -> TxOut {
    TxOut {
      value: Amount::from_sat(value),
      script_pubkey: ScriptBuf::from_bytes([&[0x51][..], &value.to_le_bytes()].concat()),
    }
  }

  /// Every input carries the script hash of the output it spends.
  fn check_script_hashes(blocks: &[ResolvedBlock]) {
    let mut outputs = std::collections::HashMap::new();
    for block in blocks {
      for tx in &block.txs {
        for input in &tx.inputs {
          let expected = outputs
            .remove(&input.previous_output)
            .expect("spent output exists");
          assert_eq!(input.script_hash, expected, "{}", input.previous_output);
        }
        for (vout, output) in tx.outputs.iter().enumerate() {
          outputs.insert(
            OutPoint::new(tx.txid, vout as u32),
            script_hash(&output.script_pubkey),
          );
        }
      }
    }
  }

  fn op_return(value: u64) -> TxOut {
    TxOut {
      value: Amount::from_sat(value),
      script_pubkey: ScriptBuf::from_bytes(vec![0x6a, 0x01, 0x00]),
    }
  }

  fn block(prev: BlockHash, txdata: Vec<Transaction>) -> Block {
    let mut block = Block {
      header: Header {
        version: Version::ONE,
        prev_blockhash: prev,
        merkle_root: TxMerkleNode::all_zeros(),
        time: 0,
        bits: CompactTarget::from_consensus(0x207fffff),
        nonce: 0,
      },
      txdata,
    };
    block.header.merkle_root = block.compute_merkle_root().unwrap();
    block
  }

  /// Prepares and resolves `blocks` as heights `0..`.
  fn resolve_chain(blocks: &[Block]) -> (Resolver, Vec<anyhow::Result<ResolvedBlock>>) {
    let mut resolver = Resolver::new(0, Arc::new(Shared::default()));
    let mut scratch = FxHashMap::default();
    let results = blocks
      .iter()
      .enumerate()
      .map(|(height, block)| {
        let raw = consensus::serialize(block);
        let prepared = prepare(height as u32, block.block_hash(), &raw, &mut scratch)?;
        resolver.apply(prepared)
      })
      .collect();
    (resolver, results)
  }

  fn value(block: &ResolvedBlock, tx: usize, vin: usize) -> u64 {
    block.txs[tx].inputs[vin].value
  }

  #[test]
  fn resolves_values_across_and_within_blocks() {
    let genesis = block(BlockHash::all_zeros(), vec![coinbase(0, vec![out(50)])]);
    let cb1 = coinbase(1, vec![out(40), out(0), op_return(10)]);
    let b1 = block(genesis.block_hash(), vec![cb1.clone()]);
    let cb1_id = cb1.compute_txid();

    let t1 = spend(
      &[OutPoint::new(cb1_id, 0), OutPoint::new(cb1_id, 1)],
      vec![out(30), out(9)],
    );
    let t1_id = t1.compute_txid();
    // Spends an output created earlier in the same block.
    let t2 = spend(&[OutPoint::new(t1_id, 1)], vec![out(8), op_return(0)]);
    let t2_id = t2.compute_txid();
    let cb2 = coinbase(2, vec![out(52)]);
    let b2 = block(b1.block_hash(), vec![cb2.clone(), t1, t2]);

    let t3 = spend(
      &[OutPoint::new(t2_id, 0), OutPoint::new(t1_id, 0)],
      vec![out(38)],
    );
    let b3 = block(b2.block_hash(), vec![coinbase(3, vec![out(50)]), t3]);

    let (resolver, results) = resolve_chain(&[genesis, b1, b2, b3]);
    let blocks = results
      .into_iter()
      .collect::<anyhow::Result<Vec<_>>>()
      .unwrap();
    check_script_hashes(&blocks);

    assert!(blocks[0].txs[0].inputs.is_empty());
    assert!(blocks[1].txs[0].inputs.is_empty());
    assert_eq!(blocks[1].txs[0].outputs.len(), 3);
    assert_eq!(value(&blocks[2], 1, 0), 40);
    assert_eq!(value(&blocks[2], 1, 1), 0);
    assert_eq!(value(&blocks[2], 2, 0), 9);
    assert_eq!(value(&blocks[3], 1, 0), 8);
    assert_eq!(value(&blocks[3], 1, 1), 30);

    // cb2:0 and cb3:0 and t3:0 remain; the genesis coinbase never entered.
    assert_eq!(resolver.map.len(), 3);
    assert_eq!(
      resolver.shared.intra_block_spends.load(Ordering::Relaxed),
      1
    );
    // Inserts: cb1 ×2; cb2, t1 ×2 (one cancelled), t2 ×1; cb3, t3.
    // Removes: t1 ×2, t3 ×2.
    assert_eq!(
      resolver.shared.map_ops.load(Ordering::Relaxed),
      2 + 3 + 2 + 2 + 2
    );
    assert_eq!(blocks[2].txs[2].outputs[1], op_return(0));
  }

  #[test]
  fn genesis_coinbase_is_not_spendable() {
    let cb0 = coinbase(0, vec![out(50)]);
    let genesis = block(BlockHash::all_zeros(), vec![cb0.clone()]);
    let b1 = block(
      genesis.block_hash(),
      vec![
        coinbase(1, vec![out(50)]),
        spend(&[OutPoint::new(cb0.compute_txid(), 0)], vec![out(50)]),
      ],
    );
    let (_, results) = resolve_chain(&[genesis, b1]);
    let error = results[1].as_ref().unwrap_err().to_string();
    assert!(error.contains("which is not in the UTXO set"), "{error}");
  }

  #[test]
  fn missing_input_is_a_hard_error_naming_everything() {
    let genesis = block(BlockHash::all_zeros(), vec![coinbase(0, vec![out(50)])]);
    let cb1 = coinbase(1, vec![out(50), op_return(0)]);
    let b1 = block(genesis.block_hash(), vec![cb1.clone()]);
    let missing = OutPoint::new(cb1.compute_txid(), 1);
    let bad = spend(
      &[OutPoint::new(cb1.compute_txid(), 0), missing],
      vec![out(1)],
    );
    let bad_id = bad.compute_txid();
    let b2 = block(b1.block_hash(), vec![coinbase(2, vec![out(50)]), bad]);
    let b2_hash = b2.block_hash();

    let (_, results) = resolve_chain(&[genesis, b1, b2]);
    assert!(results[1].is_ok());
    let error = results[2].as_ref().unwrap_err().to_string();
    assert_eq!(
      error,
      format!(
        "block 2 ({b2_hash}): input 1 of transaction {bad_id} spends {missing}, which is not in the UTXO set"
      )
    );
  }

  #[test]
  fn bip30_duplicate_coinbase_overwrites() {
    let genesis = block(BlockHash::all_zeros(), vec![coinbase(0, vec![out(50)])]);
    let dup = coinbase(7, vec![out(50), out(25)]);
    let b1 = block(genesis.block_hash(), vec![dup.clone()]);
    let b2 = block(b1.block_hash(), vec![coinbase(2, vec![out(50)])]);
    // Same coinbase transaction again: same txid, outputs overwrite.
    let b3 = block(b2.block_hash(), vec![dup.clone()]);
    let spender = spend(&[OutPoint::new(dup.compute_txid(), 1)], vec![out(25)]);
    let b4 = block(b3.block_hash(), vec![coinbase(4, vec![out(50)]), spender]);

    let (resolver, results) = resolve_chain(&[genesis, b1, b2, b3, b4]);
    let blocks = results
      .into_iter()
      .collect::<anyhow::Result<Vec<_>>>()
      .unwrap();
    check_script_hashes(&blocks);
    assert_eq!(resolver.shared.overwrites.load(Ordering::Relaxed), 2);
    assert_eq!(value(&blocks[4], 1, 0), 25);
    // dup:0, b2 cb, b4 cb, spender:0 — the overwritten entries are gone.
    assert_eq!(resolver.map.len(), 4);
  }

  #[test]
  fn broken_linkage_is_an_error() {
    let genesis = block(BlockHash::all_zeros(), vec![coinbase(0, vec![out(50)])]);
    let orphan = block(BlockHash::all_zeros(), vec![coinbase(1, vec![out(50)])]);
    let (_, results) = resolve_chain(&[genesis, orphan]);
    let error = results[1].as_ref().unwrap_err().to_string();
    assert!(
      error.contains("the chain changed during the fast sync"),
      "{error}"
    );
  }

  #[test]
  fn prepare_rejects_mismatched_hash_and_merkle_root() {
    let genesis = block(BlockHash::all_zeros(), vec![coinbase(0, vec![out(50)])]);
    let raw = consensus::serialize(&genesis);
    let mut scratch = FxHashMap::default();

    let error = prepare(0, BlockHash::all_zeros(), &raw, &mut scratch)
      .err()
      .unwrap()
      .to_string();
    assert!(error.contains("returned block"), "{error}");

    let mut tampered = genesis.clone();
    tampered.txdata[0].output[0].value = Amount::from_sat(51);
    let raw = consensus::serialize(&tampered);
    let error = prepare(0, tampered.block_hash(), &raw, &mut scratch)
      .err()
      .unwrap()
      .to_string();
    assert!(error.contains("merkle root"), "{error}");

    let error = prepare(0, genesis.block_hash(), &raw[..raw.len() - 1], &mut scratch)
      .err()
      .unwrap()
      .to_string();
    assert!(error.contains("decoding block 0"), "{error}");
  }
}
