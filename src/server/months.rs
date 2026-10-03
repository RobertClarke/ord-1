//! Supply by mint month (docs/API.md "Supply by mint month"): the `months`
//! parameter of `/supply`, `/supply/{row}` and `/supply/{row}/history`,
//! and `/supply/months`.
//!
//! The tracker keeps every row's counters per month (DESIGN.md §5.3),
//! holders included: one month's are exact, months are not combined. `slice`
//! renders it.

use {
  super::{ServerResult, ServerState, bad_request, slice::Subset},
  crate::{
    model::{MONTH_SLOTS, MonthSupply, RowMonths, RowStats, SupplySnapshot, month_name, month_row},
    sats::math::height_of,
  },
  serde::Serialize,
  std::collections::{BTreeMap, HashMap},
};

/// One mint month's blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MonthSpanJson {
  /// `2017-12`.
  pub month: String,
  pub from_height: u32,
  /// The tip for the tip's month.
  pub to_height: u32,
}

/// The months blocks were mined in, up to the tip (a month without a block
/// is left out), as month indexes: `(month, from, to)`.
pub fn month_ranges(months: &MonthSupply, tip: Option<u32>) -> Vec<(usize, u32, u32)> {
  let Some(tip) = tip else {
    return Vec::new();
  };
  let starts = &months.starts;
  starts
    .iter()
    .enumerate()
    .filter_map(|(index, &from)| {
      let to = match starts.get(index + 1) {
        Some(&next) => next.checked_sub(1)?,
        None => tip,
      };
      (from <= to).then_some((index, from, to))
    })
    .collect()
}

pub fn month_spans(months: &MonthSupply, tip: Option<u32>) -> Vec<MonthSpanJson> {
  month_ranges(months, tip)
    .into_iter()
    .map(|(month, from_height, to_height)| MonthSpanJson {
      month: month_name(month),
      from_height,
      to_height,
    })
    .collect()
}

/// The month index of a sat, by the month starts.
pub fn month_of_sat(starts: &[u32], sat: u64) -> usize {
  let height = height_of(sat);
  starts
    .partition_point(|&start| u64::from(start) <= height)
    .saturating_sub(1)
    .min(MONTH_SLOTS - 1)
}

/// `months=2017-12` (or `month=`): one month blocks were mined in, up to
/// the tip; `None` when absent. Its index.
pub fn parse_month(
  params: &HashMap<String, String>,
  supply: &SupplySnapshot,
) -> ServerResult<Option<usize>> {
  let Some(text) = params
    .get("months")
    .or_else(|| params.get("month"))
    .filter(|text| !text.is_empty())
  else {
    return Ok(None);
  };
  if text.contains(',') {
    return Err(bad_request(format!(
      "invalid months `{text}`: one month at a time (YYYY-MM)"
    )));
  }
  let bytes = text.as_bytes();
  let valid = bytes.len() == 7
    && bytes[4] == b'-'
    && bytes
      .iter()
      .enumerate()
      .all(|(i, byte)| i == 4 || byte.is_ascii_digit());
  let parsed = valid
    .then(|| {
      let year: u16 = text[..4].parse().ok()?;
      let month: u8 = text[5..].parse().ok()?;
      (1..=12).contains(&month).then_some((year, month))
    })
    .flatten();
  let Some((year, month)) = parsed else {
    return Err(bad_request(format!(
      "invalid months `{text}`: a month (YYYY-MM)"
    )));
  };
  let known = month_ranges(&supply.months, supply.height);
  let index = (usize::from(year).checked_sub(usize::from(crate::model::FIRST_YEAR)))
    .map(|years| years * 12 + usize::from(month) - 1);
  match index {
    Some(index) if known.iter().any(|&(known, _, _)| known == index) => Ok(Some(index)),
    _ => Err(bad_request(match (known.first(), known.last()) {
      (Some(first), Some(last)) => format!(
        "no block was mined in {text}: months from {} to {}",
        month_name(first.0),
        month_name(last.0)
      ),
      _ => "no block is indexed yet".into(),
    })),
  }
}

/// A row restricted to one month.
pub fn subset<'a>(rows: usize, row: &'a RowMonths, month: usize) -> Subset<'a> {
  let cells: Vec<_> = row
    .cells
    .iter()
    .filter(|(cell, _)| *cell == month)
    .map(|(_, cell)| cell)
    .collect();
  let stats = cells.first().map(|cell| cell.stats).unwrap_or_default();
  Subset {
    row: row.row,
    cells,
    ids: vec![month_row(rows, row.row, month)],
    holders: stats.holders,
    extracted_holders: stats.extracted_holders,
    top_holders: true,
  }
}

/// `GET /supply/months`: every row's counters per month, compactly.
#[derive(Serialize)]
pub struct SupplyMonthsJson {
  pub height: Option<u32>,
  pub hash: Option<bitcoin::BlockHash>,
  pub time: Option<u32>,
  pub months: Vec<MonthSpanJson>,
  /// The counters of each cell, in order.
  pub fields: [&'static str; 9],
  pub rows: Vec<RowMonthsJson>,
}

#[derive(Serialize)]
pub struct RowMonthsJson {
  pub id: String,
  /// By month (`"2017-12"`), the months the row has sats of only: the
  /// counters named by `fields`.
  pub months: BTreeMap<String, [u64; 9]>,
}

pub const FIELDS: [&str; 9] = [
  "mined",
  "located",
  "burned",
  "lost",
  "overwritten",
  "extracted",
  "holders",
  "extracted_holders",
  "inscribed",
];

fn cell(stats: &RowStats, inscribed: u64) -> [u64; 9] {
  [
    stats.mined,
    stats.located,
    stats.burned,
    stats.lost,
    stats.overwritten,
    stats.extracted,
    stats.holders,
    stats.extracted_holders,
    inscribed,
  ]
}

pub fn supply_months(
  state: &ServerState,
  supply: &SupplySnapshot,
  inscribed: &[u64],
) -> SupplyMonthsJson {
  let rows = state.catalog.rows();
  SupplyMonthsJson {
    height: supply.height,
    hash: supply.hash,
    time: supply.time,
    months: month_spans(&supply.months, supply.height),
    fields: FIELDS,
    rows: supply
      .months
      .rows
      .iter()
      .filter_map(|row| {
        let info = rows.get(usize::from(row.row))?;
        Some(RowMonthsJson {
          id: info.id.clone(),
          months: row
            .cells
            .iter()
            .map(|(month, supply)| {
              (
                month_name(*month),
                cell(
                  &supply.stats,
                  inscribed
                    .get(usize::from(supply.row))
                    .copied()
                    .unwrap_or_default(),
                ),
              )
            })
            .collect(),
        })
      })
      .collect(),
  }
}
