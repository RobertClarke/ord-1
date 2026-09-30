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
