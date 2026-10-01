//! Server tests: the router on a store holding hand-made blocks, queried
//! over HTTP, with the JSON shapes of docs/API.md.

use {
  super::*,
  crate::{
    indexer::Status,
    model::{
      BlockDelta, Holder, InscriptionEvent, MarketDelta, Move, MoveInput, MoveOutput, Piece,
      RowCounts, RowDelta, RowSupply, Sale, SaleKind, SupplySnapshot, TrackedUtxo, UtxoKind,
    },
    store::{CommitOptions, IndexMeta, SCHEMA_VERSION, StoreOptions},
  },
  bitcoin::{
    CompactTarget, Network, TxMerkleNode, Txid,
    block::{Header, Version},
    hashes::{Hash, sha256d},
  },
  pretty_assertions::assert_eq,
  reqwest::blocking::Client,
  serde_json::{Value, json},
  std::{
    net::TcpListener,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
  },
  tempfile::TempDir,
};

const COIN: u64 = 5_000_000_000;

/// Tracks block 0 whole (range row `block0`), every first (`uncommon`) and
/// last (`black`) sat of a 50 BTC block; `alpha` is every multiple of 1e8.
struct FakeCatalog {
  rows: Vec<RowInfo>,
}

impl FakeCatalog {
  fn new() -> Self {
    let row = |id: &str, types: &[&'static str], range: bool| RowInfo {
      id: id.into(),
      label: id.to_uppercase(),
      all_of: types.to_vec(),
      any_of: Vec::new(),
      heights: None,
      range,
    };
    let mut rows = vec![
      row("uncommon", &["uncommon"], false),
      row("black", &["black"], false),
      row("block0", &["block0"], true),
      row("alpha", &["alpha"], false),
    ];
    rows[3].heights = Some((0, 5923));
    Self { rows }
  }
}

fn multiples(start: u64, end: u64, step: u64, rem: u64) -> u64 {
  // Count of n in [start, end) with n % step == rem.
  let below = |x: u64| {
    if x <= rem {
      0
    } else {
      (x - rem - 1) / step + 1
    }
  };
  below(end) - below(start)
}

impl Catalog for FakeCatalog {
  fn types(&self, sat: u64) -> Vec<&'static str> {
    let mut types = Vec::new();
    if sat.is_multiple_of(COIN) {
      types.push("uncommon");
    }
    if sat % COIN == COIN - 1 {
      types.push("black");
    }
    if sat.is_multiple_of(100_000_000) {
      types.push("alpha");
    }
    if sat < COIN {
      types.push("block0");
    }
    types
  }

  fn is_tracked(&self, sat: u64) -> bool {
    sat < COIN || sat.is_multiple_of(COIN) || sat % COIN == COIN - 1
  }

  fn rows(&self) -> &[RowInfo] {
    &self.rows
  }

  fn row_counts(&self, start: u64, len: u64) -> RowCounts {
    let end = start + len;
    [
      (0, multiples(start, end, COIN, 0)),
      (1, multiples(start, end, COIN, COIN - 1)),
      (2, end.min(COIN).saturating_sub(start.min(COIN))),
      (3, multiples(start, end, 100_000_000, 0)),
    ]
    .into_iter()
    .filter(|(_, count)| *count > 0)
    .collect()
  }

  fn row_total(&self, row: RowId) -> u64 {
    [6_930_000, 6_930_000, COIN, 20_999_999][usize::from(row)]
  }

  fn fingerprint(&self) -> String {
    "fake-catalog".into()
  }
}

fn txid(n: u8) -> Txid {
  Txid::from_raw_hash(sha256d::Hash::hash(&[n]))
}

fn p2pk() -> ScriptBuf {
  let mut bytes = vec![0x41, 0x04];
  bytes.extend([7; 64]);
  bytes.push(0xac);
  ScriptBuf::from_bytes(bytes)
}

fn p2wpkh(n: u8) -> ScriptBuf {
  let mut bytes = vec![0x00, 0x14];
  bytes.extend([n; 20]);
  ScriptBuf::from_bytes(bytes)
}

fn p2tr() -> ScriptBuf {
  let mut bytes = vec![0x51, 0x20];
  bytes.extend([9; 32]);
  ScriptBuf::from_bytes(bytes)
}

fn op_return() -> ScriptBuf {
  ScriptBuf::from_bytes(vec![0x6a, 0x04, 1, 2, 3, 4])
}

fn address(script: &ScriptBuf) -> Value {
  Address::from_script(script, Network::Regtest)
    .map(|address| json!(address.to_string()))
    .unwrap_or(Value::Null)
}

const TIMES: [u32; 4] = [1_231_006_505, 1_231_469_665, 1_700_000_000, 1_700_000_600];

fn piece(offset: u64, start: u64, len: u64) -> Piece {
  Piece { offset, start, len }
}

fn utxo(value: u64, script: ScriptBuf, height: u32, pieces: Vec<Piece>) -> TrackedUtxo {
  TrackedUtxo {
    value,
    kind: if script.is_op_return() {
      UtxoKind::Unspendable
    } else {
      UtxoKind::Spendable
    },
    script_pubkey: script,
    height,
    pieces,
  }
}

fn move_output(vout: u32, utxo: &TrackedUtxo) -> MoveOutput {
  MoveOutput {
    vout,
    value: utxo.value,
    script_pubkey: utxo.script_pubkey.clone(),
    kind: utxo.kind,
    pieces: utxo.pieces.clone(),
  }
}

fn coinbase(txid: Txid, outputs: Vec<MoveOutput>, mined: Vec<Piece>, lost: Vec<Piece>) -> Move {
  Move {
    txid,
    tx_index: 0,
    coinbase: true,
    input_value: 0,
    output_value: outputs.iter().map(|output| output.value).sum(),
    inputs: Vec::new(),
    outputs,
    fee: Vec::new(),
    mined,
    lost,
  }
}

fn stats(mined: u64, located: u64, burned: u64, lost: u64, extracted: u64) -> RowStats {
  RowStats {
    mined,
    located,
    burned,
    lost,
    overwritten: 0,
    extracted,
    holders: located.min(3),
    extracted_holders: extracted.min(1),
  }
}

struct Scenario {
  a: OutPoint,
  b: OutPoint,
  t0: OutPoint,
  t1: OutPoint,
  c: OutPoint,
  u0: OutPoint,
  u1: OutPoint,
  d: OutPoint,
  deltas: Vec<BlockDelta>,
  markets: Vec<MarketDelta>,
}

impl Scenario {
  /// Commits blocks `range` with their market deltas.
  fn commit(&self, store: &Store, range: std::ops::Range<usize>) {
    store
      .commit_with(
        &self.deltas[range.clone()],
        &self.markets[range],
        &[4, 4, COIN, 53],
        true,
        &CommitOptions {
          durable: true,
          target_height: None,
        },
      )
      .unwrap();
  }
}

/// Block 0: coinbase A holds block 0's sats but the last (lost).
/// Block 1: coinbase B holds its uncommon and black sats.
/// Block 2: T spends B: T:0 (546 sats, P2TR, extracted) gets the uncommon,
///   T:1 the black; 9,454 sats of fee; coinbase C.
/// Block 3: U spends C: U:0 (OP_RETURN) burns C's uncommon, U:1 the black;
///   coinbase D.
/// Market: block 1 inscribes B's uncommon (sat 5e9), block 2's spend of B
/// is a Magisat listing sale (to T:0, 45,000 sats of premium), block 3
/// reinscribes sat 5e9 and inscribes sat 7 (in A).
fn scenario() -> Scenario {
  let (a, b, c, d) = (
    OutPoint::new(txid(0), 0),
    OutPoint::new(txid(1), 0),
    OutPoint::new(txid(2), 0),
    OutPoint::new(txid(3), 0),
  );
  let (t0, t1) = (OutPoint::new(txid(20), 0), OutPoint::new(txid(20), 1));
  let (u0, u1) = (OutPoint::new(txid(30), 0), OutPoint::new(txid(30), 1));

  let utxo_a = utxo(COIN - 1, p2pk(), 0, vec![piece(0, 0, COIN - 1)]);
  let utxo_b = utxo(
    COIN,
    p2wpkh(1),
    1,
    vec![piece(0, COIN, 1), piece(COIN - 1, 2 * COIN - 1, 1)],
  );
  let utxo_t0 = utxo(546, p2tr(), 2, vec![piece(0, COIN, 1)]);
  let utxo_t1 = utxo(
    4_999_990_000,
    p2wpkh(2),
    2,
    vec![piece(COIN - 1 - 546, 2 * COIN - 1, 1)],
  );
  let utxo_c = utxo(
    COIN + 9_454,
    p2wpkh(3),
    2,
    vec![piece(0, 2 * COIN, 1), piece(COIN - 1, 3 * COIN - 1, 1)],
  );
  let utxo_u0 = utxo(1_000, op_return(), 3, vec![piece(0, 2 * COIN, 1)]);
  let utxo_u1 = utxo(
    COIN + 8_454,
    p2wpkh(3),
    3,
    vec![piece(COIN - 1 - 1_000, 3 * COIN - 1, 1)],
  );
  let utxo_d = utxo(
    COIN,
    p2wpkh(4),
    3,
    vec![piece(0, 3 * COIN, 1), piece(COIN - 1, 4 * COIN - 1, 1)],
  );

  let mut deltas = Vec::new();
  let mut prev = None;
  let mut push = |height: u32,
                  moves: Vec<Move>,
                  spent: Vec<(OutPoint, TrackedUtxo)>,
                  created: Vec<(OutPoint, TrackedUtxo)>,
                  lost: Vec<Piece>,
                  rows: Vec<RowDelta>| {
    let header = Header {
      version: Version::ONE,
      prev_blockhash: prev.unwrap_or_else(BlockHash::all_zeros),
      merkle_root: TxMerkleNode::all_zeros(),
      time: TIMES[height as usize],
      bits: CompactTarget::from_consensus(0x207f_ffff),
      nonce: height,
    };
    prev = Some(header.block_hash());
    deltas.push(BlockDelta {
      height,
      hash: header.block_hash(),
      header,
      moves,
      spent,
      created,
      overwritten: Vec::new(),
      lost,
      lost_sats_before: u64::from(height > 0),
      lost_sats_after: 1,
      rows,
    });
  };

  push(
    0,
    vec![coinbase(
      a.txid,
      vec![move_output(0, &utxo_a)],
      vec![piece(0, 0, COIN)],
      vec![piece(0, COIN - 1, 1)],
    )],
    Vec::new(),
    vec![(a, utxo_a.clone())],
    vec![piece(0, COIN - 1, 1)],
    vec![
      RowDelta {
        row: 0,
        before: RowStats::default(),
        after: stats(1, 1, 0, 0, 0),
      },
      RowDelta {
        row: 1,
        before: RowStats::default(),
        after: stats(1, 0, 0, 1, 0),
      },
      RowDelta {
        row: 2,
        before: RowStats::default(),
        after: stats(COIN, COIN - 1, 0, 1, 0),
      },
      RowDelta {
        row: 3,
        before: RowStats::default(),
        after: stats(50, 50, 0, 0, 0),
      },
    ],
  );

  push(
    1,
    vec![coinbase(
      b.txid,
      vec![move_output(0, &utxo_b)],
      utxo_b.pieces.clone(),
      Vec::new(),
    )],
    Vec::new(),
    vec![(b, utxo_b.clone())],
    Vec::new(),
    vec![
      RowDelta {
        row: 0,
        before: stats(1, 1, 0, 0, 0),
        after: stats(2, 2, 0, 0, 0),
      },
      RowDelta {
        row: 1,
        before: stats(1, 0, 0, 1, 0),
        after: stats(2, 1, 0, 1, 0),
      },
      RowDelta {
        row: 3,
        before: stats(50, 50, 0, 0, 0),
        after: stats(51, 51, 0, 0, 0),
      },
    ],
  );

  push(
    2,
    vec![
      Move {
        txid: t0.txid,
        tx_index: 1,
        coinbase: false,
        input_value: COIN,
        output_value: 546 + 4_999_990_000,
        inputs: vec![MoveInput {
          vin: 0,
          outpoint: b,
          utxo: utxo_b.clone(),
        }],
        outputs: vec![move_output(0, &utxo_t0), move_output(1, &utxo_t1)],
        fee: Vec::new(),
        mined: Vec::new(),
        lost: Vec::new(),
      },
      coinbase(
        c.txid,
        vec![move_output(0, &utxo_c)],
        utxo_c.pieces.clone(),
        Vec::new(),
      ),
    ],
    vec![(b, utxo_b.clone())],
    vec![
      (t0, utxo_t0.clone()),
      (t1, utxo_t1.clone()),
      (c, utxo_c.clone()),
    ],
    Vec::new(),
    vec![
      RowDelta {
        row: 0,
        before: stats(2, 2, 0, 0, 0),
        after: stats(3, 3, 0, 0, 1),
      },
      RowDelta {
        row: 1,
        before: stats(2, 1, 0, 1, 0),
        after: stats(3, 2, 0, 1, 0),
      },
      RowDelta {
        row: 3,
        before: stats(51, 51, 0, 0, 0),
        after: stats(52, 52, 0, 0, 1),
      },
    ],
  );

  push(
    3,
    vec![
      Move {
        txid: u0.txid,
        tx_index: 1,
        coinbase: false,
        input_value: COIN + 9_454,
        output_value: COIN + 9_454,
        inputs: vec![MoveInput {
          vin: 0,
          outpoint: c,
          utxo: utxo_c.clone(),
        }],
        outputs: vec![move_output(0, &utxo_u0), move_output(1, &utxo_u1)],
        fee: Vec::new(),
        mined: Vec::new(),
        lost: Vec::new(),
      },
      coinbase(
        d.txid,
        vec![move_output(0, &utxo_d)],
        utxo_d.pieces.clone(),
        Vec::new(),
      ),
    ],
    vec![(c, utxo_c.clone())],
    vec![
      (u0, utxo_u0.clone()),
      (u1, utxo_u1.clone()),
      (d, utxo_d.clone()),
    ],
    Vec::new(),
    vec![
      RowDelta {
        row: 0,
        before: stats(3, 3, 0, 0, 1),
        after: stats(4, 3, 1, 0, 1),
      },
      RowDelta {
        row: 1,
        before: stats(3, 2, 0, 1, 0),
        after: stats(4, 3, 0, 1, 0),
      },
      RowDelta {
        row: 3,
        before: stats(52, 52, 0, 0, 1),
        after: stats(53, 52, 1, 0, 1),
      },
    ],
  );

  let markets = vec![
    MarketDelta::default(),
    MarketDelta {
      sales: Vec::new(),
      inscriptions: vec![InscriptionEvent {
        sat: COIN,
        txid: txid(10),
        vin: 0,
        tx_index: 1,
      }],
    },
    MarketDelta {
      sales: vec![Sale {
        txid: t0.txid,
        tx_index: 1,
        vin: 0,
        kind: SaleKind::Listing,
        marketplace: "magisat".into(),
        seller_outpoint: b,
        seller: utxo_b.clone(),
        payment_vout: Some(1),
        price: COIN + 45_000,
        seller_proceeds: COIN + 44_773,
        fees: 227,
        buyer_vout: Some(0),
        buyer_script: Some(p2tr()),
      }],
      inscriptions: Vec::new(),
    },
    MarketDelta {
      sales: Vec::new(),
      inscriptions: vec![
        InscriptionEvent {
          sat: COIN,
          txid: txid(11),
          vin: 0,
          tx_index: 1,
        },
        InscriptionEvent {
          sat: 7,
          txid: txid(11),
          vin: 1,
          tx_index: 1,
        },
      ],
    },
  ];

  Scenario {
    a,
    b,
    t0,
    t1,
    c,
    u0,
    u1,
    d,
    deltas,
    markets,
  }
}

struct TestServer {
  _dir: TempDir,
  url: String,
  client: Client,
  scenario: Scenario,
  store: Arc<Store>,
  published: Arc<Published>,
  state: Arc<ServerState>,
  shutdown: Option<tokio::sync::oneshot::Sender<()>>,
  thread: Option<thread::JoinHandle<()>>,
}

impl TestServer {
  /// The scenario indexed up to `height` (nothing when `None`).
  fn new(height: Option<u32>) -> Self {
    Self::with_retention(height, MovesRetention::All)
  }

  fn open_store(dir: &TempDir, retention: MovesRetention) -> Arc<Store> {
    let store = Arc::new(
      Store::open_with(
        &dir.path().join("index.redb"),
        StoreOptions {
          cache_size: 16 << 20,
          moves_retention: retention,
          quick_repair: false,
        },
      )
      .unwrap(),
    );
    store
      .init(&IndexMeta {
        schema_version: SCHEMA_VERSION,
        chain: "regtest".into(),
        config: TrackerConfig::default(),
        catalog_fingerprint: "fake-catalog".into(),
        height: None,
        hash: None,
        fast_sync_complete: false,
      })
      .unwrap();
    store
  }

  /// A server over `blocks` generated blocks (every block's moves kept) and
  /// the generator, to commit more.
  fn generated(
    blocks: usize,
    txs_per_block: u64,
    stream_timeout: Duration,
  ) -> (Self, crate::store::tests::TestChain) {
    let dir = TempDir::new().unwrap();
    let store = Self::open_store(&dir, MovesRetention::All);
    let mut chain = crate::store::tests::TestChain::new(21);
    chain.txs_per_block = txs_per_block;
    let deltas = (0..blocks).map(|_| chain.next_block()).collect::<Vec<_>>();
    store.commit(&deltas, &chain.mined(), true).unwrap();
    let server = Self::serve(
      dir,
      store,
      Arc::new(Published::default()),
      MovesRetention::All,
      stream_timeout,
    );
    (server, chain)
  }

  fn with_retention(height: Option<u32>, retention: MovesRetention) -> Self {
    let dir = TempDir::new().unwrap();
    let store = Self::open_store(&dir, retention);

    let scenario = scenario();
    let published = Arc::new(Published::default());
    if let Some(height) = height {
      let deltas = &scenario.deltas[..=height as usize];
      scenario.commit(&store, 0..height as usize + 1);
      let last = deltas.last().unwrap();
      published.update_status(|status| {
        *status = Status {
          mode: Mode::Live,
          height: Some(last.height),
          hash: Some(last.hash),
          time: Some(last.header.time),
          bitcoind_height: Some(last.height + 1),
          fast_sync_complete: true,
          tracked_utxos: 6,
          tracked_pieces: 9,
          lost_sats: 1,
          ..Status::default()
        };
      });
      published.set_supply(SupplySnapshot {
        height: Some(last.height),
        hash: Some(last.hash),
        time: Some(last.header.time),
        rows: (0..4)
          .map(|row| RowSupply {
            row,
            stats: stats(4, 3, 1, 0, 1),
            active_non_extracted: vec![(1, 1), (2, 2), (3, 2), (4, 2), (5, 2), (7, 2)],
            top_extracted_holders: vec![Holder {
              script_pubkey: p2tr(),
              count: 1,
              utxos: 1,
              last_received_height: 2,
            }],
            top_non_extracted_holders: vec![
              Holder {
                script_pubkey: p2wpkh(4),
                count: 1,
                utxos: 1,
                last_received_height: 3,
              },
              Holder {
                script_pubkey: p2pk(),
                count: 1,
                utxos: 1,
                last_received_height: 0,
              },
            ],
          })
          .collect(),
      });
    }

    Self::serve(dir, store, published, retention, STREAM_TIMEOUT)
  }

  fn serve(
    dir: TempDir,
    store: Arc<Store>,
    published: Arc<Published>,
    retention: MovesRetention,
    stream_timeout: Duration,
  ) -> Self {
    let state = Arc::new(ServerState {
      store: store.clone(),
      catalog: Arc::new(FakeCatalog::new()),
      published: published.clone(),
      chain: Chain::Regtest,
      config: TrackerConfig::default(),
      moves_retention: retention,
      started: 1_700_000_001,
      stream_timeout,
      streams: Default::default(),
      inscribed_rows: Default::default(),
    });

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (shutdown, signal) = tokio::sync::oneshot::channel::<()>();
    let thread = thread::spawn({
      let state = state.clone();
      move || {
        tokio::runtime::Builder::new_multi_thread()
          .worker_threads(2)
          .enable_all()
          .build()
          .unwrap()
          .block_on(async move {
            serve(
              state,
              tokio::net::TcpListener::from_std(listener).unwrap(),
              async {
                signal.await.ok();
              },
            )
            .await
            .unwrap();
          });
      }
    });

    Self {
      _dir: dir,
      url,
      client: Client::new(),
      scenario: scenario(),
      store,
      published,
      state,
      shutdown: Some(shutdown),
      thread: Some(thread),
    }
  }

  fn get(&self, path: &str) -> reqwest::blocking::Response {
    self
      .client
      .get(format!("{}{path}", self.url))
      .send()
      .unwrap()
  }

  #[track_caller]
  fn json(&self, path: &str) -> Value {
    let response = self.get(path);
    assert_eq!(response.status(), 200, "{path}");
    assert_eq!(
      response.headers()[header::CONTENT_TYPE],
      "application/json",
      "{path}"
    );
    response.json().unwrap()
  }

  #[track_caller]
  fn error(&self, path: &str, status: u16) -> String {
    let response = self.get(path);
    assert_eq!(response.status(), status, "{path}");
    let body = response.json::<Value>().unwrap();
    body["error"].as_str().unwrap().to_string()
  }

  #[track_caller]
  fn ndjson(&self, path: &str) -> Vec<Value> {
    let response = self.get(path);
    assert_eq!(response.status(), 200, "{path}");
    assert_eq!(
      response.headers()[header::CONTENT_TYPE],
      "application/x-ndjson"
    );
    response
      .text()
      .unwrap()
      .lines()
      .map(|line| serde_json::from_str(line).unwrap())
      .collect()
  }

  fn hash(&self, height: u32) -> String {
    self.scenario.deltas[height as usize].hash.to_string()
  }
}

impl Drop for TestServer {
  fn drop(&mut self) {
    self.shutdown.take().unwrap().send(()).ok();
    self.thread.take().unwrap().join().unwrap();
  }
}

fn piece_json(offset: u64, start: u64, len: u64, types: &[&str]) -> Value {
  json!({ "offset": offset, "start": start, "len": len, "types": types })
}

#[test]
fn status() {
  let server = TestServer::new(Some(3));
  let status = server.json("/status");
  assert_eq!(
    status,
    json!({
      "chain": "regtest",
      "version": env!("CARGO_PKG_VERSION"),
      "commit": status["commit"],
      "height": 3,
      "hash": server.hash(3),
      "time": TIMES[3],
      "bitcoind_height": 4,
      "mode": "live",
      "fast_sync_complete": true,
      "started": 1_700_000_001,
      "tracked_utxos": 6,
      "tracked_pieces": 9,
      "lost_sats": 1,
      "ranges": serde_json::to_value(TrackerConfig::default().ranges).unwrap(),
      "extracted_max_value": 10_000,
      "extracted_since": 1_670_976_000,
      "catalog_fingerprint": "fake-catalog",
      "moves_retention": "all",
      "moves_from": 0,
      "timings": status["timings"],
      "error": null,
    })
  );
  assert!(status["timings"]["blocks_per_second"].is_number());
  for key in ["last_commit_at", "last_poll_ok_at"] {
    assert!(status["timings"].get(key).is_some(), "timings.{key}");
  }

  // `ord server --no-sync`.
  server
    .published
    .update_status(|status| status.mode = Mode::Serving);
  assert_eq!(server.json("/status")["mode"], "serving");

  // Before the first commit, `height` is null.
  let server = TestServer::new(None);
  let status = server.json("/status");
  assert_eq!(status["height"], Value::Null);
  assert_eq!(status["mode"], "starting");
}

#[test]
fn rows() {
  let server = TestServer::new(None);
  let rows = server.json("/rows");
  assert_eq!(
    rows[0],
    json!({ "id": "uncommon", "label": "UNCOMMON", "all_of": ["uncommon"], "any_of": [],
            "heights": null, "range": false, "total": 6_930_000 })
  );
  assert_eq!(rows[2]["range"], true);
  assert_eq!(rows[3]["heights"], json!([0, 5923]));
  assert_eq!(rows.as_array().unwrap().len(), 4);
}

#[test]
fn supply() {
  let server = TestServer::new(Some(3));
  let supply = server.json("/supply");
  assert_eq!(supply["height"], 3);
  assert_eq!(supply["hash"], server.hash(3));
  assert_eq!(supply["time"], TIMES[3]);
  let uncommon = &supply["rows"][0];
  assert_eq!(
    uncommon,
    &json!({
      "id": "uncommon",
      "total": 6_930_000,
      "stats": { "mined": 4, "located": 3, "burned": 1, "lost": 0, "overwritten": 0,
                 "extracted": 1, "holders": 3, "extracted_holders": 1, "inscribed": 1 },
      "active_non_extracted": { "1y": 1, "2y": 2, "3y": 2, "4y": 2, "5y": 2, "7y": 2 },
      "delta": {
        "height": 3,
        "before": { "mined": 3, "located": 3, "burned": 0, "lost": 0, "overwritten": 0,
                    "extracted": 1, "holders": 3, "extracted_holders": 1 },
        "after": { "mined": 4, "located": 3, "burned": 1, "lost": 0, "overwritten": 0,
                   "extracted": 1, "holders": 3, "extracted_holders": 1 },
      },
    })
  );
  // block0 did not change in block 3.
  assert_eq!(supply["rows"][2]["id"], "block0");
  assert_eq!(supply["rows"][2]["delta"], Value::Null);
  // Located inscribed sats: 5e9 (uncommon, alpha) at T:0, 7 (block0) in A.
  assert_eq!(
    supply["rows"]
      .as_array()
      .unwrap()
      .iter()
      .map(|row| row["stats"]["inscribed"].as_u64().unwrap())
      .collect::<Vec<u64>>(),
    [1, 0, 1, 1]
  );

  let row = server.json("/supply/uncommon?top=1");
  assert_eq!(row["id"], "uncommon");
  assert_eq!(row["delta"]["height"], 3);
  assert_eq!(
    row["top_extracted_holders"],
    json!([{ "address": address(&p2tr()), "script_pubkey": p2tr().to_hex_string(), "n": 1,
             "utxos": 1, "last_received_height": 2, "last_received_time": TIMES[2] }])
  );
  assert_eq!(
    row["top_non_extracted_holders"],
    json!([{ "address": address(&p2wpkh(4)), "script_pubkey": p2wpkh(4).to_hex_string(),
             "n": 1, "utxos": 1, "last_received_height": 3, "last_received_time": TIMES[3] }])
  );
  let row = server.json("/supply/uncommon");
  assert_eq!(
    row["top_non_extracted_holders"][1]["address"],
    Value::Null,
    "P2PK has no address"
  );

  assert!(
    server
      .error("/supply/uncommon?top=1001", 400)
      .contains("1000")
  );
  assert!(server.error("/supply/nope", 404).contains("nope"));
  assert!(server.error("/supply/uncommon?top=x", 400).contains("top"));

  let server = TestServer::new(None);
  server.error("/supply", 503);
  server.error("/supply/uncommon", 503);
}

#[test]
fn supply_history() {
  let server = TestServer::new(Some(3));
  let history = server.json("/supply/black/history?every=1");
  assert_eq!(history["row"], "black");
  let points = history["points"].as_array().unwrap();
  assert_eq!(points.len(), 4);
  assert_eq!(points[0]["height"], 0);
  assert_eq!(points[0]["time"], TIMES[0]);
  assert_eq!(points[0]["stats"]["lost"], 1);
  assert_eq!(points[3]["stats"]["located"], 3);

  // Buckets of 2 report the value at their last block; unchanged rows
  // carry their last value forward.
  let history = server.json("/supply/block0/history?every=2");
  let points = history["points"].as_array().unwrap();
  assert_eq!(
    points
      .iter()
      .map(|point| point["height"].as_u64().unwrap())
      .collect::<Vec<u64>>(),
    [1, 3]
  );
  assert_eq!(points[1]["stats"]["mined"], COIN);

  let history = server.json("/supply/uncommon/history?from=2&to=2");
  assert_eq!(history["points"].as_array().unwrap().len(), 1);
  assert_eq!(history["points"][0]["stats"]["mined"], 3);

  // Default `every` keeps ≤ 2,000 points.
  assert_eq!(
    server.json("/supply/uncommon/history")["points"]
      .as_array()
      .unwrap()
      .len(),
    4
  );

  // A huge `every` is one bucket ending at `to`.
  let history = server.json("/supply/uncommon/history?from=1&every=4294967295");
  assert_eq!(history["points"][0]["height"], 3);

  server.error("/supply/uncommon/history?from=3&to=1", 400);
  server.error("/supply/uncommon/history?every=0", 400);
  server.error("/supply/nope/history", 404);
}

#[test]
fn sat() {
  let server = TestServer::new(Some(3));
  let a = server.scenario.a;

  let output_a = json!({
    "outpoint": a.to_string(),
    "value": COIN - 1,
    "address": null,
    "script_pubkey": p2pk().to_hex_string(),
    "kind": "spendable",
    "created_height": 0,
    "created_time": TIMES[0],
    "extracted": false,
    "pieces": [piece_json(0, 0, COIN - 1, &["uncommon", "alpha", "block0"])],
    "rows": { "uncommon": 1, "block0": COIN - 1, "alpha": 50 },
    "inscribed_sats": [7],
  });

  assert_eq!(
    server.json("/sat/7"),
    json!({
      "sat": 7,
      "height": 0,
      "types": ["block0"],
      "tracked": true,
      "rows": ["block0"],
      "location": {
        "kind": "unspent",
        "satpoint": format!("{a}:7"),
        "outpoint": a.to_string(),
        "offset": 7,
        "output": output_a,
      },
      "inscribed": true,
      "inscriptions": 1,
      "sales": [],
    })
  );

  let lost = server.json("/sat/4999999999");
  assert_eq!(lost["rows"], json!(["black", "block0"]));
  assert_eq!(
    lost["location"],
    json!({
      "kind": "lost",
      "satpoint": "0000000000000000000000000000000000000000000000000000000000000000:4294967295:0",
      "outpoint": "0000000000000000000000000000000000000000000000000000000000000000:4294967295",
      "offset": 0,
      "output": null,
    })
  );

  let extracted = server.json(&format!("/sat/{COIN}"));
  assert_eq!(extracted["height"], 1);
  assert_eq!(extracted["types"], json!(["uncommon", "alpha"]));
  assert_eq!(extracted["rows"], json!(["uncommon", "alpha"]));
  assert_eq!(extracted["location"]["kind"], "unspent");
  assert_eq!(
    extracted["location"]["satpoint"],
    format!("{}:0", server.scenario.t0)
  );
  assert_eq!(extracted["location"]["output"]["extracted"], true);
  assert_eq!(extracted["location"]["output"]["address"], address(&p2tr()));

  let burned = server.json(&format!("/sat/{}", 2 * COIN));
  assert_eq!(burned["location"]["kind"], "burned");
  assert_eq!(
    burned["location"]["outpoint"],
    server.scenario.u0.to_string()
  );
  assert_eq!(burned["location"]["output"]["kind"], "unspendable");
  assert_eq!(
    burned["location"]["output"]["extracted"], false,
    "unspendable outputs are never extracted"
  );

  let black = server.json(&format!("/sat/{}", 2 * COIN - 1));
  assert_eq!(
    black["location"]["satpoint"],
    format!("{}:{}", server.scenario.t1, COIN - 1 - 546)
  );

  assert_eq!(
    server.json(&format!("/sat/{}", COIN + 1)),
    json!({ "sat": COIN + 1, "height": 1, "types": [], "tracked": false, "rows": [],
            "location": null, "inscribed": false, "inscriptions": 0, "sales": [] })
  );
  // Tracked but not mined yet.
  let future = server.json(&format!("/sat/{}", 100 * COIN));
  assert_eq!(future["tracked"], true);
  assert_eq!(future["location"], Value::Null);

  server.error("/sat/x", 400);
  server.error("/sat/-1", 400);
  assert!(
    server
      .error("/sat/2099999997690000", 400)
      .contains("beyond")
  );
  server.json("/sat/2099999997689999");

  TestServer::new(None).error("/sat/0", 503);
}

#[test]
fn output_endpoint() {
  let server = TestServer::new(Some(3));
  let Scenario { t1, u1, d, b, .. } = server.scenario;
  assert_eq!(
    server.json(&format!("/output/{t1}")),
    json!({
      "outpoint": t1.to_string(),
      "value": 4_999_990_000u64,
      "address": address(&p2wpkh(2)),
      "script_pubkey": p2wpkh(2).to_hex_string(),
      "kind": "spendable",
      "created_height": 2,
      "created_time": TIMES[2],
      "extracted": false,
      "pieces": [piece_json(COIN - 1 - 546, 2 * COIN - 1, 1, &["black"])],
      "rows": { "black": 1 },
      "inscribed_sats": [],
    })
  );
  assert_eq!(
    server.json(&format!("/output/{d}"))["rows"],
    json!({ "uncommon": 1, "black": 1, "alpha": 1 })
  );
  server.json(&format!("/output/{u1}"));
  assert!(
    server
      .error(&format!("/output/{b}"), 404)
      .contains("no tracked sat")
  );
  server.error("/output/nonsense", 400);
  server.error(&format!("/output/{}", b.txid), 400);
}

/// The per-output flag is `model::output_extracted` (DESIGN.md §5): the
/// value against the pieces' budget, 10,000 sats per individually tracked
/// sat, a run of range (block 9) sats its length plus 10,000.
#[test]
fn extracted_flag_follows_the_budget() {
  let server = TestServer::new(Some(3));
  let reader = server.store.reader().unwrap();
  let render = server.state.render(&reader);
  let outpoint = OutPoint::new(txid(99), 0);
  // Two tracked sats, created by block 2 (after extracted_since).
  let two = |value| {
    utxo(
      value,
      p2wpkh(5),
      2,
      vec![piece(0, COIN, 1), piece(330, 2 * COIN, 1)],
    )
  };
  let flag = |utxo: &TrackedUtxo| render.output(outpoint, utxo).unwrap().extracted;
  assert!(flag(&two(20_000)));
  assert!(!flag(&two(20_001)));
  // 100,000 sats of block 9 (the config's range) behind 438 sats of padding.
  let chunk = utxo(
    100_438,
    p2wpkh(5),
    2,
    vec![piece(438, 45_000_000_000, 100_000)],
  );
  assert!(flag(&chunk));
  assert!(!flag(&utxo(
    110_001,
    p2wpkh(5),
    2,
    vec![piece(0, 45_000_000_000, 100_000)]
  )));
  // Created before extracted_since, or unspendable: never.
  assert!(!flag(&utxo(1, p2wpkh(5), 1, vec![piece(0, COIN, 1)])));
  assert!(!flag(&utxo(1, op_return(), 2, vec![piece(0, COIN, 1)])));

  let m = Move {
    txid: txid(99),
    tx_index: 1,
    coinbase: false,
    input_value: 40_001,
    output_value: 40_001,
    inputs: vec![MoveInput {
      vin: 0,
      outpoint: OutPoint::new(txid(98), 0),
      utxo: two(15_000),
    }],
    outputs: vec![move_output(0, &two(20_000)), move_output(1, &two(20_001))],
    fee: Vec::new(),
    mined: Vec::new(),
    lost: Vec::new(),
  };
  let json = render.move_json(&m, Some(TIMES[2])).unwrap();
  assert!(json.inputs[0].extracted);
  assert_eq!(
    json
      .outputs
      .iter()
      .map(|output| output.extracted)
      .collect::<Vec<_>>(),
    [true, false]
  );
  // An unknown block time: not extracted.
  assert!(!render.move_json(&m, None).unwrap().outputs[0].extracted);
}

#[test]
fn address_and_script() {
  let server = TestServer::new(Some(3));
  let Scenario { c, u1, .. } = server.scenario;
  let _ = c;

  let by_script = server.json(&format!("/script/{}", p2wpkh(3).to_hex_string()));
  assert_eq!(by_script["address"], address(&p2wpkh(3)));
  assert_eq!(by_script["script_pubkey"], p2wpkh(3).to_hex_string());
  assert_eq!(
    by_script["outputs"]
      .as_array()
      .unwrap()
      .iter()
      .map(|output| output["outpoint"].as_str().unwrap().to_string())
      .collect::<Vec<String>>(),
    [u1.to_string()]
  );
  assert_eq!(by_script["rows"], json!({ "black": 1 }));

  let by_address = server.json(&format!(
    "/address/{}",
    address(&p2wpkh(3)).as_str().unwrap()
  ));
  assert_eq!(by_address, by_script);

  // P2PK: no address, but the script works.
  let p2pk_outputs = server.json(&format!("/script/{}", p2pk().to_hex_string()));
  assert_eq!(p2pk_outputs["address"], Value::Null);
  assert_eq!(p2pk_outputs["outputs"].as_array().unwrap().len(), 1);

  // Burned outputs are found by their script too.
  assert_eq!(
    server.json(&format!("/script/{}", op_return().to_hex_string()))["outputs"][0]["kind"],
    "unspendable"
  );

  // Nothing tracked: empty.
  let empty = server.json(&format!("/script/{}", p2wpkh(99).to_hex_string()));
  assert_eq!(empty["outputs"], json!([]));
  assert_eq!(empty["rows"], json!({}));

  // A mainnet address on regtest.
  assert!(
    server
      .error("/address/bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq", 400)
      .contains("regtest")
  );
  server.error("/address/nonsense", 400);
  server.error("/script/zz", 400);
}

#[test]
fn block_and_moves() {
  let server = TestServer::new(Some(3));
  let Scenario { b, t0, t1, c, .. } = server.scenario;

  assert_eq!(
    server.json("/block/2"),
    json!({
      "height": 2,
      "hash": server.hash(2),
      "prev_hash": server.hash(1),
      "time": TIMES[2],
      "moves": 2,
      "rows_moved": { "uncommon": 2, "black": 2, "alpha": 2 },
    })
  );
  assert_eq!(
    server.json("/block/0")["rows_moved"],
    json!({ "uncommon": 1, "black": 1, "block0": COIN, "alpha": 50 })
  );
  server.error("/block/4", 404);
  server.error("/block/x", 400);

  let moves = server.json("/block/2/moves");
  assert_eq!(moves["height"], 2);
  assert_eq!(moves["hash"], server.hash(2));
  assert_eq!(moves["time"], TIMES[2]);
  assert_eq!(
    moves["moves"][0],
    json!({
      "txid": t0.txid.to_string(),
      "tx_index": 1,
      "coinbase": false,
      "input_value": COIN,
      "output_value": 546 + 4_999_990_000u64,
      "fee": 9_454,
      "inputs": [{
        "vin": 0,
        "outpoint": b.to_string(),
        "value": COIN,
        "address": address(&p2wpkh(1)),
        "script_pubkey": p2wpkh(1).to_hex_string(),
        "created_height": 1,
        "created_time": TIMES[1],
        "extracted": false,
        "pieces": [
          piece_json(0, COIN, 1, &["uncommon", "alpha"]),
          piece_json(COIN - 1, 2 * COIN - 1, 1, &["black"]),
        ],
      }],
      "outputs": [
        {
          "vout": 0,
          "value": 546,
          "address": address(&p2tr()),
          "script_pubkey": p2tr().to_hex_string(),
          "kind": "spendable",
          "extracted": true,
          "pieces": [piece_json(0, COIN, 1, &["uncommon", "alpha"])],
        },
        {
          "vout": t1.vout,
          "value": 4_999_990_000u64,
          "address": address(&p2wpkh(2)),
          "script_pubkey": p2wpkh(2).to_hex_string(),
          "kind": "spendable",
          "extracted": false,
          "pieces": [piece_json(COIN - 1 - 546, 2 * COIN - 1, 1, &["black"])],
        },
      ],
      "fee_pieces": [],
      "mined_pieces": [],
      "lost_pieces": [],
      "rows": { "uncommon": 1, "black": 1, "alpha": 1 },
    })
  );
  let coinbase = &moves["moves"][1];
  assert_eq!(coinbase["txid"], c.txid.to_string());
  assert_eq!(coinbase["coinbase"], true);
  assert_eq!(coinbase["fee"], 0);
  assert_eq!(coinbase["inputs"], json!([]));
  assert_eq!(
    coinbase["mined_pieces"],
    json!([
      piece_json(0, 2 * COIN, 1, &["uncommon", "alpha"]),
      piece_json(COIN - 1, 3 * COIN - 1, 1, &["black"]),
    ])
  );

  // The genesis coinbase minted block 0 and lost its last sat.
  let genesis = server.json("/block/0/moves");
  assert_eq!(
    genesis["moves"][0]["lost_pieces"],
    json!([piece_json(0, COIN - 1, 1, &["black", "block0"])])
  );
  assert_eq!(
    genesis["moves"][0]["rows"],
    json!({ "uncommon": 1, "black": 1, "block0": COIN, "alpha": 50 })
  );
}

#[test]
fn moves_stream() {
  let server = TestServer::new(Some(3));
  let lines = server.ndjson("/moves?from=0&to=3");
  assert_eq!(
    lines
      .iter()
      .map(|line| (
        line["height"].as_u64().unwrap(),
        line["move"]["tx_index"].as_u64().unwrap()
      ))
      .collect::<Vec<_>>(),
    [(0, 0), (1, 0), (2, 1), (2, 0), (3, 1), (3, 0)]
  );
  assert_eq!(lines[2]["time"], TIMES[2]);
  assert_eq!(lines[2]["move"]["fee"], 9_454);

  // Only moves that moved block0 sats.
  let lines = server.ndjson("/moves?from=0&to=3&row=block0");
  assert_eq!(lines.len(), 1);
  assert_eq!(lines[0]["height"], 0);

  // `to` defaults to the tip.
  assert_eq!(server.ndjson("/moves?from=3").len(), 2);

  server.error("/moves", 400);
  server.error("/moves?from=0&to=10001", 400);
  server.error("/moves?from=3&to=1", 400);
  server.error("/moves?from=0&row=nope", 404);

  // With bounded retention, older moves are refused with the floor.
  let server = TestServer::with_retention(Some(3), MovesRetention::Blocks(2));
  assert!(
    server
      .error("/moves?from=0", 404)
      .contains("kept from height 2")
  );
  assert_eq!(server.ndjson("/moves?from=2").len(), 4);
  assert_eq!(server.json("/block/0")["moves"], Value::Null);
  assert_eq!(server.json("/block/0")["rows_moved"], Value::Null);
  server.error("/block/1/moves", 404);
  assert_eq!(server.json("/status")["moves_from"], 2);
}

#[test]
fn sats_stream() {
  let server = TestServer::new(Some(3));
  let Scenario { t0, u0, d, .. } = server.scenario;

  let lines = server.ndjson("/sats?row=uncommon");
  assert_eq!(
    lines
      .iter()
      .map(|line| (
        line["piece"]["start"].as_u64().unwrap(),
        line["count"].as_u64().unwrap(),
        line["location"]["kind"].as_str().unwrap().to_string()
      ))
      .collect::<Vec<_>>(),
    [
      (0, 1, "unspent".to_string()),
      (COIN, 1, "unspent".into()),
      (2 * COIN, 1, "burned".into()),
      (3 * COIN, 1, "unspent".into()),
    ]
  );
  assert_eq!(lines[1]["location"]["outpoint"], t0.to_string());
  assert_eq!(lines[1]["location"]["satpoint"], format!("{t0}:0"));
  assert_eq!(lines[1]["location"]["output"]["value"], 546);
  assert_eq!(
    lines[1]["location"]["output"]["rows"],
    json!({ "uncommon": 1, "alpha": 1 })
  );
  // Outputs come without their pieces unless asked for.
  assert!(lines[1]["location"]["output"].get("pieces").is_none());
  let full = server.ndjson("/sats?row=uncommon&pieces=true");
  assert_eq!(
    full[1]["location"]["output"]["pieces"],
    json!([piece_json(0, COIN, 1, &["uncommon", "alpha"])])
  );
  server.error("/sats?row=uncommon&pieces=maybe", 400);
  assert_eq!(lines[2]["location"]["outpoint"], u0.to_string());
  assert_eq!(lines[3]["location"]["output"]["outpoint"], d.to_string());
  assert_eq!(
    lines[0]["piece"],
    piece_json(0, 0, COIN - 1, &["uncommon", "alpha", "block0"])
  );

  // Pieces are cut to [from, to).
  let lines = server.ndjson("/sats?row=block0&from=10&to=20");
  assert_eq!(lines.len(), 1);
  assert_eq!(lines[0]["piece"], piece_json(10, 10, 10, &["block0"]));
  assert_eq!(lines[0]["count"], 10);
  assert_eq!(lines[0]["location"]["offset"], 10);
  assert_eq!(
    lines[0]["location"]["satpoint"],
    format!("{}:10", server.scenario.a)
  );

  let lines = server.ndjson("/sats?row=black");
  assert_eq!(
    lines
      .iter()
      .map(|line| line["location"]["kind"].as_str().unwrap().to_string())
      .collect::<Vec<_>>(),
    ["lost", "unspent", "unspent", "unspent"]
  );
  assert_eq!(lines[0]["location"]["output"], Value::Null);

  server.error("/sats", 400);
  server.error("/sats?row=nope", 404);
  server.error("/sats?row=uncommon&from=5&to=5", 400);
  TestServer::new(None).error("/sats?row=uncommon", 503);
}

#[test]
fn blocks() {
  let server = TestServer::new(Some(3));
  assert_eq!(
    server.json("/blocks?from=1&to=2"),
    json!([
      { "height": 1, "hash": server.hash(1), "time": TIMES[1] },
      { "height": 2, "hash": server.hash(2), "time": TIMES[2] },
    ])
  );
  assert_eq!(server.json("/blocks?from=0").as_array().unwrap().len(), 4);
  // Past the tip: only indexed blocks.
  assert_eq!(
    server
      .json("/blocks?from=2&to=100")
      .as_array()
      .unwrap()
      .len(),
    2
  );
  server.error("/blocks?from=0&to=10001", 400);
  server.error("/blocks", 400);
}

#[test]
fn cors_compression_and_unknown_endpoints() {
  let server = TestServer::new(Some(3));
  let response = server
    .client
    .get(format!("{}/rows", server.url))
    .header("Origin", "http://dashboard.local")
    .header("Accept-Encoding", "gzip")
    .send()
    .unwrap();
  assert_eq!(response.status(), 200);
  assert_eq!(response.headers()["access-control-allow-origin"], "*");
  assert_eq!(response.headers()["content-encoding"], "gzip");

  let response = server
    .client
    .get(format!("{}/moves?from=0", server.url))
    .header("Accept-Encoding", "br")
    .send()
    .unwrap();
  assert_eq!(response.headers()["content-encoding"], "br");

  assert!(server.error("/nope", 404).contains("API.md"));
}

#[test]
fn reads_see_new_commits() {
  let server = TestServer::new(Some(2));
  assert_eq!(server.json("/block/2")["height"], 2);
  server.error("/block/3", 404);
  server.scenario.commit(&server.store, 3..4);
  server
    .published
    .update_status(|status| status.height = Some(3));
  assert_eq!(server.json("/block/3")["moves"], 2);
  assert_eq!(server.json("/status")["height"], 3);
}

#[test]
fn concurrent_requests_during_commits() {
  let server = TestServer::new(Some(0));
  let done = AtomicBool::new(false);
  thread::scope(|scope| {
    let readers = (0..4)
      .map(|_| {
        scope.spawn(|| {
          let mut requests = 0;
          while !done.load(Ordering::Relaxed) {
            server.json("/sat/7");
            server.ndjson("/sats?row=uncommon");
            requests += 1;
          }
          requests
        })
      })
      .collect::<Vec<_>>();
    for height in 1..4 {
      server.scenario.commit(&server.store, height..height + 1);
      thread::sleep(std::time::Duration::from_millis(20));
    }
    done.store(true, Ordering::Relaxed);
    for reader in readers {
      assert!(reader.join().unwrap() > 0);
    }
  });
  assert_eq!(server.ndjson("/sats?row=uncommon").len(), 4);
}

/// Sends `GET path` on a raw connection, reads the head and the first body
/// bytes, then leaves the rest unread.
fn stalled_get(server: &TestServer, path: &str) -> std::net::TcpStream {
  use std::io::{Read, Write};
  let mut client = std::net::TcpStream::connect(server.url.trim_start_matches("http://")).unwrap();
  write!(client, "GET {path} HTTP/1.1\r\nHost: test\r\n\r\n").unwrap();
  let mut head = vec![0; 16 << 10];
  let mut read = 0;
  while read < head.len() {
    let n = client.read(&mut head[read..]).unwrap();
    assert!(n > 0, "connection closed early");
    read += n;
  }
  assert!(
    head.starts_with(b"HTTP/1.1 200"),
    "{}",
    String::from_utf8_lossy(&head[..64])
  );
  client
}

#[track_caller]
fn wait_for(what: &str, condition: impl Fn() -> bool) {
  let deadline = std::time::Instant::now() + Duration::from_secs(60);
  while !condition() {
    assert!(
      std::time::Instant::now() < deadline,
      "timed out waiting for {what}"
    );
    thread::sleep(Duration::from_millis(10));
  }
}

#[derive(Clone, Copy, PartialEq)]
enum Stall {
  Nothing,
  /// One read transaction held throughout, as streams did before.
  ReadTransaction,
  /// A client that stopped reading a long `/moves` stream.
  Stream,
}

/// Growth of the index file over 600 per-block commits (the review's
/// `review_file_growth_with_a_stalled_reader`).
fn file_growth(stall: Stall) -> u64 {
  let (server, mut chain) = TestServer::generated(800, 40, STREAM_TIMEOUT);
  let reader = (stall == Stall::ReadTransaction).then(|| server.store.reader().unwrap());
  let client = (stall == Stall::Stream).then(|| stalled_get(&server, "/moves?from=0"));
  let path = server._dir.path().join("index.redb");
  let before = std::fs::metadata(&path).unwrap().len();
  for _ in 0..600 {
    let delta = chain.next_block();
    server
      .store
      .commit(std::slice::from_ref(&delta), &chain.mined(), false)
      .unwrap();
  }
  let after = std::fs::metadata(&path).unwrap().len();
  if stall == Stall::Stream {
    assert_eq!(
      server.state.streams.load(Ordering::SeqCst),
      1,
      "the stream should still be waiting for its client"
    );
  }
  drop(client);
  drop(reader);
  after - before
}

#[test]
fn a_stalled_stream_holds_no_read_transaction() {
  let baseline = file_growth(Stall::Nothing);
  let pinned = file_growth(Stall::ReadTransaction);
  let stalled = file_growth(Stall::Stream);
  eprintln!(
    "file growth: {baseline} without readers, {pinned} with a read transaction held, {stalled} with a stalled stream"
  );
  // The setup shows the problem...
  assert!(
    pinned > baseline + (16 << 20),
    "a held read transaction should grow the file: {pinned} vs {baseline}"
  );
  // ...which a stalled stream no longer causes.
  assert!(
    stalled < baseline + (8 << 20),
    "a stalled stream grew the file by {stalled} (baseline {baseline})"
  );
}

#[test]
fn a_client_that_stops_reading_is_cut_off() {
  use std::io::Read;
  let (server, _) = TestServer::generated(800, 40, Duration::from_millis(300));
  let mut client = stalled_get(&server, "/moves?from=0");
  wait_for("the stream to give up", || {
    server.state.streams.load(Ordering::SeqCst) == 0
  });

  // The client gets what was already sent, then a connection closed before
  // the chunked body's end.
  client
    .set_read_timeout(Some(Duration::from_secs(30)))
    .unwrap();
  let mut rest = Vec::new();
  let _ = client.read_to_end(&mut rest);
  assert!(!rest.is_empty());
  assert!(
    !rest.ends_with(b"\r\n0\r\n\r\n"),
    "the response should be cut short"
  );

  // Clients that read keep getting whole streams.
  let lines = server.ndjson("/moves?from=790");
  assert!(!lines.is_empty());
  assert_eq!(server.state.streams.load(Ordering::SeqCst), 0);
}

#[test]
fn streams_resume_across_read_transactions() {
  // Far more than one chunk, with commits landing between chunks.
  let (server, mut chain) = TestServer::generated(300, 20, STREAM_TIMEOUT);
  let expected_moves = (0..=299)
    .map(|height| server.store.moves(height).unwrap().unwrap().len())
    .sum::<usize>();
  let committer = {
    let store = server.store.clone();
    let deltas = (0..50).map(|_| chain.next_block()).collect::<Vec<_>>();
    let mined = chain.mined();
    thread::spawn(move || {
      for delta in deltas {
        store
          .commit(std::slice::from_ref(&delta), &mined, true)
          .unwrap();
      }
    })
  };
  let lines = server.ndjson("/moves?from=0&to=299");
  committer.join().unwrap();
  assert_eq!(lines.len(), expected_moves);
  let heights = lines
    .iter()
    .map(|line| line["height"].as_u64().unwrap())
    .collect::<Vec<u64>>();
  assert!(heights.windows(2).all(|pair| pair[0] <= pair[1]));
  assert_eq!(heights.first(), Some(&0));
  assert_eq!(heights.last(), Some(&299));

  // /sats: every piece once, in sat order, across many chunks.
  let pieces = server.ndjson("/sats?row=block0");
  let starts = pieces
    .iter()
    .map(|line| line["piece"]["start"].as_u64().unwrap())
    .collect::<Vec<u64>>();
  assert!(starts.windows(2).all(|pair| pair[0] < pair[1]));
  let tracked = server
    .store
    .reader()
    .unwrap()
    .pieces(0, u64::MAX)
    .unwrap()
    .count();
  assert_eq!(pieces.len(), tracked);
}

/// The scenario's one sale (block 2: B sold to T:0), as `/sales` renders it.
fn sale_json(server: &TestServer) -> Value {
  let Scenario { b, t0, .. } = server.scenario;
  json!({
    "txid": t0.txid.to_string(),
    "height": 2,
    "time": TIMES[2],
    "tx_index": 1,
    "vin": 0,
    "marketplace": "magisat",
    "kind": "listing",
    "seller": { "address": address(&p2wpkh(1)), "script_pubkey": p2wpkh(1).to_hex_string(),
                "outpoint": b.to_string(), "value": COIN },
    "buyer": { "address": address(&p2tr()), "script_pubkey": p2tr().to_hex_string(),
               "outpoint": t0.to_string(), "value": 546 },
    "payment_vout": 1,
    "price": COIN + 45_000,
    "seller_proceeds": COIN + 44_773,
    "fees": 227,
    "premium": 45_000,
    "pieces": [
      piece_json(0, COIN, 1, &["uncommon", "alpha"]),
      piece_json(COIN - 1, 2 * COIN - 1, 1, &["black"]),
    ],
    "sats": [
      { "sat": COIN, "height": 1, "time": TIMES[1], "inscribed": true },
      { "sat": 2 * COIN - 1, "height": 1, "time": TIMES[1], "inscribed": false },
    ],
    "rows": { "uncommon": 1, "black": 1, "alpha": 1 },
    "unit": { "row": "uncommon", "count": 1, "price": 45_000 },
    "pure_rows": [],
    "inscribed": true,
  })
}

#[test]
fn sales() {
  let server = TestServer::new(Some(3));
  let Scenario { b, t0, .. } = server.scenario;
  let expected = sale_json(&server);

  assert_eq!(server.ndjson("/sales"), vec![expected.clone()]);
  assert_eq!(server.ndjson("/sales?from=2&to=2"), vec![expected.clone()]);
  assert_eq!(
    server.ndjson("/sales?from=0&to=100"),
    vec![expected.clone()]
  );
  assert_eq!(server.ndjson("/sales?from=3"), Vec::<Value>::new());
  assert_eq!(server.ndjson("/sales?to=1"), Vec::<Value>::new());
  assert_eq!(server.ndjson("/sales?row=black"), vec![expected.clone()]);
  assert_eq!(server.ndjson("/sales?row=block0"), Vec::<Value>::new());
  server.error("/sales?row=nope", 404);
  server.error("/sales?from=3&to=1", 400);
  server.error("/sales?from=x", 400);
  TestServer::new(None).error("/sales", 503);

  assert_eq!(
    server.json(&format!("/sales/tx/{}", t0.txid)),
    json!({ "txid": t0.txid.to_string(), "sales": [expected.clone()] })
  );
  assert!(
    server
      .error(&format!("/sales/tx/{}", b.txid), 404)
      .contains("sold no tracked sat")
  );
  server.error("/sales/tx/nonsense", 400);

  // /sat lists the sales that sold the sat and its inscriptions.
  let sat = server.json(&format!("/sat/{COIN}"));
  assert_eq!(sat["inscribed"], true);
  assert_eq!(sat["inscriptions"], 2);
  assert_eq!(sat["sales"], json!([expected.clone()]));
  assert_eq!(sat["location"]["output"]["inscribed_sats"], json!([COIN]));
  let black = server.json(&format!("/sat/{}", 2 * COIN - 1));
  assert_eq!(black["inscribed"], false);
  assert_eq!(black["inscriptions"], 0);
  assert_eq!(black["sales"], json!([expected]));

  // Rolling back block 3 drops its inscriptions; block 2, its sale.
  server.store.rollback_tip().unwrap().unwrap();
  let sat = server.json(&format!("/sat/{COIN}"));
  assert_eq!(sat["inscriptions"], 1);
  assert_eq!(server.json("/sat/7")["inscribed"], false);
  server.store.rollback_tip().unwrap().unwrap();
  assert_eq!(server.ndjson("/sales"), Vec::<Value>::new());
  server.error(&format!("/sales/tx/{}", t0.txid), 404);
  assert_eq!(server.json(&format!("/sat/{COIN}"))["sales"], json!([]));
  assert_eq!(server.json(&format!("/sat/{COIN}"))["inscriptions"], 1);
}

#[test]
fn sale_sats_are_the_special_sats_of_the_sold_pieces() {
  let server = TestServer::new(Some(3));
  let reader = server.store.reader().unwrap();
  let render = server.state.render(&reader);
  let a = server.scenario.a;

  // Selling A: block 0's range piece; its special sats are its 50 alphas
  // (the first also uncommon), sat 7 (inscribed) is not one.
  // The sale is block 3's third transaction, after the one (index 1) that
  // inscribed sat 7.
  let sale_of = |seller: TrackedUtxo| crate::store::StoredSale {
    height: 3,
    sale: Sale {
      txid: txid(99),
      tx_index: 2,
      vin: 0,
      kind: SaleKind::Unknown,
      marketplace: "unknown".into(),
      seller_outpoint: a,
      seller,
      payment_vout: None,
      price: 100,
      seller_proceeds: 100,
      fees: 0,
      buyer_vout: None,
      buyer_script: None,
    },
    buyer_value: None,
  };
  let utxo_a = reader.utxo(a).unwrap().unwrap();
  let sale = render.sale(&sale_of(utxo_a.clone())).unwrap();
  assert_eq!(
    sale.sats.iter().map(|sat| sat.sat).collect::<Vec<u64>>(),
    (0..50).map(|i| i * 100_000_000).collect::<Vec<u64>>()
  );
  assert!(
    sale
      .sats
      .iter()
      .all(|sat| sat.height == 0 && !sat.inscribed)
  );
  assert!(sale.inscribed, "sat 7 was inscribed earlier in block 3");
  // An inscription later in the sale's block, or in the sale itself, was
  // not there when it sold.
  for tx_index in [0, 1] {
    let mut stored = sale_of(reader.utxo(a).unwrap().unwrap());
    stored.sale.tx_index = tx_index;
    assert!(!render.sale(&stored).unwrap().inscribed, "{tx_index}");
  }
  assert_eq!(sale.buyer, None);
  assert_eq!(sale.premium, 100 - i64::try_from(COIN - 1).unwrap());
  // Every sold sat is a block 0 sat: priced per block 0 sat, not as the
  // uncommon among them.
  assert_eq!(sale.pure_rows, ["block0"]);
  let unit = sale.unit.unwrap();
  assert_eq!((unit.row.as_str(), unit.count), ("block0", COIN - 1));
  assert_eq!(unit.price, sale.premium / i64::try_from(COIN - 1).unwrap());

  // A single sat is pure for each of its rows, rarest first.
  let single = TrackedUtxo {
    value: 546,
    script_pubkey: p2pk(),
    height: 1,
    kind: UtxoKind::Spendable,
    pieces: vec![piece(0, COIN, 1)],
  };
  let sale = render.sale(&sale_of(single)).unwrap();
  assert_eq!(sale.pure_rows, ["uncommon", "alpha"]);
  let unit = sale.unit.unwrap();
  assert_eq!(
    (unit.row.as_str(), unit.count, unit.price),
    ("uncommon", 1, 100 - 546)
  );

  // Inscriptions made after the sale do not count.
  let earlier = crate::store::StoredSale {
    height: 2,
    ..sale_of(utxo_a)
  };
  assert!(!render.sale(&earlier).unwrap().inscribed);

  // At most 100 sats: [0, 20 × 5e9) holds over a thousand alphas.
  let wide = TrackedUtxo {
    value: 20 * COIN,
    script_pubkey: p2pk(),
    height: 0,
    kind: UtxoKind::Spendable,
    pieces: vec![piece(0, 0, 20 * COIN)],
  };
  assert_eq!(
    render.sale(&sale_of(wide)).unwrap().sats.len(),
    super::sales::MAX_SALE_SATS
  );
}
