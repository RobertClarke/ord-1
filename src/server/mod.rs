//! Read-only JSON HTTP API (docs/API.md).
//!
//! OWNER: store+server+indexer agent.
//!
//! Handlers read through one redb read transaction each (on the blocking
//! pool) and the supply snapshot the indexer publishes after every commit.
//! `/moves` and `/sats` stream NDJSON from a blocking task.

pub mod catalog;
pub mod json;
#[cfg(test)]
mod tests;

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
  std::{collections::HashMap, future::Future, io, str::FromStr, sync::Arc},
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

pub struct ServerState {
  pub store: Arc<Store>,
  pub catalog: Arc<dyn Catalog>,
  pub published: Arc<Published>,
  pub chain: Chain,
  pub config: TrackerConfig,
  pub moves_retention: MovesRetention,
  /// Unix time the process started.
  pub started: u64,
}

#[derive(Debug)]
pub enum ServerError {
  BadRequest(String),
  NotFound(String),
  NotReady,
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
    .route("/supply/{row}", get(supply_row))
    .route("/supply/{row}/history", get(supply_history))
    .route("/sat/{sat}", get(sat))
    .route("/output/{outpoint}", get(output))
    .route("/address/{address}", get(address))
    .route("/script/{script}", get(script))
    .route("/block/{height}", get(block))
    .route("/block/{height}/moves", get(block_moves))
    .route("/moves", get(moves))
    .route("/sats", get(sats))
    .route("/blocks", get(blocks))
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
  timings: Timings,
  error: Option<String>,
}

async fn status(State(state): State<Arc<ServerState>>) -> ServerResult<Json<StatusJson>> {
  read(&state, |state, reader| {
    let status = state.published.status();
    let commit = env!("GIT_COMMIT").trim();
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
      timings: status.timings,
      error: status.error,
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

fn row_supply(
  state: &ServerState,
  reader: &Reader,
  supply: &crate::model::RowSupply,
  height: u32,
  top: Option<usize>,
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
    stats: supply.stats,
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

async fn supply(State(state): State<Arc<ServerState>>) -> ServerResult<Json<SupplyJson>> {
  read(&state, |state, reader| {
    let supply = state.published.supply();
    let height = supply.height.ok_or(ServerError::NotReady)?;
    Ok(Json(SupplyJson {
      height: supply.height,
      hash: supply.hash,
      time: supply.time,
      rows: supply
        .rows
        .iter()
        .map(|row| row_supply(state, reader, row, height, None))
        .collect::<ServerResult<_>>()?,
    }))
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
    )?))
  })
  .await
}

#[derive(Serialize)]
struct HistoryPoint {
  height: u32,
  time: Option<u32>,
  stats: RowStats,
}

#[derive(Serialize)]
struct HistoryJson {
  row: String,
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
      let end = (from + (point + 1) * every - 1).min(to);
      let stats = reader
        .row_stats_at(row, end)?
        .map(|(_, stats)| stats)
        .unwrap_or_default();
      result.push(HistoryPoint {
        height: end,
        time: render.time(end)?,
        stats,
      });
    }
    Ok(Json(HistoryJson {
      row: info.id.clone(),
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
) -> ServerResult<Json<json::OutputJson>> {
  let outpoint = parse::<OutPoint>("outpoint", &outpoint)?;
  read(&state, move |state, reader| {
    tip(reader)?;
    let utxo = reader
      .utxo(outpoint)?
      .ok_or_else(|| not_found(format!("output {outpoint} holds no tracked sat")))?;
    Ok(Json(state.render(reader).output(outpoint, &utxo)?))
  })
  .await
}

#[derive(Serialize)]
struct ScriptJson {
  address: Option<String>,
  script_pubkey: String,
  outputs: Vec<json::OutputJson>,
  rows: RowMap,
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

  Ok(ndjson(state, move |state, reader, emit| {
    let render = state.render(reader);
    for height in reader.heights_with_moves(from, to)? {
      let Some(moves) = reader.moves(height)? else {
        continue;
      };
      let time = render.time(height)?;
      for m in &moves {
        if let Some(row) = row
          && render
            .move_counts(m)
            .get(usize::from(row))
            .is_none_or(|count| *count == 0)
        {
          continue;
        }
        let m = render.move_json(m, time)?;
        if !emit(&MoveLine {
          height,
          time,
          m: &m,
        })? {
          return Ok(());
        }
      }
    }
    Ok(())
  }))
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

  let row = read(&state, move |state, reader| {
    tip(reader)?;
    Ok(state.row(&id)?.0)
  })
  .await?;

  Ok(ndjson(state, move |state, reader, emit| {
    let render = state.render(reader);
    // Consecutive pieces often share an output; keyed by kind too, as a
    // BIP30-overwritten outpoint holds an old and a current output.
    let mut outputs: HashMap<(OutPoint, bool), json::OutputJson> = HashMap::new();
    for location in reader.pieces(from, to)? {
      let mut location = location?;
      // Only the sats in [from, to).
      let piece = &mut location.piece;
      if piece.start < from {
        let skip = from - piece.start;
        piece.start += skip;
        piece.offset += skip;
        piece.len -= skip;
      }
      if piece.end() > to {
        piece.len = to - piece.start;
      }
      location.offset = piece.offset;
      let count = render
        .catalog
        .row_counts(piece.start, piece.len)
        .into_iter()
        .find(|(r, _)| *r == row)
        .map_or(0, |(_, count)| count);
      if count == 0 {
        continue;
      }
      let key = (
        location.outpoint,
        location.kind == LocationKind::Overwritten,
      );
      let output = match outputs.get(&key) {
        Some(output) => Some(output.clone()),
        None => {
          let output = render.location_output(&location, with_pieces)?;
          if let Some(output) = &output {
            if outputs.len() >= 10_000 {
              outputs.clear();
            }
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
      if !emit(&PieceLine {
        piece: render.piece(&location.piece),
        count,
        location: &location_json,
      })? {
        return Ok(());
      }
    }
    Ok(())
  }))
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

/// Streams newline-delimited JSON produced on the blocking pool. `produce`
/// calls `emit` per line; `emit` returns `false` once the client is gone.
fn ndjson(
  state: Arc<ServerState>,
  produce: impl FnOnce(
    &ServerState,
    &Reader,
    &mut dyn FnMut(&dyn erased::Line) -> anyhow::Result<bool>,
  ) -> anyhow::Result<()>
  + Send
  + 'static,
) -> Response {
  const CHUNK: usize = 64 << 10;

  let (sender, receiver) = mpsc::channel::<Result<Bytes, io::Error>>(8);

  tokio::task::spawn_blocking(move || {
    let mut buffer = Vec::with_capacity(CHUNK + 1024);
    let mut open = true;
    let result = (|| -> anyhow::Result<()> {
      let reader = state.store.reader()?;
      produce(&state, &reader, &mut |line| {
        line.write(&mut buffer)?;
        buffer.push(b'\n');
        if buffer.len() >= CHUNK {
          open = sender
            .blocking_send(Ok(Bytes::from(std::mem::take(&mut buffer))))
            .is_ok();
          buffer.reserve(CHUNK + 1024);
        }
        Ok(open)
      })
    })();
    if !buffer.is_empty() && open {
      open = sender.blocking_send(Ok(Bytes::from(buffer))).is_ok();
    }
    if let Err(err) = result {
      log::error!("stream failed: {err:#}");
      if open {
        // A final error line, then an aborted body so clients notice.
        let line = serde_json::json!({ "error": format!("{err:#}") });
        let _ = sender.blocking_send(Ok(Bytes::from(format!("{line}\n"))));
        let _ = sender.blocking_send(Err(io::Error::other(format!("{err:#}"))));
      }
    }
  });

  (
    [(header::CONTENT_TYPE, "application/x-ndjson")],
    Body::from_stream(ReceiverStream::new(receiver)),
  )
    .into_response()
}

mod erased {
  /// A serializable NDJSON line (object-safe).
  pub trait Line {
    fn write(&self, buffer: &mut Vec<u8>) -> anyhow::Result<()>;
  }

  impl<T: serde::Serialize> Line for T {
    fn write(&self, buffer: &mut Vec<u8>) -> anyhow::Result<()> {
      serde_json::to_writer(buffer, self)?;
      Ok(())
    }
  }
}
