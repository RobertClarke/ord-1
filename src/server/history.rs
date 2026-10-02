//! `GET /address/{address}/history` and `GET /script/{hex}/history`
//! (docs/API.md "Address history").

use {
  super::{
    ServerError, ServerResult, ServerState, bad_request, json::RowMap, param, sales::SaleJson, tip,
  },
  crate::{
    history::{HistoryEntry, HistoryKind, history_key},
    model::RowId,
    store::Reader,
  },
  bitcoin::{ScriptBuf, Txid},
  serde::Serialize,
  std::collections::HashMap,
};

/// Entries per page by default and at most.
pub const DEFAULT_HISTORY_LIMIT: usize = 100;
pub const MAX_HISTORY_LIMIT: usize = 1000;
/// Entries one request reads at most (filtered pages may come back short,
/// with a `next` cursor).
pub const HISTORY_SCAN: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LeadSatJson {
  pub sat: u64,
  pub types: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryEntryJson {
  pub txid: Txid,
  pub height: u32,
  pub time: Option<u32>,
  pub tx_index: u32,
  pub direction: &'static str,
  pub kind: &'static str,
  pub rows_in: RowMap,
  pub rows_out: RowMap,
  pub sats_in: u64,
  pub sats_out: u64,
  pub value_in: u64,
  pub value_out: u64,
  pub outputs_in: u32,
  pub inputs_out: u32,
  pub extracted_in: bool,
  pub extracted_out: bool,
  pub extracted_sats: u64,
  pub isolated: u64,
  pub burned: u64,
  pub roles: Vec<&'static str>,
  pub counterparty: Option<String>,
  pub counterparty_script_pubkey: Option<String>,
  pub counterparties: u32,
  pub lead_sat: Option<LeadSatJson>,
  pub sales: Vec<SaleJson>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryJson {
  pub address: Option<String>,
  pub script_pubkey: String,
  /// The first height the history covers (`null`: none indexed yet).
  pub history_from: Option<u32>,
  pub entries: Vec<HistoryEntryJson>,
  pub next: Option<String>,
}

/// The query of a history request, validated.
pub struct HistoryQuery {
  limit: usize,
  before: Option<(u32, u32)>,
  kinds: Option<Vec<HistoryKind>>,
  row: Option<String>,
  from: Option<u32>,
  to: Option<u32>,
}

/// A cursor: `height:tx_index` of the last entry returned (exclusive).
fn parse_cursor(value: &str) -> ServerResult<(u32, u32)> {
  let invalid = || {
    bad_request(format!(
      "invalid before `{value}`: expected a `next` cursor"
    ))
  };
  let (height, tx_index) = value.split_once(':').ok_or_else(invalid)?;
  Ok((
    height.parse().map_err(|_| invalid())?,
    tx_index.parse().map_err(|_| invalid())?,
  ))
}

fn cursor(entry: &HistoryEntry) -> String {
  format!("{}:{}", entry.height, entry.tx_index)
}

impl HistoryQuery {
  pub fn parse(params: &HashMap<String, String>) -> ServerResult<Self> {
    let limit = param::<usize>(params, "limit")?.unwrap_or(DEFAULT_HISTORY_LIMIT);
    if limit == 0 || limit > MAX_HISTORY_LIMIT {
      return Err(bad_request(format!(
        "limit must be between 1 and {MAX_HISTORY_LIMIT}"
      )));
    }
    let before = params
      .get("before")
      .filter(|value| !value.is_empty())
      .map(|value| parse_cursor(value))
      .transpose()?;
    let kinds = params
      .get("kinds")
      .filter(|value| !value.is_empty())
      .map(|value| {
        value
          .split(',')
          .map(|name| {
            HistoryKind::from_name(name.trim()).ok_or_else(|| {
              bad_request(format!(
                "unknown kind `{name}`; expected {}",
                HistoryKind::ALL.map(HistoryKind::name).join(", ")
              ))
            })
          })
          .collect::<ServerResult<Vec<HistoryKind>>>()
      })
      .transpose()?;
    let from = param::<u32>(params, "from")?;
    let to = param::<u32>(params, "to")?;
    if let (Some(from), Some(to)) = (from, to)
      && from > to
    {
      return Err(bad_request(format!("from {from} is above to {to}")));
    }
    Ok(Self {
      limit,
      before,
      kinds,
      row: params.get("row").filter(|row| !row.is_empty()).cloned(),
      from,
      to,
    })
  }
}

pub fn script_history(
  state: &ServerState,
  reader: &Reader,
  script: ScriptBuf,
  query: HistoryQuery,
) -> ServerResult<HistoryJson> {
  let tip = tip(reader)?;
  if reader.history_building()? {
    return Err(ServerError::NotReadyBecause(
      "the address history is being built (initial sync in progress)".into(),
    ));
  }
  let row: Option<RowId> = query
    .row
    .as_deref()
    .map(|id| state.row(id).map(|(row, _)| row))
    .transpose()?;
  let render = state.render(reader);
  let key = history_key(&script);
  let matches = |entry: &HistoryEntry| {
    query
      .kinds
      .as_ref()
      .is_none_or(|kinds| kinds.contains(&entry.kind))
      && row.is_none_or(|row| {
        entry
          .received
          .rows
          .iter()
          .chain(&entry.sent.rows)
          .any(|(r, _)| *r == row)
      })
  };

  let mut entries = Vec::new();
  let mut next = None;
  let mut scanned = 0;
  let mut iter = reader.history(
    key,
    query.from.unwrap_or(0),
    query.to.unwrap_or(tip),
    query.before,
  )?;
  while let Some(entry) = iter.next() {
    let entry = entry?;
    let position = cursor(&entry);
    scanned += 1;
    if matches(&entry) {
      entries.push(entry);
      if entries.len() == query.limit {
        if iter.next().is_some() {
          next = entries.last().map(cursor);
        }
        break;
      }
    }
    if scanned >= HISTORY_SCAN {
      if iter.next().is_some() {
        next = Some(position);
      }
      break;
    }
  }

  let mut json = Vec::with_capacity(entries.len());
  for entry in &entries {
    let sales = if entry.seller || entry.buyer {
      reader
        .sales_for_tx(entry.txid)?
        .iter()
        .filter(|sale| {
          sale.sale.seller.script_pubkey == script
            || sale.sale.buyer_script.as_ref() == Some(&script)
        })
        .map(|sale| render.sale(sale))
        .collect::<anyhow::Result<Vec<SaleJson>>>()?
    } else {
      Vec::new()
    };
    let mut roles = Vec::new();
    if entry.seller {
      roles.push("seller");
    }
    if entry.buyer {
      roles.push("buyer");
    }
    let row_map = |rows: &[(RowId, u64)]| {
      let mut counts = Vec::new();
      render.add(&mut counts, &rows.to_vec());
      render.row_map(&counts)
    };
    json.push(HistoryEntryJson {
      txid: entry.txid,
      height: entry.height,
      time: render.time(entry.height)?,
      tx_index: entry.tx_index,
      direction: entry.direction.name(),
      kind: entry.kind.name(),
      rows_in: row_map(&entry.received.rows),
      rows_out: row_map(&entry.sent.rows),
      sats_in: entry.received.sats,
      sats_out: entry.sent.sats,
      value_in: entry.received.value,
      value_out: entry.sent.value,
      outputs_in: entry.received.count,
      inputs_out: entry.sent.count,
      extracted_in: entry.extracted_in,
      extracted_out: entry.extracted_out,
      extracted_sats: entry.extracted_sats,
      isolated: entry.isolated,
      burned: entry.burned,
      roles,
      counterparty: entry
        .counterparty
        .as_ref()
        .and_then(|script| render.address(script)),
      counterparty_script_pubkey: entry
        .counterparty
        .as_ref()
        .map(|script| script.to_hex_string()),
      counterparties: entry.counterparties,
      lead_sat: entry.lead_sat.map(|sat| LeadSatJson {
        sat,
        types: render.catalog.types(sat),
      }),
      sales,
    });
  }

  Ok(HistoryJson {
    address: render.address(&script),
    script_pubkey: script.to_hex_string(),
    history_from: reader.history_from()?,
    entries: json,
    next,
  })
}
