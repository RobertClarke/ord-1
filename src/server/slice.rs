//! A row's supply restricted to some of its sats: those mined in a set of
//! years (`years=`, DESIGN.md §5.1), in blocks of a set of lucks (`luck=`,
//! §5.2) or in one month (`months=`, §5.3). The tracker keeps a row's
//! counters per year, per luck and per month; a slice adds the selected
//! cells up, and counts the holders from the published holder masks (exact
//! for any set), or a month's own.

use {
  super::{
    ServerResult, ServerState, bad_request,
    json::{DeltaJson, RowSupplyJson, SupplyStatsJson, YearsMap},
    lucks::{self, Lucks},
    months,
    years::{self, Years},
  },
  crate::{
    model::{FIRST_YEAR, Holder, RowId, RowStats, RowSupply, SupplySnapshot},
    store::Reader,
  },
  std::collections::HashMap,
};

/// The sats of each row asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slice {
  Years(Years),
  Lucks(Lucks),
  /// One month index.
  Month(usize),
}

/// `years=`, `luck=` or `months=`, `None` when none is given; two at once
/// are a 400 (the tracker does not cross them).
pub fn parse_slice(
  params: &HashMap<String, String>,
  supply: &SupplySnapshot,
) -> ServerResult<Option<Slice>> {
  let years = years::parse_years(params, supply)?;
  let lucks = lucks::parse_lucks(params)?;
  let month = months::parse_month(params, supply)?;
  match (years, lucks, month) {
    (Some(years), None, None) => Ok(Some(Slice::Years(years))),
    (None, Some(lucks), None) => Ok(Some(Slice::Lucks(lucks))),
    (None, None, Some(month)) => Ok(Some(Slice::Month(month))),
    (None, None, None) => Ok(None),
    _ => Err(bad_request(
      "years, luck and months cannot be combined: the supply is broken down by one of them",
    )),
  }
}

impl Slice {
  /// The slice of catalog row `row` (`None` for an unknown row).
  pub fn subset<'a>(&self, supply: &'a SupplySnapshot, row: RowId) -> Option<Subset<'a>> {
    let rows = supply.rows.len();
    match self {
      Self::Years(years) => {
        let row = supply.years.rows.iter().find(|entry| entry.row == row)?;
        Some(years::subset(rows, row, years))
      }
      Self::Lucks(lucks) => {
        let row = supply.lucks.rows.iter().find(|entry| entry.row == row)?;
        Some(lucks::subset(rows, row, lucks))
      }
      Self::Month(month) => {
        let row = supply.months.rows.iter().find(|entry| entry.row == row)?;
        Some(months::subset(rows, row, *month))
      }
    }
  }

  /// Every catalog row's slice, in `RowId` order.
  pub fn subsets<'a>(&self, supply: &'a SupplySnapshot) -> Vec<Subset<'a>> {
    let rows = supply.rows.len();
    match self {
      Self::Years(years) => supply
        .years
        .rows
        .iter()
        .map(|row| years::subset(rows, row, years))
        .collect(),
      Self::Lucks(lucks) => supply
        .lucks
        .rows
        .iter()
        .map(|row| lucks::subset(rows, row, lucks))
        .collect(),
      Self::Month(month) => supply
        .months
        .rows
        .iter()
        .map(|row| months::subset(rows, row, *month))
        .collect(),
    }
  }

  /// The tracker row ids a history point adds up for catalog row `row`,
  /// and whether its holder counts are exact (one cell, or every luck: the
  /// row itself).
  pub fn history_ids(&self, rows: usize, row: RowId) -> (Vec<RowId>, bool) {
    match self {
      Self::Years(years) => (
        years
          .list
          .iter()
          .map(|&year| crate::model::year_row(rows, row, year))
          .collect(),
        years.single(),
      ),
      Self::Lucks(lucks) if lucks.all() => (vec![row], true),
      Self::Lucks(lucks) => (
        lucks
          .list
          .iter()
          .map(|&luck| crate::model::luck_row(rows, row, luck))
          .collect(),
        lucks.list.len() == 1,
      ),
      Self::Month(month) => (vec![crate::model::month_row(rows, row, *month)], true),
    }
  }

  /// The years (`years=`) of a history response.
  pub fn years(&self) -> Option<Vec<u16>> {
    match self {
      Self::Years(years) => Some(
        years
          .list
          .iter()
          .map(|&year| FIRST_YEAR + year as u16)
          .collect(),
      ),
      Self::Lucks(_) | Self::Month(_) => None,
    }
  }

  /// The lucks (`luck=`) of a history response: `[min, max]`.
  pub fn luck(&self) -> Option<[u32; 2]> {
    match self {
      Self::Lucks(lucks) => Some([lucks.min, lucks.max]),
      Self::Years(_) | Self::Month(_) => None,
    }
  }

  /// The month (`months=`) of a history response: `2017-12`.
  pub fn month(&self) -> Option<String> {
    match self {
      Self::Month(month) => Some(crate::model::month_name(*month)),
      Self::Years(_) | Self::Lucks(_) => None,
    }
  }
}

/// A catalog row restricted to a slice of its sats.
pub struct Subset<'a> {
  pub row: RowId,
  /// The slice's cells the row has (sats mined).
  pub cells: Vec<&'a RowSupply>,
  /// The tracker row ids of every cell of the slice.
  pub ids: Vec<RowId>,
  /// Exact, from the holder masks.
  pub holders: u64,
  pub extracted_holders: u64,
  /// The cells keep top holders (year cells do, luck cells do not).
  pub top_holders: bool,
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

impl Subset<'_> {
  /// The stats and active windows of the slice.
  pub fn stats(&self) -> (RowStats, Vec<(u32, u64)>) {
    let mut stats = RowStats::default();
    let mut active: Vec<(u32, u64)> = crate::model::ACTIVE_YEARS
      .iter()
      .map(|&window| (window, 0))
      .collect();
    for cell in &self.cells {
      add(&mut stats, &cell.stats);
      for ((_, total), (_, n)) in active.iter_mut().zip(&cell.active_non_extracted) {
        *total += n;
      }
    }
    stats.holders = self.holders;
    stats.extracted_holders = self.extracted_holders;
    (stats, active)
  }

  fn single(&self) -> bool {
    self.ids.len() == 1
  }
}

/// One row's `RowSupplyJson` over a slice: top holders for a single year
/// or month only (`None` for a set, or lucks).
pub fn row_supply(
  state: &ServerState,
  reader: &Reader,
  subset: &Subset,
  height: u32,
  top: Option<usize>,
  inscribed: &[u64],
) -> ServerResult<RowSupplyJson> {
  let render = state.render(reader);
  let (stats, active) = subset.stats();
  let inscribed = subset
    .ids
    .iter()
    .map(|&id| inscribed.get(usize::from(id)).copied().unwrap_or_default())
    .sum();

  // The tip block's change: the cells it changed, plus the others as they
  // are.
  let mut changed = false;
  let (mut before, mut after) = (RowStats::default(), RowStats::default());
  for cell in &subset.cells {
    let (old, new) = match reader.row_delta(cell.row, height)? {
      Some(delta) => {
        changed = true;
        delta
      }
      None => (cell.stats, cell.stats),
    };
    add(&mut before, &old);
    add(&mut after, &new);
    if subset.single() {
      before.holders = old.holders;
      before.extracted_holders = old.extracted_holders;
      after.holders = new.holders;
      after.extracted_holders = new.extracted_holders;
    }
  }
  let delta = changed
    .then_some((before, after))
    .map(|(mut before, mut after)| {
      if !subset.single() {
        // The holders of a set of cells before the block are not known:
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

  let single = subset
    .single()
    .then(|| subset.cells.first())
    .flatten()
    .copied();
  let holders = |list: Option<&Vec<Holder>>, top: usize| -> ServerResult<_> {
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
    Some(top) if subset.single() && subset.top_holders => (
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
    id: render.row_name(subset.row),
    total: stats.mined,
    stats: SupplyStatsJson { stats, inscribed },
    active_non_extracted: YearsMap(active),
    delta,
    top_extracted_holders,
    top_non_extracted_holders,
  })
}
