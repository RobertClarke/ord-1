//! The mempool endpoints and the pending additions to the others, over a
//! hand-made overlay snapshot: a pending Magisat-style sale of T:0 (the
//! uncommon sat 5e9) to a new P2WPKH output.

use {
  super::*,
  crate::{
    history::{Direction, HistoryEntry, HistoryKind, Side, history_key},
    mempool::{Classified, PendingTx, Removed, RemovedStatus, Snapshot, Stats, classify},
    model::{ResolvedInput, ResolvedTx},
  },
  bitcoin::{Amount, TxOut},
  pretty_assertions::assert_eq,
  std::collections::HashMap as Map,
};

const FIRST_SEEN: u64 = 1_800_000_000;

/// The pending sale, and T:0.
fn pending(server: &TestServer) -> (Arc<PendingTx>, OutPoint) {
  let reader = server.store.reader().unwrap();
  let (t0, utxo) = reader
    .utxos_for_script(&p2tr())
    .unwrap()
    .into_iter()
    .next()
    .unwrap();
  let funding = OutPoint::new(txid(77), 0);
  let tx = ResolvedTx {
    txid: txid(90),
    inputs: vec![
      ResolvedInput {
        previous_output: t0,
        value: 546,
        script_hash: 0,
        sighash: Some(0x83),
        envelopes: Vec::new(),
      },
      ResolvedInput {
        previous_output: funding,
        value: 100_000,
        script_hash: 0,
        sighash: Some(0x01),
        envelopes: Vec::new(),
      },
    ],
    outputs: vec![
      TxOut {
        value: Amount::from_sat(546),
        script_pubkey: p2wpkh(5),
      },
      TxOut {
        value: Amount::from_sat(50_000),
        script_pubkey: p2tr(),
      },
      TxOut {
        value: Amount::from_sat(48_000),
        script_pubkey: p2wpkh(6),
      },
    ],
  };
  let mv = classify::tx_move(
    &tx,
    vec![MoveInput {
      vin: 0,
      outpoint: t0,
      utxo: utxo.clone(),
    }],
  )
  .unwrap();
  let sale = Sale {
    txid: tx.txid,
    tx_index: 1,
    vin: 0,
    kind: SaleKind::Listing,
    marketplace: "magisat".into(),
    seller_outpoint: t0,
    seller: utxo.clone(),
    payment_vout: Some(1),
    price: 50_000,
    seller_proceeds: 50_000,
    fees: 0,
    buyer_vout: Some(0),
    buyer_script: Some(p2wpkh(5)),
  };
  let side = |count| Side {
    sats: 1,
    value: 546,
    count,
    rows: vec![(0, 1)],
  };
  let entry = |script: &ScriptBuf, direction, seller, buyer| HistoryEntry {
    key: history_key(script),
    height: 4,
    tx_index: 1,
    txid: tx.txid,
    kind: HistoryKind::Sale,
    direction,
    received: if buyer { side(1) } else { Side::default() },
    sent: if seller { side(1) } else { Side::default() },
    extracted_in: buyer,
    extracted_out: seller,
    extracted_sats: u64::from(buyer),
    isolated: 0,
    burned: 0,
    seller,
    buyer,
    counterparties: 1,
    counterparty: None,
    lead_sat: Some(COIN),
  };
  let mut scripts = Map::new();
  scripts.insert(history_key(&p2tr()), p2tr());
  scripts.insert(history_key(&p2wpkh(5)), p2wpkh(5));
  let pending = PendingTx {
    txid: tx.txid,
    tx: tx.clone(),
    first_seen: FIRST_SEEN,
    fee: 2_000,
    vsize: 300,
    depends: Vec::new(),
    height: 4,
    classified: Classified {
      mv,
      market: MarketDelta {
        sales: vec![sale],
        inscriptions: Vec::new(),
      },
      entries: vec![
        entry(&p2tr(), Direction::Out, true, false),
        entry(&p2wpkh(5), Direction::In, false, true),
      ],
      scripts,
    },
  };
  (Arc::new(pending), t0)
}

fn publish(server: &TestServer, tx: Arc<PendingTx>) {
  let hash = server.scenario.deltas[3].hash;
  server.published.set_mempool(Snapshot::new(
    true,
    true,
    7,
    Some((3, hash)),
    vec![tx],
    vec![Removed {
      txid: txid(91),
      status: RemovedStatus::Replaced,
      height: None,
      replaced_by: Some(txid(90)),
      at: FIRST_SEEN + 5,
      seq: 6,
    }],
    Stats {
      mempool_txs: 1234,
      scanned_txs: 1234,
      pending_sales: 1,
      interval_ms: 5000,
      ..Stats::default()
    },
  ));
}

#[test]
fn disabled_overlay_adds_nothing() {
  let server = TestServer::new(Some(3));
  let status = server.json("/status");
  assert_eq!(status["mempool"], true);
  assert_eq!(status["mempool_enabled"], false);
  let mempool = server.json("/mempool/status");
  assert_eq!(mempool["enabled"], false);
  assert_eq!(mempool["ready"], false);
  assert_eq!(mempool["pending_txs"], 0);
  assert_eq!(server.json("/mempool")["txs"], json!([]));
  let (_, t0) = pending(&server);
  let output = server.json(&format!("/output/{t0}"));
  assert!(output.get("spent_by_pending").is_none());
  let script = server.json(&format!("/script/{}", p2tr().to_hex_string()));
  assert!(script.get("pending_outputs").is_none());
  assert!(server.json("/sat/5000000000").get("pending").is_none());
}

#[test]
fn pending_sale_everywhere() {
  let server = TestServer::new(Some(3));
  let (tx, t0) = pending(&server);
  let txid = tx.txid;
  publish(&server, tx);

  assert_eq!(server.json("/status")["mempool_enabled"], true);
  let status = server.json("/mempool/status");
  assert_eq!(status["seq"], 7);
  assert_eq!(status["height"], 3);
  assert_eq!(status["pending_txs"], 1);
  assert_eq!(status["pending_sales"], 1);
  assert_eq!(status["mempool_txs"], 1234);
  assert_eq!(status["removed"], 1);

  // The whole set, with its ETag; 304 when unchanged.
  let response = server.get("/mempool");
  assert_eq!(response.headers()[header::ETAG], "\"mempool-7\"");
  let all: Value = response.json().unwrap();
  assert_eq!(all["seq"], 7);
  let tx = &all["txs"][0];
  assert_eq!(tx["txid"], json!(txid));
  assert_eq!(tx["pending"], true);
  assert_eq!(tx["first_seen"], FIRST_SEEN);
  assert_eq!(tx["fee_rate"], 6.7);
  assert_eq!(tx["kinds"], json!(["sale"]));
  assert_eq!(tx["move"]["tx_index"], Value::Null);
  assert_eq!(tx["move"]["inputs"][0]["created_height"], 2);
  assert_eq!(tx["move"]["outputs"][0]["vout"], 0);
  let sale = &tx["sales"][0];
  assert_eq!(sale["height"], Value::Null);
  assert_eq!(sale["tx_index"], Value::Null);
  assert_eq!(sale["time"], FIRST_SEEN);
  assert_eq!(sale["pending"], true);
  assert_eq!(sale["marketplace"], "magisat");
  assert_eq!(sale["price"], 50_000);
  assert_eq!(sale["premium"], 50_000 - 546);
  assert_eq!(sale["buyer"]["value"], 546);
  assert_eq!(sale["rows"], json!({ "uncommon": 1, "alpha": 1 }));
  let entries = tx["entries"].as_array().unwrap();
  assert_eq!(entries.len(), 2);
  for entry in entries {
    assert_eq!(entry["pending"], true);
    assert_eq!(entry["height"], Value::Null);
    assert_eq!(entry["kind"], "sale");
    assert_eq!(entry["sales"].as_array().unwrap().len(), 1);
    assert!(entry["script_pubkey"].is_string());
  }
  assert_eq!(all["removed"][0]["status"], "replaced");
  assert_eq!(all["removed"][0]["replaced_by"], json!(txid));
  let cached = server
    .client
    .get(format!("{}/mempool", server.url))
    .header(header::IF_NONE_MATCH, "\"mempool-7\"")
    .send()
    .unwrap();
  assert_eq!(cached.status(), 304);

  // Lists.
  assert_eq!(server.json("/mempool/sales")["sales"][0]["vin"], 0);
  assert_eq!(
    server.json("/mempool/sales?row=uncommon")["sales"]
      .as_array()
      .unwrap()
      .len(),
    1
  );
  assert_eq!(server.json("/mempool/sales?row=black")["sales"], json!([]));
  assert_eq!(server.json("/mempool/extractions")["txs"], json!([]));
  assert_eq!(server.json("/mempool/transfers")["txs"], json!([]));
  assert_eq!(
    server.json("/mempool?kind=sale")["txs"]
      .as_array()
      .unwrap()
      .len(),
    1
  );
  assert_eq!(server.json("/mempool?kind=burn")["txs"], json!([]));
  server.error("/mempool?kind=nope", 400);
  server.error("/mempool?row=nope", 404);
  assert_eq!(
    server.json(&format!("/mempool/tx/{txid}"))["txid"],
    json!(txid)
  );
  let gone = server.get(&format!("/mempool/tx/{}", super::txid(91)));
  assert_eq!(gone.status(), 404);
  assert_eq!(
    gone.json::<Value>().unwrap()["removed"]["status"],
    "replaced"
  );

  // The sold output is being spent; the buyer's output exists, pending.
  let output = server.json(&format!("/output/{t0}"));
  assert_eq!(output["spent_by_pending"]["txid"], json!(txid));
  assert_eq!(output["spent_by_pending"]["vin"], 0);
  assert_eq!(output["spent_by_pending"]["sale"], true);
  assert_eq!(output["spent_by_pending"]["first_seen"], FIRST_SEEN);
  let bought = server.json(&format!("/output/{txid}:0"));
  assert_eq!(bought["pending"], true);
  assert_eq!(bought["created_height"], Value::Null);
  assert_eq!(bought["created_time"], Value::Null);
  assert_eq!(bought["rows"], json!({ "uncommon": 1, "alpha": 1 }));
  server.error(&format!("/output/{txid}:2"), 404);

  // The sat's pending location and sale.
  let sat = server.json("/sat/5000000000");
  assert_eq!(sat["location"]["outpoint"], json!(t0));
  assert_eq!(
    sat["location"]["output"]["spent_by_pending"]["txid"],
    json!(txid)
  );
  assert_eq!(sat["pending"]["kind"], "unspent");
  assert_eq!(sat["pending"]["satpoint"], format!("{txid}:0:0"));
  assert_eq!(sat["pending"]["output"]["pending"], true);
  assert_eq!(sat["pending"]["sales"][0]["pending"], true);
  assert_eq!(
    sat["sales"].as_array().unwrap().len(),
    1,
    "confirmed sales only"
  );

  // Addresses: the seller's history (opt-in), the buyer's pending output.
  let seller = p2tr().to_hex_string();
  let history = server.json(&format!("/script/{seller}/history?pending=true"));
  assert_eq!(history["entries"][0]["pending"], true);
  assert_eq!(history["entries"][0]["roles"], json!(["seller"]));
  assert!(history["entries"][1].get("pending").is_none());
  let plain = server.json(&format!("/script/{seller}/history"));
  assert!(plain["entries"][0].get("pending").is_none());
  let filtered = server.json(&format!(
    "/script/{seller}/history?pending=true&kinds=transfer"
  ));
  assert!(
    filtered["entries"]
      .as_array()
      .unwrap()
      .iter()
      .all(|e| e.get("pending").is_none())
  );
  let buyer = p2wpkh(5).to_hex_string();
  let script = server.json(&format!("/script/{buyer}"));
  assert_eq!(script["outputs"], json!([]));
  assert_eq!(
    script["pending_outputs"][0]["outpoint"],
    format!("{txid}:0")
  );
  let mine = server.json(&format!("/mempool/script/{buyer}"));
  assert_eq!(mine["entries"][0]["direction"], "in");
  assert_eq!(mine["outputs"].as_array().unwrap().len(), 1);
  let theirs = server.json(&format!("/mempool/script/{seller}"));
  assert_eq!(theirs["spent"][0]["outpoint"], json!(t0));

  // Never in the confirmed sales or the supply.
  assert_eq!(server.ndjson("/sales").len(), 1);
  let supply = server.json("/supply");
  assert!(!supply.to_string().contains("pending"));
}
