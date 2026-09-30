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
    source::{FastSource, FastSyncOptions, LiveSource, Rpc, RpcAuth, RpcConfig},
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
        "{:>7.0}s height {:>7} | {:>6.0} blocks/s {:>6.1} MB/s (avg {:>6.1}) | {:>11} utxos | rss {:>5.2} GiB (peak {:>5.2}) | ready {:>3} | resolve {:>4.0}% wait {:>4.0}% | consumer wait {:>4.0}% | fetch {:.0}s prepare {:.0}s | compared {}",
        start.elapsed().as_secs_f64(),
        block_height(&stats.height),
        interval.blocks as f64 / secs,
        (bytes - interval.bytes_mark) as f64 / secs / 1e6,
        bytes as f64 / start.elapsed().as_secs_f64() / 1e6,
        stats.utxos,
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
