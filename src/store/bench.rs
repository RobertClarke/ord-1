//! Commit throughput benchmark on synthetic fast-sync-sized batches.
//!
//! `cargo test --release --lib store::bench -- --ignored --nocapture`, tuned
//! by `BENCH_*` environment variables (see `Params`).

use {
  super::{tests::test_header, *},
  crate::model::{MoveInput, MoveOutput, RowDelta},
  bitcoin::{ScriptBuf, Txid, hashes::Hash},
  std::env,
};

struct Params {
  /// Tracked UTXOs in the index before measuring.
  prefill: usize,
  /// Blocks per measured commit.
  blocks: usize,
  /// Measured commits.
  batches: usize,
  /// Transactions moving tracked sats per block.
  txs: usize,
  retention: MovesRetention,
  durable: bool,
  cache_mib: usize,
  dir: Option<String>,
}

fn param<T: std::str::FromStr>(name: &str, default: T) -> T {
  env::var(name)
    .ok()
    .and_then(|value| value.parse().ok())
    .unwrap_or(default)
}

impl Params {
  fn from_env() -> Self {
    Self {
      prefill: param("BENCH_PREFILL", 3_000_000),
      blocks: param("BENCH_BLOCKS", 5_000),
      batches: param("BENCH_BATCHES", 3),
      txs: param("BENCH_TXS", 60),
      retention: param("BENCH_RETENTION", MovesRetention::All),
      durable: param("BENCH_DURABLE", true),
      cache_mib: param("BENCH_CACHE_MIB", 4096),
      dir: env::var("BENCH_DIR").ok(),
    }
  }
}

struct Rng(u64);

impl Rng {
  fn next(&mut self) -> u64 {
    self.0 ^= self.0 >> 12;
    self.0 ^= self.0 << 25;
    self.0 ^= self.0 >> 27;
    self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
  }

  fn below(&mut self, n: usize) -> usize {
    (self.next() % n as u64) as usize
  }
}

struct Generator {
  rng: Rng,
  height: u32,
  hash: Option<BlockHash>,
  live: Vec<(OutPoint, TrackedUtxo)>,
  next_sat: u64,
  next_txid: u64,
  scripts: Vec<ScriptBuf>,
  stats: Vec<RowStats>,
}

impl Generator {
  fn new() -> Self {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let scripts = (0..400_000)
      .map(|i| {
        let mut bytes = Vec::with_capacity(34);
        if i % 3 == 0 {
          bytes.extend([0x51, 0x20]);
          for _ in 0..4 {
            bytes.extend(rng.next().to_le_bytes());
          }
        } else {
          bytes.extend([0x00, 0x14]);
          for _ in 0..3 {
            bytes.extend(rng.next().to_le_bytes());
          }
          bytes.truncate(22);
        }
        ScriptBuf::from_bytes(bytes)
      })
      .collect();
    Self {
      rng,
      height: 0,
      hash: None,
      live: Vec::new(),
      next_sat: 0,
      next_txid: 0,
      scripts,
      stats: vec![RowStats::default(); 70],
    }
  }

  fn txid(&mut self) -> Txid {
    self.next_txid += 1;
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&self.rng.next().to_le_bytes());
    bytes[8..16].copy_from_slice(&self.next_txid.to_le_bytes());
    Txid::from_byte_array(bytes)
  }

  fn script(&mut self) -> ScriptBuf {
    let i = self.rng.below(self.scripts.len());
    self.scripts[i].clone()
  }

  fn sat(&mut self) -> u64 {
    self.next_sat += 1 + (self.rng.next() % 1_000_000_000);
    self.next_sat
  }

  fn header(&mut self) -> (u32, BlockHash, Header) {
    let height = self.height;
    let header = test_header(self.hash, height);
    let hash = header.block_hash();
    self.height += 1;
    self.hash = Some(hash);
    (height, hash, header)
  }

  /// One block creating `n` tracked UTXOs and spending none.
  fn prefill_block(&mut self, n: usize) -> BlockDelta {
    let (height, hash, header) = self.header();
    let mut created = Vec::with_capacity(n);
    for i in 0..n {
      let outpoint = OutPoint {
        txid: self.txid(),
        vout: (i % 3) as u32,
      };
      let utxo = TrackedUtxo {
        value: 546 + self.rng.next() % 100_000_000,
        script_pubkey: self.script(),
        height,
        kind: UtxoKind::Spendable,
        pieces: vec![Piece {
          offset: self.rng.next() % 546,
          start: self.sat(),
          len: 1,
        }],
      };
      created.push((outpoint, utxo.clone()));
      self.live.push((outpoint, utxo));
    }
    BlockDelta {
      height,
      hash,
      header,
      moves: Vec::new(),
      spent: Vec::new(),
      created,
      overwritten: Vec::new(),
      lost: Vec::new(),
      lost_sats_before: 0,
      lost_sats_after: 0,
      rows: Vec::new(),
    }
  }

  fn pick(&mut self) -> Option<(OutPoint, TrackedUtxo)> {
    if self.live.is_empty() {
      return None;
    }
    // Recently created outputs are spent far more often.
    let n = self.live.len();
    let i = if self.rng.below(10) < 7 {
      n - 1 - self.rng.below(n.min(20_000))
    } else {
      self.rng.below(n)
    };
    Some(self.live.swap_remove(i))
  }

  fn block(&mut self, txs: usize) -> BlockDelta {
    let (height, hash, header) = self.header();
    let mut spent = Vec::new();
    let mut created = Vec::new();
    let mut moves = Vec::new();

    for tx_index in 1..=txs {
      let mut inputs = Vec::new();
      for _ in 0..1 + self.rng.below(2) {
        if let Some(input) = self.pick() {
          inputs.push(input);
        }
      }
      if inputs.is_empty() {
        break;
      }
      let input_value = inputs.iter().map(|(_, utxo)| utxo.value).sum::<u64>();
      let mut pieces = Vec::new();
      let mut base = 0;
      for (_, utxo) in &inputs {
        for piece in &utxo.pieces {
          pieces.push(Piece {
            offset: base + piece.offset,
            ..*piece
          });
        }
        base += utxo.value;
      }
      // First output takes the tracked sats (value covers them), the rest
      // is change without tracked sats.
      let first_value = pieces.last().unwrap().offset_end().max(546);
      let txid = self.txid();
      let script_pubkey = self.script();
      let utxo = TrackedUtxo {
        value: first_value,
        script_pubkey: script_pubkey.clone(),
        height,
        kind: UtxoKind::Spendable,
        pieces: pieces.clone(),
      };
      let outpoint = OutPoint { txid, vout: 0 };
      created.push((outpoint, utxo.clone()));
      moves.push(Move {
        txid,
        tx_index: tx_index as u32,
        coinbase: false,
        input_value,
        output_value: input_value,
        inputs: inputs
          .iter()
          .enumerate()
          .map(|(vin, (outpoint, utxo))| MoveInput {
            vin: vin as u32,
            outpoint: *outpoint,
            utxo: utxo.clone(),
          })
          .collect(),
        outputs: vec![MoveOutput {
          vout: 0,
          value: first_value,
          script_pubkey,
          kind: UtxoKind::Spendable,
          pieces,
        }],
        fee: Vec::new(),
        mined: Vec::new(),
        lost: Vec::new(),
      });
      spent.extend(inputs);
    }

    // Coinbase: an uncommon and a black sat.
    let txid = self.txid();
    let script_pubkey = self.script();
    let first = self.sat();
    let pieces = vec![
      Piece {
        offset: 0,
        start: first,
        len: 1,
      },
      Piece {
        offset: 312_499_999,
        start: first + 312_499_999,
        len: 1,
      },
    ];
    self.next_sat += 312_500_000;
    let utxo = TrackedUtxo {
      value: 312_500_000,
      script_pubkey: script_pubkey.clone(),
      height,
      kind: UtxoKind::Spendable,
      pieces: pieces.clone(),
    };
    created.push((OutPoint { txid, vout: 0 }, utxo));
    moves.push(Move {
      txid,
      tx_index: 0,
      coinbase: true,
      input_value: 0,
      output_value: 312_500_000,
      inputs: Vec::new(),
      outputs: vec![MoveOutput {
        vout: 0,
        value: 312_500_000,
        script_pubkey,
        kind: UtxoKind::Spendable,
        pieces: pieces.clone(),
      }],
      fee: Vec::new(),
      mined: pieces,
      lost: Vec::new(),
    });

    let mut rows = Vec::new();
    for _ in 0..8 {
      let row = self.rng.below(self.stats.len());
      if rows
        .iter()
        .any(|delta: &RowDelta| usize::from(delta.row) == row)
      {
        continue;
      }
      let before = self.stats[row];
      self.stats[row].located += 1;
      self.stats[row].mined += 1;
      self.stats[row].holders = self.rng.next() % 1_000_000;
      rows.push(RowDelta {
        row: row as u16,
        before,
        after: self.stats[row],
      });
    }
    rows.sort_by_key(|delta| delta.row);

    self.live.extend(created.iter().cloned());

    BlockDelta {
      height,
      hash,
      header,
      moves,
      spent,
      created,
      overwritten: Vec::new(),
      lost: Vec::new(),
      lost_sats_before: 0,
      lost_sats_after: 0,
      rows,
    }
  }
}

#[test]
#[ignore = "benchmark"]
fn bench_commit() {
  let params = Params::from_env();
  let tempdir = tempfile::TempDir::new_in(params.dir.as_deref().unwrap_or(".")).unwrap();
  let store = Store::open_with(
    &tempdir.path().join("index.redb"),
    StoreOptions {
      cache_size: params.cache_mib << 20,
      moves_retention: params.retention,
      quick_repair: true,
    },
  )
  .unwrap();
  store
    .init(&IndexMeta {
      schema_version: SCHEMA_VERSION,
      chain: "regtest".into(),
      config: TrackerConfig::default(),
      catalog_fingerprint: "bench".into(),
      height: None,
      hash: None,
      fast_sync_complete: false,
    })
    .unwrap();

  let mut generator = Generator::new();
  let start = Instant::now();
  let mut remaining = params.prefill;
  while remaining > 0 {
    let n = remaining.min(500_000);
    let block = generator.prefill_block(n);
    store
      .commit_with(&[block], &[], false, &CommitOptions::default())
      .unwrap();
    remaining -= n;
  }
  store.commit(&[], &[], false).unwrap();
  eprintln!(
    "prefill: {} tracked UTXOs in {:.1}s",
    params.prefill,
    start.elapsed().as_secs_f64()
  );

  for batch in 0..params.batches {
    let start = Instant::now();
    let deltas = (0..params.blocks)
      .map(|_| generator.block(params.txs))
      .collect::<Vec<_>>();
    let generated = start.elapsed().as_secs_f64();

    let changes = deltas
      .iter()
      .map(|delta| delta.spent.len() + delta.created.len())
      .sum::<usize>();
    let moves = deltas.iter().map(|delta| delta.moves.len()).sum::<usize>();
    let inputs = deltas
      .iter()
      .flat_map(|delta| &delta.moves)
      .map(|m| m.inputs.len())
      .sum::<usize>();

    let start = Instant::now();
    let stats = store
      .commit_with(
        &deltas,
        &[],
        false,
        &CommitOptions {
          durable: params.durable,
          target_height: None,
        },
      )
      .unwrap();
    let elapsed = start.elapsed().as_secs_f64();

    eprintln!(
      "batch {batch}: {} blocks, {changes} utxo changes, {moves} moves ({inputs} inputs) \
       generated in {generated:.1}s; commit {elapsed:.2}s (net {:.2}s, write {:.2}s, fsync \
       {:.2}s) = {:.0} changes/s; writes: {} utxo, {} locator, {} script; moves {} B = {:.1} \
       B/move",
      params.blocks,
      stats.net_seconds,
      stats.write_seconds,
      stats.commit_seconds,
      changes as f64 / elapsed,
      stats.utxo_writes,
      stats.locator_writes,
      stats.script_writes,
      stats.moves_bytes,
      stats.moves_bytes as f64 / stats.moves.max(1) as f64,
    );
  }

  for table in store.table_info().unwrap() {
    eprintln!(
      "{:12} {:>10} entries {:>8.1} MiB stored {:>8.1} MiB metadata {:>8.1} MiB fragmented",
      table.name,
      table.entries,
      table.stored_bytes as f64 / (1 << 20) as f64,
      table.metadata_bytes as f64 / (1 << 20) as f64,
      table.fragmented_bytes as f64 / (1 << 20) as f64,
    );
  }
  eprintln!(
    "file: {:.1} MiB",
    std::fs::metadata(store.path()).unwrap().len() as f64 / (1 << 20) as f64
  );
}
