//! `GET /mempool*` and the pending additions to other endpoints
//! (docs/API.md "Mempool"): rendering of the mempool overlay's snapshot
//! (`crate::mempool`). A pending object is its confirmed shape plus
//! `pending: true`, `first_seen` and `fee_rate`, with `height` and
//! `tx_index` null and `time` = `first_seen`.

use {
  super::{
    ServerError, ServerResult, ServerState, address_script, bad_request, history::entry_json,
    json::Render, not_found, read,
  },
  crate::{
    history::{HistoryKind, history_key},
    mempool::{PendingLocationKind, PendingTx, Snapshot},
    model::{RowId, Sale, TrackedUtxo},
    store::{Reader, StoredSale},
  },
  axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
  },
  bitcoin::{OutPoint, Script, ScriptBuf, Txid},
  serde::Serialize,
  serde_json::{Number, Value, json},
  std::{collections::HashMap, sync::Arc},
};

/// `spent_by_pending` of an Output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpendJson {
  pub txid: Txid,
  pub vin: u32,
  pub first_seen: u64,
  pub fee_rate: Number,
  /// The spend sells this output (a pending sale of it).
  pub sale: bool,
  /// Always true (bitcoind replaces by fee whatever the signal).
  pub replaceable: bool,
}

fn fee_rate(tx: &PendingTx) -> Number {
  Number::from_f64(tx.fee_rate()).unwrap_or_else(|| Number::from(0))
}

/// Marks a rendered confirmed object as pending.
fn mark(value: &mut Value, tx: &PendingTx) {
  if let Value::Object(object) = value {
    for key in ["height", "tx_index"] {
      if object.contains_key(key) {
        object.insert(key.into(), Value::Null);
      }
    }
    if object.contains_key("time") {
      object.insert("time".into(), json!(tx.first_seen));
    }
    object.insert("pending".into(), Value::Bool(true));
    object.insert("first_seen".into(), json!(tx.first_seen));
    object.insert("fee_rate".into(), Value::Number(fee_rate(tx)));
  }
}

fn snapshot_of(render: &Render) -> Arc<Snapshot> {
  render.mempool.clone().unwrap_or_default()
}

impl Render<'_> {
  /// Header time of the pending height: the transaction's first sight.
  fn prepare(&self, tx: &PendingTx) {
    self.set_time(
      tx.height,
      Some(u32::try_from(tx.first_seen).unwrap_or(u32::MAX)),
    );
  }

  /// `spent_by_pending` of a tracked outpoint.
  pub fn spent_by_pending(&self, outpoint: &OutPoint) -> Option<SpendJson> {
    let mempool = self.mempool.as_ref()?;
    let spend = mempool.spend(outpoint)?;
    let tx = &mempool.txs[spend.tx];
    Some(SpendJson {
      txid: tx.txid,
      vin: spend.vin,
      first_seen: tx.first_seen,
      fee_rate: fee_rate(tx),
      sale: mempool.sold(outpoint),
      replaceable: true,
    })
  }

  /// A pending Sale.
  pub fn pending_sale(&self, tx: &PendingTx, sale: &Sale) -> anyhow::Result<Value> {
    self.prepare(tx);
    let stored = StoredSale {
      height: tx.height,
      sale: sale.clone(),
      buyer_value: sale
        .buyer_vout
        .and_then(|vout| tx.tx.outputs.get(vout as usize))
        .map(|output| output.value.to_sat()),
    };
    let mut value = serde_json::to_value(self.sale(&stored)?)?;
    mark(&mut value, tx);
    Ok(value)
  }

  /// A pending transaction's Move.
  pub fn pending_move(&self, tx: &PendingTx) -> anyhow::Result<Value> {
    self.prepare(tx);
    let first_seen = u32::try_from(tx.first_seen).unwrap_or(u32::MAX);
    let mut value = serde_json::to_value(self.move_json(&tx.classified.mv, Some(first_seen))?)?;
    if let Value::Object(object) = &mut value {
      object.insert("tx_index".into(), Value::Null);
      if let Some(Value::Array(inputs)) = object.get_mut("inputs") {
        for input in inputs {
          if input.get("created_height") == Some(&json!(tx.height)) {
            input["created_height"] = Value::Null;
            input["created_time"] = Value::Null;
          }
        }
      }
    }
    Ok(value)
  }

  /// A PendingEntry: `script`'s part in a pending transaction.
  fn pending_entry(
    &self,
    tx: &PendingTx,
    entry: &crate::history::HistoryEntry,
    script: &Script,
  ) -> anyhow::Result<Value> {
    self.prepare(tx);
    let sales = if entry.seller || entry.buyer {
      tx.classified
        .market
        .sales
        .iter()
        .filter(|sale| {
          sale.seller.script_pubkey.as_script() == script
            || sale.buyer_script.as_deref() == Some(script)
        })
        .map(|sale| self.pending_sale(tx, sale))
        .collect::<anyhow::Result<Vec<Value>>>()?
    } else {
      Vec::new()
    };
    let mut value = serde_json::to_value(entry_json(self, entry, Vec::new())?)?;
    mark(&mut value, tx);
    if let Value::Object(object) = &mut value {
      object.insert("sales".into(), Value::Array(sales));
      object.insert("address".into(), json!(self.address(script)));
      object.insert("script_pubkey".into(), json!(script.to_hex_string()));
    }
    Ok(value)
  }

  /// A PendingOutput: a tracked output of pending transaction `tx`.
  pub fn pending_output(
    &self,
    tx: &PendingTx,
    outpoint: OutPoint,
    utxo: &TrackedUtxo,
  ) -> anyhow::Result<Value> {
    self.prepare(tx);
    let mut value = serde_json::to_value(self.output_with(outpoint, utxo, true)?)?;
    if let Value::Object(object) = &mut value {
      object.insert("created_height".into(), Value::Null);
      object.insert("created_time".into(), Value::Null);
      object.insert("pending".into(), Value::Bool(true));
      object.insert("txid".into(), json!(tx.txid));
      object.insert("first_seen".into(), json!(tx.first_seen));
      object.insert("fee_rate".into(), Value::Number(fee_rate(tx)));
    }
    Ok(value)
  }

  /// A PendingTx.
  pub fn pending_tx(&self, tx: &PendingTx) -> anyhow::Result<Value> {
    let sales = tx
      .classified
      .market
      .sales
      .iter()
      .map(|sale| self.pending_sale(tx, sale))
      .collect::<anyhow::Result<Vec<Value>>>()?;
    let mut entries = Vec::with_capacity(tx.classified.entries.len());
    let mut sorted: Vec<&crate::history::HistoryEntry> = tx.classified.entries.iter().collect();
    sorted.sort_by_key(|entry| entry.key);
    for entry in sorted {
      if let Some(script) = tx.classified.scripts.get(&entry.key) {
        entries.push(self.pending_entry(tx, entry, script)?);
      }
    }
    Ok(json!({
      "txid": tx.txid,
      "pending": true,
      "first_seen": tx.first_seen,
      "fee": tx.fee,
      "vsize": tx.vsize,
      "fee_rate": fee_rate(tx),
      "depends": tx.depends,
      "kinds": tx.kinds().into_iter().map(HistoryKind::name).collect::<Vec<_>>(),
      "move": self.pending_move(tx)?,
      "sales": sales,
      "entries": entries,
    }))
  }

  /// `script`'s pending entries, newest first.
  pub fn pending_entries(
    &self,
    script: &Script,
    kinds: Option<&[HistoryKind]>,
    row: Option<RowId>,
  ) -> anyhow::Result<Vec<Value>> {
    let mempool = snapshot_of(self);
    let key = history_key(script);
    let mut entries = Vec::new();
    for tx in &mempool.txs {
      if tx
        .classified
        .scripts
        .get(&key)
        .is_none_or(|known| known.as_script() != script)
      {
        continue;
      }
      for entry in &tx.classified.entries {
        if entry.key != key
          || kinds.is_some_and(|kinds| !kinds.contains(&entry.kind))
          || row.is_some_and(|row| {
            !entry
              .received
              .rows
              .iter()
              .chain(&entry.sent.rows)
              .any(|(r, _)| *r == row)
          })
        {
          continue;
        }
        entries.push(self.pending_entry(tx, entry, script)?);
      }
    }
    Ok(entries)
  }

  /// `pending` of `/sat/{n}`.
  pub fn sat_pending(&self, sat: u64) -> anyhow::Result<Option<Value>> {
    let Some(mempool) = self.mempool.clone() else {
      return Ok(None);
    };
    let Some(location) = mempool.locate(sat) else {
      return Ok(None);
    };
    let tx = &mempool.txs[location.tx];
    let mut sales = Vec::new();
    for tx in &mempool.txs {
      for sale in &tx.classified.market.sales {
        if sale
          .seller
          .pieces
          .iter()
          .any(|piece| piece.start <= sat && sat < piece.end())
        {
          sales.push(self.pending_sale(tx, sale)?);
        }
      }
    }
    let (kind, satpoint, outpoint, offset, output) = match location.kind {
      PendingLocationKind::Fee => ("fee", None, None, None, None),
      PendingLocationKind::Output {
        vout,
        offset,
        burned,
      } => {
        let outpoint = OutPoint::new(tx.txid, vout);
        let output = match mempool.output(&outpoint) {
          Some((_, utxo)) => Some(self.pending_output(tx, outpoint, &utxo)?),
          None => None,
        };
        (
          if burned { "burned" } else { "unspent" },
          Some(format!("{outpoint}:{offset}")),
          Some(outpoint),
          Some(offset),
          output,
        )
      }
    };
    Ok(Some(json!({
      "txid": tx.txid,
      "first_seen": tx.first_seen,
      "fee_rate": fee_rate(tx),
      "kind": kind,
      "satpoint": satpoint,
      "outpoint": outpoint,
      "offset": offset,
      "output": output,
      "sales": sales,
    })))
  }

  /// Pending outputs paying `script` (`pending_outputs` of `/address`).
  pub fn pending_outputs(&self, script: &Script) -> anyhow::Result<Vec<Value>> {
    let mempool = snapshot_of(self);
    mempool
      .outputs_to(script)
      .into_iter()
      .map(|(outpoint, i, utxo)| self.pending_output(&mempool.txs[i], outpoint, &utxo))
      .collect()
  }

  fn tx_has_row(&self, tx: &PendingTx, row: RowId) -> bool {
    self
      .move_counts(&tx.classified.mv)
      .get(usize::from(row))
      .is_some_and(|n| *n > 0)
  }

  fn sale_row(&self, sale: &Sale, row: RowId) -> bool {
    let mut counts = Vec::new();
    self.count(&mut counts, &sale.seller.pieces);
    counts.get(usize::from(row)).is_some_and(|n| *n > 0)
  }
}

/// The `ETag` of everything rendered from a snapshot.
fn etag(seq: u64) -> String {
  format!("\"mempool-{seq}\"")
}

/// A JSON answer with the snapshot's ETag, or 304 when the client has it.
fn tagged(
  headers: &HeaderMap,
  seq: u64,
  body: impl FnOnce() -> ServerResult<Value>,
) -> ServerResult<Response> {
  let tag = etag(seq);
  if headers
    .get(header::IF_NONE_MATCH)
    .and_then(|value| value.to_str().ok())
    .is_some_and(|value| value.split(',').any(|candidate| candidate.trim() == tag))
  {
    let mut response = StatusCode::NOT_MODIFIED.into_response();
    response
      .headers_mut()
      .insert(header::ETAG, HeaderValue::from_str(&tag).unwrap());
    return Ok(response);
  }
  let mut response = Json(body()?).into_response();
  response
    .headers_mut()
    .insert(header::ETAG, HeaderValue::from_str(&tag).unwrap());
  Ok(response)
}

/// `/status` additions.
pub fn status_fields(state: &ServerState) -> (bool, bool) {
  (true, state.published.mempool().enabled)
}

pub async fn status(State(state): State<Arc<ServerState>>) -> Json<Value> {
  let mempool = state.published.mempool();
  let stats = &mempool.stats;
  let enabled = mempool.enabled;
  let count = |n: usize| if enabled { n } else { 0 };
  Json(json!({
    "enabled": enabled,
    "ready": enabled && mempool.ready,
    "seq": mempool.seq,
    "height": mempool.tip.map(|(height, _)| height),
    "hash": mempool.tip.map(|(_, hash)| hash),
    "mempool_sequence": stats.mempool_sequence,
    "mempool_txs": count(stats.mempool_txs),
    "scanned_txs": count(stats.scanned_txs),
    "pending_txs": count(mempool.txs.len()),
    "pending_sales": count(stats.pending_sales),
    "pending_extractions": count(stats.pending_extractions),
    "removed": count(mempool.removed.len()),
    "interval_ms": stats.interval_ms,
    "last_poll_at": stats.last_poll_at,
    "last_poll_ms": stats.last_poll_ms,
    "last_change_at": stats.last_change_at,
    "error": stats.error,
  }))
}

/// `kind` and `row` of the list endpoints.
struct Filter {
  kind: Option<HistoryKind>,
  row: Option<RowId>,
}

impl Filter {
  fn parse(state: &ServerState, params: &HashMap<String, String>) -> ServerResult<Self> {
    let kind = params
      .get("kind")
      .filter(|value| !value.is_empty())
      .map(|name| {
        HistoryKind::from_name(name).ok_or_else(|| bad_request(format!("unknown kind `{name}`")))
      })
      .transpose()?;
    let row = params
      .get("row")
      .filter(|value| !value.is_empty())
      .map(|id| state.row(id).map(|(row, _)| row))
      .transpose()?;
    Ok(Self { kind, row })
  }
}

/// Which list a request is for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum List {
  All,
  Extractions,
  Transfers,
}

fn txs_json(
  render: &Render,
  mempool: &Snapshot,
  filter: &Filter,
  list: List,
) -> ServerResult<Vec<Value>> {
  let mut txs = Vec::new();
  for tx in &mempool.txs {
    let keep = match list {
      List::All => true,
      List::Extractions => tx.has_kind(HistoryKind::Extraction),
      List::Transfers => !tx.has_kind(HistoryKind::Extraction) && !tx.has_kind(HistoryKind::Sale),
    };
    if keep
      && filter.kind.is_none_or(|kind| tx.has_kind(kind))
      && filter.row.is_none_or(|row| render.tx_has_row(tx, row))
    {
      txs.push(render.pending_tx(tx)?);
    }
  }
  Ok(txs)
}

fn header_json(mempool: &Snapshot) -> serde_json::Map<String, Value> {
  let mut object = serde_json::Map::new();
  object.insert("seq".into(), json!(mempool.seq));
  object.insert(
    "height".into(),
    json!(mempool.tip.map(|(height, _)| height)),
  );
  object.insert("hash".into(), json!(mempool.tip.map(|(_, hash)| hash)));
  object
}

async fn list(
  state: Arc<ServerState>,
  headers: HeaderMap,
  params: HashMap<String, String>,
  list: List,
) -> ServerResult<Response> {
  read(&state, move |state, reader| {
    let filter = Filter::parse(state, &params)?;
    let render = state.render(reader);
    let mempool = snapshot_of(&render);
    tagged(&headers, mempool.seq, || {
      let mut object = header_json(&mempool);
      object.insert(
        "txs".into(),
        Value::Array(txs_json(&render, &mempool, &filter, list)?),
      );
      if list == List::All {
        object.insert("removed".into(), json!(mempool.removed));
      }
      Ok(Value::Object(object))
    })
  })
  .await
}

pub async fn all(
  State(state): State<Arc<ServerState>>,
  headers: HeaderMap,
  Query(params): Query<HashMap<String, String>>,
) -> ServerResult<Response> {
  list(state, headers, params, List::All).await
}

pub async fn extractions(
  State(state): State<Arc<ServerState>>,
  headers: HeaderMap,
  Query(params): Query<HashMap<String, String>>,
) -> ServerResult<Response> {
  list(state, headers, params, List::Extractions).await
}

pub async fn transfers(
  State(state): State<Arc<ServerState>>,
  headers: HeaderMap,
  Query(params): Query<HashMap<String, String>>,
) -> ServerResult<Response> {
  list(state, headers, params, List::Transfers).await
}

pub async fn sales(
  State(state): State<Arc<ServerState>>,
  headers: HeaderMap,
  Query(params): Query<HashMap<String, String>>,
) -> ServerResult<Response> {
  read(&state, move |state, reader| {
    let filter = Filter::parse(state, &params)?;
    let render = state.render(reader);
    let mempool = snapshot_of(&render);
    tagged(&headers, mempool.seq, || {
      let mut sales = Vec::new();
      for tx in &mempool.txs {
        for sale in &tx.classified.market.sales {
          if filter.row.is_none_or(|row| render.sale_row(sale, row)) {
            sales.push(render.pending_sale(tx, sale)?);
          }
        }
      }
      let mut object = header_json(&mempool);
      object.insert("sales".into(), Value::Array(sales));
      Ok(Value::Object(object))
    })
  })
  .await
}

pub async fn tx(
  State(state): State<Arc<ServerState>>,
  Path(txid): Path<String>,
) -> ServerResult<Response> {
  let txid = super::parse::<Txid>("txid", &txid)?;
  read(&state, move |state, reader| {
    let render = state.render(reader);
    let mempool = snapshot_of(&render);
    if let Some(tx) = mempool.tx(&txid) {
      return Ok(Json(render.pending_tx(tx)?).into_response());
    }
    match mempool.removed(&txid) {
      Some(removed) => Ok(
        (
          StatusCode::NOT_FOUND,
          Json(json!({
            "error": format!("transaction {txid} is no longer pending"),
            "removed": removed,
          })),
        )
          .into_response(),
      ),
      None => Err(not_found(format!("transaction {txid} is not pending"))),
    }
  })
  .await
}

fn script_json(render: &Render, reader: &Reader, script: &ScriptBuf) -> ServerResult<Value> {
  let _ = reader;
  let mempool = snapshot_of(render);
  let mut spent = Vec::new();
  for tx in &mempool.txs {
    for input in &tx.classified.mv.inputs {
      if input.utxo.script_pubkey == *script && !mempool.tx(&input.outpoint.txid).is_some() {
        let mut value = serde_json::to_value(render.spent_by_pending(&input.outpoint))?;
        if let Value::Object(object) = &mut value {
          object.insert("outpoint".into(), json!(input.outpoint));
        }
        spent.push(value);
      }
    }
  }
  Ok(json!({
    "address": render.address(script),
    "script_pubkey": script.to_hex_string(),
    "seq": mempool.seq,
    "entries": render.pending_entries(script, None, None)?,
    "spent": spent,
    "outputs": render.pending_outputs(script)?,
  }))
}

pub async fn address(
  State(state): State<Arc<ServerState>>,
  Path(address): Path<String>,
) -> ServerResult<Json<Value>> {
  let script = address_script(&state, &address)?;
  read(&state, move |state, reader| {
    Ok(Json(script_json(&state.render(reader), reader, &script)?))
  })
  .await
}

pub async fn script(
  State(state): State<Arc<ServerState>>,
  Path(script): Path<String>,
) -> ServerResult<Json<Value>> {
  let script = ScriptBuf::from_hex(&script)
    .map_err(|err| bad_request(format!("invalid script hex: {err}")))?;
  read(&state, move |state, reader| {
    Ok(Json(script_json(&state.render(reader), reader, &script)?))
  })
  .await
}

/// `/output/{outpoint}` of an output a pending transaction creates.
pub fn pending_output(render: &Render, outpoint: OutPoint) -> ServerResult<Option<Value>> {
  let mempool = snapshot_of(render);
  match mempool.output(&outpoint) {
    Some((i, utxo)) => Ok(Some(render.pending_output(
      &mempool.txs[i],
      outpoint,
      &utxo,
    )?)),
    None => Ok(None),
  }
}

impl From<serde_json::Error> for ServerError {
  fn from(err: serde_json::Error) -> Self {
    Self::Internal(err.into())
  }
}
