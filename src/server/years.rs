//! Supply by mint year (docs/API.md "Supply by mint year"): the `years`
//! parameter of `/supply`, `/supply/{row}` and `/supply/{row}/history`,
//! and `/supply/years`.
//!
//! The tracker keeps every row's counters per year (DESIGN.md §5.1); a set
//! of years adds them up, but for the holders, which it counts from the
//! published year masks (exact for any set). `slice` renders it.

use {
  super::{
    ServerError, ServerResult, ServerState, bad_request,
    json::{SupplyStatsJson, YearsMap},
    slice::Subset,
  },
  crate::{
    model::{
      FIRST_YEAR, RowYears, SupplySnapshot, YearMask, YearSupply, luck_row, month_row,
      tracked_row_ids, year_row,
    },
    sats::math::height_of,
  },
  serde::Serialize,
  std::collections::{BTreeMap, HashMap},
};

/// Years per request at most.
pub const MAX_YEARS: usize = 64;

/// One mint year's blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct YearSpanJson {
  pub year: u16,
  pub from_height: u32,
  /// The tip for the tip's year.
  pub to_height: u32,
}

/// The years blocks were mined in, up to the tip (a year without a block
/// is left out).
pub fn year_spans(years: &YearSupply, tip: Option<u32>) -> Vec<YearSpanJson> {
  let Some(tip) = tip else {
    return Vec::new();
  };
  let starts = &years.starts;
  starts
    .iter()
    .enumerate()
    .filter_map(|(index, &from)| {
      let to = match starts.get(index + 1) {
        Some(&next) => next.checked_sub(1)?,
        None => tip,
      };
      (from <= to).then(|| YearSpanJson {
        year: FIRST_YEAR + index as u16,
        from_height: from,
        to_height: to,
      })
    })
    .collect()
}

/// The year index of a sat, by the year starts.
pub fn year_of_sat(starts: &[u32], sat: u64) -> usize {
  let height = height_of(sat);
  starts
    .partition_point(|&start| u64::from(start) <= height)
    .saturating_sub(1)
}

/// A set of mint years asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Years {
  /// Year indexes (0 for `FIRST_YEAR`), ascending.
  pub list: Vec<usize>,
  pub mask: YearMask,
}

impl Years {
  pub fn single(&self) -> bool {
    self.list.len() == 1
  }
}

/// `years=2009,2010` (or `year=`), each a year blocks were mined in up to
/// the tip; `None` when absent.
pub fn parse_years(
  params: &HashMap<String, String>,
  supply: &SupplySnapshot,
) -> ServerResult<Option<Years>> {
  let Some(text) = params
    .get("years")
    .or_else(|| params.get("year"))
    .filter(|text| !text.is_empty())
  else {
    return Ok(None);
  };
  let known = year_spans(&supply.years, supply.height);
  let mut list = Vec::new();
  for part in text.split(',') {
    if part.len() != 4 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
      return Err(bad_request(format!(
        "invalid years `{text}`: comma-separated years (YYYY)"
      )));
    }
    let year: u16 = part.parse().unwrap();
    if !known.iter().any(|span| span.year == year) {
      return Err(bad_request(match (known.first(), known.last()) {
        (Some(first), Some(last)) => format!(
          "no block was mined in {year}: years from {} to {}",
          first.year, last.year
        ),
        _ => "no block is indexed yet".into(),
      }));
    }
    list.push(usize::from(year - FIRST_YEAR));
  }
  list.sort_unstable();
  list.dedup();
  if list.len() > MAX_YEARS {
    return Err(bad_request(format!("at most {MAX_YEARS} years")));
  }
  let mask = list.iter().fold(0, |mask, &year| mask | (1 << year));
  Ok(Some(Years { list, mask }))
}

/// A row restricted to `years`.
pub fn subset<'a>(rows: usize, row: &'a RowYears, years: &Years) -> Subset<'a> {
  Subset {
    row: row.row,
    cells: row
      .cells
      .iter()
      .filter(|(year, _)| years.list.binary_search(year).is_ok())
      .map(|(_, cell)| cell)
      .collect(),
    ids: years
      .list
      .iter()
      .map(|&year| year_row(rows, row.row, year))
      .collect(),
    holders: row.holders(years.mask, false),
    extracted_holders: row.holders(years.mask, true),
    top_holders: true,
  }
}

/// Located inscribed sats per tracker row id (catalog rows, then their
/// year, luck and month rows), as `ServerState::inscribed_rows` counts them.
pub fn inscribed_by_row(
  state: &ServerState,
  supply: &SupplySnapshot,
  sat: u64,
  counts: &mut [u64],
) {
  let rows = state.catalog.rows().len();
  let year = year_of_sat(&supply.years.starts, sat).min(crate::model::YEAR_SLOTS - 1);
  let luck = usize::try_from(height_of(sat))
    .ok()
    .and_then(|height| supply.lucks.heights.get(height))
    .map_or(0, |&luck| usize::from(luck));
  let month = super::months::month_of_sat(&supply.months.starts, sat);
  for row in state.catalog.sat_rows(sat) {
    if usize::from(row) < rows {
      counts[usize::from(row)] += 1;
      counts[usize::from(year_row(rows, row, year))] += 1;
      counts[usize::from(luck_row(rows, row, luck))] += 1;
      counts[usize::from(month_row(rows, row, month))] += 1;
    }
  }
}

/// Room for every tracker row id.
pub fn inscribed_len(state: &ServerState) -> usize {
  tracked_row_ids(state.catalog.rows().len())
}

/// `GET /supply/years`: every row's cells.
#[derive(Serialize)]
pub struct SupplyYearsJson {
  pub height: Option<u32>,
  pub hash: Option<bitcoin::BlockHash>,
  pub time: Option<u32>,
  pub years: Vec<YearSpanJson>,
  pub rows: Vec<RowYearsJson>,
}

#[derive(Serialize)]
pub struct RowYearsJson {
  pub id: String,
  /// By year (`"2009"`), the years the row has sats of only.
  pub years: BTreeMap<String, YearCellJson>,
}

#[derive(Serialize)]
pub struct YearCellJson {
  pub total: u64,
  pub stats: SupplyStatsJson,
  pub active_non_extracted: YearsMap,
}

pub fn supply_years(
  state: &ServerState,
  supply: &SupplySnapshot,
  inscribed: &[u64],
) -> ServerResult<SupplyYearsJson> {
  let rows = state.catalog.rows();
  Ok(SupplyYearsJson {
    height: supply.height,
    hash: supply.hash,
    time: supply.time,
    years: year_spans(&supply.years, supply.height),
    rows: supply
      .years
      .rows
      .iter()
      .filter_map(|row| {
        let info = rows.get(usize::from(row.row))?;
        Some(RowYearsJson {
          id: info.id.clone(),
          years: row
            .cells
            .iter()
            .map(|(year, cell)| {
              (
                (usize::from(FIRST_YEAR) + year).to_string(),
                YearCellJson {
                  total: cell.stats.mined,
                  stats: SupplyStatsJson {
                    stats: cell.stats,
                    inscribed: inscribed
                      .get(usize::from(cell.row))
                      .copied()
                      .unwrap_or_default(),
                  },
                  active_non_extracted: YearsMap(cell.active_non_extracted.clone()),
                },
              )
            })
            .collect(),
        })
      })
      .collect(),
  })
}

/// The supply's year data, or 503 before the first commit.
pub fn ready(supply: &SupplySnapshot) -> ServerResult<u32> {
  supply.height.ok_or(ServerError::NotReady)
}
