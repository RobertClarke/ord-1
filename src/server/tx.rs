//! `GET /tx/{txid}` (docs/API.md "Transactions"): one transaction as a
//! block explorer shows it (every input and output, fee, status), plus what
//! it did to tracked sats: each input's tracked pieces, the output (or the
//! fee) each one went to under ordinal theory's first-in-first-out rule,
//! its sales and its address-history entries.
//!
//! bitcoind supplies the transaction (`TxNode`: txindex or mempool); the
//! sat flow comes from the index:
//!
//! * confirmed, its block indexed and its moves retained: the tracker's own
//!   `Move` (`sat_flow: "complete"`);
//! * confirmed, its moves pruned (`--moves-retention`): only the tracked
//!   sats still in its unspent outputs (`"outputs"`); its sales and history
//!   entries are kept forever;
//! * confirmed, its block not indexed yet: `"unknown"`;
//! * pending: the mempool overlay's classification, or, for a transaction
//!   the overlay does not hold, the same read-only flow (`mempool::flow`)
//!   over the tracked UTXOs it spends (`"complete"`; `"unknown"` while the
//!   index is behind bitcoind or an input's value is unknown).

use {
  super::{
    ServerError, ServerResult, ServerState, history::entry_json, json::PieceJson, not_found, parse,
    read,
  },
  crate::{
    history::{HistoryKind, history_key},
    mempool::{NodeTx, Removed, flow},
    model::{Move, MoveInput, Piece, ResolvedInput, ResolvedTx, TrackedUtxo, UtxoKind},
    source::{input_envelopes, input_sighash, is_unspendable},
    store::Reader,
  },
  axum::{
    Json,
    extract::{Path, State},
  },
  bitcoin::{BlockHash, OutPoint, ScriptBuf, Txid},
  serde::Serialize,
  serde_json::{Number, Value},
  std::{collections::BTreeSet, sync::Arc},
};

/// Distinct scripts whose history entries are looked up at most.
const MAX_ENTRY_SCRIPTS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TxInputJson {
  pub vin: u32,
  /// `null` for the coinbase's input.
  pub outpoint: Option<OutPoint>,
  /// The spent output's value; `null` for the coinbase or when bitcoind no
  /// longer knows it.
  pub value: Option<u64>,
  pub address: Option<String>,
  pub script_pubkey: Option<String>,
  pub sequence: u32,
  /// The height of the block that created the spent output, when known.
  pub created_height: Option<u32>,
  /// The sighash flag of its first signature (`131` = SINGLE|ANYONECANPAY,
  /// a marketplace listing's).
  pub sighash: Option<u8>,
  /// Inscription envelopes it reveals.
  pub inscriptions: usize,
  /// It spent a tracked UTXO.
  pub tracked: bool,
  /// The tracked UTXO it spent was extracted.
  pub extracted: bool,
  /// The tracked sats it spent, offsets within the spent output.
  pub pieces: Vec<PieceJson>,
  /// Of those, the inscribed ones.
  pub inscribed_sats: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TxOutputJson {
  pub vout: u32,
  pub value: u64,
  pub address: Option<String>,
  pub script_pubkey: String,
  pub kind: UtxoKind,
  /// It received tracked sats.
  pub tracked: bool,
  pub extracted: bool,
  /// The tracked sats it received, offsets within it.
  pub pieces: Vec<PieceJson>,
  pub inscribed_sats: Vec<u64>,
  /// The pending transaction spending it, when it holds tracked sats.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub spent_by_pending: Option<super::mempool::SpendJson>,
}

/// One run of tracked sats from where the transaction took it to where it
/// put it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlowJson {
  /// `input` (a spent UTXO), `subsidy` (newly mined, the coinbase) or
  /// `fees` (the block's fees, the coinbase).
  pub source: &'static str,
  pub vin: Option<u32>,
  /// The run's offset within the spent output (`input`) or the coinbase's
  /// input stream (`subsidy`).
  pub input_offset: Option<u64>,
  /// `output`, `fee` (paid to the miner) or `lost` (a coinbase that did
  /// not claim it).
  pub dest: &'static str,
  pub vout: Option<u32>,
  /// The run's offset within the output (`output`) or the fee (`fee`).
  pub offset: Option<u64>,
  pub start: u64,
  pub len: u64,
  pub types: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TxJson {
  pub txid: Txid,
  /// `confirmed`, `pending`, or, for a transaction gone from the mempool
  /// that bitcoind no longer knows, `replaced`, `conflicted` or `evicted`.
  pub status: &'static str,
  pub height: Option<u32>,
  pub hash: Option<BlockHash>,
  pub time: Option<u32>,
  pub confirmations: Option<u32>,
  pub tx_index: Option<u32>,
  /// The index holds its block.
  pub indexed: bool,
  pub first_seen: Option<u64>,
  /// The overlay's record of its leaving the mempool.
  pub removed: Option<Removed>,
  pub version: Option<i32>,
  pub locktime: Option<u32>,
  pub size: Option<u64>,
  pub vsize: Option<u64>,
  pub weight: Option<u64>,
  pub coinbase: bool,
  pub input_value: Option<u64>,
  pub output_value: Option<u64>,
  pub fee: Option<u64>,
  /// sat/vB, one decimal.
  pub fee_rate: Option<Number>,
  /// `complete`, `outputs` or `unknown` (module docs).
  pub sat_flow: &'static str,
  pub inputs: Vec<TxInputJson>,
  pub outputs: Vec<TxOutputJson>,
  pub flow: Vec<FlowJson>,
  pub fee_pieces: Vec<PieceJson>,
  pub mined_pieces: Vec<PieceJson>,
  pub lost_pieces: Vec<PieceJson>,
  /// Its history kinds, in the history's precedence order.
  pub kinds: Vec<&'static str>,
  /// The tracker's Move (`/block/{h}/moves`), or the pending one
  /// (`/mempool/tx/{txid}`); `null` when it moved no tracked sat or the
  /// flow is not complete.
  #[serde(rename = "move")]
  pub mv: Option<Value>,
  pub sales: Vec<Value>,
  pub entries: Vec<Value>,
  /// Pending parents whose tracked outputs it spends.
  pub depends: Vec<Txid>,
}

/// Distinct kinds in the history's precedence order.
fn ordered_kinds(kinds: impl IntoIterator<Item = HistoryKind>) -> Vec<&'static str> {
  let kinds: BTreeSet<HistoryKind> = kinds.into_iter().collect();
  [
    HistoryKind::Mined,
    HistoryKind::Sale,
    HistoryKind::Extraction,
    HistoryKind::Burn,
    HistoryKind::Consolidation,
    HistoryKind::Transfer,
  ]
  .into_iter()
  .filter(|kind| kinds.contains(kind))
  .map(HistoryKind::name)
  .collect()
}

fn fee_rate(fee: u64, vsize: u64) -> Option<Number> {
  if vsize == 0 {
    return None;
  }
  Number::from_f64((fee as f64 / vsize as f64 * 10.0).round() / 10.0)
}

/// Where each tracked run of `mv` went: its pieces matched by sat number
/// from the inputs (or, for a coinbase, the subsidy and the fees) to the
/// outputs, the fee and the lost stream. Inputs' runs first, by vin and
/// offset; a coinbase's by output.
pub fn sat_flow(mv: &Move, types: impl Fn(u64) -> Vec<&'static str>) -> Vec<FlowJson> {
  // (start, len, dest, vout, offset)
  type Dest = (u64, u64, &'static str, Option<u32>, Option<u64>);
  let mut dests: Vec<Dest> = Vec::new();
  for output in &mv.outputs {
    for piece in &output.pieces {
      dests.push((
        piece.start,
        piece.len,
        "output",
        Some(output.vout),
        Some(piece.offset),
      ));
    }
  }
  for piece in &mv.fee {
    dests.push((piece.start, piece.len, "fee", None, Some(piece.offset)));
  }
  for piece in &mv.lost {
    dests.push((piece.start, piece.len, "lost", None, None));
  }
  dests.sort_by_key(|dest| dest.0);

  // (start, len, source, vin, offset)
  let mut sources: Vec<(u64, u64, &'static str, Option<u32>, u64)> = Vec::new();
  if mv.coinbase {
    for piece in &mv.mined {
      sources.push((piece.start, piece.len, "subsidy", None, piece.offset));
    }
  } else {
    for input in &mv.inputs {
      for piece in &input.utxo.pieces {
        sources.push((
          piece.start,
          piece.len,
          "input",
          Some(input.vin),
          piece.offset,
        ));
      }
    }
  }
  sources.sort_by_key(|source| source.0);

  let mut flow = Vec::new();
  for &(start, len, dest, vout, offset) in &dests {
    let end = start + len;
    let mut at = start;
    let first = sources.partition_point(|source| source.0 + source.1 <= start);
    for &(s_start, s_len, source, vin, s_offset) in &sources[first..] {
      if s_start >= end {
        break;
      }
      let from = s_start.max(start);
      let to = (s_start + s_len).min(end);
      if from > at && mv.coinbase {
        flow.push(FlowJson {
          source: "fees",
          vin: None,
          input_offset: None,
          dest,
          vout,
          offset: offset.map(|offset| offset + (at - start)),
          start: at,
          len: from - at,
          types: types(at),
        });
      }
      flow.push(FlowJson {
        source,
        vin,
        input_offset: Some(s_offset + (from - s_start)),
        dest,
        vout,
        offset: offset.map(|offset| offset + (from - start)),
        start: from,
        len: to - from,
        types: types(from),
      });
      at = to;
    }
    if at < end && mv.coinbase {
      flow.push(FlowJson {
        source: "fees",
        vin: None,
        input_offset: None,
        dest,
        vout,
        offset: offset.map(|offset| offset + (at - start)),
        start: at,
        len: end - at,
        types: types(at),
      });
    }
  }
  if mv.coinbase {
    flow.sort_by_key(|run| (run.vout.unwrap_or(u32::MAX), run.offset, run.start));
  } else {
    flow.sort_by_key(|run| (run.vin, run.input_offset, run.start));
  }
  flow
}

fn inscribed(reader: &Reader, pieces: &[Piece]) -> anyhow::Result<Vec<u64>> {
  Ok(
    reader
      .inscribed_in_pieces(pieces)?
      .into_iter()
      .map(|(sat, _)| sat)
      .collect(),
  )
}

/// What the index says the transaction did.
#[derive(Default)]
struct Tracked {
  sat_flow: &'static str,
  mv: Option<Move>,
  /// The rendered move (pending ones are marked as such).
  move_json: Option<Value>,
  /// Pieces of outputs still unspent (`sat_flow: "outputs"`), by vout.
  outputs: Vec<(u32, TrackedUtxo)>,
  sales: Vec<Value>,
  entries: Vec<Value>,
  kinds: Vec<&'static str>,
  depends: Vec<Txid>,
  tx_index: Option<u32>,
}

/// The history entries of the transaction at `height`, one per script.
fn confirmed_entries(
  state: &ServerState,
  reader: &Reader,
  node: &NodeTx,
  txid: Txid,
  height: u32,
) -> ServerResult<(Vec<Value>, Vec<&'static str>, Option<u32>)> {
  let render = state.render(reader);
  let mut scripts: Vec<&ScriptBuf> = node
    .prevouts
    .iter()
    .flatten()
    .map(|prevout| &prevout.script_pubkey)
    .chain(node.tx.output.iter().map(|output| &output.script_pubkey))
    .filter(|script| !is_unspendable(script))
    .collect();
  scripts.sort();
  scripts.dedup();
  let mut entries = Vec::new();
  let mut kinds = Vec::new();
  let mut tx_index = None;
  for script in scripts.into_iter().take(MAX_ENTRY_SCRIPTS) {
    for entry in reader.history(history_key(script), height, height, None)? {
      let entry = entry?;
      if entry.txid != txid {
        continue;
      }
      tx_index = Some(entry.tx_index);
      kinds.push(entry.kind);
      let mut value = serde_json::to_value(entry_json(&render, &entry, Vec::new())?)?;
      if let Value::Object(object) = &mut value {
        object.insert("address".into(), render.address(script).into());
        object.insert("script_pubkey".into(), script.to_hex_string().into());
      }
      entries.push(value);
    }
  }
  Ok((entries, ordered_kinds(kinds), tx_index))
}

fn confirmed(
  state: &ServerState,
  reader: &Reader,
  node: &NodeTx,
  txid: Txid,
  height: u32,
  time: u32,
) -> ServerResult<Tracked> {
  let render = state.render(reader);
  let sales = reader
    .sales_for_tx(txid)?
    .iter()
    .map(|sale| Ok(serde_json::to_value(render.sale(sale)?)?))
    .collect::<anyhow::Result<Vec<Value>>>()?;
  let (entries, kinds, entry_index) = confirmed_entries(state, reader, node, txid, height)?;
  if height < reader.moves_floor()? {
    // The moves are pruned: what is still in its outputs.
    let mut outputs = Vec::new();
    for vout in 0..node.tx.output.len() as u32 {
      if let Some(utxo) = reader.utxo(OutPoint::new(txid, vout))?
        && utxo.height == height
      {
        outputs.push((vout, utxo));
      }
    }
    return Ok(Tracked {
      sat_flow: "outputs",
      outputs,
      sales,
      entries,
      kinds,
      tx_index: entry_index,
      ..Default::default()
    });
  }
  let mv = reader
    .moves(height)?
    .and_then(|moves| moves.into_iter().find(|m| m.txid == txid));
  let move_json = match &mv {
    Some(mv) => Some(serde_json::to_value(render.move_json(mv, Some(time))?)?),
    None => None,
  };
  Ok(Tracked {
    sat_flow: "complete",
    tx_index: mv.as_ref().map(|mv| mv.tx_index).or(entry_index),
    mv,
    move_json,
    sales,
    entries,
    kinds,
    ..Default::default()
  })
}

fn pending(
  state: &ServerState,
  reader: &Reader,
  node: &NodeTx,
  txid: Txid,
) -> ServerResult<Tracked> {
  let render = state.render(reader);
  let mempool = render.mempool.clone().unwrap_or_default();
  if let Some(tx) = mempool.tx(&txid) {
    let mut json = render.pending_tx(tx)?;
    let take = |json: &mut Value, key: &str| json.get_mut(key).map(Value::take);
    let list = |value: Option<Value>| match value {
      Some(Value::Array(list)) => list,
      _ => Vec::new(),
    };
    let kinds = tx.kinds();
    return Ok(Tracked {
      sat_flow: "complete",
      mv: Some(tx.classified.mv.clone()),
      move_json: take(&mut json, "move"),
      sales: list(take(&mut json, "sales")),
      entries: list(take(&mut json, "entries")),
      kinds: ordered_kinds(kinds),
      depends: tx.depends.clone(),
      ..Default::default()
    });
  }
  // Not in the overlay: the flow over the tracked UTXOs it spends.
  let status = state.published.status();
  let behind = match (status.height, status.bitcoind_height) {
    (Some(height), Some(bitcoind)) => height < bitcoind,
    _ => true,
  };
  let mut inputs = Vec::new();
  let mut resolved = Vec::with_capacity(node.tx.input.len());
  let mut depends = Vec::new();
  for (vin, (input, prevout)) in node.tx.input.iter().zip(&node.prevouts).enumerate() {
    let outpoint = input.previous_output;
    let utxo = match reader.utxo(outpoint)? {
      Some(utxo) => Some(utxo),
      None => mempool.output(&outpoint).map(|(i, utxo)| {
        depends.push(mempool.txs[i].txid);
        utxo
      }),
    };
    if let Some(utxo) = utxo {
      inputs.push(MoveInput {
        vin: vin as u32,
        outpoint,
        utxo,
      });
    }
    let Some(prevout) = prevout else {
      return Ok(Tracked {
        sat_flow: "unknown",
        ..Default::default()
      });
    };
    resolved.push(ResolvedInput {
      previous_output: outpoint,
      value: prevout.value.to_sat(),
      script_hash: 0,
      sighash: None,
      envelopes: Vec::new(),
    });
  }
  if inputs.is_empty() {
    return Ok(Tracked {
      sat_flow: if behind { "unknown" } else { "complete" },
      ..Default::default()
    });
  }
  let tx = ResolvedTx {
    txid,
    inputs: resolved,
    outputs: node.tx.output.clone(),
  };
  let mv = flow::tx_move(&tx, inputs, 0)?;
  let mut move_json = serde_json::to_value(render.move_json(&mv, None)?)?;
  if let Value::Object(object) = &mut move_json {
    object.insert("tx_index".into(), Value::Null);
  }
  depends.sort();
  depends.dedup();
  Ok(Tracked {
    sat_flow: if behind { "unknown" } else { "complete" },
    mv: Some(mv),
    move_json: Some(move_json),
    depends,
    ..Default::default()
  })
}

fn render_tx(
  state: &ServerState,
  reader: &Reader,
  txid: Txid,
  node: NodeTx,
) -> ServerResult<TxJson> {
  let render = state.render(reader);
  let tip = reader.height()?;
  let coinbase = node.tx.is_coinbase();

  let (status, indexed, tracked) = match node.block {
    Some(block) => {
      let indexed = tip.is_some_and(|tip| block.height <= tip)
        && reader.block_hash(block.height)? == Some(block.hash);
      let tracked = if indexed {
        confirmed(state, reader, &node, txid, block.height, block.time)?
      } else {
        Tracked {
          sat_flow: "unknown",
          ..Default::default()
        }
      };
      ("confirmed", indexed, tracked)
    }
    None => ("pending", false, pending(state, reader, &node, txid)?),
  };

  let input_value = if coinbase {
    None
  } else {
    node
      .prevouts
      .iter()
      .map(|prevout| prevout.as_ref().map(|prevout| prevout.value.to_sat()))
      .sum::<Option<u64>>()
  };
  let output_value = node
    .tx
    .output
    .iter()
    .map(|output| output.value.to_sat())
    .sum::<u64>();
  let fee = match (&node.entry, input_value) {
    (Some(entry), _) => Some(entry.fee),
    (None, Some(input_value)) => input_value.checked_sub(output_value),
    (None, None) => None,
  };
  let vsize = node.tx.vsize() as u64;

  let time = |height: u32| render.time(height);
  let mv = tracked.mv.as_ref();
  let mut inputs = Vec::with_capacity(node.tx.input.len());
  for (vin, input) in node.tx.input.iter().enumerate() {
    let prevout = node.prevouts.get(vin).cloned().flatten();
    let spent = mv.and_then(|mv| mv.inputs.iter().find(|i| i.vin == vin as u32));
    let (pieces, extracted, inscribed_sats, created_height) = match spent {
      Some(spent) => {
        let created_time = time(spent.utxo.height)?;
        (
          render.pieces(&spent.utxo.pieces),
          render.extracted(
            spent.utxo.kind,
            spent.utxo.value,
            &spent.utxo.pieces,
            created_time,
          ),
          inscribed(reader, &spent.utxo.pieces)?,
          // A pending parent's output has no block yet.
          (status == "confirmed" || reader.utxo(spent.outpoint)?.is_some())
            .then_some(spent.utxo.height),
        )
      }
      None => (Vec::new(), false, Vec::new(), None),
    };
    inputs.push(TxInputJson {
      vin: vin as u32,
      outpoint: (!coinbase).then_some(input.previous_output),
      value: prevout.as_ref().map(|prevout| prevout.value.to_sat()),
      address: prevout
        .as_ref()
        .and_then(|prevout| render.address(&prevout.script_pubkey)),
      script_pubkey: prevout
        .as_ref()
        .map(|prevout| prevout.script_pubkey.to_hex_string()),
      sequence: input.sequence.0,
      created_height: node
        .prevout_heights
        .get(vin)
        .copied()
        .flatten()
        .or(created_height),
      sighash: if coinbase {
        None
      } else {
        input_sighash(&input.script_sig, &input.witness)
      },
      inscriptions: if coinbase {
        0
      } else {
        input_envelopes(&input.witness).len()
      },
      tracked: spent.is_some(),
      extracted,
      pieces,
      inscribed_sats,
    });
  }

  let block_time = node.block.map(|block| block.time);
  let mut outputs = Vec::with_capacity(node.tx.output.len());
  for (vout, output) in node.tx.output.iter().enumerate() {
    let vout = vout as u32;
    let kind = if is_unspendable(&output.script_pubkey) {
      UtxoKind::Unspendable
    } else {
      UtxoKind::Spendable
    };
    let pieces: Option<&[Piece]> = match mv {
      Some(mv) => mv
        .outputs
        .iter()
        .find(|o| o.vout == vout)
        .map(|o| o.pieces.as_slice()),
      None => tracked
        .outputs
        .iter()
        .find(|(v, _)| *v == vout)
        .map(|(_, utxo)| utxo.pieces.as_slice()),
    };
    let pieces = pieces.unwrap_or_default();
    let outpoint = OutPoint::new(txid, vout);
    outputs.push(TxOutputJson {
      vout,
      value: output.value.to_sat(),
      address: render.address(&output.script_pubkey),
      script_pubkey: output.script_pubkey.to_hex_string(),
      kind,
      tracked: !pieces.is_empty(),
      extracted: !pieces.is_empty()
        && render.extracted(kind, output.value.to_sat(), pieces, block_time),
      pieces: render.pieces(pieces),
      inscribed_sats: inscribed(reader, pieces)?,
      spent_by_pending: if pieces.is_empty() {
        None
      } else {
        render.spent_by_pending(&outpoint)
      },
    });
  }

  let removed = render
    .mempool
    .as_ref()
    .and_then(|mempool| mempool.removed(&txid).copied());

  Ok(TxJson {
    txid,
    status,
    height: node.block.map(|block| block.height),
    hash: node.block.map(|block| block.hash),
    time: block_time,
    confirmations: node.block.map(|block| block.confirmations),
    tx_index: tracked.tx_index,
    indexed,
    first_seen: node.entry.map(|entry| entry.time),
    removed: if status == "pending" { None } else { removed },
    version: Some(node.tx.version.0),
    locktime: Some(node.tx.lock_time.to_consensus_u32()),
    size: Some(node.tx.total_size() as u64),
    vsize: Some(node.entry.map_or(vsize, |entry| entry.vsize)),
    weight: Some(node.tx.weight().to_wu()),
    coinbase,
    input_value,
    output_value: Some(output_value),
    fee,
    fee_rate: fee.and_then(|fee| fee_rate(fee, node.entry.map_or(vsize, |entry| entry.vsize))),
    sat_flow: tracked.sat_flow,
    inputs,
    outputs,
    flow: mv.map_or_else(Vec::new, |mv| sat_flow(mv, |sat| render.catalog.types(sat))),
    fee_pieces: mv.map_or_else(Vec::new, |mv| render.pieces(&mv.fee)),
    mined_pieces: mv.map_or_else(Vec::new, |mv| render.pieces(&mv.mined)),
    lost_pieces: mv.map_or_else(Vec::new, |mv| render.pieces(&mv.lost)),
    kinds: tracked.kinds,
    mv: tracked.move_json,
    sales: tracked.sales,
    entries: tracked.entries,
    depends: tracked.depends,
  })
}

/// A transaction gone from the mempool that bitcoind no longer knows.
fn removed_tx(txid: Txid, removed: Removed) -> TxJson {
  let status = match removed.status {
    crate::mempool::RemovedStatus::Confirmed => "confirmed",
    crate::mempool::RemovedStatus::Replaced => "replaced",
    crate::mempool::RemovedStatus::Conflicted => "conflicted",
    crate::mempool::RemovedStatus::Evicted => "evicted",
  };
  TxJson {
    txid,
    status,
    height: removed.height,
    hash: None,
    time: None,
    confirmations: None,
    tx_index: None,
    indexed: false,
    first_seen: None,
    removed: Some(removed),
    version: None,
    locktime: None,
    size: None,
    vsize: None,
    weight: None,
    coinbase: false,
    input_value: None,
    output_value: None,
    fee: None,
    fee_rate: None,
    sat_flow: "unknown",
    inputs: Vec::new(),
    outputs: Vec::new(),
    flow: Vec::new(),
    fee_pieces: Vec::new(),
    mined_pieces: Vec::new(),
    lost_pieces: Vec::new(),
    kinds: Vec::new(),
    mv: None,
    sales: Vec::new(),
    entries: Vec::new(),
    depends: Vec::new(),
  }
}

pub async fn tx(
  State(state): State<Arc<ServerState>>,
  Path(txid): Path<String>,
) -> ServerResult<Json<TxJson>> {
  let txid = parse::<Txid>("txid", &txid)?;
  let Some(node) = state.node.get().cloned() else {
    return Err(ServerError::NotReadyBecause(
      "no bitcoind to read transactions from".into(),
    ));
  };
  // bitcoind first, outside any read transaction.
  let found = tokio::task::spawn_blocking(move || node.transaction(txid))
    .await
    .map_err(|err| ServerError::Internal(err.into()))??;
  read(&state, move |state, reader| {
    super::tip(reader)?;
    match found {
      Some(node) => Ok(Json(render_tx(state, reader, txid, node)?)),
      None => match state.published.mempool().removed(&txid) {
        Some(removed) => Ok(Json(removed_tx(txid, *removed))),
        None => Err(not_found(format!("bitcoind knows no transaction {txid}"))),
      },
    }
  })
  .await
}
