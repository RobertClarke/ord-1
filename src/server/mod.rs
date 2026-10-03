//! Read-only JSON HTTP API (docs/API.md).
//!
//! OWNER: store+server+indexer agent.
//!
//! Handlers read through one redb read transaction each (on the blocking
//! pool) and the supply snapshot the indexer publishes after every commit.
//! `/moves` and `/sats` stream NDJSON from a blocking task that opens a fresh
//! read transaction for every chunk and holds none while the client is slow,
//! so a stalled client never keeps old pages from being reused; a client
//! that takes no chunk for `ServerState::stream_timeout` gets its stream
//! aborted.

pub mod catalog;
pub mod history;
pub mod json;
pub mod mempool;
pub mod sales;
#[cfg(test)]
mod tests;
pub mod years;

use {
  self::{
    catalog::{Catalog, RowInfo},
    json::{BlockJson, DeltaJson, Render, RowMap, RowSupplyJson, YearsMap},
  },
  crate::{
    indexer::{Mode, Published, Timings},
    model::{LocationKind, RowId, RowStats, TrackedRange, TrackerConfig},
    settings::Chain,
    store::{MovesRetention, Reader, Store},
  },
  axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
  },
  bitcoin::{Address, BlockHash, OutPoint, ScriptBuf, address::NetworkUnchecked},
  serde::Serialize,
  std::{
    collections::HashMap,
    future::Future,
    io,
    str::FromStr,
    sync::{
      Arc, Mutex,
      atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
  },
  tokio::sync::mpsc,
  tokio_stream::wrappers::ReceiverStream,
  tower_http::{compression::CompressionLayer, cors::CorsLayer},
};

/// `top` holders at most.
pub const MAX_TOP: usize = 1000;
/// Blocks per `/moves` or `/blocks` request at most (`to - from`).
pub const MAX_BLOCK_RANGE: u32 = 10_000;
/// Points per history request at most.
pub const MAX_HISTORY_POINTS: u32 = 10_000;
const DEFAULT_HISTORY_POINTS: u32 = 2_000;
/// How long a stream waits for the client to take a chunk before aborting.
pub const STREAM_TIMEOUT: Duration = Duration::from_secs(60);
/// NDJSON bytes per chunk (and per read transaction).
const CHUNK: usize = 64 << 10;
/// Blocks (`/moves`) or pieces (`/sats`) read per read transaction at most,
/// so filtered streams that emit little still move to a fresh one.
const MOVES_SCAN: u32 = 256;
const SATS_SCAN: usize = 100_000;
const SALES_SCAN: usize = 10_000;

pub struct ServerState {
  pub store: Arc<Store>,
  pub catalog: Arc<dyn Catalog>,
  pub published: Arc<Published>,
  pub chain: Chain,
  pub config: TrackerConfig,
  pub moves_retention: MovesRetention,
  /// Unix time the process started.
  pub started: u64,
  /// `STREAM_TIMEOUT`, shorter in tests.
  pub stream_timeout: Duration,
  /// NDJSON streams in progress.
  pub streams: AtomicUsize,
  /// Per-row counts of located inscribed sats, and the height they are for.
  pub inscribed_rows: Mutex<Option<(u32, Arc<Vec<u64>>)>>,
}

#[derive(Debug)]
pub enum ServerError {
  BadRequest(String),
  NotFound(String),
  NotReady,
  /// 503 with a reason.
  NotReadyBecause(String),
  Internal(anyhow::Error),
}

impl From<anyhow::Error> for ServerError {
  fn from(err: anyhow::Error) -> Self {
    Self::Internal(err)
  }
}

impl IntoResponse for ServerError {
  fn into_response(self) -> Response {
    let (status, message) = match self {
      Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
      Self::NotFound(message) => (StatusCode::NOT_FOUND, message),
      Self::NotReady => (
        StatusCode::SERVICE_UNAVAILABLE,
        "the index is not ready: no block indexed yet".into(),
      ),
      Self::NotReadyBecause(message) => (StatusCode::SERVICE_UNAVAILABLE, message),
      Self::Internal(err) => {
        log::error!("request failed: {err:#}");
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))
      }
    };
    (status, Json(serde_json::json!({ "error": message }))).into_response()
  }
}

type ServerResult<T> = Result<T, ServerError>;

fn bad_request(message: impl Into<String>) -> ServerError {
  ServerError::BadRequest(message.into())
}

fn not_found(message: impl Into<String>) -> ServerError {
  ServerError::NotFound(message.into())
}

/// Runs `f` on the blocking pool with a fresh read transaction.
async fn read<T: Send + 'static>(
  state: &Arc<ServerState>,
  f: impl FnOnce(&ServerState, &Reader) -> ServerResult<T> + Send + 'static,
) -> ServerResult<T> {
  let state = state.clone();
  tokio::task::spawn_blocking(move || {
    let reader = state.store.reader()?;
    f(&state, &reader)
  })
  .await
  .map_err(|err| ServerError::Internal(err.into()))?
}

impl ServerState {
  fn render<'a>(&'a self, reader: &'a Reader) -> Render<'a> {
    Render::new(&*self.catalog, reader, self.chain.network(), &self.config)
      .with_mempool(self.published.mempool())
  }

  fn row(&self, id: &str) -> ServerResult<(RowId, &RowInfo)> {
    let row = self
      .catalog
      .row_id(id)
      .ok_or_else(|| not_found(format!("unknown row `{id}`")))?;
    Ok((row, &self.catalog.rows()[usize::from(row)]))
  }
}

/// The committed tip, or 503.
fn tip(reader: &Reader) -> ServerResult<u32> {
  reader.height()?.ok_or(ServerError::NotReady)
}

fn param<T: FromStr>(params: &HashMap<String, String>, name: &str) -> ServerResult<Option<T>>
where
  T::Err: std::fmt::Display,
{
  params
    .get(name)
    .filter(|value| !value.is_empty())
    .map(|value| {
      value
        .parse()
        .map_err(|err| bad_request(format!("invalid {name} `{value}`: {err}")))
    })
    .transpose()
}

fn parse<T: FromStr>(what: &str, value: &str) -> ServerResult<T>
where
  T::Err: std::fmt::Display,
{
  value
    .parse()
    .map_err(|err| bad_request(format!("invalid {what} `{value}`: {err}")))
}

pub fn router(state: Arc<ServerState>) -> Router {
  Router::new()
    .route("/status", get(status))
    .route("/rows", get(rows))
    .route("/supply", get(supply))
    .route("/supply/years", get(supply_years))
    .route("/supply/{row}", get(supply_row))
    .route("/supply/{row}/history", get(supply_history))
    .route("/sat/{sat}", get(sat))
    .route("/output/{outpoint}", get(output))
    .route("/address/{address}", get(address))
    .route("/script/{script}", get(script))
    .route("/address/{address}/history", get(address_history))
    .route("/script/{script}/history", get(script_history))
    .route("/block/{height}", get(block))
    .route("/block/{height}/moves", get(block_moves))
    .route("/moves", get(moves))
    .route("/sats", get(sats))
    .route("/blocks", get(blocks))
    .route("/sales", get(sales_stream))
    .route("/sales/tx/{txid}", get(sales_for_tx))
    .route("/mempool", get(mempool::all))
    .route("/mempool/status", get(mempool::status))
    .route("/mempool/sales", get(mempool::sales))
    .route("/mempool/extractions", get(mempool::extractions))
    .route("/mempool/transfers", get(mempool::transfers))
    .route("/mempool/tx/{txid}", get(mempool::tx))
    .route("/mempool/address/{address}", get(mempool::address))
    .route("/mempool/script/{script}", get(mempool::script))
    .fallback(|| async { not_found("no such endpoint; see docs/API.md") })
    .layer(CompressionLayer::new())
    .layer(CorsLayer::permissive())
    .with_state(state)
}

/// Serves until `shutdown` resolves (then finishes in-flight requests).
pub async fn serve(
  state: Arc<ServerState>,
  listener: tokio::net::TcpListener,
  shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
  axum::serve(listener, router(state))
    .with_graceful_shutdown(shutdown)
    .await?;
  Ok(())
}

#[derive(Serialize)]
struct StatusJson {
  chain: &'static str,
  version: &'static str,
  commit: Option<&'static str>,
  height: Option<u32>,
  hash: Option<BlockHash>,
  time: Option<u32>,
  bitcoind_height: Option<u32>,
  mode: Mode,
  fast_sync_complete: bool,
  started: u64,
  tracked_utxos: u64,
  tracked_pieces: u64,
  lost_sats: u64,
  ranges: Vec<TrackedRange>,
  extracted_max_value: u64,
  extracted_since: u32,
  catalog_fingerprint: String,
  moves_retention: String,
  /// Lowest height whose moves are kept.
  moves_from: u32,
  /// `/address/{address}/history` is served.
  address_history: bool,
  /// `--address-history-from`: `ordinals`, `all` or a height.
  address_history_from: String,
  /// The first height the address history covers, and its time (`null`
  /// until a covered block is indexed).
  address_history_from_height: Option<u32>,
  address_history_from_time: Option<u32>,
  /// The history is still being built (initial sync): history requests
  /// answer 503.
  address_history_building: bool,
  /// The supply by mint year is served (`years=` on `/supply…`,
  /// `/supply/years`).
  year_stats: bool,
  /// The mint years of the indexed blocks.
  years: Vec<years::YearSpanJson>,
  timings: Timings,
  error: Option<String>,
  /// `/mempool*` is served (docs/API.md "Mempool").
  mempool: bool,
  /// The mempool overlay is running.
  mempool_enabled: bool,
}

async fn status(State(state): State<Arc<ServerState>>) -> ServerResult<Json<StatusJson>> {
  read(&state, |state, reader| {
    let status = state.published.status();
    let commit = env!("GIT_COMMIT").trim();
    let history_from = reader.history_from()?;
    Ok(Json(StatusJson {
      chain: state.chain.name(),
      version: env!("CARGO_PKG_VERSION"),
      commit: (!commit.is_empty()).then_some(commit),
      height: status.height,
      hash: status.hash,
      time: status.time,
      bitcoind_height: status.bitcoind_height,
      mode: status.mode,
      fast_sync_complete: status.fast_sync_complete,
      started: state.started,
      tracked_utxos: status.tracked_utxos,
      tracked_pieces: status.tracked_pieces,
      lost_sats: status.lost_sats,
      ranges: state.config.ranges.clone(),
      extracted_max_value: state.config.extracted_max_value,
      extracted_since: state.config.extracted_since_time,
      catalog_fingerprint: state.catalog.fingerprint(),
      moves_retention: state.moves_retention.to_string(),
      moves_from: reader.moves_floor()?,
      address_history: true,
      address_history_from: state.config.address_history_from.to_string(),
      address_history_from_height: history_from,
      address_history_from_time: match history_from {
        Some(height) => reader.header(height)?.map(|header| header.time),
        None => None,
      },
      address_history_building: reader.history_building()?,
      year_stats: true,
      years: {
        let supply = state.published.supply();
        years::year_spans(&supply.years, supply.height)
      },
      timings: status.timings,
      error: status.error,
      mempool: mempool::status_fields(state).0,
      mempool_enabled: mempool::status_fields(state).1,
    }))
  })
  .await
}

#[derive(Serialize)]
struct RowJson {
  #[serde(flatten)]
  row: RowInfo,
  total: u64,
}

async fn rows(State(state): State<Arc<ServerState>>) -> Json<Vec<RowJson>> {
  Json(
    state
      .catalog
      .rows()
      .iter()
      .enumerate()
      .map(|(row, info)| RowJson {
        row: info.clone(),
        total: state.catalog.row_total(row as RowId),
      })
      .collect(),
  )
}

#[derive(Serialize)]
struct SupplyJson {
  height: Option<u32>,
  hash: Option<BlockHash>,
  time: Option<u32>,
  rows: Vec<RowSupplyJson>,
}

/// Located inscribed sats per row at the published supply's height, by
/// tracker row id (the catalog's `RowId`s, then their year rows), computed
/// from the `inscribed` table and the sats' current locations, once per
/// height.
fn inscribed_rows(
  state: &ServerState,
  reader: &Reader,
  supply: &crate::model::SupplySnapshot,
  height: u32,
) -> anyhow::Result<Arc<Vec<u64>>> {
  if let Some((cached, counts)) = &*state.inscribed_rows.lock().unwrap()
    && *cached == height
  {
    return Ok(counts.clone());
  }
  let mut counts = vec![0; years::inscribed_len(state)];
  for entry in reader.inscribed_from(0)? {
    let (sat, _) = entry?;
    if reader
      .locate(sat)?
      .is_some_and(|location| location.kind == LocationKind::Unspent)
    {
      years::inscribed_by_year(state, &supply.years.starts, sat, &mut counts);
    }
  }
  let counts = Arc::new(counts);
  *state.inscribed_rows.lock().unwrap() = Some((height, counts.clone()));
  Ok(counts)
}

fn row_supply(
  state: &ServerState,
  reader: &Reader,
  supply: &crate::model::RowSupply,
  height: u32,
  top: Option<usize>,
  inscribed: &[u64],
) -> ServerResult<RowSupplyJson> {
  let render = state.render(reader);
  let delta = reader
    .row_delta(supply.row, height)?
    .map(|(before, after)| DeltaJson {
      height,
      before,
      after,
    });
  let holders = |holders: &[crate::model::Holder], top: usize| -> ServerResult<_> {
    Ok(
      holders
        .iter()
        .take(top)
        .map(|holder| render.holder(holder))
        .collect::<anyhow::Result<Vec<_>>>()?,
    )
  };
  Ok(RowSupplyJson {
    id: render.row_name(supply.row),
    total: state.catalog.row_total(supply.row),
    stats: json::SupplyStatsJson {
      stats: supply.stats,
      inscribed: inscribed
        .get(usize::from(supply.row))
        .copied()
        .unwrap_or_default(),
    },
    active_non_extracted: YearsMap(supply.active_non_extracted.clone()),
    delta,
    top_extracted_holders: top
      .map(|top| holders(&supply.top_extracted_holders, top))
      .transpose()?,
    top_non_extracted_holders: top
      .map(|top| holders(&supply.top_non_extracted_holders, top))
      .transpose()?,
  })
}

async fn supply(
  State(state): State<Arc<ServerState>>,
  Query(params): Query<HashMap<String, String>>,
) -> ServerResult<Json<SupplyJson>> {
  read(&state, move |state, reader| {
    let supply = state.published.supply();
    let height = supply.height.ok_or(ServerError::NotReady)?;
    let years = years::parse_years(&params, &supply)?;
    let inscribed = inscribed_rows(state, reader, &supply, height)?;
    let rows = match &years {
      None => supply
        .rows
        .iter()
        .map(|row| row_supply(state, reader, row, height, None, &inscribed))
        .collect::<ServerResult<_>>()?,
      Some(years) => supply
        .years
        .rows
        .iter()
        .map(|row| years::row_supply(state, reader, row, years, height, None, &inscribed))
        .collect::<ServerResult<_>>()?,
    };
    Ok(Json(SupplyJson {
      height: supply.height,
      hash: supply.hash,
      time: supply.time,
      rows,
    }))
  })
  .await
}

async fn supply_years(
  State(state): State<Arc<ServerState>>,
) -> ServerResult<Json<years::SupplyYearsJson>> {
  read(&state, |state, reader| {
    let supply = state.published.supply();
    let height = years::ready(&supply)?;
    let inscribed = inscribed_rows(state, reader, &supply, height)?;
    Ok(Json(years::supply_years(state, &supply, &inscribed)?))
  })
  .await
}

async fn supply_row(
  State(state): State<Arc<ServerState>>,
  Path(id): Path<String>,
  Query(params): Query<HashMap<String, String>>,
) -> ServerResult<Json<RowSupplyJson>> {
  let top = param::<usize>(&params, "top")?.unwrap_or(100);
  if top > MAX_TOP {
    return Err(bad_request(format!("top must be at most {MAX_TOP}")));
  }
  read(&state, move |state, reader| {
    let (row, _) = state.row(&id)?;
    let supply = state.published.supply();
    let height = supply.height.ok_or(ServerError::NotReady)?;
    let years = years::parse_years(&params, &supply)?;
    let inscribed = inscribed_rows(state, reader, &supply, height)?;
    if let Some(years) = years {
      let row_years = supply
        .years
        .rows
        .iter()
        .find(|supply| supply.row == row)
        .ok_or_else(|| not_found(format!("no supply for row `{id}`")))?;
      return Ok(Json(years::row_supply(
        state,
        reader,
        row_years,
        &years,
        height,
        Some(top.min(crate::model::YEAR_TOP_HOLDERS)),
        &inscribed,
      )?));
    }
    let row_supply_entry = supply
      .rows
      .iter()
      .find(|supply| supply.row == row)
      .ok_or_else(|| not_found(format!("no supply for row `{id}`")))?;
    Ok(Json(row_supply(
      state,
      reader,
      row_supply_entry,
      height,
      Some(top),
      &inscribed,
    )?))
  })
  .await
}

#[derive(Serialize)]
struct HistoryPoint {
  height: u32,
  time: Option<u32>,
  stats: HistoryStats,
}

/// `RowStats`, but holder counts are `null` over more than one year (they
/// do not add up).
#[derive(Serialize)]
struct HistoryStats {
  mined: u64,
  located: u64,
  burned: u64,
  lost: u64,
  overwritten: u64,
  extracted: u64,
  holders: Option<u64>,
  extracted_holders: Option<u64>,
}

impl HistoryStats {
  fn new(stats: RowStats, holders: bool) -> Self {
    Self {
      mined: stats.mined,
      located: stats.located,
      burned: stats.burned,
      lost: stats.lost,
      overwritten: stats.overwritten,
      extracted: stats.extracted,
      holders: holders.then_some(stats.holders),
      extracted_holders: holders.then_some(stats.extracted_holders),
    }
  }
}

#[derive(Serialize)]
struct HistoryJson {
  row: String,
  /// The years asked for (`years=`), else absent.
  #[serde(skip_serializing_if = "Option::is_none")]
  years: Option<Vec<u16>>,
  points: Vec<HistoryPoint>,
}

async fn supply_history(
  State(state): State<Arc<ServerState>>,
  Path(id): Path<String>,
  Query(params): Query<HashMap<String, String>>,
) -> ServerResult<Json<HistoryJson>> {
  let from = param::<u32>(&params, "from")?;
  let to = param::<u32>(&params, "to")?;
  let every = param::<u32>(&params, "every")?;
  read(&state, move |state, reader| {
    let (row, info) = state.row(&id)?;
    let tip = tip(reader)?;
    let years = years::parse_years(&params, &state.published.supply())?;
    // The tracker rows to add up: the row, or its year rows.
    let ids: Vec<RowId> = match &years {
      None => vec![row],
      Some(years) => years
        .list
        .iter()
        .map(|&year| crate::model::year_row(state.catalog.rows().len(), row, year))
        .collect(),
    };
    let holders = years.as_ref().is_none_or(|years| years.single());
    let from = from.unwrap_or(0);
    let to = to.unwrap_or(tip).min(tip);
    if from > to {
      return Err(bad_request(format!("from {from} is above to {to}")));
    }
    let span = to - from + 1;
    let every = match every {
      Some(0) => return Err(bad_request("every must be at least 1")),
      Some(every) => every,
      None => span.div_ceil(DEFAULT_HISTORY_POINTS),
    };
    let points = span.div_ceil(every);
    if points > MAX_HISTORY_POINTS {
      return Err(bad_request(format!(
        "{points} points; at most {MAX_HISTORY_POINTS} (raise `every`)"
      )));
    }
    let render = state.render(reader);
    let mut result = Vec::with_capacity(points as usize);
    for point in 0..points {
      let end =
        (u64::from(from) + u64::from(point + 1) * u64::from(every) - 1).min(u64::from(to)) as u32;
      let mut stats = RowStats::default();
      for &id in &ids {
        let at = reader
          .row_stats_at(id, end)?
          .map(|(_, stats)| stats)
          .unwrap_or_default();
        stats.mined += at.mined;
        stats.located += at.located;
        stats.burned += at.burned;
        stats.lost += at.lost;
        stats.overwritten += at.overwritten;
        stats.extracted += at.extracted;
        stats.holders += at.holders;
        stats.extracted_holders += at.extracted_holders;
      }
      result.push(HistoryPoint {
        height: end,
        time: render.time(end)?,
        stats: HistoryStats::new(stats, holders),
      });
    }
    Ok(Json(HistoryJson {
      row: info.id.clone(),
      years: years.map(|years| {
        years
          .list
          .iter()
          .map(|&year| crate::model::FIRST_YEAR + year as u16)
          .collect()
      }),
      points: result,
    }))
  })
  .await
}

async fn sat(
  State(state): State<Arc<ServerState>>,
  Path(sat): Path<String>,
) -> ServerResult<Json<json::SatJson>> {
  let sat = parse::<u64>("sat", &sat)?;
  if sat >= ordinals::Sat::SUPPLY {
    return Err(bad_request(format!(
      "sat {sat} is beyond the last sat {}",
      ordinals::Sat::LAST.n()
    )));
  }
  read(&state, move |state, reader| {
    tip(reader)?;
    Ok(Json(state.render(reader).sat(sat)?))
  })
  .await
}

async fn output(
  State(state): State<Arc<ServerState>>,
  Path(outpoint): Path<String>,
) -> ServerResult<Response> {
  let outpoint = parse::<OutPoint>("outpoint", &outpoint)?;
  read(&state, move |state, reader| {
    tip(reader)?;
    let render = state.render(reader);
    match reader.utxo(outpoint)? {
      Some(utxo) => Ok(Json(render.output(outpoint, &utxo)?).into_response()),
      None => match mempool::pending_output(&render, outpoint)? {
        Some(pending) => Ok(Json(pending).into_response()),
        None => Err(not_found(format!("output {outpoint} holds no tracked sat"))),
      },
    }
  })
  .await
}

#[derive(Serialize)]
struct ScriptJson {
  address: Option<String>,
  script_pubkey: String,
  outputs: Vec<json::OutputJson>,
  rows: RowMap,
  /// Tracked outputs pending transactions pay it (omitted when the mempool
  /// overlay is off).
  #[serde(skip_serializing_if = "Option::is_none")]
  pending_outputs: Option<Vec<serde_json::Value>>,
}

fn script_outputs(
  state: &ServerState,
  reader: &Reader,
  script: ScriptBuf,
) -> ServerResult<ScriptJson> {
  tip(reader)?;
  let render = state.render(reader);
  let mut outputs = Vec::new();
  let mut counts = Vec::new();
  for (outpoint, utxo) in reader.utxos_for_script(&script)? {
    render.count(&mut counts, &utxo.pieces);
    outputs.push(render.output(outpoint, &utxo)?);
  }
  Ok(ScriptJson {
    address: render.address(&script),
    script_pubkey: script.to_hex_string(),
    outputs,
    rows: render.row_map(&counts),
    pending_outputs: match &render.mempool {
      Some(mempool) if mempool.enabled => Some(render.pending_outputs(&script)?),
      _ => None,
    },
  })
}

async fn address(
  State(state): State<Arc<ServerState>>,
  Path(address): Path<String>,
) -> ServerResult<Json<ScriptJson>> {
  let address = parse::<Address<NetworkUnchecked>>("address", &address)?
    .require_network(state.chain.network())
    .map_err(|err| bad_request(format!("address is not for {}: {err}", state.chain)))?;
  read(&state, move |state, reader| {
    Ok(Json(script_outputs(
      state,
      reader,
      address.script_pubkey(),
    )?))
  })
  .await
}

async fn script(
  State(state): State<Arc<ServerState>>,
  Path(script): Path<String>,
) -> ServerResult<Json<ScriptJson>> {
  let script = ScriptBuf::from_hex(&script)
    .map_err(|err| bad_request(format!("invalid script hex: {err}")))?;
  read(&state, move |state, reader| {
    Ok(Json(script_outputs(state, reader, script)?))
  })
  .await
}

fn address_script(state: &ServerState, address: &str) -> ServerResult<ScriptBuf> {
  Ok(
    parse::<Address<NetworkUnchecked>>("address", address)?
      .require_network(state.chain.network())
      .map_err(|err| bad_request(format!("address is not for {}: {err}", state.chain)))?
      .script_pubkey(),
  )
}

async fn address_history(
  State(state): State<Arc<ServerState>>,
  Path(address): Path<String>,
  Query(params): Query<HashMap<String, String>>,
) -> ServerResult<Json<history::HistoryJson>> {
  let script = address_script(&state, &address)?;
  let query = history::HistoryQuery::parse(&params)?;
  read(&state, move |state, reader| {
    Ok(Json(history::script_history(state, reader, script, query)?))
  })
  .await
}

async fn script_history(
  State(state): State<Arc<ServerState>>,
  Path(script): Path<String>,
  Query(params): Query<HashMap<String, String>>,
) -> ServerResult<Json<history::HistoryJson>> {
  let script = ScriptBuf::from_hex(&script)
    .map_err(|err| bad_request(format!("invalid script hex: {err}")))?;
  let query = history::HistoryQuery::parse(&params)?;
  read(&state, move |state, reader| {
    Ok(Json(history::script_history(state, reader, script, query)?))
  })
  .await
}

async fn block(
  State(state): State<Arc<ServerState>>,
  Path(height): Path<String>,
) -> ServerResult<Json<BlockJson>> {
  let height = parse::<u32>("height", &height)?;
  read(&state, move |state, reader| {
    tip(reader)?;
    let header = reader
      .header(height)?
      .ok_or_else(|| not_found(format!("block {height} is not indexed")))?;
    let render = state.render(reader);
    let (moves, rows_moved) = match reader.moves(height)? {
      Some(moves) => {
        let mut counts = Vec::<u64>::new();
        for m in &moves {
          let move_counts = render.move_counts(m);
          if counts.len() < move_counts.len() {
            counts.resize(move_counts.len(), 0);
          }
          for (total, count) in counts.iter_mut().zip(move_counts) {
            *total += count;
          }
        }
        (Some(moves.len() as u64), Some(render.row_map(&counts)))
      }
      None => (None, None),
    };
    Ok(Json(BlockJson {
      height,
      hash: header.block_hash(),
      prev_hash: header.prev_blockhash,
      time: header.time,
      moves,
      rows_moved,
    }))
  })
  .await
}

#[derive(Serialize)]
struct BlockMovesJson {
  height: u32,
  hash: BlockHash,
  time: u32,
  moves: Vec<json::MoveJson>,
}

fn moves_not_retained(reader: &Reader, height: u32) -> ServerResult<ServerError> {
  Ok(not_found(format!(
    "the moves of block {height} are not retained; moves are kept from height {} \
     (--moves-retention)",
    reader.moves_floor()?
  )))
}

async fn block_moves(
  State(state): State<Arc<ServerState>>,
  Path(height): Path<String>,
) -> ServerResult<Json<BlockMovesJson>> {
  let height = parse::<u32>("height", &height)?;
  read(&state, move |state, reader| {
    tip(reader)?;
    let header = reader
      .header(height)?
      .ok_or_else(|| not_found(format!("block {height} is not indexed")))?;
    let Some(moves) = reader.moves(height)? else {
      return Err(moves_not_retained(reader, height)?);
    };
    let render = state.render(reader);
    Ok(Json(BlockMovesJson {
      height,
      hash: header.block_hash(),
      time: header.time,
      moves: moves
        .iter()
        .map(|m| render.move_json(m, Some(header.time)))
        .collect::<anyhow::Result<_>>()?,
    }))
  })
  .await
}

/// Validates a `from`/`to` block range (inclusive, at most
/// `MAX_BLOCK_RANGE` apart) against the tip.
fn block_range(reader: &Reader, from: Option<u32>, to: Option<u32>) -> ServerResult<(u32, u32)> {
  let tip = tip(reader)?;
  let from = from.ok_or_else(|| bad_request("`from` is required"))?;
  let to = to.unwrap_or(tip);
  if from > to {
    return Err(bad_request(format!("from {from} is above to {to}")));
  }
  if to - from > MAX_BLOCK_RANGE {
    return Err(bad_request(format!(
      "to - from must be at most {MAX_BLOCK_RANGE}"
    )));
  }
  Ok((from, to.min(tip)))
}

#[derive(Serialize)]
struct MoveLine<'a> {
  height: u32,
  time: Option<u32>,
  #[serde(rename = "move")]
  m: &'a json::MoveJson,
}

async fn moves(
  State(state): State<Arc<ServerState>>,
  Query(params): Query<HashMap<String, String>>,
) -> ServerResult<Response> {
  let from = param::<u32>(&params, "from")?;
  let to = param::<u32>(&params, "to")?;
  let row = params.get("row").cloned();

  let (from, to, row) = read(&state, move |state, reader| {
    let (from, to) = block_range(reader, from, to)?;
    if from < reader.moves_floor()? {
      return Err(moves_not_retained(reader, from)?);
    }
    let row = row
      .map(|id| state.row(&id).map(|(row, _)| row))
      .transpose()?;
    Ok((from, to, row))
  })
  .await?;

  Ok(ndjson(
    state,
    MovesStream {
      next: Some(from),
      to,
      row,
    },
  ))
}

/// `/moves` from `next` to `to`, one block at a time.
struct MovesStream {
  /// `None` once past `to`.
  next: Option<u32>,
  to: u32,
  row: Option<RowId>,
}

impl Produce for MovesStream {
  fn fill(
    &mut self,
    state: &ServerState,
    reader: &Reader,
    buffer: &mut Vec<u8>,
  ) -> anyhow::Result<bool> {
    let render = state.render(reader);
    let floor = reader.moves_floor()?;
    let mut scanned = 0;
    while let Some(next) = self.next.filter(|next| *next <= self.to) {
      anyhow::ensure!(
        next >= floor,
        "the moves of block {next} were pruned during the stream; moves are kept from height \
         {floor}"
      );
      let Some(height) = reader.next_height_with_moves(next, self.to)? else {
        self.next = None;
        break;
      };
      self.next = height.checked_add(1);
      // Rolled back since the stream started: nothing to send for it.
      let Some(moves) = reader.moves(height)? else {
        continue;
      };
      let time = render.time(height)?;
      for m in &moves {
        if let Some(row) = self.row
          && render
            .move_counts(m)
            .get(usize::from(row))
            .is_none_or(|count| *count == 0)
        {
          continue;
        }
        let m = render.move_json(m, time)?;
        line(
          buffer,
          &MoveLine {
            height,
            time,
            m: &m,
          },
        )?;
      }
      scanned += 1;
      if buffer.len() >= CHUNK || scanned >= MOVES_SCAN {
        break;
      }
    }
    Ok(self.next.is_some_and(|next| next <= self.to))
  }
}

#[derive(Serialize)]
struct PieceLine<'a> {
  piece: json::PieceJson,
  count: u64,
  location: &'a json::LocationJson,
}

async fn sats(
  State(state): State<Arc<ServerState>>,
  Query(params): Query<HashMap<String, String>>,
) -> ServerResult<Response> {
  let from = param::<u64>(&params, "from")?.unwrap_or(0);
  let to = param::<u64>(&params, "to")?.unwrap_or(u64::MAX);
  if from >= to {
    return Err(bad_request(format!("from {from} must be below to {to}")));
  }
  let id = params
    .get("row")
    .cloned()
    .ok_or_else(|| bad_request("`row` is required"))?;
  let with_pieces = param::<bool>(&params, "pieces")?.unwrap_or(false);
  let count_only = match params.get("count").map(String::as_str) {
    None | Some("" | "0" | "false") => false,
    Some("1" | "true") => true,
    Some(value) => return Err(bad_request(format!("invalid count `{value}`"))),
  };

  if count_only {
    return read(&state, move |state, reader| {
      tip(reader)?;
      let (row, _) = state.row(&id)?;
      Ok(Json(sats_count(state, reader, row, id, from, to)?).into_response())
    })
    .await;
  }

  let row = read(&state, move |state, reader| {
    tip(reader)?;
    Ok(state.row(&id)?.0)
  })
  .await?;

  Ok(ndjson(
    state,
    SatsStream {
      next: from,
      to,
      row,
      with_pieces,
    },
  ))
}

/// The row's tracked sats by where they are now.
#[derive(Serialize, Default)]
struct SatsByKind {
  unspent: u64,
  burned: u64,
  lost: u64,
  overwritten: u64,
}

/// `/sats?count=1`.
#[derive(Serialize)]
struct SatsCountJson {
  row: String,
  from: u64,
  to: u64,
  /// The row's tracked sats in `[from, to)`.
  count: u64,
  by_kind: SatsByKind,
  /// Tracked pieces scanned (of any row).
  pieces: u64,
}

/// The `/sats` scan of `[from, to)`, counted instead of rendered: no output
/// lookups, no JSON per piece, one read transaction.
fn sats_count(
  state: &ServerState,
  reader: &Reader,
  row: RowId,
  id: String,
  from: u64,
  to: u64,
) -> anyhow::Result<SatsCountJson> {
  let to = to.min(crate::sats::math::SUPPLY);
  let mut by_kind = SatsByKind::default();
  let mut pieces = 0;
  for location in reader.pieces(from, to)? {
    let location = location?;
    let piece = &location.piece;
    // Only the sats in [from, to).
    let start = piece.start.max(from);
    let end = piece.end().min(to);
    if start >= end {
      continue;
    }
    pieces += 1;
    let count = state
      .catalog
      .row_counts(start, end - start)
      .into_iter()
      .find(|(r, _)| *r == row)
      .map_or(0, |(_, count)| count);
    *match location.kind {
      LocationKind::Unspent => &mut by_kind.unspent,
      LocationKind::Burned => &mut by_kind.burned,
      LocationKind::Lost => &mut by_kind.lost,
      LocationKind::Overwritten => &mut by_kind.overwritten,
    } += count;
  }
  Ok(SatsCountJson {
    row: id,
    from,
    to,
    count: by_kind.unspent + by_kind.burned + by_kind.lost + by_kind.overwritten,
    by_kind,
    pieces,
  })
}

/// `/sats` of `[next, to)`; resumes after the last sat it went through.
struct SatsStream {
  next: u64,
  to: u64,
  row: RowId,
  with_pieces: bool,
}

impl Produce for SatsStream {
  fn fill(
    &mut self,
    state: &ServerState,
    reader: &Reader,
    buffer: &mut Vec<u8>,
  ) -> anyhow::Result<bool> {
    let render = state.render(reader);
    // Consecutive pieces often share an output; keyed by kind too, as a
    // BIP30-overwritten outpoint holds an old and a current output.
    let mut outputs: HashMap<(OutPoint, bool), json::OutputJson> = HashMap::new();
    let mut scanned = 0;
    for location in reader.pieces(self.next, self.to)? {
      let mut location = location?;
      // Only the sats in [next, to).
      let piece = &mut location.piece;
      if piece.start < self.next {
        let skip = self.next - piece.start;
        piece.start += skip;
        piece.offset += skip;
        piece.len -= skip;
      }
      if piece.end() > self.to {
        piece.len = self.to - piece.start;
      }
      location.offset = piece.offset;
      self.next = piece.end();
      scanned += 1;
      let count = render
        .catalog
        .row_counts(piece.start, piece.len)
        .into_iter()
        .find(|(r, _)| *r == self.row)
        .map_or(0, |(_, count)| count);
      if count > 0 {
        let key = (
          location.outpoint,
          location.kind == LocationKind::Overwritten,
        );
        let output = match outputs.get(&key) {
          Some(output) => Some(output.clone()),
          None => {
            let output = render.location_output(&location, self.with_pieces)?;
            if let Some(output) = &output {
              outputs.insert(key, output.clone());
            }
            output
          }
        };
        let location_json = json::LocationJson {
          kind: location.kind,
          satpoint: format!("{}:{}", location.outpoint, location.offset),
          outpoint: location.outpoint,
          offset: location.offset,
          output,
        };
        line(
          buffer,
          &PieceLine {
            piece: render.piece(&location.piece),
            count,
            location: &location_json,
          },
        )?;
      }
      if buffer.len() >= CHUNK || scanned >= SATS_SCAN {
        return Ok(self.next < self.to);
      }
    }
    Ok(false)
  }
}

async fn sales_stream(
  State(state): State<Arc<ServerState>>,
  Query(params): Query<HashMap<String, String>>,
) -> ServerResult<Response> {
  let from = param::<u32>(&params, "from")?.unwrap_or(0);
  let to = param::<u32>(&params, "to")?;
  let row = params.get("row").cloned();

  let (to, row) = read(&state, move |state, reader| {
    let tip = tip(reader)?;
    let to = to.unwrap_or(tip);
    if from > to {
      return Err(bad_request(format!("from {from} is above to {to}")));
    }
    let row = row
      .map(|id| state.row(&id).map(|(row, _)| row))
      .transpose()?;
    Ok((to, row))
  })
  .await?;

  Ok(ndjson(
    state,
    SalesStream {
      next: Some((from, 0, 0)),
      to,
      row,
    },
  ))
}

/// `/sales` from the sale key `next` to the end of block `to`.
struct SalesStream {
  /// `None` once done.
  next: Option<crate::store::SaleKey>,
  to: u32,
  row: Option<RowId>,
}

/// The sale key right after `key`.
fn next_sale_key((height, tx_index, vin): crate::store::SaleKey) -> Option<crate::store::SaleKey> {
  if let Some(vin) = vin.checked_add(1) {
    Some((height, tx_index, vin))
  } else if let Some(tx_index) = tx_index.checked_add(1) {
    Some((height, tx_index, 0))
  } else {
    height.checked_add(1).map(|height| (height, 0, 0))
  }
}

impl Produce for SalesStream {
  fn fill(
    &mut self,
    state: &ServerState,
    reader: &Reader,
    buffer: &mut Vec<u8>,
  ) -> anyhow::Result<bool> {
    let Some(next) = self.next else {
      return Ok(false);
    };
    let render = state.render(reader);
    let mut scanned = 0;
    for sale in reader.sales(next, self.to)? {
      let sale = sale?;
      self.next = next_sale_key(sale.key());
      scanned += 1;
      if self.row.is_none_or(|row| render.sale_has_row(&sale, row)) {
        line(buffer, &render.sale(&sale)?)?;
      }
      if buffer.len() >= CHUNK || scanned >= SALES_SCAN {
        return Ok(self.next.is_some());
      }
    }
    self.next = None;
    Ok(false)
  }
}

#[derive(Serialize)]
struct TxSalesJson {
  txid: bitcoin::Txid,
  sales: Vec<sales::SaleJson>,
}

async fn sales_for_tx(
  State(state): State<Arc<ServerState>>,
  Path(txid): Path<String>,
) -> ServerResult<Json<TxSalesJson>> {
  let txid = parse::<bitcoin::Txid>("txid", &txid)?;
  read(&state, move |state, reader| {
    tip(reader)?;
    let stored = reader.sales_for_tx(txid)?;
    if stored.is_empty() {
      return Err(not_found(format!("transaction {txid} sold no tracked sat")));
    }
    let render = state.render(reader);
    Ok(Json(TxSalesJson {
      txid,
      sales: stored
        .iter()
        .map(|sale| render.sale(sale))
        .collect::<anyhow::Result<_>>()?,
    }))
  })
  .await
}

#[derive(Serialize)]
struct BlockLine {
  height: u32,
  hash: BlockHash,
  time: u32,
}

async fn blocks(
  State(state): State<Arc<ServerState>>,
  Query(params): Query<HashMap<String, String>>,
) -> ServerResult<Json<Vec<BlockLine>>> {
  let from = param::<u32>(&params, "from")?;
  let to = param::<u32>(&params, "to")?;
  read(&state, move |_, reader| {
    let (from, to) = block_range(reader, from, to)?;
    Ok(Json(
      reader
        .headers(from, to)?
        .into_iter()
        .map(|(height, header)| BlockLine {
          height,
          hash: header.block_hash(),
          time: header.time,
        })
        .collect(),
    ))
  })
  .await
}

/// Resumable NDJSON production for `ndjson`.
trait Produce: Send + 'static {
  /// Appends lines to `buffer` from `reader` until about a chunk's worth or
  /// a scan budget; `Ok(false)` once nothing is left. Each call gets its own
  /// read transaction.
  fn fill(
    &mut self,
    state: &ServerState,
    reader: &Reader,
    buffer: &mut Vec<u8>,
  ) -> anyhow::Result<bool>;
}

fn line(buffer: &mut Vec<u8>, line: &impl Serialize) -> anyhow::Result<()> {
  serde_json::to_writer(&mut *buffer, line)?;
  buffer.push(b'\n');
  Ok(())
}

/// Counts a stream in `ServerState::streams` while it runs.
struct StreamGuard(Arc<ServerState>);

impl Drop for StreamGuard {
  fn drop(&mut self) {
    self.0.streams.fetch_sub(1, Ordering::SeqCst);
  }
}

/// Streams newline-delimited JSON produced on the blocking pool, one chunk
/// per read transaction. The body ends with an error (the client sees a
/// truncated response) if production fails or the client takes no chunk
/// for `stream_timeout`.
fn ndjson(state: Arc<ServerState>, mut producer: impl Produce) -> Response {
  let (sender, receiver) = mpsc::channel::<Result<Bytes, io::Error>>(8);
  let aborted = Arc::new(AtomicBool::new(false));
  let runtime = tokio::runtime::Handle::current();

  state.streams.fetch_add(1, Ordering::SeqCst);
  let guard = StreamGuard(state.clone());

  tokio::task::spawn_blocking({
    let aborted = aborted.clone();
    move || {
      let _guard = guard;
      let state = &*_guard.0;

      // `false` when the client is gone or stopped reading.
      let send = |bytes: Vec<u8>| -> bool {
        match runtime.block_on(tokio::time::timeout(
          state.stream_timeout,
          sender.send(Ok(Bytes::from(bytes))),
        )) {
          Ok(Ok(())) => true,
          Ok(Err(_)) => false,
          Err(_) => {
            log::warn!(
              "a client took no data for {:?}; aborting its stream",
              state.stream_timeout
            );
            aborted.store(true, Ordering::SeqCst);
            false
          }
        }
      };

      let mut buffer = Vec::with_capacity(CHUNK + 4096);
      loop {
        // A fresh read transaction per chunk, dropped before sending.
        let filled = state
          .store
          .reader()
          .and_then(|reader| producer.fill(state, &reader, &mut buffer));
        match filled {
          Ok(more) => {
            if !buffer.is_empty() && !send(std::mem::take(&mut buffer)) {
              return;
            }
            if !more {
              return;
            }
          }
          Err(err) => {
            log::error!("stream failed: {err:#}");
            // What was produced, a final error line, then the aborted end.
            aborted.store(true, Ordering::SeqCst);
            let _ = line(
              &mut buffer,
              &serde_json::json!({ "error": format!("{err:#}") }),
            );
            send(std::mem::take(&mut buffer));
            return;
          }
        }
      }
    }
  });

  let end = tokio_stream::iter(
    std::iter::once_with(move || aborted.load(Ordering::SeqCst))
      .filter(|aborted| *aborted)
      .map(|_| Err(io::Error::other("stream aborted"))),
  );

  (
    [(header::CONTENT_TYPE, "application/x-ndjson")],
    Body::from_stream(tokio_stream::StreamExt::chain(
      ReceiverStream::new(receiver),
      end,
    )),
  )
    .into_response()
}
