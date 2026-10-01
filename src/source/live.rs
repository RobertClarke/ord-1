//! The live source: `getblock <hash> 3`, whose inputs carry the spent
//! outputs (`prevout`) from Core's undo data.

use {
  super::{
    Rpc, hex, input_envelopes, input_sighash,
    rpc::{ParseError, RpcError},
  },
  crate::model::{ResolvedBlock, ResolvedInput, ResolvedTx, script_hash},
  anyhow::{Context, anyhow, bail, ensure},
  bitcoin::{
    Amount, BlockHash, CompactTarget, OutPoint, Script, ScriptBuf, TxMerkleNode, TxOut, Txid,
    Witness,
    block::{Header, Version},
    hashes::Hash,
    merkle_tree,
  },
  crossbeam_channel::{Receiver, Sender, bounded, select, unbounded},
  serde::Deserialize,
  serde_json::value::RawValue,
  std::{
    collections::BTreeMap,
    sync::{
      Arc,
      atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
  },
};

/// `getblock <hash> 3`: prevout values come from Core's undo data, so any
/// height can be resolved without local UTXO state.
#[derive(Clone, Debug)]
pub struct LiveSource {
  rpc: Rpc,
}

impl LiveSource {
  pub fn new(rpc: Rpc) -> Self {
    Self { rpc }
  }

  pub fn block(&self, hash: BlockHash, height: u32) -> anyhow::Result<ResolvedBlock> {
    let block = self.rpc.call_parsed(
      "getblock",
      &[hash.to_string().into(), 3.into()],
      parse_block,
    )?;
    ensure!(
      block.hash == hash,
      "getblock {hash} returned block {}",
      block.hash
    );
    ensure!(
      block.height == height,
      "block {hash} is at height {}, not {height}",
      block.height
    );
    Ok(block)
  }

  /// Heights `from..=to` in order, fetched `threads` at a time.
  pub fn blocks(
    &self,
    from: u32,
    to: u32,
    threads: usize,
  ) -> Box<dyn Iterator<Item = anyhow::Result<ResolvedBlock>> + Send + '_> {
    if from > to {
      return Box::new(std::iter::empty());
    }
    Box::new(Blocks::start(self.clone(), from, to, threads.max(1)))
  }
}

/// Parallel prefetch, in-order delivery. Holds at most `2 × threads` blocks.
struct Blocks {
  results: Receiver<(u32, anyhow::Result<ResolvedBlock>)>,
  permits: Sender<()>,
  pending: BTreeMap<u32, anyhow::Result<ResolvedBlock>>,
  next: u32,
  to: u32,
  done: bool,
  /// Dropped (with the iterator) to stop the workers.
  _shutdown: Sender<()>,
  /// Set on drop to stop the workers' RPC retries.
  cancel: Arc<AtomicBool>,
}

impl Drop for Blocks {
  fn drop(&mut self) {
    self.cancel.store(true, Ordering::Relaxed);
  }
}

impl Blocks {
  fn start(source: LiveSource, from: u32, to: u32, threads: usize) -> Self {
    let window = 2 * threads;
    let (shutdown_tx, shutdown_rx) = bounded::<()>(0);
    let (permits_tx, permits_rx) = bounded::<()>(window);
    for _ in 0..window {
      permits_tx.send(()).unwrap();
    }
    let (results_tx, results_rx) = unbounded();
    let next_height = Arc::new(AtomicU32::new(from));
    let cancel = Arc::new(AtomicBool::new(false));
    let source = LiveSource::new(source.rpc.with_cancel(cancel.clone()));

    for i in 0..threads.min((to - from) as usize + 1) {
      let source = source.clone();
      let permits = permits_rx.clone();
      let results = results_tx.clone();
      let shutdown = shutdown_rx.clone();
      let next_height = next_height.clone();
      let spawned = thread::Builder::new()
        .name(format!("live-fetch-{i}"))
        .spawn(move || {
          loop {
            select! {
              recv(permits) -> permit => if permit.is_err() { return },
              recv(shutdown) -> _ => return,
            }
            let height = next_height.fetch_add(1, Ordering::Relaxed);
            if height > to || height < from {
              return;
            }
            let block = source
              .rpc
              .block_hash(height)
              .and_then(|hash| source.block(hash, height))
              .with_context(|| format!("fetching block {height} with getblock verbosity 3"));
            if results.send((height, block)).is_err() {
              return;
            }
          }
        });
      if let Err(error) = spawned {
        let _ = results_tx.send((from, Err(anyhow!(error).context("spawning live fetcher"))));
        break;
      }
    }

    Self {
      results: results_rx,
      permits: permits_tx,
      pending: BTreeMap::new(),
      next: from,
      to,
      done: false,
      _shutdown: shutdown_tx,
      cancel,
    }
  }
}

impl Iterator for Blocks {
  type Item = anyhow::Result<ResolvedBlock>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.done {
      return None;
    }

    let result = loop {
      if let Some(result) = self.pending.remove(&self.next) {
        break result;
      }
      match self.results.recv() {
        Ok((height, result)) => {
          self.pending.insert(height, result);
        }
        Err(_) => break Err(anyhow!("live fetchers stopped before block {}", self.next)),
      }
    };

    let _ = self.permits.try_send(());

    if result.is_err() || self.next == self.to {
      self.done = true;
    } else {
      self.next += 1;
    }

    Some(result)
  }
}

#[derive(Deserialize)]
struct Envelope<'a> {
  #[serde(borrow, default)]
  result: Option<VerboseBlock<'a>>,
  #[serde(default)]
  error: Option<RpcError>,
}

#[derive(Deserialize)]
struct VerboseBlock<'a> {
  hash: BlockHash,
  height: u32,
  version: i32,
  merkleroot: TxMerkleNode,
  time: u32,
  nonce: u32,
  bits: &'a str,
  #[serde(default)]
  previousblockhash: Option<BlockHash>,
  #[serde(borrow)]
  tx: Vec<VerboseTx<'a>>,
}

#[derive(Deserialize)]
struct VerboseTx<'a> {
  txid: Txid,
  #[serde(borrow)]
  vin: Vec<VerboseInput<'a>>,
  #[serde(borrow)]
  vout: Vec<VerboseOutput<'a>>,
}

#[derive(Deserialize)]
struct VerboseInput<'a> {
  #[serde(borrow, default)]
  coinbase: Option<&'a str>,
  #[serde(default)]
  txid: Option<Txid>,
  #[serde(default)]
  vout: Option<u32>,
  #[serde(rename = "scriptSig", borrow, default)]
  script_sig: Option<VerboseScript<'a>>,
  /// Hex elements; absent when the input has no witness.
  #[serde(borrow, default)]
  txinwitness: Vec<&'a str>,
  #[serde(borrow, default)]
  prevout: Option<VerbosePrevout<'a>>,
}

#[derive(Deserialize)]
struct VerbosePrevout<'a> {
  #[serde(borrow)]
  value: &'a RawValue,
  #[serde(rename = "scriptPubKey", borrow)]
  script_pub_key: VerboseScript<'a>,
}

#[derive(Deserialize)]
struct VerboseOutput<'a> {
  #[serde(borrow)]
  value: &'a RawValue,
  n: u32,
  #[serde(rename = "scriptPubKey", borrow)]
  script_pub_key: VerboseScript<'a>,
}

#[derive(Deserialize)]
struct VerboseScript<'a> {
  hex: &'a str,
}

fn parse_block(body: &[u8]) -> Result<ResolvedBlock, ParseError> {
  let envelope = serde_json::from_slice::<Envelope>(body)
    .map_err(|error| ParseError::Invalid(anyhow!(error).context("parsing getblock verbosity 3")))?;

  if let Some(error) = envelope.error {
    return Err(ParseError::Rpc(error));
  }

  let block = envelope
    .result
    .ok_or_else(|| ParseError::Invalid(anyhow!("getblock returned a null result")))?;

  resolve(block).map_err(ParseError::Invalid)
}

fn resolve(block: VerboseBlock) -> anyhow::Result<ResolvedBlock> {
  let header = Header {
    version: Version::from_consensus(block.version),
    prev_blockhash: block.previousblockhash.unwrap_or_else(BlockHash::all_zeros),
    merkle_root: block.merkleroot,
    time: block.time,
    bits: CompactTarget::from_consensus(
      u32::from_str_radix(block.bits, 16)
        .with_context(|| format!("invalid bits `{}`", block.bits))?,
    ),
    nonce: block.nonce,
  };

  ensure!(
    header.block_hash() == block.hash,
    "the header fields of block {} hash to {}",
    block.hash,
    header.block_hash()
  );

  let merkle_root = merkle_tree::calculate_root(block.tx.iter().map(|tx| tx.txid.to_raw_hash()))
    .map(TxMerkleNode::from_raw_hash);
  ensure!(
    merkle_root == Some(header.merkle_root),
    "the txids of block {} do not match its merkle root",
    block.hash
  );

  let txs = block
    .tx
    .into_iter()
    .enumerate()
    .map(|(index, tx)| {
      resolve_tx(index, &tx).with_context(|| {
        format!(
          "transaction {} ({index}) of block {} ({})",
          tx.txid, block.height, block.hash
        )
      })
    })
    .collect::<anyhow::Result<Vec<ResolvedTx>>>()?;

  Ok(ResolvedBlock {
    height: block.height,
    hash: block.hash,
    header,
    txs,
  })
}

fn resolve_tx(index: usize, tx: &VerboseTx) -> anyhow::Result<ResolvedTx> {
  let coinbase = tx.vin.first().is_some_and(|input| input.coinbase.is_some());
  ensure!(
    coinbase == (index == 0),
    "{}",
    if coinbase {
      "coinbase input outside the first transaction"
    } else {
      "the first transaction is not a coinbase"
    }
  );

  let inputs = if coinbase {
    Vec::new()
  } else {
    tx.vin
      .iter()
      .enumerate()
      .map(|(vin, input)| {
        let (Some(txid), Some(vout)) = (input.txid, input.vout) else {
          bail!("input {vin} has no txid and vout");
        };
        let prevout = input.prevout.as_ref().ok_or_else(|| {
          anyhow!("input {vin} has no prevout (is the block's undo data pruned?)")
        })?;
        let script_sig = ScriptBuf::from_bytes(
          hex::decode(
            input
              .script_sig
              .as_ref()
              .map_or("", |script| script.hex)
              .as_bytes(),
          )
          .with_context(|| format!("scriptSig of input {vin}"))?,
        );
        let witness =
          witness(&input.txinwitness).with_context(|| format!("witness of input {vin}"))?;
        let spent_script = hex::decode(prevout.script_pub_key.hex.as_bytes())
          .with_context(|| format!("script of the output spent by input {vin}"))?;
        Ok(ResolvedInput {
          previous_output: OutPoint { txid, vout },
          value: parse_btc(prevout.value.get())
            .with_context(|| format!("value of the output spent by input {vin}"))?,
          script_hash: script_hash(Script::from_bytes(&spent_script)),
          sighash: input_sighash(&script_sig, &witness),
          envelopes: input_envelopes(&witness),
        })
      })
      .collect::<anyhow::Result<Vec<ResolvedInput>>>()?
  };

  let outputs = tx
    .vout
    .iter()
    .enumerate()
    .map(|(n, output)| {
      ensure!(
        output.n as usize == n,
        "output {n} is numbered {}",
        output.n
      );
      Ok(TxOut {
        value: Amount::from_sat(
          parse_btc(output.value.get()).with_context(|| format!("value of output {n}"))?,
        ),
        script_pubkey: ScriptBuf::from_bytes(
          hex::decode(output.script_pub_key.hex.as_bytes())
            .with_context(|| format!("script of output {n}"))?,
        ),
      })
    })
    .collect::<anyhow::Result<Vec<TxOut>>>()?;

  Ok(ResolvedTx {
    txid: tx.txid,
    inputs,
    outputs,
  })
}

/// A witness from its hex elements.
fn witness(elements: &[&str]) -> anyhow::Result<Witness> {
  let mut witness = Witness::new();
  for element in elements {
    witness.push(hex::decode(element.as_bytes())?);
  }
  Ok(witness)
}

/// Parses a JSON number of BTC exactly into sats, with integer arithmetic
/// only (never through `f64`). Core prints `<int>.<8 digits>`; any unsigned
/// JSON number is accepted (e.g. `5.46e-6`, as serde_json prints an `f64`)
/// as long as it is a whole number of sats and at most 21M BTC.
pub(crate) fn parse_btc(text: &str) -> anyhow::Result<u64> {
  /// 21M BTC in sats has 16 digits.
  const MAX_DIGITS: i64 = 16;

  let (mantissa, exponent) = match text.split_once(['e', 'E']) {
    Some((mantissa, exponent)) => {
      let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
      if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        bail!("invalid amount `{text}`");
      }
      let Ok(exponent) = exponent.parse::<i32>() else {
        bail!("amount `{text}` is out of range");
      };
      (mantissa, exponent)
    }
    None => (text, 0),
  };

  let (int, frac) = match mantissa.split_once('.') {
    Some((_, "")) => bail!("invalid amount `{text}`"),
    Some((int, frac)) => (int, frac),
    None => (mantissa, ""),
  };

  if int.is_empty() || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
    bail!("invalid amount `{text}`");
  }

  // sats = digits × 10^scale
  let mut scale = i64::from(exponent) + 8 - frac.len() as i64;
  let digits = int.bytes().chain(frac.bytes()).collect::<Vec<u8>>();
  let mut digits = &digits[digits.iter().take_while(|&&b| b == b'0').count()..];

  if digits.is_empty() {
    return Ok(0);
  }

  while scale < 0 {
    match digits.split_last() {
      Some((b'0', rest)) => {
        digits = rest;
        scale += 1;
      }
      _ => bail!("amount `{text}` is not a whole number of sats"),
    }
  }

  if digits.len() as i64 + scale > MAX_DIGITS {
    bail!("amount `{text}` is above 21M BTC");
  }

  let sats = digits
    .iter()
    .fold(0u64, |sats, digit| sats * 10 + u64::from(digit - b'0'))
    * 10u64.pow(scale as u32);

  ensure!(
    sats <= Amount::MAX_MONEY.to_sat(),
    "amount `{text}` is above 21M BTC"
  );

  Ok(sats)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn exact_btc_amounts() {
    for (text, sats) in [
      ("0", 0),
      ("0.00000000", 0),
      ("0.00000001", 1),
      ("0.1", 10_000_000),
      ("1", 100_000_000),
      ("50.00000000", 5_000_000_000),
      ("0.29099999", 29_099_999),
      ("0.00000546", 546),
      ("20999999.97690000", 2_099_999_997_690_000),
      ("21000000.00000000", 2_100_000_000_000_000),
      ("000.5", 50_000_000),
      ("1.000000000", 100_000_000),
      // Not exactly representable in binary floating point.
      ("0.07", 7_000_000),
      ("9007199.25474099", 900_719_925_474_099),
      ("1.23456789", 123_456_789),
      // Exponents, as serde_json prints f64s.
      ("1e-8", 1),
      ("1E-8", 1),
      ("5.46e-6", 546),
      ("0.5e-7", 5),
      ("50E0", 5_000_000_000),
      ("1e+2", 10_000_000_000),
      ("2.1e7", 2_100_000_000_000_000),
      ("0e999999", 0),
      ("123456789e-8", 123_456_789),
      ("1234567.8e-7", 12_345_678),
    ] {
      assert_eq!(parse_btc(text).unwrap(), sats, "{text}");
    }
  }

  #[test]
  fn rejects_inexact_or_invalid_amounts() {
    for text in [
      "",
      ".",
      ".5",
      "5.",
      "-1",
      "-0.00000001",
      "+1",
      "1e8",
      "1e-9",
      "1.5e-8",
      "1e",
      "1e+",
      "e5",
      "1e5.0",
      "1e99999999999",
      "0.000000001",
      "0.123456789",
      "1234567.89e-8",
      "0x10",
      " 1",
      "1 ",
      "1.0.0",
      "21000000.00000001",
      "99999999999999999999",
      "184467440737.09551616",
      "null",
      "\"1.0\"",
    ] {
      assert!(parse_btc(text).is_err(), "{text}");
    }
  }

  fn body(block: &str) -> String {
    format!("{{\"result\":{block},\"error\":null,\"id\":1}}")
  }

  #[test]
  fn rpc_errors_are_reported_as_rpc_errors() {
    let error =
      parse_block(br#"{"result":null,"error":{"code":-5,"message":"Block not found"},"id":1}"#)
        .err()
        .unwrap();
    assert!(matches!(error, ParseError::Rpc(RpcError { code: -5, .. })));
    assert!(matches!(
      parse_block(b"<html>").err().unwrap(),
      ParseError::Invalid(_)
    ));
    assert!(matches!(
      parse_block(body("null").as_bytes()).err().unwrap(),
      ParseError::Invalid(_)
    ));
  }

  #[test]
  fn parses_genesis() {
    // `getblock 000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f 3` on mainnet.
    let genesis = r#"{"hash":"000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f","confirmations":969242,"height":0,"version":1,"versionHex":"00000001","merkleroot":"4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b","time":1231006505,"mediantime":1231006505,"nonce":2083236893,"bits":"1d00ffff","target":"00000000ffff0000000000000000000000000000000000000000000000000000","difficulty":1,"chainwork":"0000000000000000000000000000000000000000000000000000000100010001","nTx":1,"nextblockhash":"00000000839a8e6886ab5951d76f411475428afc90947ee320161bbf18eb6048","strippedsize":285,"size":285,"weight":1140,"tx":[{"txid":"4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b","hash":"4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b","version":1,"size":204,"vsize":204,"weight":816,"locktime":0,"vin":[{"coinbase":"04ffff001d0104455468652054696d65732030332f4a616e2f32303039204368616e63656c6c6f72206f6e206272696e6b206f66207365636f6e64206261696c6f757420666f722062616e6b73","sequence":4294967295}],"vout":[{"value":50.00000000,"n":0,"scriptPubKey":{"asm":"04678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5f OP_CHECKSIG","desc":"pk(04678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5f)#vlz6ztea","hex":"4104678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5fac","type":"pubkey"}}],"hex":"01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff4d04ffff001d0104455468652054696d65732030332f4a616e2f32303039204368616e63656c6c6f72206f6e206272696e6b206f66207365636f6e64206261696c6f757420666f722062616e6b73ffffffff0100f2052a01000000434104678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5fac00000000"}]}"#;

    let block = parse_block(body(genesis).as_bytes()).ok().unwrap();
    let expected = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin);
    assert_eq!(block.height, 0);
    assert_eq!(block.hash, expected.block_hash());
    assert_eq!(block.header, expected.header);
    assert_eq!(block.txs.len(), 1);
    assert_eq!(block.txs[0].txid, expected.txdata[0].compute_txid());
    assert!(block.txs[0].inputs.is_empty());
    assert_eq!(block.txs[0].outputs, expected.txdata[0].output);

    let tampered = genesis.replace("\"nonce\":2083236893", "\"nonce\":2083236894");
    let ParseError::Invalid(error) = parse_block(body(&tampered).as_bytes()).err().unwrap() else {
      panic!()
    };
    assert!(error.to_string().contains("hash to"), "{error}");
  }
}
