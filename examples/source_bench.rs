//! Mainnet benchmark and validation of the block sources.
//!
//! ```text
//! cargo run --release --example source_bench -- --from 0 --to 300000 \
//!   [--fetch-threads 16] [--buffer-blocks 256] [--utxo-capacity 200000000] \
//!   [--compare-live-every 1000] [--compare-heights 91722,91812,91842,91880]
//! cargo run --release --example source_bench -- --live-only --from 800000 --to 800100
//! ```
//!
//! Runs the fast source from genesis to `--to`, reporting blocks/s, MB/s,
//! UTXO map entries and process RSS every `--report-secs`. Blocks at heights
//! ≥ `--from` that are multiples of `--compare-live-every` (or listed in
//! `--compare-heights`), and the last block, are fetched again with the live
//! source (`getblock <hash> 3`) and must be identical. `--live-only`
//! benchmarks `LiveSource::blocks(from, to)` instead.
//!
//! Connects with `ORD_BITCOIN_RPC_URL` and `ORD_BITCOIN_RPC_USERNAME` /
//! `ORD_BITCOIN_RPC_PASSWORD`, or `--cookie-file`.

use {
  anyhow::bail,
  clap::Parser,
  crossbeam_channel::bounded,
  ord::{
    model::ResolvedBlock,
    source::{FastSource, FastSyncOptions, LiveSource, Rpc, RpcAuth, RpcConfig, decode_block},
  },
  std::{
    fs,
    path::PathBuf,
    sync::{
      Arc,
      atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
  },
};

#[derive(Parser)]
struct Args {
  /// First height compared with the live source (`--live-only`: first
  /// height fetched). The fast source always starts at genesis.
  #[arg(long, default_value_t = 0)]
  from: u32,
  /// Last height.
  #[arg(long)]
  to: u32,
  #[arg(long, default_value_t = 16)]
  fetch_threads: usize,
  #[arg(long, default_value_t = 256)]
  buffer_blocks: usize,
  #[arg(long, default_value_t = 200_000_000)]
  utxo_capacity: usize,
  /// Compare every K-th block with the live source.
  #[arg(long)]
  compare_live_every: Option<u32>,
  /// Extra heights to compare.
  #[arg(long, value_delimiter = ',')]
  compare_heights: Vec<u32>,
  /// Threads fetching live blocks (comparisons, `--live-only`).
  #[arg(long, default_value_t = 4)]
  live_threads: usize,
  /// Benchmark the live source over `--from..=--to` instead.
  #[arg(long)]
  live_only: bool,
  /// Benchmark fetching and decoding `--from..=--to` with `getblock <hash> 0`
  /// on `--fetch-threads` threads (no UTXO map), e.g. to pick fetch_threads.
  #[arg(long)]
  fetch_only: bool,
  /// Like `--fetch-only`, but decode every block twice with the fast
  /// source's own preparation, without and with reading sighash flags and
  /// envelopes, and report the CPU cost of the latter.
  #[arg(long)]
  prepare_only: bool,
  #[arg(long, default_value_t = 10)]
  report_secs: u64,
  #[arg(long, env = "ORD_BITCOIN_RPC_URL")]
  rpc_url: String,
  #[arg(long, env = "ORD_BITCOIN_RPC_USERNAME")]
  rpc_username: Option<String>,
  #[arg(long, env = "ORD_BITCOIN_RPC_PASSWORD", hide_env_values = true)]
  rpc_password: Option<String>,
  #[arg(long)]
  cookie_file: Option<PathBuf>,
  #[arg(long, default_value_t = 120)]
  timeout_secs: u64,
}

fn main() -> anyhow::Result<()> {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

  let args = Args::parse();

  let auth = match (&args.cookie_file, &args.rpc_username, &args.rpc_password) {
    (Some(path), _, _) => RpcAuth::Cookie(path.clone()),
    (None, Some(username), Some(password)) => RpcAuth::UserPass {
      username: username.clone(),
      password: password.clone(),
    },
    (None, None, None) => RpcAuth::None,
    _ => bail!("give both ORD_BITCOIN_RPC_USERNAME and ORD_BITCOIN_RPC_PASSWORD, or --cookie-file"),
  };

  let rpc = Rpc::new(RpcConfig {
    url: args.rpc_url.clone(),
    auth,
    timeout_secs: args.timeout_secs,
  })?;

  println!("chain {}, tip {}", rpc.chain()?, rpc.block_count()?);

  if args.live_only {
    live_only(&args, rpc)
  } else if args.fetch_only {
    fetch_only(&args, rpc)
  } else if args.prepare_only {
    prepare_only(&args, rpc)
  } else {
    fast(&args, rpc)
  }
}

fn fast(args: &Args, rpc: Rpc) -> anyhow::Result<()> {
  let start = Instant::now();

  let mut source = FastSource::start(
    rpc.clone(),
    FastSyncOptions {
      stop_height: args.to,
      fetch_threads: args.fetch_threads,
      buffer_blocks: args.buffer_blocks,
      utxo_capacity: args.utxo_capacity,
    },
  )?;

  let compared = Arc::new(AtomicU64::new(0));
  let (compare_tx, compare_rx) = bounded::<ResolvedBlock>(2 * args.live_threads);
  let comparers = (0..args.live_threads)
    .map(|_| {
      let live = LiveSource::new(rpc.clone());
      let blocks = compare_rx.clone();
      let compared = compared.clone();
      thread::spawn(move || -> anyhow::Result<()> {
        for fast in blocks {
          let live = live.block(fast.hash, fast.height)?;
          if let Some(difference) = difference(&fast, &live) {
            bail!(
              "block {} differs between the sources: {difference}",
              fast.height
            );
          }
          compared.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
      })
    })
    .collect::<Vec<_>>();
  drop(compare_rx);

  let compare = |height: u32| {
    height >= args.from
      && (height == args.to
        || args.compare_heights.contains(&height)
        || args
          .compare_live_every
          .is_some_and(|every| every > 0 && height.is_multiple_of(every)))
  };

  let mut totals = Totals::default();
  let mut interval = Totals::default();
  let mut consumer_wait = Duration::ZERO;
  let mut last_report = Instant::now();
  let report_every = Duration::from_secs(args.report_secs);

  loop {
    let wait = Instant::now();
    let Some(block) = source.next_block() else {
      break;
    };
    consumer_wait += wait.elapsed();
    let block = block?;

    totals.add(&block);
    interval.add(&block);

    if compare(block.height) && compare_tx.send(block).is_err() {
      // Every comparer stopped: report why.
      for comparer in comparers {
        comparer.join().unwrap()?;
      }
      bail!("live comparers stopped");
    }

    if last_report.elapsed() >= report_every {
      let stats = source.stats();
      let diagnostics = source.diagnostics();
      let secs = last_report.elapsed().as_secs_f64();
      let bytes = stats.bytes_fetched;
      println!(
        "{:>7.0}s height {:>7} | {:>6.0} blocks/s {:>6.1} MB/s (avg {:>6.1}) | {:>11} utxos {:>9} tombstones | rss {:>5.2} GiB (peak {:>5.2}) | ready {:>3} | resolve {:>4.0}% wait {:>4.0}% | consumer wait {:>4.0}% | fetch {:.0}s prepare {:.0}s | compared {}",
        start.elapsed().as_secs_f64(),
        block_height(&stats.height),
        interval.blocks as f64 / secs,
        (bytes - interval.bytes_mark) as f64 / secs / 1e6,
        bytes as f64 / start.elapsed().as_secs_f64() / 1e6,
        stats.utxos,
        diagnostics.map_tombstones,
        rss_gib("VmRSS:"),
        rss_gib("VmHWM:"),
        diagnostics.ready_blocks,
        100.0 * (diagnostics.resolve_secs - interval.resolve_mark) / secs,
        100.0 * (diagnostics.resolve_wait_secs - interval.resolve_wait_mark) / secs,
        100.0 * (consumer_wait.as_secs_f64() - interval.consumer_wait_mark) / secs,
        diagnostics.fetch_secs,
        diagnostics.prepare_secs,
        compared.load(Ordering::Relaxed),
      );
      interval = Totals {
        bytes_mark: bytes,
        resolve_mark: diagnostics.resolve_secs,
        resolve_wait_mark: diagnostics.resolve_wait_secs,
        consumer_wait_mark: consumer_wait.as_secs_f64(),
        ..Totals::default()
      };
      last_report = Instant::now();
    }
  }

  let stats = source.stats();
  let diagnostics = source.diagnostics();
  let elapsed = start.elapsed();

  drop(compare_tx);
  for comparer in comparers {
    comparer.join().unwrap()?;
  }

  println!("done: {stats:?}");
  println!("{diagnostics:#?}");
  println!(
    "{} blocks, {} txs, {} inputs, {} outputs in {:.1}s: {:.0} blocks/s, {:.1} MB/s raw; consumer waited {:.1}s; peak RSS {:.2} GiB; {} blocks identical to the live source",
    totals.blocks,
    totals.txs,
    totals.inputs,
    totals.outputs,
    elapsed.as_secs_f64(),
    totals.blocks as f64 / elapsed.as_secs_f64(),
    stats.bytes_fetched as f64 / elapsed.as_secs_f64() / 1e6,
    consumer_wait.as_secs_f64(),
    rss_gib("VmHWM:"),
    compared.load(Ordering::Relaxed),
  );

  let drop_start = Instant::now();
  drop(source);
  println!("dropped the fast source in {:?}", drop_start.elapsed());

  Ok(())
}

fn fetch_only(args: &Args, rpc: Rpc) -> anyhow::Result<()> {
  let mut hashes = Vec::new();
  let mut height = args.from;
  while height <= args.to {
    let end = args.to.min(height.saturating_add(1999));
    hashes.extend(rpc.block_hashes(height..end + 1)?);
    height = end + 1;
  }

  let start = Instant::now();
  let next = Arc::new(AtomicU64::new(0));
  let bytes = Arc::new(AtomicU64::new(0));
  let hashes = Arc::new(hashes);

  let workers = (0..args.fetch_threads)
    .map(|_| {
      let (rpc, next, bytes, hashes) = (rpc.clone(), next.clone(), bytes.clone(), hashes.clone());
      thread::spawn(move || -> anyhow::Result<()> {
        loop {
          let i = next.fetch_add(1, Ordering::Relaxed) as usize;
          let Some(hash) = hashes.get(i) else {
            return Ok(());
          };
          let raw = rpc.block_raw(*hash)?;
          bytes.fetch_add(raw.len() as u64, Ordering::Relaxed);
          let block = bitcoin::consensus::deserialize::<bitcoin::Block>(&raw)?;
          for tx in &block.txdata {
            std::hint::black_box(tx.compute_txid());
          }
        }
      })
    })
    .collect::<Vec<_>>();

  let mut last = (Instant::now(), 0);
  while workers.iter().any(|worker| !worker.is_finished()) {
    thread::sleep(Duration::from_millis(100));
    if last.0.elapsed() >= Duration::from_secs(args.report_secs) {
      let now = bytes.load(Ordering::Relaxed);
      println!(
        "{:>6.0}s {:>7} blocks {:>6.1} MB/s",
        start.elapsed().as_secs_f64(),
        next.load(Ordering::Relaxed).min(hashes.len() as u64),
        (now - last.1) as f64 / last.0.elapsed().as_secs_f64() / 1e6
      );
      last = (Instant::now(), now);
    }
  }
  for worker in workers {
    worker.join().unwrap()?;
  }

  let elapsed = start.elapsed().as_secs_f64();
  println!(
    "fetched {} blocks, {:.2} GB raw in {elapsed:.1}s with {} threads: {:.1} blocks/s, {:.1} MB/s raw",
    hashes.len(),
    bytes.load(Ordering::Relaxed) as f64 / 1e9,
    args.fetch_threads,
    hashes.len() as f64 / elapsed,
    bytes.load(Ordering::Relaxed) as f64 / elapsed / 1e6,
  );
  Ok(())
}

fn prepare_only(args: &Args, rpc: Rpc) -> anyhow::Result<()> {
  let mut hashes = Vec::new();
  let mut height = args.from;
  while height <= args.to {
    let end = args.to.min(height.saturating_add(1999));
    hashes.extend(rpc.block_hashes(height..end + 1)?);
    height = end + 1;
  }

  #[derive(Default)]
  struct Sums {
    blocks: u64,
    bytes: u64,
    inputs: u64,
    signed: u64,
    envelopes: u64,
    fetch: Duration,
    without: Duration,
    with: Duration,
  }

  let start = Instant::now();
  let next = Arc::new(AtomicU64::new(0));
  let hashes = Arc::new(hashes);
  let workers = (0..args.fetch_threads)
    .map(|_| {
      let (rpc, next, hashes) = (rpc.clone(), next.clone(), hashes.clone());
      let from = args.from;
      thread::spawn(move || -> anyhow::Result<Sums> {
        let mut sums = Sums::default();
        loop {
          let i = next.fetch_add(1, Ordering::Relaxed) as usize;
          let Some(&hash) = hashes.get(i) else {
            return Ok(sums);
          };
          let height = from + i as u32;

          let t = Instant::now();
          let raw = rpc.block_raw(hash)?;
          sums.fetch += t.elapsed();

          // Alternate the order so neither variant always runs warm.
          let timed = |witness: bool| -> anyhow::Result<(Duration, ResolvedBlock)> {
            let t = Instant::now();
            let block = decode_block(height, hash, &raw, witness)?;
            Ok((t.elapsed(), block))
          };
          let (without, with, block) = if i.is_multiple_of(2) {
            let (without, _) = timed(false)?;
            let (with, block) = timed(true)?;
            (without, with, block)
          } else {
            let (with, block) = timed(true)?;
            let (without, _) = timed(false)?;
            (without, with, block)
          };

          sums.blocks += 1;
          sums.bytes += raw.len() as u64;
          sums.without += without;
          sums.with += with;
          for tx in &block.txs {
            sums.inputs += tx.inputs.len() as u64;
            for input in &tx.inputs {
              sums.signed += u64::from(input.sighash.is_some());
              sums.envelopes += input.envelopes.len() as u64;
            }
          }
        }
      })
    })
    .collect::<Vec<_>>();

  let mut total = Sums::default();
  for worker in workers {
    let sums = worker.join().unwrap()?;
    total.blocks += sums.blocks;
    total.bytes += sums.bytes;
    total.inputs += sums.inputs;
    total.signed += sums.signed;
    total.envelopes += sums.envelopes;
    total.fetch += sums.fetch;
    total.without += sums.without;
    total.with += sums.with;
  }

  let elapsed = start.elapsed().as_secs_f64();
  let per_block = |duration: Duration| duration.as_secs_f64() * 1e3 / total.blocks as f64;
  println!(
    "{} blocks {}..={}, {:.2} GB raw in {elapsed:.1}s with {} threads ({:.1} blocks/s, {:.1} MB/s raw)",
    total.blocks,
    args.from,
    args.to,
    total.bytes as f64 / 1e9,
    args.fetch_threads,
    total.blocks as f64 / elapsed,
    total.bytes as f64 / elapsed / 1e6,
  );
  println!(
    "per block: fetch {:.2} ms, prepare without witness {:.3} ms, with {:.3} ms (+{:.3} ms, {:+.1}%)",
    per_block(total.fetch),
    per_block(total.without),
    per_block(total.with),
    per_block(total.with) - per_block(total.without),
    100.0 * (total.with.as_secs_f64() / total.without.as_secs_f64() - 1.0),
  );
  println!(
    "{} inputs, {} with a sighash flag ({:.1}%), {} envelopes; witness reading costs {:.1} ns per input",
    total.inputs,
    total.signed,
    100.0 * total.signed as f64 / total.inputs as f64,
    total.envelopes,
    (total.with.as_secs_f64() - total.without.as_secs_f64()) * 1e9 / total.inputs as f64,
  );
  Ok(())
}

fn live_only(args: &Args, rpc: Rpc) -> anyhow::Result<()> {
  let start = Instant::now();
  let live = LiveSource::new(rpc);
  let mut totals = Totals::default();
  let mut last_report = Instant::now();
  for block in live.blocks(args.from, args.to, args.live_threads) {
    let block = block?;
    totals.add(&block);
    if last_report.elapsed() >= Duration::from_secs(args.report_secs) {
      println!(
        "{:>7.0}s height {:>7} | {:>6.1} blocks/s | rss {:.2} GiB",
        start.elapsed().as_secs_f64(),
        block.height,
        totals.blocks as f64 / start.elapsed().as_secs_f64(),
        rss_gib("VmRSS:"),
      );
      last_report = Instant::now();
    }
  }
  println!(
    "{} blocks, {} txs, {} inputs, {} outputs in {:.1}s: {:.1} blocks/s with {} threads",
    totals.blocks,
    totals.txs,
    totals.inputs,
    totals.outputs,
    start.elapsed().as_secs_f64(),
    totals.blocks as f64 / start.elapsed().as_secs_f64(),
    args.live_threads,
  );
  Ok(())
}

#[derive(Default)]
struct Totals {
  blocks: u64,
  txs: u64,
  inputs: u64,
  outputs: u64,
  bytes_mark: u64,
  resolve_mark: f64,
  resolve_wait_mark: f64,
  consumer_wait_mark: f64,
}

impl Totals {
  fn add(&mut self, block: &ResolvedBlock) {
    self.blocks += 1;
    self.txs += block.txs.len() as u64;
    for tx in &block.txs {
      self.inputs += tx.inputs.len() as u64;
      self.outputs += tx.outputs.len() as u64;
    }
  }
}

fn block_height(height: &Option<u32>) -> String {
  height.map(|height| height.to_string()).unwrap_or_default()
}

/// The first difference between two resolved blocks, if any.
fn difference(fast: &ResolvedBlock, live: &ResolvedBlock) -> Option<String> {
  if fast == live {
    return None;
  }
  if (fast.height, fast.hash, fast.header) != (live.height, live.hash, live.header) {
    return Some(format!(
      "headers differ: fast {:?} live {:?}",
      (fast.height, fast.hash, fast.header),
      (live.height, live.hash, live.header)
    ));
  }
  if fast.txs.len() != live.txs.len() {
    return Some(format!(
      "{} transactions (fast) vs {} (live)",
      fast.txs.len(),
      live.txs.len()
    ));
  }
  for (index, (fast, live)) in fast.txs.iter().zip(&live.txs).enumerate() {
    if fast != live {
      return Some(format!(
        "transaction {index} differs:\nfast {fast:?}\nlive {live:?}"
      ));
    }
  }
  Some("blocks differ".into())
}

fn rss_gib(field: &str) -> f64 {
  fs::read_to_string("/proc/self/status")
    .ok()
    .and_then(|status| {
      status.lines().find_map(|line| {
        line
          .strip_prefix(field)?
          .trim()
          .strip_suffix(" kB")?
          .trim()
          .parse::<f64>()
          .ok()
      })
    })
    .map_or(0.0, |kib| kib / (1024.0 * 1024.0))
}
