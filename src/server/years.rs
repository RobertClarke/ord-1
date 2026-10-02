//! Supply by mint year (docs/API.md "Supply by mint year"): the `years`
//! parameter of `/supply`, `/supply/{row}` and `/supply/{row}/history`,
//! and `/supply/years`.
//!
//! The tracker keeps every row's counters per year (DESIGN.md §5.1); a set
//! of years adds them up, but for the holders, which it counts from the
//! published year masks (exact for any set).

use {
  super::{
    ServerError, ServerResult, ServerState, bad_request,
    json::{DeltaJson, RowSupplyJson, SupplyStatsJson, YearsMap},
  },
  crate::{
    model::{
      FIRST_YEAR, RowStats, RowSupply, RowYears, SupplySnapshot, YearMask, YearSupply,
      tracked_row_ids, year_row,
    },
    sats::math::height_of,
    store::Reader,
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

/// The cells of `years` a row has.
fn cells<'a>(row: &'a RowYears, years: &'a Years) -> impl Iterator<Item = &'a RowSupply> {
  row
    .cells
    .iter()
    .filter(|(year, _)| years.list.binary_search(year).is_ok())
    .map(|(_, cell)| cell)
}

/// Every counter added up, but the holders.
fn add(sum: &mut RowStats, stats: &RowStats) {
  sum.mined += stats.mined;
  sum.located += stats.located;
  sum.burned += stats.burned;
  sum.lost += stats.lost;
  sum.overwritten += stats.overwritten;
  sum.extracted += stats.extracted;
}

/// A row's stats and active windows over `years`.
pub fn year_stats(row: &RowYears, years: &Years) -> (RowStats, Vec<(u32, u64)>) {
  let mut stats = RowStats::default();
  let mut active: Vec<(u32, u64)> = crate::model::ACTIVE_YEARS
    .iter()
    .map(|&window| (window, 0))
    .collect();
  for cell in cells(row, years) {
    add(&mut stats, &cell.stats);
    for ((_, total), (_, n)) in active.iter_mut().zip(&cell.active_non_extracted) {
      *total += n;
    }
  }
  stats.holders = row.holders(years.mask, false);
  stats.extracted_holders = row.holders(years.mask, true);
  (stats, active)
}

/// Located inscribed sats per tracker row id (catalog rows, then their
/// year rows), as `ServerState::inscribed_rows` counts them.
pub fn inscribed_by_year(state: &ServerState, starts: &[u32], sat: u64, counts: &mut [u64]) {
  let rows = state.catalog.rows().len();
  let year = year_of_sat(starts, sat).min(crate::model::YEAR_SLOTS - 1);
  for row in state.catalog.sat_rows(sat) {
    if usize::from(row) < rows {
      counts[usize::from(row)] += 1;
      counts[usize::from(year_row(rows, row, year))] += 1;
    }
  }
}

/// Room for every tracker row id.
pub fn inscribed_len(state: &ServerState) -> usize {
  tracked_row_ids(state.catalog.rows().len())
}

/// One row's `RowSupplyJson` over `years`: top holders for a single year
/// only (`None` for a set).
pub fn row_supply(
  state: &ServerState,
  reader: &Reader,
  row: &RowYears,
  years: &Years,
  height: u32,
  top: Option<usize>,
  inscribed: &[u64],
) -> ServerResult<RowSupplyJson> {
  let render = state.render(reader);
  let rows = state.catalog.rows().len();
  let (stats, active) = year_stats(row, years);
  let inscribed = years
    .list
    .iter()
    .map(|&year| {
      inscribed
        .get(usize::from(year_row(rows, row.row, year)))
        .copied()
        .unwrap_or_default()
    })
    .sum();

  // The tip block's change: the years it changed, plus the others as
  // they are.
  let mut changed = false;
  let (mut before, mut after) = (RowStats::default(), RowStats::default());
  for cell in cells(row, years) {
    let (old, new) = match reader.row_delta(cell.row, height)? {
      Some(delta) => {
        changed = true;
        delta
      }
      None => (cell.stats, cell.stats),
    };
    add(&mut before, &old);
    add(&mut after, &new);
    if years.single() {
      before.holders = old.holders;
      before.extracted_holders = old.extracted_holders;
      after.holders = new.holders;
      after.extracted_holders = new.extracted_holders;
    }
  }
  let delta = changed
    .then_some((before, after))
    .map(|(mut before, mut after)| {
      if !years.single() {
        // The holders of a set of years before the block are not known:
        // reported unchanged.
        before.holders = stats.holders;
        before.extracted_holders = stats.extracted_holders;
        after.holders = stats.holders;
        after.extracted_holders = stats.extracted_holders;
      }
      DeltaJson {
        height,
        before,
        after,
      }
    });

  let single = years.single().then(|| cells(row, years).next()).flatten();
  let holders = |list: Option<&Vec<crate::model::Holder>>, top: usize| -> ServerResult<_> {
    Ok(
      list
        .map(|list| {
          list
            .iter()
            .take(top)
            .map(|holder| render.holder(holder))
            .collect::<anyhow::Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default(),
    )
  };
  let (top_extracted_holders, top_non_extracted_holders) = match top {
    Some(top) if years.single() => (
      Some(holders(
        single.map(|cell| &cell.top_extracted_holders),
        top,
      )?),
      Some(holders(
        single.map(|cell| &cell.top_non_extracted_holders),
        top,
      )?),
    ),
    _ => (None, None),
  };

  Ok(RowSupplyJson {
    id: render.row_name(row.row),
    total: stats.mined,
    stats: SupplyStatsJson { stats, inscribed },
    active_non_extracted: YearsMap(active),
    delta,
    top_extracted_holders,
    top_non_extracted_holders,
  })
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
