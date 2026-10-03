//! Indexer tests with a fake bitcoind and a fake tracker.

use {
  super::*,
  crate::{
    model::{
      InscriptionEvent, Move, MoveInput, MoveOutput, Piece, ResolvedTx, RowDelta, RowStats, Sale,
      SaleKind, TrackedUtxo, UtxoKind,
    },
    store::{MovesRetention, StoreOptions},
  },
  anyhow::Context as _,
  bitcoin::{
    Amount, CompactTarget, OutPoint, ScriptBuf, TxMerkleNode, TxOut, Txid,
    block::{Header, Version},
    hashes::{Hash, sha256d},
  },
  std::{
    collections::BTreeMap,
    sync::{
      Mutex,
      atomic::{AtomicU32, AtomicU64},
    },
  },
  tempfile::TempDir,
};

type Hook = Box<dyn Fn(&FakeChain, u32) + Send + Sync>;

/// bitcoind's chain, shared by the fake node and its fast source.
pub(crate) struct FakeChain {
  blocks: Mutex<Vec<ResolvedBlock>>,
  forks: AtomicU64,
  /// Fail this many upcoming calls.
  failures: AtomicU32,
  pub(crate) fast_syncs: AtomicU32,
  pub(crate) fast_blocks_served: AtomicU32,
  pub(crate) live_blocks_served: AtomicU32,
  on_fast_block: Mutex<Option<Hook>>,
  on_live_block: Mutex<Option<Hook>>,
}

/// A bitcoind whose chain the test controls.
pub(crate) struct FakeNode {
  name: String,
  chain: Arc<FakeChain>,
}

impl std::ops::Deref for FakeNode {
  type Target = FakeChain;

  fn deref(&self) -> &FakeChain {
    &self.chain
  }
}

pub(crate) fn fake_block(prev: Option<&ResolvedBlock>, fork: u64) -> ResolvedBlock {
  let height = prev.map_or(0, |prev| prev.height + 1);
  let header = Header {
    version: Version::ONE,
    prev_blockhash: prev.map_or_else(BlockHash::all_zeros, |prev| prev.hash),
    merkle_root: TxMerkleNode::all_zeros(),
    time: 1_231_006_505 + height * 600,
    bits: CompactTarget::from_consensus(0x207f_ffff),
    nonce: u32::try_from(fork).unwrap(),
  };
  let hash = header.block_hash();
  ResolvedBlock {
    height,
    hash,
    header,
    txs: vec![ResolvedTx {
      txid: Txid::from_raw_hash(sha256d::Hash::hash(hash.as_byte_array())),
      inputs: Vec::new(),
      outputs: vec![TxOut {
        value: Amount::from_sat(5_000_000_000),
        script_pubkey: ScriptBuf::new(),
      }],
    }],
  }
}

impl FakeNode {
  pub(crate) fn new(blocks: u32) -> Self {
    let node = Self {
      name: "regtest".into(),
      chain: Arc::new(FakeChain {
        blocks: Mutex::new(Vec::new()),
        forks: AtomicU64::new(0),
        failures: AtomicU32::new(0),
        fast_syncs: AtomicU32::new(0),
        fast_blocks_served: AtomicU32::new(0),
        live_blocks_served: AtomicU32::new(0),
        on_fast_block: Mutex::new(None),
        on_live_block: Mutex::new(None),
      }),
    };
    node.mine(blocks);
    node
  }
}

impl FakeChain {
  pub(crate) fn mine(&self, n: u32) {
    let fork = self.forks.load(Ordering::Relaxed);
    let mut blocks = self.blocks.lock().unwrap();
    for _ in 0..n {
      let block = fake_block(blocks.last(), fork);
      blocks.push(block);
    }
  }

  /// Replaces the top `depth` blocks with `blocks` new ones.
  pub(crate) fn reorg(&self, depth: u32, blocks: u32) {
    {
      let mut chain = self.blocks.lock().unwrap();
      let len = chain.len() - depth as usize;
      chain.truncate(len);
    }
    self.forks.fetch_add(1, Ordering::Relaxed);
    self.mine(blocks);
  }

  pub(crate) fn tip(&self) -> u32 {
    self.blocks.lock().unwrap().len() as u32 - 1
  }

  pub(crate) fn hash(&self, height: u32) -> BlockHash {
    self.blocks.lock().unwrap()[height as usize].hash
  }

  pub(crate) fn blocks_vec(&self) -> Vec<ResolvedBlock> {
    self.blocks.lock().unwrap().clone()
  }

  fn fail(&self) -> anyhow::Result<()> {
    if self
      .failures
      .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
      .is_ok()
    {
      anyhow::bail!("connection refused");
    }
    Ok(())
  }

  fn block(&self, height: u32) -> anyhow::Result<ResolvedBlock> {
    self.fail()?;
    self
      .blocks
      .lock()
      .unwrap()
      .get(height as usize)
      .cloned()
      .with_context(|| format!("block height {height} out of range"))
  }
}

impl Node for FakeNode {
  fn chain(&self) -> anyhow::Result<String> {
    self.fail()?;
    Ok(self.name.clone())
  }

  fn block_count(&self) -> anyhow::Result<u32> {
    self.fail()?;
    Ok(self.tip())
  }

  fn best_block_hash(&self) -> anyhow::Result<BlockHash> {
    self.fail()?;
    Ok(self.blocks.lock().unwrap().last().unwrap().hash)
  }

  fn block_hash(&self, height: u32) -> anyhow::Result<BlockHash> {
    Ok(self.block(height)?.hash)
  }

  fn blocks(
    &self,
    from: u32,
    to: u32,
    _threads: usize,
  ) -> Box<dyn Iterator<Item = anyhow::Result<ResolvedBlock>> + Send + '_> {
    Box::new((from..=to).map(move |height| {
      if let Some(hook) = &*self.on_live_block.lock().unwrap() {
        hook(&self.chain, height);
      }
      let block = self.block(height)?;
      self.live_blocks_served.fetch_add(1, Ordering::Relaxed);
      Ok(block)
    }))
  }

  fn fast_blocks(&self, options: FastSyncOptions) -> anyhow::Result<Box<dyn FastBlocks>> {
    self.fail()?;
    self.fast_syncs.fetch_add(1, Ordering::Relaxed);
    Ok(Box::new(FakeFast {
      chain: self.chain.clone(),
      next: 0,
      stop: options.stop_height,
    }))
  }
}

struct FakeFast {
  chain: Arc<FakeChain>,
  next: u32,
  stop: u32,
}

impl FastBlocks for FakeFast {
  fn next_block(&mut self) -> Option<anyhow::Result<ResolvedBlock>> {
    if self.next > self.stop {
      return None;
    }
    let height = self.next;
    self.next += 1;
    if let Some(hook) = &*self.chain.on_fast_block.lock().unwrap() {
      hook(&self.chain, height);
    }
    self
      .chain
      .fast_blocks_served
      .fetch_add(1, Ordering::Relaxed);
    Some(self.chain.block(height))
  }

  fn stats(&self) -> FastSourceStats {
    FastSourceStats::default()
  }
}

fn script(n: u32) -> ScriptBuf {
  let mut bytes = vec![0x00, 0x14];
  bytes.extend([n as u8; 20]);
  ScriptBuf::from_bytes(bytes)
}

/// Each block mints one tracked sat (its first) into its coinbase, and a
/// transaction moves the previous coinbase's sat to its vout 1.
#[derive(Default)]
pub(crate) struct FakeEngine {
  height: Option<u32>,
  hash: Option<BlockHash>,
  utxos: BTreeMap<OutPoint, TrackedUtxo>,
  last_coinbase: Option<OutPoint>,
}

fn stats_at(height: u32) -> RowStats {
  RowStats {
    mined: u64::from(height) + 1,
    located: u64::from(height) + 1,
    holders: 3.min(u64::from(height) + 1),
    ..RowStats::default()
  }
}

impl Engine for FakeEngine {
  fn height(&self) -> Option<u32> {
    self.height
  }

  fn hash(&self) -> Option<BlockHash> {
    self.hash
  }

  fn apply_block(&mut self, block: &ResolvedBlock) -> anyhow::Result<BlockDelta> {
    let height = self.height.map_or(0, |height| height + 1);
    ensure!(block.height == height, "expected block {height}");
    ensure!(
      block.header.prev_blockhash == self.hash.unwrap_or_else(BlockHash::all_zeros),
      "block {height} does not extend the tip"
    );

    let txid = block.txs[0].txid;
    let coinbase = OutPoint { txid, vout: 0 };
    let minted = TrackedUtxo {
      value: 5_000_000_000,
      script_pubkey: script(height % 3),
      height,
      kind: UtxoKind::Spendable,
      pieces: vec![Piece {
        offset: 0,
        start: u64::from(height) * 5_000_000_000,
        len: 1,
      }],
    };

    let mut spent = Vec::new();
    let mut created = Vec::new();
    let mut moves = Vec::new();

    if let Some(previous) = self.last_coinbase {
      let utxo = self.utxos.remove(&previous).unwrap();
      let spend = Txid::from_raw_hash(sha256d::Hash::hash(txid.as_byte_array()));
      let moved = OutPoint {
        txid: spend,
        vout: 1,
      };
      let output = TrackedUtxo {
        value: utxo.value,
        script_pubkey: script(7),
        height,
        kind: UtxoKind::Spendable,
        pieces: utxo.pieces.clone(),
      };
      moves.push(Move {
        txid: spend,
        tx_index: 1,
        coinbase: false,
        input_value: utxo.value,
        output_value: utxo.value,
        inputs: vec![MoveInput {
          vin: 0,
          outpoint: previous,
          utxo: utxo.clone(),
        }],
        outputs: vec![MoveOutput {
          vout: 1,
          value: utxo.value,
          script_pubkey: script(7),
          kind: UtxoKind::Spendable,
          pieces: utxo.pieces.clone(),
        }],
        fee: Vec::new(),
        mined: Vec::new(),
        lost: Vec::new(),
      });
      spent.push((previous, utxo));
      self.utxos.insert(moved, output.clone());
      created.push((moved, output));
    }

    moves.push(Move {
      txid,
      tx_index: 0,
      coinbase: true,
      input_value: 0,
      output_value: minted.value,
      inputs: Vec::new(),
      outputs: vec![MoveOutput {
        vout: 0,
        value: minted.value,
        script_pubkey: minted.script_pubkey.clone(),
        kind: UtxoKind::Spendable,
        pieces: minted.pieces.clone(),
      }],
      fee: Vec::new(),
      mined: minted.pieces.clone(),
      lost: Vec::new(),
    });
    self.utxos.insert(coinbase, minted.clone());
    created.push((coinbase, minted));
    created.sort_by_key(|(outpoint, _)| *outpoint);

    self.height = Some(height);
    self.hash = Some(block.hash);
    self.last_coinbase = Some(coinbase);

    Ok(BlockDelta {
      height,
      hash: block.hash,
      header: block.header,
      moves,
      spent,
      created,
      overwritten: Vec::new(),
      lost: Vec::new(),
      lost_sats_before: 0,
      lost_sats_after: 0,
      rows: vec![RowDelta {
        row: 0,
        before: height.checked_sub(1).map(stats_at).unwrap_or_default(),
        after: stats_at(height),
      }],
    })
  }

  /// Every spend is a sale; every fourth block inscribes its minted sat.
  fn analyze(&self, _block: &ResolvedBlock, delta: &BlockDelta) -> MarketDelta {
    fake_market(delta)
  }

  fn undo_block(&mut self, delta: &BlockDelta) -> anyhow::Result<()> {
    ensure!(
      Some(delta.height) == self.height && Some(delta.hash) == self.hash,
      "not the tip"
    );
    for (outpoint, _) in &delta.created {
      self.utxos.remove(outpoint).unwrap();
    }
    for (outpoint, utxo) in &delta.spent {
      self.utxos.insert(*outpoint, utxo.clone());
    }
    self.last_coinbase = delta.spent.first().map(|(outpoint, _)| *outpoint);
    self.height = delta.height.checked_sub(1);
    self.hash = self.height.map(|_| delta.header.prev_blockhash);
    Ok(())
  }

  fn mined(&self) -> Vec<u64> {
    vec![self.height.map_or(0, |height| u64::from(height) + 1)]
  }

  fn supply(&self, _top: usize) -> SupplySnapshot {
    SupplySnapshot {
      height: self.height,
      hash: self.hash,
      time: None,
      rows: Vec::new(),
      years: Default::default(),
      lucks: Default::default(),
    }
  }

  fn utxo_count(&self) -> usize {
    self.utxos.len()
  }

  fn piece_count(&self) -> usize {
    self.utxos.values().map(|utxo| utxo.pieces.len()).sum()
  }
}

pub(crate) fn fake_market(delta: &BlockDelta) -> MarketDelta {
  let mut market = MarketDelta::default();
  for m in delta.moves.iter().filter(|m| !m.coinbase) {
    for input in &m.inputs {
      market.sales.push(Sale {
        txid: m.txid,
        tx_index: m.tx_index,
        vin: input.vin,
        kind: SaleKind::Listing,
        marketplace: "fake".into(),
        seller_outpoint: input.outpoint,
        seller: input.utxo.clone(),
        payment_vout: None,
        price: 10_000,
        seller_proceeds: 9_000,
        fees: 1_000,
        buyer_vout: m.outputs.first().map(|output| output.vout),
        buyer_script: m.outputs.first().map(|output| output.script_pubkey.clone()),
      });
    }
  }
  if delta.height.is_multiple_of(4) {
    let coinbase = delta.moves.iter().find(|m| m.coinbase).unwrap();
    market.inscriptions.push(InscriptionEvent {
      sat: coinbase.mined[0].start,
      txid: coinbase.txid,
      vin: 0,
      tx_index: 0,
    });
  }
  market
}

pub(crate) struct FakeFactory;

impl EngineFactory for FakeFactory {
  fn fingerprint(&self) -> String {
    "fake".into()
  }

  fn fresh(&self) -> anyhow::Result<Box<dyn Engine>> {
    Ok(Box::new(FakeEngine::default()))
  }

  fn restore(&self, snapshot: TrackerSnapshot) -> anyhow::Result<Box<dyn Engine>> {
    let last_coinbase = snapshot.utxos.iter().find_map(|(outpoint, utxo)| {
      (Some(utxo.height) == snapshot.height && outpoint.vout == 0).then_some(*outpoint)
    });
    Ok(Box::new(FakeEngine {
      height: snapshot.height,
      hash: snapshot.hash,
      utxos: snapshot.utxos.into_iter().collect(),
      last_coinbase,
    }))
  }
}

pub(crate) fn options() -> IndexerOptions {
  IndexerOptions {
    chain: Chain::Regtest,
    config: TrackerConfig::default(),
    commit_interval: 4,
    commit_every: Duration::from_secs(3600),
    fetch_threads: 2,
    fast_sync: true,
    fast_sync_restart_threshold: FAST_SYNC_RESTART_THRESHOLD,
    poll_interval: Duration::from_millis(10),
    supply_top: 10,
    fast_sync_supply_top: 10,
    retry_interval: Duration::from_millis(1),
    max_retries: Some(20),
    utxo_capacity: 16,
    height_limit: None,
  }
}

struct Context {
  _dir: TempDir,
  store: Arc<Store>,
  node: Arc<FakeNode>,
  published: Arc<Published>,
  shutdown: Arc<AtomicBool>,
}

impl Context {
  fn new(blocks: u32) -> Self {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(
      Store::open_with(
        &dir.path().join("index.redb"),
        StoreOptions {
          cache_size: 16 << 20,
          moves_retention: MovesRetention::All,
          quick_repair: false,
        },
      )
      .unwrap(),
    );
    Self {
      _dir: dir,
      store,
      node: Arc::new(FakeNode::new(blocks)),
      published: Arc::new(Published::default()),
      shutdown: Arc::new(AtomicBool::new(false)),
    }
  }

  fn indexer(&self, options: IndexerOptions) -> Indexer {
    Indexer::new(
      self.store.clone(),
      self.node.clone(),
      Arc::new(FakeFactory),
      self.published.clone(),
      self.shutdown.clone(),
      options,
    )
  }

  fn run(&self, options: IndexerOptions) -> anyhow::Result<()> {
    self.indexer(options).run(RunMode::UntilCaughtUp)
  }

  /// The index matches bitcoind's chain and what a tracker that applied
  /// that chain from scratch holds.
  fn assert_consistent(&self) {
    let meta = self.store.meta().unwrap().unwrap();
    let tip = self.node.tip();
    assert_eq!(meta.height, Some(tip));
    assert_eq!(meta.hash, Some(self.node.hash(tip)));
    for height in 0..=tip {
      assert_eq!(
        self.store.block_hash(height).unwrap(),
        Some(self.node.hash(height)),
        "block {height}"
      );
    }

    let mut engine = FakeEngine::default();
    for block in self.node.blocks_vec() {
      engine.apply_block(&block).unwrap();
    }
    let mut snapshot = self.store.load_snapshot().unwrap();
    snapshot.utxos.sort_by_key(|(outpoint, _)| *outpoint);
    assert_eq!(snapshot.utxos, engine.utxos.into_iter().collect::<Vec<_>>());
    assert_eq!(snapshot.mined, vec![u64::from(tip) + 1]);

    let status = self.published.status();
    assert_eq!(status.height, Some(tip));
    assert_eq!(status.hash, Some(self.node.hash(tip)));
    assert_eq!(status.bitcoind_height, Some(tip));
    assert_eq!(status.tracked_utxos, snapshot.utxos.len() as u64);
    assert_eq!(self.published.supply().height, Some(tip));

    // Market rows: those of the chain as it is now, none of rolled-back
    // blocks.
    let blocks = self.node.blocks_vec();
    let reader = self.store.reader().unwrap();
    let sales = reader
      .sales((0, 0, 0), tip)
      .unwrap()
      .collect::<anyhow::Result<Vec<_>>>()
      .unwrap();
    assert_eq!(sales.len(), tip as usize);
    for (sale, height) in sales.iter().zip(1..) {
      assert_eq!(sale.height, height);
      let spend = Txid::from_raw_hash(sha256d::Hash::hash(
        blocks[height as usize].txs[0].txid.as_byte_array(),
      ));
      assert_eq!(sale.sale.txid, spend, "sale of block {height}");
      assert_eq!(
        sale.sale.seller_outpoint.txid,
        blocks[height as usize - 1].txs[0].txid
      );
      assert_eq!(sale.buyer_value, Some(5_000_000_000));
      assert_eq!(reader.sales_for_tx(spend).unwrap(), vec![sale.clone()]);
    }
    let inscribed = reader
      .inscribed_from(0)
      .unwrap()
      .collect::<anyhow::Result<Vec<_>>>()
      .unwrap();
    let expected = (0..=tip)
      .filter(|height| height % 4 == 0)
      .map(|height| u64::from(height) * 5_000_000_000)
      .collect::<Vec<u64>>();
    assert_eq!(
      inscribed.iter().map(|(sat, _)| *sat).collect::<Vec<u64>>(),
      expected
    );
    for (sat, inscribed) in inscribed {
      let height = u32::try_from(sat / 5_000_000_000).unwrap();
      assert_eq!(inscribed.count, 1);
      assert_eq!(inscribed.first_height, height);
      assert_eq!(inscribed.first_tx_index, 0);
      assert_eq!(inscribed.first_txid, blocks[height as usize].txs[0].txid);
    }
  }
}

#[test]
fn fast_sync_then_live() {
  let context = Context::new(30);
  context.run(options()).unwrap();
  context.assert_consistent();

  // Genesis to tip − 12 came from the fast source, the rest live.
  assert_eq!(context.node.fast_syncs.load(Ordering::Relaxed), 1);
  assert_eq!(context.node.fast_blocks_served.load(Ordering::Relaxed), 18);
  assert_eq!(context.node.live_blocks_served.load(Ordering::Relaxed), 12);

  let meta = context.store.meta().unwrap().unwrap();
  assert!(meta.fast_sync_complete);
  let status = context.published.status();
  assert_eq!(status.mode, Mode::Live);
  assert!(status.fast_sync_complete);
  assert!(status.timings.fast_sync_seconds.is_some());
  // 18 fast blocks in commits of 4, then 12 live blocks in commits of 4.
  assert_eq!(status.timings.commits, 5 + 3);
  assert_eq!(status.error, None);

  // Moves of every block are stored.
  for height in 0..=29 {
    assert!(!context.store.moves(height).unwrap().unwrap().is_empty());
  }

  // New blocks are picked up by the next run, one commit each when at tip.
  context.node.mine(2);
  context.run(options()).unwrap();
  context.assert_consistent();
  assert_eq!(context.node.fast_syncs.load(Ordering::Relaxed), 1);
}

#[test]
fn live_only_without_fast_sync_or_on_short_chains() {
  let context = Context::new(20);
  context
    .run(IndexerOptions {
      fast_sync: false,
      ..options()
    })
    .unwrap();
  context.assert_consistent();
  assert_eq!(context.node.fast_syncs.load(Ordering::Relaxed), 0);
  assert!(context.store.meta().unwrap().unwrap().fast_sync_complete);

  let context = Context::new(8);
  context.run(options()).unwrap();
  context.assert_consistent();
  assert_eq!(context.node.fast_syncs.load(Ordering::Relaxed), 0);
}

#[test]
fn reorg_rolls_back_to_the_fork_point() {
  let context = Context::new(30);
  context.run(options()).unwrap();

  context.node.reorg(3, 5);
  context.run(options()).unwrap();
  context.assert_consistent();
  assert_eq!(context.node.tip(), 31);

  // The rolled-back blocks' moves are the new chain's.
  let moves = context.store.moves(27).unwrap().unwrap();
  assert_eq!(moves[1].txid, context.node.blocks_vec()[27].txs[0].txid);
  assert_eq!(
    moves[0].inputs[0].outpoint.txid,
    context.node.blocks_vec()[26].txs[0].txid
  );
}

#[test]
fn shorter_chain_waits_then_reorgs_when_it_grows() {
  let context = Context::new(30);
  context.run(options()).unwrap();

  // `invalidateblock`-style: bitcoind drops its tip and has nothing better.
  context.node.reorg(1, 0);
  context.run(options()).unwrap();
  assert_eq!(context.store.meta().unwrap().unwrap().height, Some(29));

  // Then a fork outgrows the old chain.
  context.node.mine(2);
  context.run(options()).unwrap();
  context.assert_consistent();
}

#[test]
fn bitcoind_behind_the_index_is_waited_for() {
  let context = Context::new(30);
  context.run(options()).unwrap();
  // bitcoind restarted and is re-syncing: its chain is a prefix of ours.
  context.node.blocks.lock().unwrap().truncate(20);
  context.run(options()).unwrap();
  assert_eq!(context.store.meta().unwrap().unwrap().height, Some(29));
}

#[test]
fn reorg_during_catch_up_is_detected_mid_batch() {
  let context = Context::new(30);
  let fired = Arc::new(AtomicBool::new(false));
  let hook_fired = fired.clone();
  *context.node.on_live_block.lock().unwrap() = Some(Box::new(move |chain, height| {
    if height == 24 && !hook_fired.swap(true, Ordering::Relaxed) {
      chain.reorg(8, 10);
    }
  }));
  context.run(options()).unwrap();
  assert!(fired.load(Ordering::Relaxed));
  *context.node.on_live_block.lock().unwrap() = None;
  context.assert_consistent();
}

#[test]
fn reorg_deeper_than_the_undo_records_asks_for_a_reindex() {
  let context = Context::new(200);
  context.run(options()).unwrap();
  context.node.reorg(150, 151);
  let err = context.run(options()).unwrap_err();
  assert!(format!("{err:#}").contains("--reindex"), "{err:#}");
  assert!(
    context
      .published
      .status()
      .error
      .unwrap()
      .contains("--reindex")
  );
}

#[test]
fn shutdown_during_the_fast_sync_commits_whole_blocks() {
  let context = Context::new(60);
  let shutdown = context.shutdown.clone();
  *context.node.on_fast_block.lock().unwrap() = Some(Box::new(move |_, height| {
    if height == 21 {
      shutdown.store(true, Ordering::Relaxed);
    }
  }));
  context.run(options()).unwrap();

  let meta = context.store.meta().unwrap().unwrap();
  assert!(!meta.fast_sync_complete);
  // Block 21 was served and applied before the loop saw the flag: every
  // applied block is committed.
  assert_eq!(meta.height, Some(21));
  assert_eq!(
    context.store.load_snapshot().unwrap().height,
    context.published.status().height
  );
}

#[test]
fn interrupted_fast_sync_is_finished_live_when_close() {
  let context = Context::new(60);
  let shutdown = context.shutdown.clone();
  *context.node.on_fast_block.lock().unwrap() = Some(Box::new(move |_, height| {
    if height == 30 {
      shutdown.store(true, Ordering::Relaxed);
    }
  }));
  context.run(options()).unwrap();
  *context.node.on_fast_block.lock().unwrap() = None;
  context.shutdown.store(false, Ordering::Relaxed);
  assert_eq!(context.store.meta().unwrap().unwrap().height, Some(30));

  context.run(options()).unwrap();
  context.assert_consistent();
  assert_eq!(context.node.fast_syncs.load(Ordering::Relaxed), 1);
  assert!(context.store.meta().unwrap().unwrap().fast_sync_complete);
}

#[test]
fn interrupted_fast_sync_restarts_from_genesis_when_far() {
  let context = Context::new(60);
  let shutdown = context.shutdown.clone();
  *context.node.on_fast_block.lock().unwrap() = Some(Box::new(move |_, height| {
    if height == 10 {
      shutdown.store(true, Ordering::Relaxed);
    }
  }));
  context.run(options()).unwrap();
  *context.node.on_fast_block.lock().unwrap() = None;
  context.shutdown.store(false, Ordering::Relaxed);

  // 47 − 10 = 37 blocks left, more than the threshold.
  context
    .run(IndexerOptions {
      fast_sync_restart_threshold: 20,
      ..options()
    })
    .unwrap();
  context.assert_consistent();
  assert_eq!(context.node.fast_syncs.load(Ordering::Relaxed), 2);
  assert_eq!(
    context.node.fast_blocks_served.load(Ordering::Relaxed),
    11 + 48
  );
}

#[test]
fn bitcoind_errors_are_retried() {
  let context = Context::new(30);
  context.node.failures.store(5, Ordering::Relaxed);
  context.run(options()).unwrap();
  context.assert_consistent();

  context.node.mine(3);
  context.node.failures.store(1000, Ordering::Relaxed);
  let err = context
    .run(IndexerOptions {
      max_retries: Some(3),
      ..options()
    })
    .unwrap_err();
  assert!(format!("{err:#}").contains("connection refused"), "{err:#}");
}

#[test]
fn refuses_the_wrong_chain_and_incompatible_indexes() {
  let mut context = Context::new(5);
  Arc::get_mut(&mut context.node).unwrap().name = "main".into();
  let err = context.run(options()).unwrap_err().to_string();
  assert!(err.contains("--chain regtest"), "{err}");

  let context = Context::new(5);
  context
    .store
    .init(&IndexMeta {
      schema_version: SCHEMA_VERSION,
      chain: "regtest".into(),
      config: TrackerConfig::default(),
      catalog_fingerprint: "other".into(),
      height: None,
      hash: None,
      fast_sync_complete: false,
    })
    .unwrap();
  let err = format!("{:#}", context.run(options()).unwrap_err());
  assert!(err.contains("--reindex"), "{err}");
}

#[test]
fn forever_mode_follows_new_blocks_until_shutdown() {
  let context = Context::new(20);
  let indexer = context.indexer(options());
  let node = context.node.clone();
  let store = context.store.clone();
  let shutdown = context.shutdown.clone();
  thread::scope(|scope| {
    let handle = scope.spawn(|| indexer.run(RunMode::Forever));
    let wait_for = |height: u32| {
      let deadline = Instant::now() + Duration::from_secs(20);
      while store.meta().unwrap().and_then(|meta| meta.height) != Some(height) {
        assert!(Instant::now() < deadline, "timed out waiting for {height}");
        thread::sleep(Duration::from_millis(5));
      }
    };
    wait_for(19);
    node.mine(1);
    wait_for(20);
    node.reorg(2, 3);
    wait_for(21);
    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap().unwrap();
  });
  context.assert_consistent();
}

#[test]
fn height_limit_stops_below_it() {
  let context = Context::new(60);
  context
    .run(IndexerOptions {
      height_limit: Some(40),
      ..options()
    })
    .unwrap();
  let meta = context.store.meta().unwrap().unwrap();
  assert_eq!(meta.height, Some(39));
  assert_eq!(meta.hash, Some(context.node.hash(39)));
  assert!(meta.fast_sync_complete);
  // All of it by the fast sync: the limit is far enough below the tip.
  assert_eq!(context.node.fast_blocks_served.load(Ordering::Relaxed), 40);
  assert_eq!(context.node.live_blocks_served.load(Ordering::Relaxed), 0);

  // Raising the limit continues live.
  context
    .run(IndexerOptions {
      height_limit: Some(50),
      ..options()
    })
    .unwrap();
  assert_eq!(context.store.meta().unwrap().unwrap().height, Some(49));
  context.run(options()).unwrap();
  context.assert_consistent();
}

#[test]
fn status_tells_when_bitcoind_last_answered() {
  let context = Context::new(30);
  let start = unix_now();
  context.run(options()).unwrap();
  let status = context.published.status();
  assert!(status.timings.last_poll_ok_at.unwrap() >= start);
  assert!(status.timings.last_commit_at.unwrap() >= start);
  assert_eq!(status.error, None);

  // bitcoind stops answering: the times stay, the error says why.
  context
    .published
    .update_status(|status| status.timings.last_poll_ok_at = Some(1));
  context.node.mine(1);
  context.node.failures.store(1000, Ordering::Relaxed);
  assert!(
    context
      .run(IndexerOptions {
        max_retries: Some(2),
        ..options()
      })
      .is_err()
  );
  let status = context.published.status();
  assert_eq!(status.timings.last_poll_ok_at, Some(1));
  assert!(status.error.unwrap().contains("connection refused"));
}
