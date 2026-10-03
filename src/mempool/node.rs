//! The overlay's bitcoind (`RpcMempoolNode`) and index (`StoreIndex`).

use {
  super::{MempoolEntry, MempoolNode, RawMempool, TrackedIndex},
  crate::{
    model::TrackedUtxo,
    source::{Rpc, RpcError},
    store::Reader,
  },
  anyhow::{Context, ensure},
  bitcoin::{Amount, BlockHash, OutPoint, ScriptBuf, Transaction, TxOut, Txid, consensus::encode},
  serde::Deserialize,
  serde_json::{Value, json},
  std::collections::HashMap,
};

/// Calls per JSON-RPC batch.
const BATCH: usize = 1000;
/// Batches in flight while fetching many transactions.
const FETCH_THREADS: usize = 4;

pub struct RpcMempoolNode {
  rpc: Rpc,
}

impl RpcMempoolNode {
  pub fn new(rpc: Rpc) -> Self {
    Self { rpc }
  }
}

#[derive(Deserialize)]
struct RawMempoolJson {
  txids: Vec<Txid>,
  #[serde(default)]
  mempool_sequence: Option<u64>,
}

#[derive(Deserialize)]
struct FeesJson {
  base: f64,
}

#[derive(Deserialize)]
struct EntryJson {
  time: u64,
  vsize: u64,
  fees: FeesJson,
}

#[derive(Deserialize)]
struct ScriptJson {
  hex: String,
}

#[derive(Deserialize)]
struct TxOutJson {
  value: f64,
  #[serde(rename = "scriptPubKey")]
  script_pubkey: ScriptJson,
}

/// A BTC amount as bitcoind prints it (8 decimals) in sats.
fn sats(btc: f64) -> u64 {
  (btc * 100_000_000.0).round().max(0.0) as u64
}

/// A missing transaction or output: not an error for the overlay.
fn missing(error: &RpcError) -> bool {
  error.code == RpcError::INVALID_ADDRESS_OR_KEY
}

fn decode_tx(txid: Txid, hex: &str) -> anyhow::Result<Transaction> {
  let tx: Transaction =
    encode::deserialize_hex(hex).with_context(|| format!("decoding transaction {txid}"))?;
  ensure!(
    tx.compute_txid() == txid,
    "getrawtransaction {txid} returned {}",
    tx.compute_txid()
  );
  Ok(tx)
}

impl RpcMempoolNode {
  fn raw_transactions(&self, txids: &[Txid]) -> anyhow::Result<Vec<Option<Transaction>>> {
    let chunk = |txids: &[Txid]| -> anyhow::Result<Vec<Option<Transaction>>> {
      let params: Vec<Vec<Value>> = txids
        .iter()
        .map(|txid| vec![json!(txid), json!(false)])
        .collect();
      let results = self
        .rpc
        .batch_lenient::<String>("getrawtransaction", &params)?;
      txids
        .iter()
        .zip(results)
        .map(|(txid, result)| match result {
          Ok(hex) => decode_tx(*txid, &hex).map(Some),
          Err(error) if missing(&error) => Ok(None),
          Err(error) => Err(anyhow::Error::new(error).context(format!("getrawtransaction {txid}"))),
        })
        .collect()
    };
    let chunks: Vec<&[Txid]> = txids.chunks(BATCH).collect();
    if chunks.len() <= 1 {
      return chunks.first().map_or(Ok(Vec::new()), |txids| chunk(txids));
    }
    let mut out: Vec<Option<anyhow::Result<Vec<Option<Transaction>>>>> =
      (0..chunks.len()).map(|_| None).collect();
    for group in (0..chunks.len()).collect::<Vec<_>>().chunks(FETCH_THREADS) {
      std::thread::scope(|scope| {
        let handles: Vec<_> = group
          .iter()
          .map(|&i| {
            let chunk = &chunk;
            let txids = chunks[i];
            (i, scope.spawn(move || chunk(txids)))
          })
          .collect();
        for (i, handle) in handles {
          out[i] = Some(
            handle
              .join()
              .unwrap_or_else(|_| Err(anyhow::anyhow!("transaction fetch panicked"))),
          );
        }
      });
    }
    let mut txs = Vec::with_capacity(txids.len());
    for result in out {
      txs.extend(result.expect("every chunk fetched")?);
    }
    Ok(txs)
  }
}

impl MempoolNode for RpcMempoolNode {
  fn best_block_hash(&self) -> anyhow::Result<BlockHash> {
    self.rpc.best_block_hash()
  }

  fn raw_mempool(&self) -> anyhow::Result<RawMempool> {
    let raw: RawMempoolJson = self
      .rpc
      .call("getrawmempool", &[json!(false), json!(true)])?;
    Ok(RawMempool {
      txids: raw.txids,
      sequence: raw.mempool_sequence,
    })
  }

  fn transactions(&self, txids: &[Txid]) -> anyhow::Result<Vec<Option<Transaction>>> {
    self.raw_transactions(txids)
  }

  fn entries(&self, txids: &[Txid]) -> anyhow::Result<Vec<Option<MempoolEntry>>> {
    let mut entries = Vec::with_capacity(txids.len());
    for chunk in txids.chunks(BATCH) {
      let params: Vec<Vec<Value>> = chunk.iter().map(|txid| vec![json!(txid)]).collect();
      for (txid, result) in chunk.iter().zip(
        self
          .rpc
          .batch_lenient::<EntryJson>("getmempoolentry", &params)?,
      ) {
        entries.push(match result {
          Ok(entry) => Some(MempoolEntry {
            time: entry.time,
            fee: sats(entry.fees.base),
            vsize: entry.vsize,
          }),
          Err(error) if missing(&error) => None,
          Err(error) => {
            return Err(anyhow::Error::new(error).context(format!("getmempoolentry {txid}")));
          }
        });
      }
    }
    Ok(entries)
  }

  fn prevouts(&self, outpoints: &[OutPoint]) -> anyhow::Result<Vec<Option<TxOut>>> {
    let mut out: Vec<Option<TxOut>> = vec![None; outpoints.len()];
    // Confirmed outputs: the chainstate (spent or not in the mempool).
    for (base, chunk) in outpoints.chunks(BATCH).enumerate() {
      let params: Vec<Vec<Value>> = chunk
        .iter()
        .map(|outpoint| vec![json!(outpoint.txid), json!(outpoint.vout), json!(false)])
        .collect();
      for (i, result) in self
        .rpc
        .batch_lenient::<Option<TxOutJson>>("gettxout", &params)?
        .into_iter()
        .enumerate()
      {
        if let Ok(Some(output)) = result {
          out[base * BATCH + i] = Some(TxOut {
            value: Amount::from_sat(sats(output.value)),
            script_pubkey: ScriptBuf::from_hex(&output.script_pubkey.hex)
              .context("gettxout returned a bad script")?,
          });
        }
      }
    }
    // The others (unconfirmed parents): their transactions.
    let mut parents: Vec<Txid> = outpoints
      .iter()
      .zip(&out)
      .filter(|(_, output)| output.is_none())
      .map(|(outpoint, _)| outpoint.txid)
      .collect();
    parents.sort();
    parents.dedup();
    if !parents.is_empty() {
      let txs: HashMap<Txid, Transaction> = parents
        .iter()
        .copied()
        .zip(self.raw_transactions(&parents)?)
        .filter_map(|(txid, tx)| Some((txid, tx?)))
        .collect();
      for (outpoint, output) in outpoints.iter().zip(out.iter_mut()) {
        if output.is_none() {
          *output = txs
            .get(&outpoint.txid)
            .and_then(|tx| tx.output.get(outpoint.vout as usize))
            .cloned();
        }
      }
    }
    Ok(out)
  }
}

/// The index through one read transaction.
pub struct StoreIndex(pub Reader);

impl TrackedIndex for StoreIndex {
  fn tip(&self) -> anyhow::Result<Option<(u32, BlockHash)>> {
    let Some(height) = self.0.height()? else {
      return Ok(None);
    };
    Ok(self.0.block_hash(height)?.map(|hash| (height, hash)))
  }

  fn utxo(&self, outpoint: OutPoint) -> anyhow::Result<Option<TrackedUtxo>> {
    self.0.utxo(outpoint)
  }

  fn time(&self, height: u32) -> anyhow::Result<Option<u32>> {
    Ok(self.0.header(height)?.map(|header| header.time))
  }

  fn block_moves(&self, height: u32) -> anyhow::Result<Option<super::BlockMoves>> {
    Ok(self.0.moves(height)?.map(|moves| {
      moves
        .into_iter()
        .filter(|m| !m.coinbase)
        .map(|m| {
          (
            m.txid,
            m.inputs.iter().map(|input| input.outpoint).collect(),
          )
        })
        .collect()
    }))
  }
}

#[derive(Deserialize)]
struct PrevoutJson {
  value: f64,
  #[serde(default)]
  height: Option<u32>,
  #[serde(rename = "scriptPubKey")]
  script_pubkey: ScriptJson,
}

#[derive(Deserialize)]
struct VinJson {
  #[serde(default)]
  prevout: Option<PrevoutJson>,
}

/// `getrawtransaction <txid> 2`, the parts `TxNode` uses.
#[derive(Deserialize)]
struct VerboseTxJson {
  hex: String,
  #[serde(default)]
  blockhash: Option<BlockHash>,
  #[serde(default)]
  confirmations: Option<u32>,
  #[serde(default)]
  vin: Vec<VinJson>,
}

#[derive(Deserialize)]
struct HeaderJson {
  height: u32,
  time: u32,
}

impl super::TxNode for RpcMempoolNode {
  fn transaction(&self, txid: Txid) -> anyhow::Result<Option<super::NodeTx>> {
    let Some(result) = self
      .rpc
      .batch_lenient::<VerboseTxJson>("getrawtransaction", &[vec![json!(txid), json!(2)]])?
      .pop()
    else {
      return Ok(None);
    };
    let verbose = match result {
      Ok(verbose) => verbose,
      Err(error) if missing(&error) => return Ok(None),
      Err(error) => {
        return Err(anyhow::Error::new(error).context(format!("getrawtransaction {txid}")));
      }
    };
    let tx = decode_tx(txid, &verbose.hex)?;
    let coinbase = tx.is_coinbase();
    let block = match verbose.blockhash {
      Some(hash) => {
        let header: HeaderJson = self
          .rpc
          .call("getblockheader", &[json!(hash), json!(true)])
          .with_context(|| format!("getblockheader {hash}"))?;
        Some(super::NodeBlock {
          hash,
          height: header.height,
          time: header.time,
          confirmations: verbose.confirmations.unwrap_or(1),
        })
      }
      None => None,
    };
    let (prevouts, prevout_heights, entry) = if coinbase {
      (vec![None; tx.input.len()], vec![None; tx.input.len()], None)
    } else if block.is_some() {
      // bitcoind gives a confirmed transaction's prevouts from its undo data.
      let mut prevouts = Vec::with_capacity(tx.input.len());
      let mut heights = Vec::with_capacity(tx.input.len());
      for i in 0..tx.input.len() {
        match verbose.vin.get(i).and_then(|vin| vin.prevout.as_ref()) {
          Some(prevout) => {
            prevouts.push(Some(TxOut {
              value: Amount::from_sat(sats(prevout.value)),
              script_pubkey: ScriptBuf::from_hex(&prevout.script_pubkey.hex)
                .context("getrawtransaction returned a bad prevout script")?,
            }));
            heights.push(prevout.height);
          }
          None => {
            prevouts.push(None);
            heights.push(None);
          }
        }
      }
      (prevouts, heights, None)
    } else {
      // In the mempool: the chainstate's outputs, or the parents'.
      let outpoints: Vec<OutPoint> = tx.input.iter().map(|input| input.previous_output).collect();
      let prevouts = self.prevouts(&outpoints)?;
      let entry = self.entries(&[txid])?.pop().flatten();
      (prevouts, vec![None; tx.input.len()], entry)
    };
    Ok(Some(super::NodeTx {
      tx,
      prevouts,
      prevout_heights,
      block,
      entry,
    }))
  }
}
