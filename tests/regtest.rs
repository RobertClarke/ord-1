//! End to end against a mockcore regtest chain. Default tracker config:
//! block 9 is tracked whole; other blocks track their uncommon (first) and
//! black (last) sats, and heights < 5924 their alphas.

use {
  super::*,
  bitcoin::{ScriptBuf, Witness, opcodes, script},
  mockcore::TransactionTemplate,
};

fn op_return() -> ScriptBuf {
  script::Builder::new()
    .push_opcode(opcodes::all::OP_RETURN)
    .push_slice(b"burn")
    .into_script()
}

#[test]
#[ignore = "needs source"]
fn index_update_and_find_block_1_uncommon() {
  let core = regtest();
  core.mine_blocks(1);
  let dir = tempdir();

  let update = ord_json(Some(&core), dir.path(), &["index", "update"]);
  assert_eq!(update["height"], 1);
  assert!(update["tracked_utxos"].as_u64().unwrap() >= 2);

  let sat = ord_json(Some(&core), dir.path(), &["find", "5000000000"]);
  assert_eq!(sat["sat"], BLOCK);
  assert_eq!(sat["height"], 1);
  assert_eq!(sat["tracked"], true);
  assert!(
    sat["types"]
      .as_array()
      .unwrap()
      .contains(&"uncommon".into())
  );
  assert!(sat["rows"].as_array().unwrap().contains(&"uncommon".into()));
  let location = &sat["location"];
  assert_eq!(location["kind"], "unspent");
  assert_eq!(
    location["satpoint"],
    format!("{}:0", outpoint(coinbase(&core, 1), 0))
  );
  assert_eq!(location["output"]["value"], BLOCK);
  assert_eq!(location["output"]["created_height"], 1);

  let info = ord_json(Some(&core), dir.path(), &["index", "info"]);
  assert_eq!(info["meta"]["height"], 1);
  assert_eq!(info["meta"]["chain"], "regtest");

  // A second update finds nothing new.
  assert_eq!(
    ord_json(Some(&core), dir.path(), &["index", "update"])["height"],
    1
  );
}

#[test]
#[ignore = "needs source"]
fn fast_sync_then_live_on_a_longer_chain() {
  let core = regtest();
  core.mine_blocks(40);
  let dir = tempdir();

  let update = ord_json(
    Some(&core),
    dir.path(),
    &["--commit-interval", "7", "index", "update"],
  );
  assert_eq!(update["height"], 40);
  assert_eq!(update["fast_sync_complete"], true);

  // Block 9 is tracked whole: any of its sats is in its coinbase.
  let sat = ord_json(Some(&core), dir.path(), &["find", "45000000123"]);
  assert_eq!(sat["tracked"], true);
  assert!(sat["rows"].as_array().unwrap().contains(&"block9".into()));
  assert_eq!(
    sat["location"]["satpoint"],
    format!("{}:123", outpoint(coinbase(&core, 9), 0))
  );

  // Black sats of fast-synced and live blocks.
  for height in [5u64, 35] {
    let black = (height + 1) * BLOCK - 1;
    let sat = ord_json(Some(&core), dir.path(), &["find", &black.to_string()]);
    assert_eq!(
      sat["location"]["satpoint"],
      format!(
        "{}:{}",
        outpoint(coinbase(&core, height as usize), 0),
        BLOCK - 1
      )
    );
  }

  // Untracked sats have no location.
  let sat = ord_json(Some(&core), dir.path(), &["find", "5000000001"]);
  assert_eq!(sat["tracked"], false);
  assert_eq!(sat["location"], Value::Null);
}

#[test]
#[ignore = "needs source"]
fn lost_sats_from_an_under_claiming_coinbase() {
  let core = regtest();
  core.mine_blocks_with_subsidy(1, 25 * COIN);
  let dir = tempdir();
  let server = TestServer::spawn(&core, dir.path(), &[], &[]);
  let status = server.wait_for_tip(&core);

  // Block 1's coinbase claims half its subsidy: the other half is lost.
  assert_eq!(status["lost_sats"], 25 * COIN);

  let black = server.json(&format!("/sat/{}", 2 * BLOCK - 1));
  assert_eq!(black["tracked"], true);
  assert_eq!(black["location"]["kind"], "lost");
  assert_eq!(
    black["location"]["satpoint"],
    format!("{}:{}", OutPoint::null(), 25 * COIN - 1)
  );
  assert_eq!(black["location"]["output"], Value::Null);

  // The uncommon sat was claimed.
  let uncommon = server.json(&format!("/sat/{BLOCK}"));
  assert_eq!(uncommon["location"]["kind"], "unspent");

  let moves = server.json("/block/1/moves");
  let coinbase_move = moves["moves"]
    .as_array()
    .unwrap()
    .iter()
    .find(|m| m["coinbase"] == true)
    .unwrap();
  assert!(
    coinbase_move["lost_pieces"]
      .as_array()
      .unwrap()
      .iter()
      .any(|piece| piece["start"] == 2 * BLOCK - 1)
  );

  assert!(server.terminate().success());
}

#[test]
#[ignore = "needs source"]
fn op_return_burn() {
  let core = regtest();
  core.mine_blocks(1);

  // Block 1's coinbase goes to a transaction whose first output is an
  // OP_RETURN worth 1 sat: its first (uncommon) sat is burned.
  let txid = core.broadcast_tx(TransactionTemplate {
    inputs: &[(1, 0, 0, Witness::new())],
    outputs: 1,
    fee: 1,
    op_return: Some(op_return()),
    op_return_index: Some(0),
    op_return_value: Some(1),
    ..TransactionTemplate::default()
  });
  core.mine_blocks(1);

  let dir = tempdir();
  let server = TestServer::spawn(&core, dir.path(), &[], &[]);
  server.wait_for_tip(&core);

  let sat = server.json(&format!("/sat/{BLOCK}"));
  assert_eq!(sat["location"]["kind"], "burned");
  assert_eq!(
    sat["location"]["satpoint"],
    format!("{}:0", outpoint(txid, 0))
  );
  assert_eq!(sat["location"]["output"]["kind"], "unspendable");
  assert_eq!(sat["location"]["output"]["value"], 1);

  // Block 1's black sat went on to the second output.
  let black = server.json(&format!("/sat/{}", 2 * BLOCK - 1));
  assert_eq!(black["location"]["kind"], "unspent");
  assert_eq!(
    black["location"]["satpoint"],
    format!("{}:{}", outpoint(txid, 1), BLOCK - 2)
  );

  let output = server.json(&format!("/output/{}", outpoint(txid, 0)));
  assert_eq!(output["kind"], "unspendable");
  assert_eq!(output["created_height"], 2);

  let moves = server.json("/block/2/moves");
  let spend = moves["moves"]
    .as_array()
    .unwrap()
    .iter()
    .find(|m| m["txid"] == txid.to_string())
    .unwrap();
  assert_eq!(
    spend["inputs"][0]["outpoint"],
    outpoint(coinbase(&core, 1), 0)
  );
  assert_eq!(spend["outputs"][0]["kind"], "unspendable");

  let lines = server
    .get("/moves?from=0")
    .unwrap()
    .text()
    .unwrap()
    .lines()
    .map(|line| serde_json::from_str::<Value>(line).unwrap())
    .collect::<Vec<Value>>();
  assert!(
    lines
      .iter()
      .any(|line| line["height"] == 2 && line["move"]["txid"] == txid.to_string())
  );
}

#[test]
#[ignore = "needs source"]
fn reorg_via_invalidated_tip() {
  let core = regtest();
  core.mine_blocks(3);
  let dir = tempdir();
  let server = TestServer::spawn(&core, dir.path(), &[], &[]);
  server.wait_for_tip(&core);

  let uncommon = format!("/sat/{}", 3 * BLOCK);
  let before = server.json(&uncommon);
  assert_eq!(
    before["location"]["outpoint"],
    outpoint(coinbase(&core, 3), 0)
  );

  core.invalidate_tip();
  core.mine_blocks(2);
  let status = server.wait_for_tip(&core);
  assert_eq!(status["height"], 4);

  let after = server.json(&uncommon);
  assert_eq!(
    after["location"]["outpoint"],
    outpoint(coinbase(&core, 3), 0)
  );
  assert_ne!(
    after["location"]["outpoint"],
    before["location"]["outpoint"]
  );
  assert_eq!(
    server.json("/block/3")["hash"],
    core.state().hashes[3].to_string()
  );

  assert!(server.terminate().success());
}

#[test]
#[ignore = "needs source"]
fn server_follows_new_blocks_and_serves_supply() {
  let core = regtest();
  core.mine_blocks(2);
  let dir = tempdir();
  let server = TestServer::spawn(&core, dir.path(), &[], &[]);
  server.wait_for_tip(&core);

  let rows = server.json("/rows");
  let ids = rows
    .as_array()
    .unwrap()
    .iter()
    .map(|row| row["id"].as_str().unwrap().to_string())
    .collect::<Vec<String>>();
  assert!(ids.contains(&"uncommon".into()));
  assert!(ids.contains(&"block9".into()));

  core.mine_blocks(10);
  let status = server.wait_for_tip(&core);
  assert_eq!(status["mode"], "live");
  assert_eq!(status["chain"], "regtest");
  assert_eq!(status["ranges"][0]["id"], "block9", "default tracked range");

  let supply = server.json("/supply");
  assert_eq!(supply["height"], 12);
  let uncommon = supply["rows"]
    .as_array()
    .unwrap()
    .iter()
    .find(|row| row["id"] == "uncommon")
    .unwrap();
  assert_eq!(uncommon["stats"]["mined"], 13);
  assert_eq!(uncommon["delta"]["height"], 12);

  let block9 = server.json("/supply/block9?top=5");
  assert_eq!(block9["stats"]["mined"], BLOCK);
  assert!(block9["top_non_extracted_holders"].is_array());

  let history = server.json("/supply/uncommon/history?every=1");
  assert_eq!(history["points"].as_array().unwrap().len(), 13);

  let blocks = server.json("/blocks?from=0");
  assert_eq!(blocks.as_array().unwrap().len(), 13);

  let address = core.address(OutPoint {
    txid: coinbase(&core, 5),
    vout: 0,
  });
  let holdings = server.json(&format!("/address/{address}"));
  assert_eq!(
    holdings["outputs"][0]["outpoint"],
    outpoint(coinbase(&core, 5), 0)
  );

  assert!(server.terminate().success());
}

#[test]
#[ignore = "needs source"]
fn restarts_resume_from_the_index() {
  let core = regtest();
  core.mine_blocks(5);
  let dir = tempdir();

  let server = TestServer::spawn(&core, dir.path(), &[], &[]);
  server.wait_for_tip(&core);
  assert!(server.terminate().success());

  core.mine_blocks(3);
  let server = TestServer::spawn(&core, dir.path(), &[], &[]);
  let status = server.wait_for_tip(&core);
  assert_eq!(status["height"], 8);
  assert_eq!(
    server.json(&format!("/sat/{}", 7 * BLOCK))["location"]["kind"],
    "unspent"
  );

  // The index is locked while a server runs.
  let output = ord(Some(&core), dir.path(), &["index", "info"]);
  assert!(!output.status.success());
  assert!(String::from_utf8_lossy(&output.stderr).contains("in use by another ord process"));
  assert!(server.terminate().success());

  // --no-sync serves the index as it is.
  core.mine_blocks(1);
  let server = TestServer::spawn(&core, dir.path(), &[], &["--no-sync"]);
  thread::sleep(Duration::from_secs(3));
  let status = server.json("/status");
  assert_eq!(status["height"], 8);
  assert_eq!(status["mode"], "starting");
  assert_eq!(server.json("/supply")["height"], 8);
  assert!(server.terminate().success());
}

#[test]
#[ignore = "needs source"]
fn reindex_rebuilds_and_config_changes_need_it() {
  let core = regtest();
  core.mine_blocks(3);
  let dir = tempdir();
  ord_json(Some(&core), dir.path(), &["index", "update"]);

  let output = ord(
    Some(&core),
    dir.path(),
    &["--extracted-max-value", "546", "index", "update"],
  );
  assert!(!output.status.success());
  assert!(String::from_utf8_lossy(&output.stderr).contains("--reindex"));

  let update = ord_json(
    Some(&core),
    dir.path(),
    &[
      "--extracted-max-value",
      "546",
      "--reindex",
      "index",
      "update",
    ],
  );
  assert_eq!(update["height"], 3);
}

#[test]
#[ignore = "needs source"]
fn wrong_chain_is_refused() {
  let core = mockcore::spawn();
  let dir = tempdir();
  let output = ord(Some(&core), dir.path(), &["index", "update"]);
  assert!(!output.status.success());
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("--chain"), "{stderr}");
}
