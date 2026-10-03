//! The overlay against a fake bitcoind and an index made of a real tracker:
//! pending transactions are added, chained, confirmed into the identical
//! records, replaced (RBF), evicted and conflicted; the sat flow equals the
//! tracker's; and every sale fixture classifies as pending exactly as in
//! its block.

use {
  super::*,
  crate::{
    history::HistoryKind,
    market::{
      self,
      fixtures::{fixture_blocks, fixture_context},
    },
    model::{ResolvedInput, ResolvedTx, TrackerConfig},
    sats::math::{first_sat, subsidy},
    tracker::{
      Tracker,
      tests::{Chain, catalog_for, out_to, tracker_at, txid},
    },
  },
  bitcoin::{
    ScriptBuf, Sequence, TxIn, Witness, absolute::LockTime, hashes::Hash, transaction::Version,
  },
  std::cell::RefCell,
};

/// bitcoind, faked: a best block, a mempool, mempool entries and the
/// outputs `prevouts` answers with.
struct NodeState {
  best: BlockHash,
  mempool: Vec<Transaction>,
  entries: HashMap<Txid, MempoolEntry>,
  outputs: HashMap<OutPoint, TxOut>,
  sequence: u64,
}

struct FakeNode(RefCell<NodeState>);

impl Default for FakeNode {
  fn default() -> Self {
    Self(RefCell::new(NodeState {
      best: BlockHash::all_zeros(),
      mempool: Vec::new(),
      entries: HashMap::new(),
      outputs: HashMap::new(),
      sequence: 0,
    }))
  }
}

impl FakeNode {
  fn set_mempool(&self, txs: Vec<Transaction>) {
    let mut state = self.0.borrow_mut();
    for (i, tx) in txs.iter().enumerate() {
      state
        .entries
        .entry(tx.compute_txid())
        .or_insert(MempoolEntry {
          time: 1_800_000_000 + i as u64,
          fee: 1_000,
          vsize: 200,
        });
      for (vout, output) in tx.output.iter().enumerate() {
        state.outputs.insert(
          OutPoint::new(tx.compute_txid(), vout as u32),
          output.clone(),
        );
      }
    }
    state.mempool = txs;
    state.sequence += 1;
  }
}

impl MempoolNode for FakeNode {
  fn best_block_hash(&self) -> anyhow::Result<BlockHash> {
    Ok(self.0.borrow().best)
  }

  fn raw_mempool(&self) -> anyhow::Result<RawMempool> {
    let state = self.0.borrow();
    Ok(RawMempool {
      txids: state
        .mempool
        .iter()
        .map(Transaction::compute_txid)
        .collect(),
      sequence: Some(state.sequence),
    })
  }

  fn transactions(&self, txids: &[Txid]) -> anyhow::Result<Vec<Option<Transaction>>> {
    let state = self.0.borrow();
    Ok(
      txids
        .iter()
        .map(|txid| {
          state
            .mempool
            .iter()
            .find(|tx| tx.compute_txid() == *txid)
            .cloned()
        })
        .collect(),
    )
  }

  fn entries(&self, txids: &[Txid]) -> anyhow::Result<Vec<Option<MempoolEntry>>> {
    let state = self.0.borrow();
    Ok(
      txids
        .iter()
        .map(|txid| state.entries.get(txid).copied())
        .collect(),
    )
  }

  fn prevouts(&self, outpoints: &[OutPoint]) -> anyhow::Result<Vec<Option<TxOut>>> {
    let state = self.0.borrow();
    Ok(
      outpoints
        .iter()
        .map(|outpoint| state.outputs.get(outpoint).cloned())
        .collect(),
    )
  }
}

/// The index: a tracker, and the moves of the blocks applied to it.
struct FakeIndex<'a> {
  tracker: &'a Tracker,
  moves: &'a HashMap<u32, Vec<(Txid, Vec<OutPoint>)>>,
}

impl TrackedIndex for FakeIndex<'_> {
  fn tip(&self) -> anyhow::Result<Option<(u32, BlockHash)>> {
    Ok(self.tracker.height().zip(self.tracker.hash()))
  }

  fn utxo(&self, outpoint: OutPoint) -> anyhow::Result<Option<TrackedUtxo>> {
    Ok(self.tracker.utxo(&outpoint).cloned())
  }

  fn time(&self, height: u32) -> anyhow::Result<Option<u32>> {
    Ok(self.tracker.time(height))
  }

  fn block_moves(&self, height: u32) -> anyhow::Result<Option<super::BlockMoves>> {
    Ok(self.moves.get(&height).cloned())
  }
}

fn btx(inputs: &[OutPoint], outputs: Vec<TxOut>) -> Transaction {
  Transaction {
    version: Version::TWO,
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
    output: outputs,
  }
}

/// A test world: a tracker at a recent height whose last block mined one
/// tracked output pair, an untracked funding output, and a fake bitcoind.
struct World {
  tracker: Tracker,
  chain: Chain,
  moves: HashMap<u32, Vec<(Txid, Vec<OutPoint>)>>,
  node: FakeNode,
  overlay: Overlay,
  ctx: MarketContext,
  /// The coinbase output holding the block's uncommon sat (10,000 sats).
  uncommon: OutPoint,
  /// Its uncommon sat.
  sat: u64,
  /// Untracked outputs of 100,000 sats.
  funds: Vec<OutPoint>,
  next_txid: u64,
}

impl World {
  fn new() -> Self {
    let config = TrackerConfig::default();
    let (mut tracker, mut chain) = tracker_at(&config, Some(900_000));
    let height: u32 = 900_001;
    let reward = subsidy(height.into());
    let coinbase = ResolvedTx {
      txid: txid(1),
      inputs: Vec::new(),
      outputs: vec![out_to(10_000, 1), out_to(reward - 10_000, 1)],
    };
    tracker
      .apply_block(&chain.block(vec![coinbase.clone()]))
      .unwrap();
    let node = FakeNode::default();
    {
      let mut state = node.0.borrow_mut();
      state.best = tracker.hash().unwrap();
      for (vout, output) in coinbase.outputs.iter().enumerate() {
        state
          .outputs
          .insert(OutPoint::new(coinbase.txid, vout as u32), output.clone());
      }
    }
    let funds: Vec<OutPoint> = (0..4).map(|n| OutPoint::new(txid(100 + n), 0)).collect();
    for fund in &funds {
      node
        .0
        .borrow_mut()
        .outputs
        .insert(*fund, out_to(100_000, 9));
    }
    World {
      ctx: MarketContext::new(
        catalog_for(&config),
        market::Marketplaces::embedded(),
        crate::settings::Chain::Mainnet,
      ),
      tracker,
      chain,
      moves: HashMap::new(),
      node,
      overlay: Overlay::default(),
      uncommon: OutPoint::new(txid(1), 0),
      sat: first_sat(height.into()),
      funds,
      next_txid: 1000,
    }
  }

  fn poll(&mut self) -> PollOutcome {
    let index = FakeIndex {
      tracker: &self.tracker,
      moves: &self.moves,
    };
    self
      .overlay
      .poll(&self.node, &index, &self.ctx, 1_800_000_100)
      .unwrap()
  }

  fn snapshot(&self) -> Snapshot {
    self.overlay.snapshot(true)
  }

  /// The value of an output, for resolving a transaction into a block.
  fn value(&self, outpoint: &OutPoint) -> u64 {
    self.node.0.borrow().outputs[outpoint].value.to_sat()
  }

  /// Mines `txs` (as given) into the next block and tells bitcoind.
  fn mine(&mut self, txs: &[Transaction]) -> crate::model::BlockDelta {
    let height = self.chain.next;
    let fees: u64 = txs
      .iter()
      .map(|tx| {
        tx.input
          .iter()
          .map(|input| self.value(&input.previous_output))
          .sum::<u64>()
          - tx
            .output
            .iter()
            .map(|output| output.value.to_sat())
            .sum::<u64>()
      })
      .sum();
    self.next_txid += 1;
    let mut resolved = vec![ResolvedTx {
      txid: txid(self.next_txid),
      inputs: Vec::new(),
      outputs: vec![out_to(subsidy(height.into()) + fees, 7)],
    }];
    for tx in txs {
      for (vout, output) in tx.output.iter().enumerate() {
        self.node.0.borrow_mut().outputs.insert(
          OutPoint::new(tx.compute_txid(), vout as u32),
          output.clone(),
        );
      }
      resolved.push(ResolvedTx {
        txid: tx.compute_txid(),
        inputs: tx
          .input
          .iter()
          .map(|input| ResolvedInput {
            previous_output: input.previous_output,
            value: self.value(&input.previous_output),
            script_hash: 0,
            sighash: None,
            envelopes: Vec::new(),
          })
          .collect(),
        outputs: tx.output.clone(),
      });
    }
    let block = self.chain.block(resolved);
    let delta = self.tracker.apply_block(&block).unwrap();
    self.moves.insert(
      height,
      delta
        .moves
        .iter()
        .filter(|m| !m.coinbase)
        .map(|m| {
          (
            m.txid,
            m.inputs.iter().map(|input| input.outpoint).collect(),
          )
        })
        .collect(),
    );
    self.node.0.borrow_mut().best = self.tracker.hash().unwrap();
    delta
  }
}

#[test]
fn pending_transfer_confirms_into_the_identical_move() {
  let mut world = World::new();
  // The uncommon (and 100,000 untracked sats) to owner 2 and 3.
  let t1 = btx(
    &[world.uncommon, world.funds[0]],
    vec![out_to(10_000, 2), out_to(95_000, 3)],
  );
  world.node.set_mempool(vec![t1.clone()]);
  assert_eq!(world.poll(), PollOutcome::Polled { changed: true });
  let snapshot = world.snapshot();
  assert!(snapshot.ready);
  assert_eq!(snapshot.seq, 1);
  assert_eq!(snapshot.txs.len(), 1);
  let pending = snapshot.txs[0].clone();
  assert_eq!(pending.txid, t1.compute_txid());
  assert_eq!(pending.height, 900_002);
  assert_eq!(pending.first_seen, 1_800_000_000);
  assert_eq!(pending.fee_rate(), 5.0);
  assert!(pending.depends.is_empty());
  assert_eq!(pending.kinds(), [HistoryKind::Transfer]);
  assert_eq!(
    snapshot.spend(&world.uncommon),
    Some(Spend { tx: 0, vin: 0 })
  );
  assert!(snapshot.spend(&world.funds[0]).is_none(), "untracked");
  assert_eq!(
    snapshot.locate(world.sat),
    Some(PendingLocation {
      tx: 0,
      kind: PendingLocationKind::Output {
        vout: 0,
        offset: 0,
        burned: false
      }
    })
  );
  let (_, output) = snapshot
    .output(&OutPoint::new(t1.compute_txid(), 0))
    .unwrap();
  assert_eq!(output.pieces[0].start, world.sat);
  assert_eq!(output.script_pubkey, out_to(0, 2).script_pubkey);
  assert_eq!(snapshot.outputs_to(&out_to(0, 2).script_pubkey).len(), 1);
  // One entry for the sender (owner 1) and one for the receiver (owner 2).
  assert_eq!(pending.classified.entries.len(), 2);

  // Nothing new: nothing to do.
  assert_eq!(world.poll(), PollOutcome::Unchanged);

  // Mined: the block's move is the pending one, and the pending one goes.
  let delta = world.mine(std::slice::from_ref(&t1));
  assert_eq!(delta.moves[0], pending.classified.mv);
  world.node.set_mempool(Vec::new());
  assert_eq!(world.poll(), PollOutcome::Polled { changed: true });
  let snapshot = world.snapshot();
  assert!(snapshot.txs.is_empty());
  assert_eq!(snapshot.seq, 2);
  assert_eq!(
    snapshot.removed,
    [Removed {
      txid: t1.compute_txid(),
      status: RemovedStatus::Confirmed,
      height: Some(900_002),
      replaced_by: None,
      at: 1_800_000_100,
      seq: 2,
    }]
  );
  assert!(snapshot.spend(&world.uncommon).is_none());
}

#[test]
fn a_block_not_yet_indexed_holds_the_pending_set() {
  let mut world = World::new();
  let t1 = btx(&[world.uncommon], vec![out_to(9_000, 2)]);
  world.node.set_mempool(vec![t1.clone()]);
  world.poll();
  // bitcoind mined it, the index has not caught up: no "dropped" flicker.
  world.node.0.borrow_mut().best = BlockHash::from_byte_array([7; 32]);
  world.node.set_mempool(Vec::new());
  assert_eq!(world.poll(), PollOutcome::Behind);
  assert_eq!(world.snapshot().txs.len(), 1);
  assert!(world.snapshot().removed.is_empty());
}

#[test]
fn chained_pending_transactions_and_replacement() {
  let mut world = World::new();
  let t1 = btx(
    &[world.uncommon, world.funds[0]],
    vec![out_to(10_000, 2), out_to(95_000, 3)],
  );
  let t1_out = OutPoint::new(t1.compute_txid(), 0);
  let t2 = btx(
    &[t1_out, world.funds[1]],
    vec![out_to(600, 4), out_to(105_000, 3)],
  );
  // A child listed before its parent is still processed after it.
  world.node.set_mempool(vec![t2.clone(), t1.clone()]);
  world.poll();
  let snapshot = world.snapshot();
  assert_eq!(snapshot.txs.len(), 2);
  let child = snapshot.tx(&t2.compute_txid()).unwrap();
  assert_eq!(child.depends, [t1.compute_txid()]);
  assert!(child.classified.mv.inputs[0].utxo.height == 900_002);
  // The sat's latest pending location is the end of the chain.
  let location = snapshot.locate(world.sat).unwrap();
  assert_eq!(snapshot.txs[location.tx].txid, t2.compute_txid());
  assert_eq!(
    location.kind,
    PendingLocationKind::Output {
      vout: 0,
      offset: 0,
      burned: false
    }
  );
  assert_eq!(
    snapshot
      .spend(&t1_out)
      .map(|spend| snapshot.txs[spend.tx].txid),
    Some(t2.compute_txid())
  );

  // RBF: t1' spends the same uncommon; t1 and its child go.
  let t1b = btx(
    &[world.uncommon, world.funds[0]],
    vec![out_to(10_000, 5), out_to(90_000, 3)],
  );
  world.node.set_mempool(vec![t1b.clone()]);
  assert_eq!(world.poll(), PollOutcome::Polled { changed: true });
  let snapshot = world.snapshot();
  assert_eq!(snapshot.txs.len(), 1);
  assert_eq!(snapshot.txs[0].txid, t1b.compute_txid());
  let mut removed: Vec<(Txid, RemovedStatus, Option<Txid>)> = snapshot
    .removed
    .iter()
    .map(|removed| (removed.txid, removed.status, removed.replaced_by))
    .collect();
  removed.sort();
  let mut expected = vec![
    (
      t1.compute_txid(),
      RemovedStatus::Replaced,
      Some(t1b.compute_txid()),
    ),
    (
      t2.compute_txid(),
      RemovedStatus::Replaced,
      Some(t1b.compute_txid()),
    ),
  ];
  expected.sort();
  assert_eq!(removed, expected);
  assert_eq!(
    snapshot.locate(world.sat).map(|l| snapshot.txs[l.tx].txid),
    Some(t1b.compute_txid())
  );

  // The parent confirms; the pending child now spends an indexed output.
  let t3 = btx(
    &[OutPoint::new(t1b.compute_txid(), 0)],
    vec![out_to(9_000, 6)],
  );
  world.node.set_mempool(vec![t1b.clone(), t3.clone()]);
  world.poll();
  assert_eq!(world.snapshot().txs.len(), 2);
  world.mine(std::slice::from_ref(&t1b));
  world.node.set_mempool(vec![t3.clone()]);
  world.poll();
  let snapshot = world.snapshot();
  assert_eq!(snapshot.txs.len(), 1);
  let child = &snapshot.txs[0];
  assert!(child.depends.is_empty());
  assert_eq!(child.classified.mv.inputs[0].utxo.height, 900_002);
  assert_eq!(child.height, 900_003);
  assert_eq!(snapshot.removed[0].status, RemovedStatus::Confirmed);
}

#[test]
fn evicted_and_conflicted() {
  let mut world = World::new();
  let t1 = btx(&[world.uncommon], vec![out_to(9_000, 2)]);
  world.node.set_mempool(vec![t1.clone()]);
  world.poll();
  world.node.set_mempool(Vec::new());
  world.poll();
  assert_eq!(world.snapshot().removed[0].status, RemovedStatus::Evicted);

  // Back in the mempool, then a block spends its input another way.
  world.node.set_mempool(vec![t1.clone()]);
  world.poll();
  assert_eq!(world.snapshot().txs.len(), 1);
  let other = btx(&[world.uncommon], vec![out_to(9_500, 8)]);
  world.mine(&[other]);
  world.node.set_mempool(Vec::new());
  world.poll();
  let snapshot = world.snapshot();
  assert!(snapshot.txs.is_empty());
  assert_eq!(snapshot.removed[0].txid, t1.compute_txid());
  assert_eq!(snapshot.removed[0].status, RemovedStatus::Conflicted);
}

#[test]
fn untracked_transactions_are_ignored_and_fees_have_no_satpoint() {
  let mut world = World::new();
  let plain = btx(&[world.funds[2]], vec![out_to(99_000, 2)]);
  // All of the uncommon UTXO goes to fees.
  let burn = btx(&[world.funds[3], world.uncommon], vec![out_to(100_000, 2)]);
  world.node.set_mempool(vec![plain, burn.clone()]);
  world.poll();
  let snapshot = world.snapshot();
  assert_eq!(snapshot.stats.mempool_txs, 2);
  assert_eq!(snapshot.stats.scanned_txs, 2);
  assert_eq!(snapshot.txs.len(), 1);
  assert_eq!(snapshot.txs[0].txid, burn.compute_txid());
  assert_eq!(
    snapshot.locate(world.sat).map(|l| l.kind),
    Some(PendingLocationKind::Fee)
  );
}

#[test]
fn a_transaction_without_resolvable_inputs_is_retried() {
  let mut world = World::new();
  let unknown = OutPoint::new(txid(555), 0);
  let t1 = btx(&[world.uncommon, unknown], vec![out_to(9_000, 2)]);
  world.node.set_mempool(vec![t1.clone()]);
  assert_eq!(world.poll(), PollOutcome::Polled { changed: false });
  assert!(world.snapshot().txs.is_empty());
  // bitcoind can value it now; the same mempool is examined again.
  world
    .node
    .0
    .borrow_mut()
    .outputs
    .insert(unknown, out_to(5_000, 9));
  assert_eq!(world.poll(), PollOutcome::Polled { changed: true });
  assert_eq!(world.snapshot().txs.len(), 1);
}

/// `flow::tx_move` equals the tracker's move for the same transaction,
/// over block 9 (one 5e9-sat piece) cut every which way, with untracked
/// inputs in between and sats going to fees.
#[test]
fn flow_matches_the_tracker() {
  let config = TrackerConfig::default();
  let mut seed = 0x2545_f491_4f6c_dd1du64;
  let mut random = move |n: u64| {
    seed ^= seed << 13;
    seed ^= seed >> 7;
    seed ^= seed << 17;
    seed % n
  };
  for round in 0..40 {
    let (mut tracker, mut chain) = tracker_at(&config, Some(8));
    let block9 = ResolvedTx {
      txid: txid(9),
      inputs: Vec::new(),
      outputs: vec![out_to(3_000_000_000, 1), out_to(2_000_000_000, 2)],
    };
    tracker.apply_block(&chain.block(vec![block9])).unwrap();
    let mut inputs = vec![(OutPoint::new(txid(9), 1), 2_000_000_000)];
    for n in 0..random(3) {
      inputs.insert(
        random(inputs.len() as u64 + 1) as usize,
        (OutPoint::new(txid(50 + n), 0), 1 + random(1_000_000_000)),
      );
    }
    if round % 2 == 0 {
      inputs.insert(
        random(inputs.len() as u64 + 1) as usize,
        (OutPoint::new(txid(9), 0), 3_000_000_000),
      );
    }
    let total: u64 = inputs.iter().map(|(_, value)| value).sum();
    let mut left = total - random(total / 4 + 1);
    let mut outputs = Vec::new();
    while left > 0 && outputs.len() < 12 {
      let value = if outputs.len() == 11 {
        left
      } else {
        1 + random(left)
      };
      outputs.push(out_to(value, 3 + (outputs.len() as u8 % 3)));
      left -= value;
    }
    let tx = ResolvedTx {
      txid: txid(1_000 + round),
      inputs: inputs
        .iter()
        .map(|&(previous_output, value)| ResolvedInput {
          previous_output,
          value,
          script_hash: 0,
          sighash: None,
          envelopes: Vec::new(),
        })
        .collect(),
      outputs,
    };
    let tracked: Vec<crate::model::MoveInput> = tx
      .inputs
      .iter()
      .enumerate()
      .filter_map(|(vin, input)| {
        Some(crate::model::MoveInput {
          vin: vin as u32,
          outpoint: input.previous_output,
          utxo: tracker.utxo(&input.previous_output)?.clone(),
        })
      })
      .collect();
    let pending = flow::tx_move(&tx, tracked, 1).unwrap();
    let fee: u64 = total - tx.outputs.iter().map(|o| o.value.to_sat()).sum::<u64>();
    let coinbase = ResolvedTx {
      txid: txid(2_000 + round),
      inputs: Vec::new(),
      outputs: vec![out_to(subsidy(10) + fee, 7)],
    };
    let delta = tracker
      .apply_block(&chain.block(vec![coinbase, tx]))
      .unwrap();
    let confirmed = delta.moves.iter().find(|m| !m.coinbase).unwrap();
    assert_eq!(*confirmed, pending, "round {round}");
  }
}

/// Every sale fixture (real mainnet shapes of every marketplace), seen in
/// the mempool, has the sat flow and the sales of its block.
#[test]
fn fixtures_classify_as_pending_like_in_their_blocks() {
  let ctx = fixture_context();
  let fixtures = fixture_blocks();
  assert!(fixtures.len() >= 40);
  for (name, block, delta) in fixtures {
    let tx = &block.txs[1];
    let confirmed = &delta.moves[0];
    let pending = flow::tx_move(tx, confirmed.inputs.clone(), 1).unwrap();
    assert_eq!(pending.inputs, confirmed.inputs, "{name}");
    assert_eq!(pending.outputs, confirmed.outputs, "{name}");
    let spenders: Vec<&ResolvedTx> = block.txs[2..].iter().collect();
    let time = block.header.time;
    let classified = classify::classify(
      tx,
      confirmed.clone(),
      &spenders,
      block.height,
      time,
      &ctx,
      &|_| Some(time),
    );
    let mut expected = market::analyze(&block, &delta, &ctx);
    expected.sales.retain(|sale| sale.txid == tx.txid);
    expected
      .inscriptions
      .retain(|inscription| inscription.txid == tx.txid);
    assert_eq!(classified.market, expected, "{name}");
    let sold = classified
      .entries
      .iter()
      .filter(|entry| entry.kind == HistoryKind::Sale)
      .count();
    assert_eq!(sold > 0, !expected.sales.is_empty(), "{name}");
  }
}
