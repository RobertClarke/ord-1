//! Mainnet check against upstream ord with `--index-sats` (runs on gp):
//!
//! ```sh
//! source ~/.config/ord-sats/bitcoind.env
//! MAINNET_TO=200000 cargo test --release --lib tracker::tests::mainnet \
//!   -- --ignored --nocapture
//! ```
//!
//! Replays mainnet from genesis to `MAINNET_TO` through the tracker with
//! blocks from `getblock <hash> 3`, then compares with the oracle (which is
//! at the chain tip) every tracked sat that cannot have moved since:
//! lost sats (ord's lost offsets never change), burned sats, overwritten
//! BIP30 sats, and sats in outputs `gettxout` still reports unspent.

use {
  super::*,
  crate::model::{ResolvedInput, ResolvedTx},
  bitcoin::{BlockHash, block::Header},
  serde_json::{Value, json},
  std::{
    collections::BTreeMap,
    str::FromStr,
    sync::{
      Arc as StdArc,
      atomic::{AtomicU32, Ordering},
      mpsc,
    },
    time::{Duration, Instant},
  },
};

struct Rpc {
  client: reqwest::blocking::Client,
  url: String,
  user: String,
  password: String,
}

impl Rpc {
  fn from_env() -> Option<Self> {
    Some(Self {
      client: reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap(),
      url: std::env::var("ORD_BITCOIN_RPC_URL").ok()?,
      user: std::env::var("ORD_BITCOIN_RPC_USERNAME").ok()?,
      password: std::env::var("ORD_BITCOIN_RPC_PASSWORD").ok()?,
    })
  }

  fn call(&self, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "1.0", "id": 0, "method": method, "params": params});
    for attempt in 0.. {
      let response = self
        .client
        .post(&self.url)
        .basic_auth(&self.user, Some(&self.password))
        .json(&body)
        .send()
        .and_then(|r| r.json::<Value>());
      match response {
        Ok(mut value) if value["error"].is_null() => return value["result"].take(),
        Ok(value) if attempt < 5 => {
          eprintln!("{method}: {} (retrying)", value["error"]);
        }
        Err(err) if attempt < 5 => eprintln!("{method}: {err} (retrying)"),
        other => panic!("{method} failed: {other:?}"),
      }
      std::thread::sleep(Duration::from_millis(200 << attempt));
    }
    unreachable!()
  }
}

fn sats(btc: &Value) -> u64 {
  (btc.as_f64().unwrap() * 100_000_000.0).round() as u64
}

fn block(rpc: &Rpc, height: u32) -> ResolvedBlock {
  let hash = rpc.call("getblockhash", json!([height]));
  let v = rpc.call("getblock", json!([hash, 3]));
  let header = Header {
    version: bitcoin::block::Version::from_consensus(v["version"].as_i64().unwrap() as i32),
    prev_blockhash: v["previousblockhash"]
      .as_str()
      .map_or(BlockHash::all_zeros(), |h| h.parse().unwrap()),
    merkle_root: v["merkleroot"].as_str().unwrap().parse().unwrap(),
    time: v["time"].as_u64().unwrap() as u32,
    bits: CompactTarget::from_consensus(
      u32::from_str_radix(v["bits"].as_str().unwrap(), 16).unwrap(),
    ),
    nonce: v["nonce"].as_u64().unwrap() as u32,
  };
  let txs = v["tx"]
    .as_array()
    .unwrap()
    .iter()
    .map(|tx| ResolvedTx {
      txid: tx["txid"].as_str().unwrap().parse().unwrap(),
      inputs: tx["vin"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|vin| vin.get("coinbase").is_none())
        .map(|vin| ResolvedInput {
          previous_output: OutPoint {
            txid: vin["txid"].as_str().unwrap().parse().unwrap(),
            vout: vin["vout"].as_u64().unwrap() as u32,
          },
          value: sats(&vin["prevout"]["value"]),
          script_hash: crate::model::script_hash(bitcoin::Script::from_bytes(
            &hex::decode(vin["prevout"]["scriptPubKey"]["hex"].as_str().unwrap()).unwrap(),
          )),
          sighash: None,
          envelopes: Vec::new(),
        })
        .collect(),
      outputs: tx["vout"]
        .as_array()
        .unwrap()
        .iter()
        .map(|out| TxOut {
          value: Amount::from_sat(sats(&out["value"])),
          script_pubkey: ScriptBuf::from_bytes(
            hex::decode(out["scriptPubKey"]["hex"].as_str().unwrap()).unwrap(),
          ),
        })
        .collect(),
    })
    .collect();
  ResolvedBlock {
    height,
    hash: BlockHash::from_str(v["hash"].as_str().unwrap()).unwrap(),
    header,
    txs,
  }
}

struct Oracle {
  client: reqwest::blocking::Client,
  url: String,
}

impl Oracle {
  fn get(&self, path: &str) -> Value {
    for attempt in 0.. {
      match self
        .client
        .get(format!("{}{path}", self.url))
        .header("Accept", "application/json")
        .send()
        .and_then(|r| r.json::<Value>())
      {
        Ok(v) => return v,
        Err(err) if attempt < 5 => {
          eprintln!("oracle {path}: {err} (retrying)");
          std::thread::sleep(Duration::from_millis(200 << attempt));
        }
        Err(err) => panic!("oracle {path}: {err}"),
      }
    }
    unreachable!()
  }

  /// The output's sat ranges, in offset order.
  fn ranges(&self, outpoint: &OutPoint) -> Vec<(u64, u64)> {
    self.get(&format!("/output/{outpoint}"))["sat_ranges"]
      .as_array()
      .unwrap_or_else(|| panic!("no sat ranges for {outpoint}"))
      .iter()
      .map(|r| (r[0].as_u64().unwrap(), r[1].as_u64().unwrap()))
      .collect()
  }
}

/// The tracked pieces of consecutive sat ranges, the first at offset 0.
fn tracked_pieces(catalog: &SatCatalog, ranges: &[(u64, u64)], limit: u64) -> Vec<Piece> {
  let mut pieces = Vec::new();
  let mut offset = 0;
  for &(start, end) in ranges {
    if offset >= limit {
      break;
    }
    let end = end.min(start.saturating_add(limit - offset));
    for (run, len) in catalog.tracked_runs(start, end) {
      push_merged(
        &mut pieces,
        Piece {
          offset: offset + (run - start),
          start: run,
          len,
        },
      );
    }
    offset += end - start;
  }
  pieces
}

#[test]
#[ignore]
fn mainnet_matches_oracle() {
  let (Some(rpc), Ok(oracle)) = (Rpc::from_env(), std::env::var("ORACLE_ORD_URL")) else {
    eprintln!("set ORD_BITCOIN_RPC_* and ORACLE_ORD_URL");
    return;
  };
  let to: u32 = std::env::var("MAINNET_TO")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(100_000);
  let threads: u32 = std::env::var("MAINNET_THREADS")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(8);
  let config = TrackerConfig::default();
  let mut tracker = Tracker::new(catalog_for(&config), config);

  // Parallel fetch, in-order apply.
  let rpc = StdArc::new(rpc);
  let next = StdArc::new(AtomicU32::new(0));
  let (tx, rx) = mpsc::sync_channel::<(u32, ResolvedBlock)>(256);
  let workers: Vec<_> = (0..threads)
    .map(|_| {
      let (rpc, next, tx) = (rpc.clone(), next.clone(), tx.clone());
      std::thread::spawn(move || {
        loop {
          let height = next.fetch_add(1, Ordering::Relaxed);
          if height > to {
            break;
          }
          if tx.send((height, block(&rpc, height))).is_err() {
            break;
          }
        }
      })
    })
    .collect();
  drop(tx);
  let start = Instant::now();
  let mut pending = BTreeMap::new();
  let mut applying = Duration::ZERO;
  let mut txs = 0u64;
  let mut height = 0;
  for (h, block) in rx {
    pending.insert(h, block);
    while let Some(block) = pending.remove(&height) {
      txs += block.txs.len() as u64;
      let t = Instant::now();
      tracker
        .apply_block(&block)
        .unwrap_or_else(|err| panic!("{err:#}"));
      applying += t.elapsed();
      if height % 20_000 == 0 {
        eprintln!(
          "{height}: {:.0?} elapsed, {:.2?} applying, {txs} txs, {} tracked UTXOs, {} pieces, {} lost sats",
          start.elapsed(),
          applying,
          tracker.utxo_count(),
          tracker.piece_count(),
          tracker.lost_sats()
        );
      }
      height += 1;
    }
  }
  for worker in workers {
    worker.join().unwrap();
  }
  assert_eq!(tracker.height(), Some(to));
  eprintln!(
    "applied 0..={to} ({txs} txs) in {:.0?}, tracker time {applying:.2?}",
    start.elapsed()
  );
  let t = Instant::now();
  tracker.verify().unwrap();
  eprintln!("verify: {:.2?}", t.elapsed());

  let oracle = Oracle {
    client: reqwest::blocking::Client::builder()
      .timeout(Duration::from_secs(120))
      .build()
      .unwrap(),
    url: oracle,
  };
  let catalog = tracker.catalog().clone();
  let mut mismatches = Vec::new();
  let mut checked = BTreeMap::<&str, u64>::new();

  // Lost sats never move: the null outpoint's ranges are ord's lost stream,
  // in order; its first `lost_sats` sats are what was lost up to `to`.
  let lost = tracked_pieces(
    &catalog,
    &oracle.ranges(&OutPoint::null()),
    tracker.lost_sats(),
  );
  let mut ours = Vec::new();
  for &piece in tracker.lost_pieces() {
    push_merged(&mut ours, piece);
  }
  *checked.entry("lost pieces").or_default() += ours.len() as u64;
  if ours != lost {
    mismatches.push(format!("lost pieces: tracker {ours:?}, ord {lost:?}"));
  }

  // Unspendable outputs never move; unspent outputs still hold their sats.
  let snapshot = tracker.snapshot();
  let mut rng = 0x9e37_79b9_7f4a_7c15u64;
  let mut random = |n: u64| {
    rng ^= rng << 13;
    rng ^= rng >> 7;
    rng ^= rng << 17;
    rng % n.max(1)
  };
  let wanted: u64 = std::env::var("MAINNET_OUTPUTS")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(600);
  let (mut unspent, mut spent, mut burned) = (0, 0, 0);
  let utxos = &snapshot.utxos;
  for (outpoint, utxo) in utxos
    .iter()
    .filter(|(_, utxo)| utxo.kind == UtxoKind::Unspendable)
  {
    let expected = tracked_pieces(&catalog, &oracle.ranges(outpoint), u64::MAX);
    burned += 1;
    if expected != utxo.pieces {
      mismatches.push(format!(
        "burned {outpoint}: tracker {:?}, ord {expected:?}",
        utxo.pieces
      ));
    }
  }
  for _ in 0..wanted * 50 {
    if unspent >= wanted {
      break;
    }
    let (outpoint, utxo) = &utxos[random(utxos.len() as u64) as usize];
    if utxo.kind == UtxoKind::Unspendable {
      continue;
    }
    let txout = rpc.call("gettxout", json!([outpoint.txid, outpoint.vout, false]));
    if txout.is_null() {
      spent += 1;
      continue;
    }
    assert_eq!(sats(&txout["value"]), utxo.value, "{outpoint}");
    unspent += 1;
    let kind = if txout["coinbase"].as_bool() == Some(true) {
      "unspent coinbase outputs"
    } else {
      "unspent outputs"
    };
    *checked.entry(kind).or_default() += 1;
    let expected = tracked_pieces(&catalog, &oracle.ranges(outpoint), u64::MAX);
    if expected != utxo.pieces {
      mismatches.push(format!(
        "{kind} {outpoint}: tracker {:?}, ord {expected:?}",
        utxo.pieces
      ));
    }
  }
  *checked.entry("burned outputs").or_default() += burned;

  // Overwritten (BIP30) sats: ord keeps the old satpoint for rare sats.
  for (outpoint, height, utxo) in &snapshot.overwritten {
    for piece in &utxo.pieces {
      let v = oracle.get(&format!("/sat/{}", piece.start));
      if v["rarity"].as_str() == Some("common") {
        continue;
      }
      *checked.entry("overwritten rare sats").or_default() += 1;
      let expected = format!("{outpoint}:{}", piece.offset);
      if v["satpoint"].as_str() != Some(expected.as_str()) {
        mismatches.push(format!(
          "overwritten at {height}: sat {} tracker {expected}, ord {}",
          piece.start, v["satpoint"]
        ));
      }
    }
  }

  eprintln!("checked {checked:?}; {spent} sampled outputs were spent since");
  assert!(
    mismatches.is_empty(),
    "{} mismatches:\n{}",
    mismatches.len(),
    mismatches.join("\n")
  );
}
