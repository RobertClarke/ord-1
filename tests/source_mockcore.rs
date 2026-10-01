//! The block sources against mockcore: the fast and the live source must
//! produce identical `ResolvedBlock`s.

use {
  bitcoin::{
    Amount, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, WPubkeyHash,
    Witness, absolute::LockTime, consensus, hashes::Hash, transaction::Version,
  },
  mockcore::Handle,
  ord::{
    model::{ResolvedBlock, script_hash},
    source::{
      FastSource, FastSyncOptions, LiveSource, Rpc, RpcAuth, RpcConfig, RpcError, is_unspendable,
    },
  },
  std::time::{Duration, Instant},
};

const COIN: u64 = 100_000_000;

fn connect(core: &Handle) -> Rpc {
  Rpc::new(RpcConfig {
    url: core.url(),
    auth: RpcAuth::Cookie(core.cookie_file()),
    timeout_secs: 30,
  })
  .unwrap()
}

fn options(stop_height: u32, fetch_threads: usize, buffer_blocks: usize) -> FastSyncOptions {
  FastSyncOptions {
    stop_height,
    fetch_threads,
    buffer_blocks,
    utxo_capacity: 1_000,
  }
}

fn fast_blocks(rpc: &Rpc, options: FastSyncOptions) -> (Vec<ResolvedBlock>, FastSource) {
  let mut source = FastSource::start(rpc.clone(), options).unwrap();
  let mut blocks = Vec::new();
  while let Some(block) = source.next_block() {
    blocks.push(block.unwrap());
  }
  assert!(source.next_block().is_none());
  (blocks, source)
}

fn out(value: u64) -> TxOut {
  TxOut {
    value: Amount::from_sat(value),
    script_pubkey: ScriptBuf::new_p2wpkh(&WPubkeyHash::all_zeros()),
  }
}

fn op_return(value: u64) -> TxOut {
  TxOut {
    value: Amount::from_sat(value),
    script_pubkey: ScriptBuf::from_bytes(
      [&[0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed][..], &[7; 32]].concat(),
    ),
  }
}

/// An output whose script is `len` bytes of OP_TRUE.
fn sized(value: u64, len: usize) -> TxOut {
  TxOut {
    value: Amount::from_sat(value),
    script_pubkey: ScriptBuf::from_bytes(vec![0x51; len]),
  }
}

fn tx(inputs: &[OutPoint], outputs: Vec<TxOut>) -> Transaction {
  Transaction {
    version: Version::TWO,
    lock_time: LockTime::ZERO,
    input: inputs
      .iter()
      .map(|outpoint| TxIn {
        previous_output: *outpoint,
        script_sig: ScriptBuf::new(),
        sequence: Sequence::MAX,
        witness: Witness::from_slice(&[[1u8; 72].as_slice(), [2u8; 33].as_slice()]),
      })
      .collect(),
    output: outputs,
  }
}

fn coinbase(tag: &[u8], outputs: Vec<TxOut>) -> Transaction {
  Transaction {
    version: Version::ONE,
    lock_time: LockTime::ZERO,
    input: vec![TxIn {
      previous_output: OutPoint::null(),
      script_sig: ScriptBuf::from_bytes(tag.to_vec()),
      sequence: Sequence::MAX,
      witness: Witness::new(),
    }],
    output: outputs,
  }
}

fn outpoint(txid: Txid, vout: u32) -> OutPoint {
  OutPoint { txid, vout }
}

fn block(core: &Handle, height: usize) -> bitcoin::Block {
  let hash = core.block_hash(height);
  core.state().blocks[&hash].clone()
}

fn coinbase_txid(core: &Handle, height: usize) -> Txid {
  core.tx(height, 0).compute_txid()
}

struct Chain {
  core: Handle,
  /// Transactions whose input values are checked, with those values.
  expected: Vec<(u32, usize, Vec<u64>)>,
}

/// A regtest chain exercising multi-input/multi-output transactions,
/// intra-block spend chains, OP_RETURN, zero-value and oversized outputs,
/// custom coinbases and a BIP30 duplicate coinbase.
fn chain() -> Chain {
  let core = mockcore::builder().network(Network::Regtest).build();
  let mut expected = Vec::new();

  core.mine_blocks(3);

  // Block 4: a coinbase with several outputs, then a chain of spends.
  let c4 = coinbase(
    b"\x04four",
    vec![out(25 * COIN), out(0), op_return(0), out(25 * COIN)],
  );
  let t1 = tx(
    &[
      outpoint(coinbase_txid(&core, 1), 0),
      outpoint(coinbase_txid(&core, 2), 0),
    ],
    vec![out(60 * COIN), out(0), op_return(COIN), out(38 * COIN)],
  );
  let t1_id = core.broadcast_raw_tx(t1);
  let t2 = tx(&[outpoint(t1_id, 0)], vec![out(30 * COIN), out(29 * COIN)]);
  let t2_id = core.broadcast_raw_tx(t2);
  let t3 = tx(
    &[outpoint(t2_id, 1), outpoint(t1_id, 3)],
    vec![out(66 * COIN), out(1)],
  );
  core.broadcast_raw_tx(t3);
  let t4 = tx(&[outpoint(t1_id, 1)], vec![out(0)]);
  core.broadcast_raw_tx(t4);
  let t5 = tx(
    &[outpoint(coinbase_txid(&core, 3), 0)],
    vec![sized(COIN, 10_001), sized(COIN, 10_000), out(47 * COIN)],
  );
  let t5_id = core.broadcast_raw_tx(t5);
  let c4_id = c4.compute_txid();
  core.mine_block_with_coinbase(c4);
  expected.push((4, 1, vec![50 * COIN, 50 * COIN]));
  expected.push((4, 2, vec![60 * COIN]));
  expected.push((4, 3, vec![29 * COIN, 38 * COIN]));
  expected.push((4, 4, vec![0]));
  expected.push((4, 5, vec![50 * COIN]));

  // Block 5: spends of earlier blocks, including a zero-value coinbase
  // output and the 10,000-byte script (still spendable).
  let t6 = tx(
    &[
      outpoint(c4_id, 0),
      outpoint(c4_id, 1),
      outpoint(t2_id, 0),
      outpoint(t5_id, 1),
    ],
    vec![out(50 * COIN), out(6 * COIN)],
  );
  let t6_id = core.broadcast_raw_tx(t6);
  let t7 = tx(&[outpoint(t6_id, 1)], vec![out(5 * COIN), out(COIN - 1000)]);
  core.broadcast_raw_tx(t7);
  core.mine_blocks(1);
  expected.push((5, 1, vec![25 * COIN, 0, 30 * COIN, COIN]));
  expected.push((5, 2, vec![6 * COIN]));

  // Blocks 6–8: the same coinbase twice (BIP30 duplicate).
  let dup = coinbase(b"\x06dup", vec![out(50 * COIN), out(0)]);
  let dup_id = dup.compute_txid();
  core.mine_block_with_coinbase(dup.clone());
  core.mine_blocks(1);
  core.mine_block_with_coinbase(dup);

  // Block 9: spends the duplicate's outputs.
  core.broadcast_raw_tx(tx(
    &[outpoint(dup_id, 0), outpoint(dup_id, 1)],
    vec![out(50 * COIN)],
  ));
  core.mine_blocks(1);
  expected.push((9, 1, vec![50 * COIN, 0]));

  core.mine_blocks(3);

  Chain { core, expected }
}

/// UTXO set size Core would report: mockcore keeps oversized scripts.
fn utxo_count(core: &Handle) -> u64 {
  let state = core.state();
  state
    .utxos
    .keys()
    .filter(|outpoint| {
      !is_unspendable(
        &state.transactions[&outpoint.txid].output[outpoint.vout as usize].script_pubkey,
      )
    })
    .count() as u64
}

#[test]
fn fast_and_live_sources_agree() {
  let Chain { core, expected } = chain();
  let tip = core.height() as u32;
  assert_eq!(tip, 12);
  let rpc = connect(&core);

  let (fast, source) = fast_blocks(&rpc, options(tip, 4, 3));
  assert_eq!(fast.len(), tip as usize + 1);

  for (height, block) in fast.iter().enumerate() {
    assert_eq!(block.height, height as u32);
    assert_eq!(block.hash, core.block_hash(height));
    assert!(block.txs[0].inputs.is_empty());
  }

  for (height, tx, values) in &expected {
    let block = &fast[*height as usize];
    assert_eq!(
      block.txs[*tx]
        .inputs
        .iter()
        .map(|input| input.value)
        .collect::<Vec<u64>>(),
      *values,
      "block {height} tx {tx}"
    );
  }

  // Outputs are delivered in full, including OP_RETURN, zero-value and
  // oversized ones.
  assert_eq!(fast[4].txs[0].outputs[1], out(0));
  assert_eq!(fast[4].txs[0].outputs[2], op_return(0));
  assert_eq!(fast[4].txs[1].outputs[2], op_return(COIN));
  assert_eq!(fast[4].txs[5].outputs[0], sized(COIN, 10_001));
  assert_eq!(fast[6].txs[0].txid, fast[8].txs[0].txid);

  let stats = source.stats();
  assert_eq!(stats.height, Some(tip));
  assert_eq!(stats.utxos, utxo_count(&core));
  assert_eq!(
    stats.bytes_fetched,
    (0..=tip as usize)
      .map(|height| consensus::serialize(&block(&core, height)).len() as u64)
      .sum::<u64>()
  );
  let diagnostics = source.diagnostics();
  assert_eq!(diagnostics.overwrites, 2);
  assert_eq!(diagnostics.intra_block_spends, 5);
  assert_eq!(diagnostics.blocks_fetched, u64::from(tip) + 1);

  let live = LiveSource::new(rpc.clone());
  for block in &fast {
    assert_eq!(&live.block(block.hash, block.height).unwrap(), block);
  }
  assert_eq!(
    live
      .blocks(0, tip, 3)
      .collect::<anyhow::Result<Vec<ResolvedBlock>>>()
      .unwrap(),
    fast
  );

  // Every input carries the script hash of the output it spends, whether
  // that output came through the UTXO map, was spent in its own block, or
  // was re-created by the duplicate coinbase.
  let mut outputs = std::collections::HashMap::new();
  let mut checked = 0;
  for block in &fast {
    for tx in &block.txs {
      for input in &tx.inputs {
        let spent: &ScriptBuf = outputs
          .remove(&input.previous_output)
          .expect("spends an earlier output");
        assert_eq!(
          input.script_hash,
          script_hash(spent),
          "{}",
          input.previous_output
        );
        checked += 1;
      }
      for (vout, output) in tx.outputs.iter().enumerate() {
        outputs.insert(outpoint(tx.txid, vout as u32), &output.script_pubkey);
      }
    }
  }
  assert_eq!(checked, 14);
  // Distinct scripts were spent, so the hashes are not all alike.
  let hashes = fast
    .iter()
    .flat_map(|block| &block.txs)
    .flat_map(|tx| &tx.inputs)
    .map(|input| input.script_hash)
    .collect::<std::collections::BTreeSet<u32>>();
  assert!(hashes.len() >= 4, "{hashes:?}");
}

#[test]
fn fast_source_results_do_not_depend_on_parallelism() {
  let Chain { core, .. } = chain();
  let tip = core.height() as u32;
  let rpc = connect(&core);
  let (reference, _) = fast_blocks(&rpc, options(tip, 1, 1));
  for (fetch_threads, buffer_blocks) in [(2, 1), (3, 2), (8, 64), (16, 4)] {
    let (blocks, _) = fast_blocks(&rpc, options(tip, fetch_threads, buffer_blocks));
    assert_eq!(
      blocks, reference,
      "{fetch_threads} threads, {buffer_blocks} buffered"
    );
  }
}

#[test]
fn fast_source_stops_at_stop_height() {
  let Chain { core, .. } = chain();
  let rpc = connect(&core);
  let (blocks, source) = fast_blocks(&rpc, options(5, 3, 2));
  assert_eq!(
    blocks
      .iter()
      .map(|block| block.height)
      .collect::<Vec<u32>>(),
    (0..=5).collect::<Vec<u32>>()
  );
  assert_eq!(source.stats().height, Some(5));

  let (blocks, _) = fast_blocks(&rpc, options(0, 3, 2));
  assert_eq!(blocks.len(), 1);
  assert_eq!(blocks[0].hash, core.block_hash(0));

  let error = FastSource::start(rpc.clone(), options(core.height() as u32 + 1, 2, 2))
    .err()
    .unwrap();
  assert!(
    error.to_string().contains("above bitcoind's tip"),
    "{error}"
  );
}

#[test]
fn dropping_the_fast_source_stops_it_promptly() {
  let core = mockcore::builder().network(Network::Regtest).build();
  core.mine_blocks(200);
  let rpc = connect(&core);

  let mut source = FastSource::start(rpc.clone(), options(200, 8, 4)).unwrap();
  for height in 0..3 {
    assert_eq!(source.next_block().unwrap().unwrap().height, height);
  }
  assert_eq!(source.stats().height, Some(2));
  let start = Instant::now();
  drop(source);
  assert!(
    start.elapsed() < Duration::from_secs(1),
    "{:?}",
    start.elapsed()
  );

  // Dropping before the first block.
  let source = FastSource::start(rpc, options(200, 8, 64)).unwrap();
  let start = Instant::now();
  drop(source);
  assert!(
    start.elapsed() < Duration::from_secs(1),
    "{:?}",
    start.elapsed()
  );
}

#[test]
fn dropping_the_fast_source_while_bitcoind_is_down_is_prompt() {
  let core = mockcore::builder().network(Network::Regtest).build();
  core.mine_blocks(50);
  let mut source = FastSource::start(connect(&core), options(50, 4, 2)).unwrap();
  assert_eq!(source.next_block().unwrap().unwrap().height, 0);

  // The fetchers now fail and keep retrying (for minutes).
  drop(core);
  std::thread::sleep(Duration::from_millis(300));

  let start = Instant::now();
  drop(source);
  assert!(
    start.elapsed() < Duration::from_secs(1),
    "{:?}",
    start.elapsed()
  );
}

#[test]
fn live_blocks_iterator() {
  let core = mockcore::builder().network(Network::Regtest).build();
  core.mine_blocks(10);
  let rpc = connect(&core);
  let live = LiveSource::new(rpc);

  let heights = |from, to, threads| {
    live
      .blocks(from, to, threads)
      .map(|block| block.unwrap().height)
      .collect::<Vec<u32>>()
  };
  assert_eq!(heights(2, 7, 3), (2..=7).collect::<Vec<u32>>());
  assert_eq!(heights(4, 4, 8), vec![4]);
  assert_eq!(heights(0, 10, 1), (0..=10).collect::<Vec<u32>>());
  assert!(heights(5, 4, 2).is_empty());

  // Past the tip: the tip, then the error, then nothing.
  let mut blocks = live.blocks(10, 12, 2);
  assert_eq!(blocks.next().unwrap().unwrap().height, 10);
  let error = blocks.next().unwrap().unwrap_err();
  assert_eq!(
    error.downcast_ref::<RpcError>().unwrap().code,
    RpcError::INVALID_PARAMETER,
    "{error:#}"
  );
  assert!(blocks.next().is_none());

  // Wrong height for a hash.
  let error = live.block(core.block_hash(3), 4).unwrap_err();
  assert!(error.to_string().contains("is at height 3"), "{error}");
}

#[test]
fn rpc_against_mockcore() {
  let core = mockcore::builder().network(Network::Regtest).build();
  core.mine_blocks(5);
  let rpc = connect(&core);

  assert_eq!(rpc.chain().unwrap(), "regtest");
  assert_eq!(rpc.block_count().unwrap(), 5);
  assert_eq!(rpc.best_block_hash().unwrap(), core.block_hash(5));
  assert_eq!(rpc.block_hash(3).unwrap(), core.block_hash(3));
  assert_eq!(
    rpc.block_hashes(0..6).unwrap(),
    (0..6)
      .map(|height| core.block_hash(height))
      .collect::<Vec<_>>()
  );

  let block = block(&core, 4);
  assert_eq!(rpc.block_header(block.block_hash()).unwrap(), block.header);
  assert_eq!(
    rpc.block_raw(block.block_hash()).unwrap(),
    consensus::serialize(&block)
  );

  let error = rpc.block_hash(6).unwrap_err();
  assert_eq!(
    error.downcast_ref::<RpcError>().unwrap().code,
    RpcError::INVALID_PARAMETER
  );
  let error = rpc.block_raw(bitcoin::BlockHash::all_zeros()).unwrap_err();
  assert_eq!(
    error.downcast_ref::<RpcError>().unwrap().code,
    RpcError::INVALID_ADDRESS_OR_KEY
  );

  let mainnet = mockcore::spawn();
  assert_eq!(connect(&mainnet).chain().unwrap(), "main");
}

#[test]
fn mockcore_serves_core_shaped_json() {
  use serde_json::{Value, json};

  let core = mockcore::builder().network(Network::Regtest).build();
  core.mine_blocks(2);
  let spend = tx(
    &[outpoint(coinbase_txid(&core, 1), 0)],
    vec![out(30 * COIN), op_return(0), out(19 * COIN)],
  );
  let spend_id = core.broadcast_raw_tx(spend);
  core.mine_blocks(2);
  let rpc = connect(&core);
  let hash = core.block_hash(3);

  let header = rpc
    .call::<Value>("getblockheader", &[hash.to_string().into()])
    .unwrap();
  assert_eq!(header["hash"], hash.to_string());
  assert_eq!(header["height"], 3);
  assert_eq!(header["confirmations"], 2);
  assert_eq!(header["previousblockhash"], core.block_hash(2).to_string());
  assert_eq!(header["nextblockhash"], core.block_hash(4).to_string());
  assert_eq!(header["nTx"], 2);
  assert_eq!(header["time"], 3);
  assert!(header["difficulty"].is_number());
  let genesis = rpc
    .call::<Value>(
      "getblockheader",
      &[core.block_hash(0).to_string().into(), true.into()],
    )
    .unwrap();
  assert!(genesis.get("previousblockhash").is_none());
  assert_eq!(genesis["bits"], "207fffff");

  let v1 = rpc
    .call::<Value>("getblock", &[hash.to_string().into()])
    .unwrap();
  assert_eq!(v1["tx"][1], spend_id.to_string());
  assert_eq!(
    rpc
      .call::<Value>("getblock", &[hash.to_string().into(), true.into()])
      .unwrap(),
    v1
  );
  assert!(
    rpc
      .call::<Value>("getblock", &[hash.to_string().into(), false.into()])
      .unwrap()
      .is_string()
  );

  let v2 = rpc
    .call::<Value>("getblock", &[hash.to_string().into(), 2.into()])
    .unwrap();
  let tx = &v2["tx"][1];
  assert_eq!(tx["txid"], spend_id.to_string());
  assert!(tx["vin"][0].get("prevout").is_none());
  assert!(tx.get("fee").is_none());
  assert_eq!(tx["vout"][0]["value"], json!(30.0));
  assert_eq!(tx["vout"][1]["scriptPubKey"]["type"], "nulldata");
  assert_eq!(tx["vout"][2]["n"], 2);
  assert!(v2["tx"][0]["vin"][0]["coinbase"].is_string());

  let v3 = rpc
    .call::<Value>("getblock", &[hash.to_string().into(), 3.into()])
    .unwrap();
  let tx = &v3["tx"][1];
  assert_eq!(tx["vin"][0]["prevout"]["value"], json!(50.0));
  assert_eq!(tx["vin"][0]["prevout"]["height"], 1);
  assert_eq!(tx["vin"][0]["prevout"]["generated"], true);
  assert_eq!(tx["fee"], json!(1.0));
  assert!(v3["tx"][0]["vin"][0].get("prevout").is_none());

  let info = rpc.call::<Value>("getblockchaininfo", &[]).unwrap();
  assert_eq!(info["chain"], "regtest");
  assert_eq!(info["blocks"], 4);
  assert_eq!(info["bestblockhash"], core.block_hash(4).to_string());

  let error = rpc
    .call::<Value>("getblock", &[hash.to_string().into(), 4.into()])
    .unwrap_err();
  assert!(error.downcast_ref::<RpcError>().is_some(), "{error:#}");
}

mod witness {
  use {
    super::*,
    bitcoin::{
      opcodes::{self, all::*},
      script::{self, PushBytesBuf},
      secp256k1::{Message, PublicKey, Secp256k1, SecretKey},
    },
    ord::model::Envelope,
  };

  fn push(bytes: &[u8]) -> PushBytesBuf {
    PushBytesBuf::try_from(bytes.to_vec()).unwrap()
  }

  fn der(flag: u8) -> Vec<u8> {
    let secp = Secp256k1::new();
    let key = SecretKey::from_slice(&[0x42; 32]).unwrap();
    let mut sig = secp
      .sign_ecdsa(&Message::from_digest([flag; 32]), &key)
      .serialize_der()
      .to_vec();
    sig.push(flag);
    sig
  }

  fn pubkey() -> Vec<u8> {
    let secp = Secp256k1::new();
    PublicKey::from_secret_key(&secp, &SecretKey::from_slice(&[0x42; 32]).unwrap())
      .serialize()
      .to_vec()
  }

  fn schnorr(flag: Option<u8>) -> Vec<u8> {
    let mut sig = vec![0x77; 64];
    sig.extend(flag);
    sig
  }

  fn control_block() -> Vec<u8> {
    let mut control = vec![0xc1];
    control.extend([0x55; 64]);
    control
  }

  /// A reveal script: `<key> OP_CHECKSIG` and one envelope per item list.
  fn reveal(envelopes: &[&[&[u8]]]) -> ScriptBuf {
    let mut builder = script::Builder::new()
      .push_slice([0x20; 32])
      .push_opcode(OP_CHECKSIG);
    for items in envelopes {
      builder = builder
        .push_opcode(opcodes::OP_FALSE)
        .push_opcode(OP_IF)
        .push_slice(b"ord");
      for item in *items {
        builder = builder.push_slice(push(item));
      }
      builder = builder.push_opcode(OP_ENDIF);
    }
    builder.into_script()
  }

  fn reveal_witness(signature: Vec<u8>, script: &ScriptBuf) -> Witness {
    Witness::from_slice(&[signature, script.to_bytes(), control_block()])
  }

  fn spend(inputs: Vec<(OutPoint, ScriptBuf, Witness)>, outputs: Vec<TxOut>) -> Transaction {
    Transaction {
      version: Version::TWO,
      lock_time: LockTime::ZERO,
      input: inputs
        .into_iter()
        .map(|(previous_output, script_sig, witness)| TxIn {
          previous_output,
          script_sig,
          sequence: Sequence::MAX,
          witness,
        })
        .collect(),
      output: outputs,
    }
  }

  fn key_path(signature: Vec<u8>) -> (ScriptBuf, Witness) {
    (ScriptBuf::new(), Witness::from_slice(&[signature]))
  }

  const PLAIN: Envelope = Envelope {
    pointer: None,
    unrecognized_even_field: false,
  };

  #[test]
  fn sighash_and_envelopes_agree_between_sources() {
    let core = mockcore::builder().network(Network::Regtest).build();
    core.mine_blocks(6);
    let cb = |height| outpoint(coinbase_txid(&core, height), 0);

    let text: &[&[u8]] = &[&[1], b"text/plain;charset=utf-8", &[], b"hello"];

    // Block 7.
    let (s, w) = key_path(schnorr(None));
    let t1 = core.broadcast_raw_tx(spend(vec![(cb(1), s, w)], vec![out(50 * COIN)]));
    let (s, w) = key_path(schnorr(Some(0x83)));
    let t2 = core.broadcast_raw_tx(spend(vec![(cb(2), s, w)], vec![out(50 * COIN)]));
    let t3 = core.broadcast_raw_tx(spend(
      vec![(
        cb(3),
        ScriptBuf::new(),
        Witness::from_slice(&[der(0x83), pubkey()]),
      )],
      vec![out(50 * COIN)],
    ));
    let legacy = script::Builder::new()
      .push_slice(push(&der(0x01)))
      .push_slice(push(&pubkey()))
      .into_script();
    core.broadcast_raw_tx(spend(
      vec![(cb(4), legacy, Witness::new())],
      vec![out(50 * COIN)],
    ));
    let multisig = script::Builder::new()
      .push_opcode(opcodes::OP_0)
      .push_slice(push(&der(0x02)))
      .push_slice(push(&der(0x01)))
      .push_slice(push(&[0x52; 71]))
      .into_script();
    let nested = script::Builder::new()
      .push_slice(push(
        ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([3; 20])).as_bytes(),
      ))
      .into_script();
    core.broadcast_raw_tx(spend(
      vec![
        (cb(5), multisig, Witness::new()),
        (cb(6), nested, Witness::from_slice(&[der(0x81), pubkey()])),
      ],
      vec![out(60 * COIN), out(40 * COIN)],
    ));
    // Reveals an inscription spending t1's output in the same block.
    core.broadcast_raw_tx(spend(
      vec![(
        outpoint(t1, 0),
        ScriptBuf::new(),
        reveal_witness(schnorr(None), &reveal(&[text])),
      )],
      vec![out(50 * COIN)],
    ));
    core.mine_blocks(1);

    // Block 8: envelopes in a second input, several per input, pointers,
    // and a script that does not parse.
    let mut truncated = reveal(&[text]).into_bytes();
    truncated.push(0x02);
    core.broadcast_raw_tx(spend(
      vec![
        (
          outpoint(t2, 0),
          ScriptBuf::new(),
          Witness::from_slice(&[schnorr(Some(0x01))]),
        ),
        (
          outpoint(t3, 0),
          ScriptBuf::new(),
          reveal_witness(
            schnorr(Some(0x83)),
            &reveal(&[
              text,
              &[&[2], &[0xe8, 0x03], &[], b"x"],
              &[&[2], &[9; 9]],
              // ord's reserved "unbound" tag: an unrecognized even field.
              &[&[66], &[1], &[], b"y"],
            ]),
          ),
        ),
      ],
      vec![out(100 * COIN)],
    ));
    core.mine_blocks(1);
    let block8 = core.block_hash(8);
    let malformed_input = outpoint(core.tx(8, 1).compute_txid(), 0);
    core.broadcast_raw_tx(spend(
      vec![(
        malformed_input,
        ScriptBuf::new(),
        reveal_witness(schnorr(None), &ScriptBuf::from_bytes(truncated)),
      )],
      vec![out(100 * COIN)],
    ));
    core.mine_blocks(1);

    let rpc = connect(&core);
    let tip = core.height() as u32;
    let (fast, _) = fast_blocks(&rpc, options(tip, 3, 4));
    let live = LiveSource::new(rpc.clone());
    for block in &fast {
      assert_eq!(&live.block(block.hash, block.height).unwrap(), block);
    }

    let inputs = |height: usize, tx: usize| {
      fast[height].txs[tx]
        .inputs
        .iter()
        .map(|input| (input.sighash, input.envelopes.clone()))
        .collect::<Vec<_>>()
    };

    assert_eq!(inputs(7, 1), [(Some(0x00), vec![])]);
    assert_eq!(inputs(7, 2), [(Some(0x83), vec![])]);
    assert_eq!(inputs(7, 3), [(Some(0x83), vec![])]);
    assert_eq!(inputs(7, 4), [(Some(0x01), vec![])]);
    assert_eq!(inputs(7, 5), [(Some(0x02), vec![]), (Some(0x81), vec![])]);
    assert_eq!(inputs(7, 6), [(Some(0x00), vec![PLAIN])]);
    assert_eq!(fast[8].hash, block8);
    assert_eq!(
      inputs(8, 1),
      [
        (Some(0x01), vec![]),
        (
          Some(0x83),
          vec![
            PLAIN,
            Envelope {
              pointer: Some(1000),
              unrecognized_even_field: false
            },
            PLAIN,
            Envelope {
              pointer: None,
              unrecognized_even_field: true
            },
          ]
        ),
      ]
    );
    assert_eq!(inputs(9, 1), [(Some(0x00), vec![])]);

    // Coinbases and plain mockcore spends carry nothing.
    for block in &fast {
      assert!(block.txs[0].inputs.is_empty());
    }
  }
}
