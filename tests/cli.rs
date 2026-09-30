//! Command-line behaviour that needs neither bitcoind nor the catalog.

use super::*;

#[test]
fn help_is_clean() {
  let output = Command::new(env!("CARGO_BIN_EXE_ord"))
    .arg("--help")
    .output()
    .unwrap();
  assert!(output.status.success());
  assert!(output.stderr.is_empty());
  let help = String::from_utf8(output.stdout).unwrap();
  for command in ["server", "index", "find", "types", "rows"] {
    assert!(help.contains(&format!("  {command} ")), "{help}");
  }
  for option in [
    "--bitcoin-rpc-url",
    "--bitcoin-rpc-username",
    "--bitcoin-rpc-password",
    "--cookie-file",
    "--data-dir",
    "--chain",
    "--commit-interval",
    "--fetch-threads",
    "--fast-sync",
    "--reindex",
    "--moves-retention",
  ] {
    assert!(help.contains(option), "{option} missing from --help");
  }
  assert!(help.contains("ORD_BITCOIN_RPC_URL"));

  for args in [
    &["server", "--help"][..],
    &["index", "--help"],
    &["find", "--help"],
  ] {
    let output = Command::new(env!("CARGO_BIN_EXE_ord"))
      .args(args)
      .output()
      .unwrap();
    assert!(output.status.success(), "{args:?}");
  }
}

#[test]
fn password_is_not_echoed_by_help() {
  let output = Command::new(env!("CARGO_BIN_EXE_ord"))
    .env("ORD_BITCOIN_RPC_PASSWORD", "hunter2")
    .arg("--help")
    .output()
    .unwrap();
  assert!(!String::from_utf8_lossy(&output.stdout).contains("hunter2"));
}

#[test]
fn bad_arguments_fail_with_usage() {
  let dir = tempdir();
  let output = ord(None, dir.path(), &["--moves-retention", "forever", "rows"]);
  assert!(!output.status.success());
  assert!(String::from_utf8_lossy(&output.stderr).contains("`all`"));

  let output = ord(None, dir.path(), &["find", "2099999997690000"]);
  assert!(!output.status.success());
  assert!(String::from_utf8_lossy(&output.stderr).contains("beyond the last sat"));

  let output = ord(
    None,
    dir.path(),
    &["--bitcoin-rpc-username", "alice", "index", "info"],
  );
  assert!(!output.status.success());
  assert!(String::from_utf8_lossy(&output.stderr).contains("--bitcoin-rpc-password"));
}

#[test]
fn index_info_without_an_index() {
  let dir = tempdir();
  let output = ord(None, dir.path(), &["index", "info"]);
  assert!(!output.status.success());
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("there is no index"), "{stderr}");
  assert!(stderr.contains("regtest"), "{stderr}");
}

#[test]
fn options_are_read_from_the_environment() {
  let dir = tempdir();
  let output = Command::new(env!("CARGO_BIN_EXE_ord"))
    .env_clear()
    .env("ORD_CHAIN", "signet")
    .env("ORD_DATA_DIR", dir.path())
    .args(["index", "info"])
    .output()
    .unwrap();
  assert!(!output.status.success());
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains(
      &dir
        .path()
        .join("signet")
        .join("index.redb")
        .display()
        .to_string()
    ),
    "{stderr}"
  );
}

#[test]
fn types_of_a_sat() {
  let dir = tempdir();
  let sat = ord_json(None, dir.path(), &["types", "45000000000"]);
  assert_eq!(sat["sat"], 45_000_000_000u64);
  assert_eq!(sat["height"], 9);
  assert_eq!(sat["tracked"], true);
  for expected in ["uncommon", "alpha", "block9", "450x", "nakamoto"] {
    assert!(
      sat["types"].as_array().unwrap().contains(&expected.into()),
      "{sat}"
    );
  }
  for expected in ["uncommon", "block9", "450x"] {
    assert!(
      sat["rows"].as_array().unwrap().contains(&expected.into()),
      "{sat}"
    );
  }

  let common = ord_json(None, dir.path(), &["types", "5000000001"]);
  assert_eq!(common["tracked"], false);
  assert_eq!(common["rows"], serde_json::json!([]));
}

#[test]
fn rows_lists_definitions_and_totals() {
  let dir = tempdir();
  let rows = ord_json(None, dir.path(), &["rows"]);
  let rows = rows.as_array().unwrap();
  assert!(rows.len() > 50);
  let uncommon = &rows[0];
  assert_eq!(uncommon["id"], "uncommon");
  assert_eq!(uncommon["all_of"], serde_json::json!(["uncommon"]));
  assert!(uncommon["total"].as_u64().unwrap() > 6_000_000);
  let block9 = rows.iter().find(|row| row["id"] == "block9").unwrap();
  assert_eq!(block9["range"], true);
  assert_eq!(block9["total"], 5_000_000_000u64);
  let scoped = rows
    .iter()
    .find(|row| row["id"] == "uncommon_2009")
    .unwrap();
  assert_eq!(scoped["heights"], serde_json::json!([0, 32489]));
}

#[test]
fn find_without_an_index() {
  let dir = tempdir();
  let output = ord(None, dir.path(), &["find", "5000000000"]);
  assert!(!output.status.success());
  assert!(String::from_utf8_lossy(&output.stderr).contains("there is no index"));
}
