//! Integration tests: the `ord` binary against mockcore.
//!
//! Tests marked `#[ignore = "needs core+source"]` need the tracker, the sat
//! catalog and the block sources (including mockcore support for
//! `getblock <hash> 3`); run them with `cargo test --test integration --
//! --ignored` once those are merged.

use {
  bitcoin::{Network, OutPoint, Txid},
  mockcore::Handle,
  reqwest::blocking::Client,
  serde_json::Value,
  std::{
    net::TcpListener,
    path::Path,
    process::{Child, Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant},
  },
  tempfile::TempDir,
};

mod cli;
mod regtest;

const COIN: u64 = 100_000_000;
const BLOCK: u64 = 50 * COIN;

fn ord_command(core: Option<&Handle>, data_dir: &Path) -> Command {
  let mut command = Command::new(env!("CARGO_BIN_EXE_ord"));
  command
    .env_clear()
    .env("RUST_LOG", "ord=debug")
    .env("RUST_BACKTRACE", "1")
    .arg("--chain")
    .arg("regtest")
    .arg("--data-dir")
    .arg(data_dir)
    .arg("--db-cache-mib")
    .arg("64");
  if let Some(core) = core {
    command
      .arg("--bitcoin-rpc-url")
      .arg(core.url())
      .arg("--cookie-file")
      .arg(core.cookie_file());
  }
  command
}

/// Runs `ord <args>` to completion.
fn ord(core: Option<&Handle>, data_dir: &Path, args: &[&str]) -> Output {
  ord_command(core, data_dir)
    .args(args)
    .output()
    .expect("failed to run ord")
}

#[track_caller]
fn ord_json(core: Option<&Handle>, data_dir: &Path, args: &[&str]) -> Value {
  let output = ord(core, data_dir, args);
  assert!(
    output.status.success(),
    "ord {args:?} failed: {}",
    String::from_utf8_lossy(&output.stderr)
  );
  serde_json::from_slice(&output.stdout).unwrap()
}

fn regtest() -> Handle {
  mockcore::builder().network(Network::Regtest).build()
}

fn coinbase(core: &Handle, height: usize) -> Txid {
  core.tx(height, 0).compute_txid()
}

fn free_port() -> u16 {
  TcpListener::bind("127.0.0.1:0")
    .unwrap()
    .local_addr()
    .unwrap()
    .port()
}

/// `ord server` running against mockcore.
struct TestServer {
  child: Option<Child>,
  url: String,
  client: Client,
}

impl TestServer {
  /// `ord <options> server <server_options> --http-port <free port>`.
  fn spawn(core: &Handle, data_dir: &Path, options: &[&str], server_options: &[&str]) -> Self {
    let port = free_port();
    let child = ord_command(Some(core), data_dir)
      .args(options)
      .arg("server")
      .args(server_options)
      .arg("--http-port")
      .arg(port.to_string())
      .stdout(Stdio::null())
      .spawn()
      .expect("failed to spawn ord server");
    let server = Self {
      child: Some(child),
      url: format!("http://127.0.0.1:{port}"),
      client: Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap(),
    };
    server.wait_until(|status| status["height"].is_u64());
    server
  }

  fn get(&self, path: &str) -> reqwest::Result<reqwest::blocking::Response> {
    self.client.get(format!("{}{path}", self.url)).send()
  }

  #[track_caller]
  fn json(&self, path: &str) -> Value {
    let response = self.get(path).unwrap();
    assert_eq!(response.status(), 200, "{path}");
    response.json().unwrap()
  }

  /// Waits until `/status` satisfies `done`.
  #[track_caller]
  fn wait_until(&self, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
      if let Ok(response) = self.get("/status")
        && let Ok(status) = response.json::<Value>()
        && done(&status)
      {
        return status;
      }
      assert!(
        Instant::now() < deadline,
        "timed out waiting for ord server"
      );
      thread::sleep(Duration::from_millis(50));
    }
  }

  #[track_caller]
  fn wait_for_tip(&self, core: &Handle) -> Value {
    let height = core.height();
    let hash = core.state().hashes[height as usize].to_string();
    self.wait_until(|status| status["height"] == height && status["hash"] == hash)
  }

  /// SIGTERM, then the exit status.
  fn terminate(mut self) -> ExitStatus {
    let mut child = self.child.take().unwrap();
    Command::new("kill")
      .arg("-TERM")
      .arg(child.id().to_string())
      .status()
      .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
      if let Some(status) = child.try_wait().unwrap() {
        return status;
      }
      assert!(Instant::now() < deadline, "ord server did not stop");
      thread::sleep(Duration::from_millis(50));
    }
  }
}

impl Drop for TestServer {
  fn drop(&mut self) {
    if let Some(mut child) = self.child.take() {
      let _ = child.kill();
      let _ = child.wait();
    }
  }
}

fn outpoint(txid: Txid, vout: u32) -> String {
  OutPoint { txid, vout }.to_string()
}

fn tempdir() -> TempDir {
  TempDir::new().unwrap()
}
