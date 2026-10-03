//! Supply by block luck (docs/API.md "Supply by luck"): the `luck`
//! parameter of `/supply`, `/supply/{row}` and `/supply/{row}/history`,
//! and `/supply/lucks`.
//!
//! The tracker keeps every row's counters per luck (DESIGN.md §5.2); a
//! range of lucks adds them up, but for the holders, which it counts from
//! the published luck masks (exact for any range). `slice` renders it.

use {
  super::{ServerResult, ServerState, bad_request, json::SupplyStatsJson, json::YearsMap},
  crate::model::{LUCK_SLOTS, LuckMask, RowLucks, SupplySnapshot, luck_row},
  serde::Serialize,
  std::collections::{BTreeMap, HashMap},
};

/// The highest luck asked for: the last luck slot, which also counts the
/// (so far never seen) luckier blocks.
pub const MAX_LUCK: u32 = LUCK_SLOTS as u32 - 1;

/// A range of lucks asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lucks {
  pub min: u32,
  pub max: u32,
  /// Luck slots `min..=max`.
  pub list: Vec<usize>,
  pub mask: LuckMask,
}

impl Lucks {
  /// Every luck: the row itself.
  pub fn all(&self) -> bool {
    self.min == 0 && self.max == MAX_LUCK
  }
}

/// `luck=N` (N or more) or `luck=N-M` (N to M), each from 0 to
/// `MAX_LUCK`; `None` when absent.
pub fn parse_lucks(params: &HashMap<String, String>) -> ServerResult<Option<Lucks>> {
  let Some(text) = params.get("luck").filter(|text| !text.is_empty()) else {
    return Ok(None);
  };
  let invalid = || {
    bad_request(format!(
      "invalid luck `{text}`: N (N or more) or N-M, from 0 to {MAX_LUCK}"
    ))
  };
  let number = |part: &str| -> ServerResult<u32> {
    if part.is_empty() || part.len() > 3 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
      return Err(invalid());
    }
    let luck: u32 = part.parse().map_err(|_| invalid())?;
    if luck > MAX_LUCK {
      return Err(bad_request(format!(
        "luck {luck}: lucks are counted up to {MAX_LUCK} ({MAX_LUCK} or more)"
      )));
    }
    Ok(luck)
  };
  let (min, max) = match text.split_once('-') {
    None => (number(text)?, MAX_LUCK),
    Some((min, max)) => (number(min)?, number(max)?),
  };
  if min > max {
    return Err(bad_request(format!(
      "invalid luck `{text}`: {min} is above {max}"
    )));
  }
  let list: Vec<usize> = (min as usize..=max as usize).collect();
  let mask = list.iter().fold(0, |mask, &luck| mask | (1 << luck));
  Ok(Some(Lucks {
    min,
    max,
    list,
    mask,
  }))
}

/// A row restricted to `lucks`.
pub fn subset<'a>(rows: usize, row: &'a RowLucks, lucks: &Lucks) -> super::slice::Subset<'a> {
  super::slice::Subset {
    row: row.row,
    cells: row
      .cells
      .iter()
      .filter(|(luck, _)| lucks.list.binary_search(luck).is_ok())
      .map(|(_, cell)| cell)
      .collect(),
    ids: lucks
      .list
      .iter()
      .map(|&luck| luck_row(rows, row.row, luck))
      .collect(),
    holders: row.holders(lucks.mask, false),
    extracted_holders: row.holders(lucks.mask, true),
    top_holders: false,
  }
}

/// Blocks of each luck, up to the tip: `[(luck, blocks)]`, lucks with a
/// block only.
pub fn luck_blocks(supply: &SupplySnapshot) -> Vec<LuckBlocksJson> {
  let mut blocks = [0u64; LUCK_SLOTS];
  for &luck in &supply.lucks.heights {
    blocks[usize::from(luck)] += 1;
  }
  blocks
    .iter()
    .enumerate()
    .filter(|(_, blocks)| **blocks > 0)
    .map(|(luck, &blocks)| LuckBlocksJson {
      luck: luck as u32,
      blocks,
    })
    .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LuckBlocksJson {
  pub luck: u32,
  pub blocks: u64,
}

/// `GET /supply/lucks`: every row's cells.
#[derive(Serialize)]
pub struct SupplyLucksJson {
  pub height: Option<u32>,
  pub hash: Option<bitcoin::BlockHash>,
  pub time: Option<u32>,
  pub lucks: Vec<LuckBlocksJson>,
  pub rows: Vec<RowLucksJson>,
}

#[derive(Serialize)]
pub struct RowLucksJson {
  pub id: String,
  /// By luck (`"12"`), the lucks the row has sats of only.
  pub lucks: BTreeMap<String, LuckCellJson>,
}

#[derive(Serialize)]
pub struct LuckCellJson {
  pub total: u64,
  pub stats: SupplyStatsJson,
  pub active_non_extracted: YearsMap,
}

pub fn supply_lucks(
  state: &ServerState,
  supply: &SupplySnapshot,
  inscribed: &[u64],
) -> SupplyLucksJson {
  let rows = state.catalog.rows();
  SupplyLucksJson {
    height: supply.height,
    hash: supply.hash,
    time: supply.time,
    lucks: luck_blocks(supply),
    rows: supply
      .lucks
      .rows
      .iter()
      .filter_map(|row| {
        let info = rows.get(usize::from(row.row))?;
        Some(RowLucksJson {
          id: info.id.clone(),
          lucks: row
            .cells
            .iter()
            .map(|(luck, cell)| {
              (
                luck.to_string(),
                LuckCellJson {
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
  }
}
