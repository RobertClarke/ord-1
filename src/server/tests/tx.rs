//! `GET /tx/{txid}` over the scenario, with a fake bitcoind.

use {
  super::{
    mempool::{pending, publish},
    *,
  },
  crate::mempool::{MempoolEntry, NodeBlock, NodeTx, TxNode},
  bitcoin::{
    Amount, Sequence, Transaction, TxIn, TxOut, Witness, absolute::LockTime,
    transaction::Version as TxVersion,
  },
  pretty_assertions::assert_eq,
  std::collections::HashMap as Map,
};

#[derive(Default)]
struct FakeNode(Map<Txid, NodeTx>);

impl TxNode for FakeNode {
  fn transaction(&self, txid: Txid) -> anyhow::Result<Option<NodeTx>> {
    Ok(self.0.get(&txid).cloned())
  }
}

fn tx(inputs: &[OutPoint], outputs: &[(u64, ScriptBuf)]) -> Transaction {
  Transaction {
    version: TxVersion::TWO,
    lock_time: LockTime::ZERO,
    input: inputs
      .iter()
      .map(|outpoint| TxIn {
        previous_output: *outpoint,
        script_sig: ScriptBuf::new(),
        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        witness: Witness::new(),
      })
      .collect(),
    output: outputs
      .iter()
      .map(|(value, script)| TxOut {
        value: Amount::from_sat(*value),
        script_pubkey: script.clone(),
      })
      .collect(),
  }
}

fn coinbase_tx(outputs: &[(u64, ScriptBuf)]) -> Transaction {
  let mut tx = tx(&[OutPoint::null()], outputs);
  tx.input[0].script_sig = ScriptBuf::from_bytes(vec![1, 2]);
  tx
}

fn confirmed(
  server: &TestServer,
  tx: Transaction,
  prevouts: &[(u64, ScriptBuf)],
  height: u32,
) -> NodeTx {
  NodeTx {
    prevouts: if tx.is_coinbase() {
      vec![None]
    } else {
      prevouts
        .iter()
        .map(|(value, script)| {
          Some(TxOut {
            value: Amount::from_sat(*value),
            script_pubkey: script.clone(),
          })
        })
        .collect()
    },
    prevout_heights: vec![Some(1); tx.input.len()],
    tx,
    block: Some(NodeBlock {
      hash: server.scenario.deltas[height as usize].hash,
      height,
      time: TIMES[height as usize],
      confirmations: 4 - height,
    }),
    entry: None,
  }
}

/// T (block 2) spends B, the Magisat sale: B's uncommon to T:0, its black to T:1.
fn t_tx() -> Transaction {
  tx(
    &[OutPoint::new(txid(1), 0)],
    &[(546, p2tr()), (4_999_990_000, p2wpkh(2))],
  )
}

fn with_node(server: &TestServer, txs: Vec<(Txid, NodeTx)>) {
  assert!(
    server
      .state
      .node
      .set(Arc::new(FakeNode(txs.into_iter().collect())))
      .is_ok()
  );
}

#[test]
fn confirmed_flow_sales_and_entries() {
  let server = TestServer::new(Some(3));
  let node = confirmed(&server, t_tx(), &[(COIN, p2wpkh(1))], 2);
  with_node(&server, vec![(txid(20), node)]);
  let tx = server.json(&format!("/tx/{}", txid(20)));
  assert_eq!(tx["status"], "confirmed");
  assert_eq!(tx["height"], 2);
  assert_eq!(tx["hash"], server.hash(2));
  assert_eq!(tx["time"], TIMES[2]);
  assert_eq!(tx["confirmations"], 2);
  assert_eq!(tx["tx_index"], 1);
  assert_eq!(tx["indexed"], true);
  assert_eq!(tx["sat_flow"], "complete");
  assert_eq!(tx["coinbase"], false);
  assert_eq!(tx["input_value"], COIN);
  assert_eq!(tx["fee"], 9_454);
  assert_eq!(tx["inputs"].as_array().unwrap().len(), 1);
  assert_eq!(
    tx["inputs"][0],
    json!({
      "vin": 0,
      "outpoint": format!("{}:0", txid(1)),
      "value": COIN,
      "address": address(&p2wpkh(1)),
      "script_pubkey": p2wpkh(1).to_hex_string(),
      "sequence": 0xffff_fffd_u32,
      "created_height": 1,
      "sighash": null,
      "inscriptions": 0,
      "tracked": true,
      "extracted": false,
      "pieces": [
        piece_json(0, COIN, 1, &["uncommon", "alpha"]),
        piece_json(COIN - 1, 2 * COIN - 1, 1, &["black"]),
      ],
      "inscribed_sats": [COIN],
    })
  );
  assert_eq!(tx["outputs"][0]["tracked"], true);
  assert_eq!(tx["outputs"][0]["extracted"], true);
  assert_eq!(tx["outputs"][0]["address"], address(&p2tr()));
  assert_eq!(tx["outputs"][1]["pieces"][0]["start"], 2 * COIN - 1);
  assert_eq!(
    tx["flow"],
    json!([
      {
        "source": "input", "vin": 0, "input_offset": 0,
        "dest": "output", "vout": 0, "offset": 0,
        "start": COIN, "len": 1, "types": ["uncommon", "alpha"],
      },
      {
        "source": "input", "vin": 0, "input_offset": COIN - 1,
        "dest": "output", "vout": 1, "offset": COIN - 1 - 546,
        "start": 2 * COIN - 1, "len": 1, "types": ["black"],
      },
    ])
  );
  // The seller and buyer, and the black sat's receiver.
  assert_eq!(tx["kinds"], json!(["sale", "transfer"]));
  assert_eq!(tx["sales"].as_array().unwrap().len(), 1);
  assert_eq!(tx["sales"][0]["marketplace"], "magisat");
  assert_eq!(tx["move"]["txid"], txid(20).to_string());
  assert_eq!(tx["move"]["tx_index"], 1);
  assert!(!tx["entries"].as_array().unwrap().is_empty());
  assert!(tx.get("removed").is_some_and(Value::is_null));
}

#[test]
fn coinbase_flow_is_subsidy_and_fees() {
  let server = TestServer::new(Some(3));
  let node = confirmed(&server, coinbase_tx(&[(COIN + 9_454, p2wpkh(3))]), &[], 2);
  with_node(&server, vec![(txid(2), node)]);
  let tx = server.json(&format!("/tx/{}", txid(2)));
  assert_eq!(tx["coinbase"], true);
  assert_eq!(tx["inputs"][0]["outpoint"], Value::Null);
  assert_eq!(tx["inputs"][0]["value"], Value::Null);
  assert_eq!(tx["fee"], Value::Null);
  assert_eq!(tx["sat_flow"], "complete");
  let flow = tx["flow"].as_array().unwrap();
  assert_eq!(flow.len(), 2);
  assert!(
    flow
      .iter()
      .all(|run| run["source"] == "subsidy" && run["vout"] == 0)
  );
  assert_eq!(tx["mined_pieces"].as_array().unwrap().len(), 2);
}

#[test]
fn pruned_moves_show_what_is_left_in_the_outputs() {
  let server = TestServer::with_retention(Some(3), MovesRetention::Blocks(1));
  let node = confirmed(&server, t_tx(), &[(COIN, p2wpkh(1))], 2);
  with_node(&server, vec![(txid(20), node)]);
  let tx = server.json(&format!("/tx/{}", txid(20)));
  assert_eq!(tx["sat_flow"], "outputs");
  assert_eq!(tx["move"], Value::Null);
  assert_eq!(tx["flow"], json!([]));
  assert_eq!(tx["inputs"][0]["tracked"], false);
  assert_eq!(tx["outputs"][0]["pieces"][0]["start"], COIN);
  assert_eq!(tx["outputs"][1]["pieces"][0]["start"], 2 * COIN - 1);
  // Sales and history are kept forever.
  assert_eq!(tx["sales"].as_array().unwrap().len(), 1);
  assert_eq!(tx["kinds"], json!(["sale", "transfer"]));
}

#[test]
fn a_block_not_indexed_yet_has_an_unknown_flow() {
  let server = TestServer::new(Some(1));
  let node = confirmed(&server, t_tx(), &[(COIN, p2wpkh(1))], 2);
  with_node(&server, vec![(txid(20), node)]);
  let tx = server.json(&format!("/tx/{}", txid(20)));
  assert_eq!(tx["status"], "confirmed");
  assert_eq!(tx["indexed"], false);
  assert_eq!(tx["sat_flow"], "unknown");
  assert_eq!(tx["outputs"][0]["tracked"], false);
}

#[test]
fn a_transaction_moving_nothing_tracked() {
  let server = TestServer::new(Some(3));
  let plain = tx(&[OutPoint::new(txid(99), 0)], &[(1_000, p2wpkh(8))]);
  let node = confirmed(&server, plain, &[(2_000, p2wpkh(9))], 3);
  with_node(&server, vec![(txid(98), node)]);
  let tx = server.json(&format!("/tx/{}", txid(98)));
  assert_eq!(tx["sat_flow"], "complete");
  assert_eq!(tx["move"], Value::Null);
  assert_eq!(tx["flow"], json!([]));
  assert_eq!(tx["kinds"], json!([]));
  assert_eq!(tx["fee"], 1_000);
  assert_eq!(tx["inputs"][0]["tracked"], false);
}

fn mempool_tx(tx: Transaction, prevouts: &[(u64, ScriptBuf)]) -> NodeTx {
  NodeTx {
    prevouts: prevouts
      .iter()
      .map(|(value, script)| {
        Some(TxOut {
          value: Amount::from_sat(*value),
          script_pubkey: script.clone(),
        })
      })
      .collect(),
    prevout_heights: vec![None; tx.input.len()],
    tx,
    block: None,
    entry: Some(MempoolEntry {
      time: 1_800_000_000,
      fee: 2_000,
      vsize: 300,
    }),
  }
}

#[test]
fn pending_in_the_overlay() {
  let server = TestServer::new(Some(3));
  let (pending, t0) = pending(&server);
  publish(&server, pending.clone());
  let raw = tx(
    &[t0, OutPoint::new(txid(77), 0)],
    &[(546, p2wpkh(5)), (50_000, p2tr()), (48_000, p2wpkh(6))],
  );
  let node = mempool_tx(raw, &[(546, p2tr()), (100_000, p2wpkh(7))]);
  with_node(&server, vec![(pending.txid, node)]);
  let tx = server.json(&format!("/tx/{}", pending.txid));
  assert_eq!(tx["status"], "pending");
  assert_eq!(tx["first_seen"], 1_800_000_000_u64);
  assert_eq!(tx["fee"], 2_000);
  assert_eq!(tx["fee_rate"], 6.7);
  assert_eq!(tx["sat_flow"], "complete");
  assert_eq!(tx["kinds"], json!(["sale"]));
  assert_eq!(tx["sales"][0]["pending"], true);
  assert_eq!(tx["move"]["tx_index"], Value::Null);
  assert_eq!(tx["inputs"][0]["tracked"], true);
  assert_eq!(tx["flow"][0]["vout"], 0);
  assert_eq!(tx["flow"][0]["start"], COIN);
}

#[test]
fn pending_outside_the_overlay_is_flowed_here() {
  let server = TestServer::new(Some(3));
  server
    .published
    .update_status(|status| status.bitcoind_height = Some(3));
  let d = OutPoint::new(txid(3), 0);
  let raw = tx(&[d], &[(1_000, p2tr()), (COIN - 3_000, p2wpkh(4))]);
  let node = mempool_tx(raw, &[(COIN, p2wpkh(4))]);
  with_node(&server, vec![(txid(97), node)]);
  let tx = server.json(&format!("/tx/{}", txid(97)));
  assert_eq!(tx["status"], "pending");
  assert_eq!(tx["sat_flow"], "complete");
  assert_eq!(tx["kinds"], json!([]));
  assert_eq!(tx["move"]["tx_index"], Value::Null);
  let flow = tx["flow"].as_array().unwrap();
  assert_eq!(flow.len(), 2);
  assert_eq!(flow[0]["start"], 3 * COIN);
  assert_eq!(flow[0]["vout"], 0);
  assert_eq!(flow[1]["start"], 4 * COIN - 1);
  assert_eq!(flow[1]["vout"], Value::Null);
  assert_eq!(flow[1]["dest"], "fee");
  assert_eq!(tx["fee_pieces"][0]["start"], 4 * COIN - 1);
}

#[test]
fn unknown_removed_and_without_bitcoind() {
  let server = TestServer::new(Some(3));
  assert!(
    server
      .error(&format!("/tx/{}", txid(20)), 503)
      .contains("bitcoind")
  );
  with_node(&server, Vec::new());
  assert!(
    server
      .error(&format!("/tx/{}", txid(20)), 404)
      .contains("no transaction")
  );
  server.error("/tx/nope", 400);
  let (pending, _) = pending(&server);
  publish(&server, pending);
  let tx = server.json(&format!("/tx/{}", txid(91)));
  assert_eq!(tx["status"], "replaced");
  assert_eq!(tx["removed"]["replaced_by"], txid(90).to_string());
  assert_eq!(tx["inputs"], json!([]));
}
