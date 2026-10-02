//! Throughput benchmark on mainnet-shaped synthetic blocks:
//!
//! ```sh
//! cargo test --release --lib tracker::tests::bench -- --ignored --nocapture
//! ```
//!
//! `BENCH_UTXOS` (default 1,600,000) tracked UTXOs are restored first so the
//! hash tables have mainnet-like sizes; then `BENCH_BLOCKS` (default 300)
//! blocks of `BENCH_TXS` (default 3,000) transactions with 2–3 inputs and
//! 2–3 outputs each, 1% of them spending a tracked UTXO, are applied.

use {super::*, std::time::Instant};

fn env(name: &str, default: u64) -> u64 {
  std::env::var(name)
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(default)
}

struct Rng(u64);

impl Rng {
  fn next(&mut self) -> u64 {
    self.0 ^= self.0 >> 12;
    self.0 ^= self.0 << 25;
    self.0 ^= self.0 >> 27;
    self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
  }

  fn below(&mut self, n: u64) -> u64 {
    self.next() % n
  }
}

fn random_txid(rng: &mut Rng) -> Txid {
  let mut bytes = [0u8; 32];
  for chunk in bytes.chunks_mut(8) {
    chunk.copy_from_slice(&rng.next().to_le_bytes());
  }
  Txid::from_byte_array(bytes)
}

#[test]
#[ignore]
fn catalog_costs() {
  let t = Instant::now();
  let catalog = SatCatalog::new(&TrackerConfig::default()).unwrap();
  eprintln!("SatCatalog::new in {:.2?}", t.elapsed());
  let t = Instant::now();
  let totals: Vec<u64> = (0..catalog.rows().len() as RowId)
    .map(|row| catalog.row_total(row))
    .collect();
  eprintln!("all {} row totals in {:.2?}", totals.len(), t.elapsed());
  for (row, total) in catalog.rows().iter().zip(&totals) {
    eprintln!("  {:32} {total}", row.id);
  }
  let t = Instant::now();
  let mut runs = 0;
  let mut sats = 0;
  for height in (0..6_930_000u64).step_by(7) {
    let first = first_sat(height);
    for (_, len) in catalog.tracked_runs(first, first + subsidy(height)) {
      runs += 1;
      sats += len;
    }
  }
  eprintln!(
    "tracked_runs for every 7th coinbase: {:.2?} ({runs} runs, {sats} sats)",
    t.elapsed()
  );
  let t = Instant::now();
  let mut n = 0u64;
  for i in 0..1_000_000u64 {
    n += catalog
      .types(i.wrapping_mul(2_099_999_997_689_999) % 2_099_999_997_690_000)
      .len() as u64;
  }
  eprintln!("1M random types() in {:.2?} ({n})", t.elapsed());
  let t = Instant::now();
  for i in 0..1_000_000u64 {
    n += catalog.row_counts(first_sat(i % 6_930_000), 1).len() as u64;
  }
  eprintln!("1M single-sat row_counts in {:.2?} ({n})", t.elapsed());
}

#[test]
#[ignore]
fn tracker_throughput() {
  let utxos = env("BENCH_UTXOS", 1_600_000);
  let blocks = env("BENCH_BLOCKS", 300) as u32;
  let txs_per_block = env("BENCH_TXS", 3000);
  let tracked_percent = env("BENCH_TRACKED_PERCENT", 1);
  let config = TrackerConfig::default();
  let catalog = catalog_for(&config);
  // Past every block whose boundary sats the synthetic UTXOs hold.
  let start = (utxos / 2 + 1).max(870_000) as u32;
  let mut rng = Rng(0x1234_5678);

  // Tracked UTXOs: the uncommon and black sats of blocks 0..utxos/2, each
  // alone in a 10,000–60,000-sat output of one of 50,000 scripts.
  let t = Instant::now();
  let mut pool: Vec<(OutPoint, u64)> = Vec::with_capacity(utxos as usize);
  let mut snapshot_utxos = Vec::with_capacity(utxos as usize);
  for i in 0..utxos {
    let height = i / 2;
    let sat = if i % 2 == 0 {
      first_sat(height)
    } else {
      first_sat(height) + subsidy(height) - 1
    };
    let outpoint = op(random_txid(&mut rng), (i % 3) as u32);
    let value = 10_000 + rng.below(50_000);
    pool.push((outpoint, value));
    let mut script = vec![0x00, 0x14];
    script.extend((rng.below(50_000) as u32).to_le_bytes());
    script.extend([0u8; 16]);
    snapshot_utxos.push((
      outpoint,
      TrackedUtxo {
        value,
        script_pubkey: ScriptBuf::from_bytes(script),
        height: rng.below(u64::from(start)) as u32,
        kind: UtxoKind::Spendable,
        pieces: vec![Piece {
          offset: rng.below(value),
          start: sat,
          len: 1,
        }],
      },
    ));
  }
  let chain = Chain::at(Some(start - 1));
  let snapshot = TrackerSnapshot {
    height: Some(start - 1),
    hash: Some(chain.prev),
    times: (0..start).map(|h| chain.time_of(h)).collect(),
    lost_sats: 0,
    utxos: snapshot_utxos,
    lost: Vec::new(),
    overwritten: Vec::new(),
    mined: vec![0; crate::model::tracked_row_ids(catalog.rows().len())],
  };
  let mut tracker = Tracker::restore(catalog, config, snapshot).unwrap();
  eprintln!(
    "restored {} tracked UTXOs in {:.2?}",
    tracker.utxo_count(),
    t.elapsed()
  );

  // Pre-generate the blocks.
  let t = Instant::now();
  let mut chain = chain;
  let mut generated = Vec::with_capacity(blocks as usize);
  let mut scripts: Vec<ScriptBuf> = (0..1000u32)
    .map(|i| {
      let mut bytes = vec![0x00, 0x14];
      bytes.extend(i.to_le_bytes());
      bytes.extend([1u8; 16]);
      ScriptBuf::from_bytes(bytes)
    })
    .collect();
  scripts.push(ScriptBuf::from_bytes(vec![0x6a, 0x24, 0xaa]));
  for _ in 0..blocks {
    let mut txs = Vec::with_capacity(txs_per_block as usize + 1);
    let mut fees = 0;
    for _ in 0..txs_per_block {
      let n_in = 2 + rng.below(2) as usize;
      let n_out = 2 + rng.below(2) as usize;
      let mut inputs = Vec::with_capacity(n_in);
      let tracked = rng.below(100) < tracked_percent && !pool.is_empty();
      if tracked {
        let i = rng.below(pool.len() as u64) as usize;
        inputs.push(pool.swap_remove(i));
      }
      while inputs.len() < n_in {
        inputs.push((
          op(random_txid(&mut rng), rng.below(4) as u32),
          1_000 + rng.below(10_000_000),
        ));
      }
      let total: u64 = inputs.iter().map(|(_, v)| v).sum();
      let fee = rng.below(total.min(20_000));
      let mut left = total - fee;
      let txid = random_txid(&mut rng);
      let mut outputs = Vec::with_capacity(n_out);
      for j in 0..n_out {
        let value = if j + 1 == n_out {
          left
        } else {
          rng.below(left + 1)
        };
        left -= value;
        outputs.push(TxOut {
          value: Amount::from_sat(value),
          script_pubkey: scripts[rng.below(scripts.len() as u64 - 1) as usize].clone(),
        });
      }
      if tracked {
        // Where the tracked sat lands is not known here; any output may be
        // spent again, so offer all of them to later blocks.
        for (vout, output) in outputs.iter().enumerate() {
          if output.value.to_sat() > 0 {
            pool.push((op(txid, vout as u32), output.value.to_sat()));
          }
        }
      }
      fees += fee;
      let mut resolved = tx(txid, &inputs, outputs);
      if tracked {
        // Half the tracked spends look like listings, a tenth inscribe.
        if rng.below(2) == 0 {
          resolved.inputs[0].sighash = Some(crate::market::LISTING_SIGHASH);
        }
        if rng.below(10) == 0 {
          resolved.inputs[0].envelopes.push(crate::model::Envelope {
            pointer: None,
            unrecognized_even_field: false,
          });
        }
      }
      txs.push(resolved);
    }
    let claim = subsidy(chain.next.into()) + fees;
    let mut all = vec![coinbase(
      random_txid(&mut rng),
      vec![out(claim / 2), out(claim - claim / 2), op_return(0)],
    )];
    all.extend(txs);
    generated.push(chain.block(all));
  }
  eprintln!("generated {blocks} blocks in {:.2?}", t.elapsed());

  let ctx = crate::market::MarketContext::new(
    tracker.catalog().clone(),
    crate::market::Marketplaces::default(),
    crate::settings::Chain::Mainnet,
  );
  let t = Instant::now();
  let mut moves = 0;
  let mut analyzing = std::time::Duration::ZERO;
  let (mut sales, mut inscriptions) = (0, 0);
  for block in &generated {
    let delta = tracker.apply_block(block).unwrap();
    moves += delta.moves.len();
    let a = Instant::now();
    let market = crate::market::analyze(block, &delta, &ctx);
    analyzing += a.elapsed();
    sales += market.sales.len();
    inscriptions += market.inscriptions.len();
  }
  let elapsed = t.elapsed() - analyzing;
  eprintln!(
    "market::analyze: {analyzing:.2?} for {blocks} blocks ({:.1}% of applying them), {sales} sales, {inscriptions} inscriptions",
    100.0 * analyzing.as_secs_f64() / elapsed.as_secs_f64()
  );
  let txs = u64::from(blocks) * txs_per_block;
  eprintln!(
    "applied {blocks} blocks ({txs} txs, {moves} moves) in {elapsed:.2?}: {:.0} blocks/s, {:.2} M txs/s; {} tracked UTXOs, {} pieces",
    f64::from(blocks) / elapsed.as_secs_f64(),
    txs as f64 / elapsed.as_secs_f64() / 1e6,
    tracker.utxo_count(),
    tracker.piece_count(),
  );
  let t = Instant::now();
  let supply = tracker.supply(100);
  eprintln!(
    "supply(100) in {:.2?} ({} rows)",
    t.elapsed(),
    supply.rows.len()
  );
  let t = Instant::now();
  let snapshot = tracker.snapshot();
  eprintln!("snapshot in {:.2?}", t.elapsed());
  let t = Instant::now();
  let restored = Tracker::restore(
    tracker.catalog().clone(),
    tracker.config().clone(),
    snapshot,
  )
  .unwrap();
  eprintln!("restore in {:.2?}", t.elapsed());
  assert_eq!(restored.utxo_count(), tracker.utxo_count());
}
