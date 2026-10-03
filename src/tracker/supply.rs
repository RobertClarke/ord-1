//! Supply snapshots: active windows (Fenwick trees over creation-height
//! buckets) and top holders.

use {
  super::*,
  crate::model::{
    ACTIVE_YEARS, Holder, LuckMask, LuckSupply, RowLucks, RowSupply, RowYears, TrackedRow,
    YEAR_TOP_HOLDERS, YearMask, YearSupply, split_row,
  },
  std::cmp::Ordering,
};

/// A Fenwick tree of `u64` counts that grows on demand. Updates wrap, so a
/// subtraction that undoes an addition is exact.
#[derive(Default)]
pub(super) struct Fenwick {
  tree: Vec<u64>,
  total: u64,
}

impl Fenwick {
  fn grow(&mut self, index: usize) {
    let old = self.tree.len();
    if index < old {
      return;
    }
    let new = (index + 1).next_power_of_two().max(64);
    for node in old + 1..=new {
      // Node `node` (1-based) covers (node - lowbit, node]; only its part at
      // or below `old` can be non-zero.
      let low = node - (node & node.wrapping_neg());
      let value = if low < old {
        self.prefix(node.min(old)).wrapping_sub(self.prefix(low))
      } else {
        0
      };
      self.tree.push(value);
    }
  }

  pub fn add(&mut self, index: usize, n: u64) {
    self.grow(index);
    let mut node = index + 1;
    while node <= self.tree.len() {
      self.tree[node - 1] = self.tree[node - 1].wrapping_add(n);
      node += node & node.wrapping_neg();
    }
    self.total = self.total.wrapping_add(n);
  }

  pub fn sub(&mut self, index: usize, n: u64) {
    self.add(index, n.wrapping_neg());
  }

  /// Sum of `[0, index)`.
  pub fn prefix(&self, index: usize) -> u64 {
    let mut node = index.min(self.tree.len());
    let mut sum = 0u64;
    while node > 0 {
      sum = sum.wrapping_add(self.tree[node - 1]);
      node &= node - 1;
    }
    sum
  }

  pub fn total(&self) -> u64 {
    self.total
  }
}

const YEAR: u32 = 365 * 86_400;

impl Tracker {
  /// Heights from which a UTXO counts as active within each of
  /// `ACTIVE_YEARS` (see the module docs on time → height).
  fn active_from(&self) -> Option<Vec<usize>> {
    let tip_time = self.times.last().copied()?;
    Some(
      ACTIVE_YEARS
        .iter()
        .map(|&years| {
          let since = tip_time.saturating_sub(years * YEAR);
          self.max_times.partition_point(|&time| time < since)
        })
        .collect(),
    )
  }

  /// `active[row][window]`: non-extracted located sats in UTXOs created at or
  /// after each window's start height.
  fn active_non_extracted(&self) -> Vec<Vec<u64>> {
    let rows = self.stats.len();
    let mut active = vec![vec![0u64; ACTIVE_YEARS.len()]; rows];
    let Some(from) = self.active_from() else {
      return active;
    };
    for (window, &from) in from.iter().enumerate() {
      let bucket = from >> BUCKET_SHIFT;
      for (row, fenwick) in self.fenwick.iter().enumerate() {
        active[row][window] = fenwick.total().wrapping_sub(fenwick.prefix(bucket + 1));
      }
      // The boundary bucket, height by height.
      for &id in self.buckets.get(bucket).map_or(&[][..], Vec::as_slice) {
        let slot = &self.slots[id as usize];
        if (slot.utxo.height as usize) < from {
          continue;
        }
        if slot.extracted {
          continue;
        }
        for &(row, n) in &slot.rows {
          active[usize::from(row)][window] += n;
        }
      }
    }
    active
  }

  /// Per row: (top extracted holders, top non-extracted holders); `top`
  /// per list for a catalog row, `YEAR_TOP_HOLDERS` at most for a year row,
  /// none for a luck row.
  fn top_holders(&self, top: usize) -> Vec<(Vec<Holder>, Vec<Holder>)> {
    let catalog_rows = self.catalog_rows();
    let year_top = top.min(YEAR_TOP_HOLDERS);
    let rows = self.stats.len();
    // Catalog and year rows (the luck rows follow them).
    let with_top = catalog_rows * (1 + crate::model::YEAR_SLOTS);
    let mut scripts: Vec<Option<&ScriptBuf>> = vec![None; self.script_entries.len()];
    for (script, &entry) in &self.scripts {
      scripts[entry as usize] = Some(script);
    }
    let mut extracted: Vec<Vec<(u64, u32)>> = vec![Vec::new(); rows];
    let mut non_extracted: Vec<Vec<(u64, u32)>> = vec![Vec::new(); rows];
    for (&script, entry) in scripts.iter().zip(&self.script_entries) {
      let Some(script) = script else { continue };
      let id = self.scripts[script.as_script()];
      for &(row, located, ext) in &entry.rows {
        // Luck rows keep no top holders.
        if usize::from(row) >= with_top {
          continue;
        }
        if ext > 0 {
          extracted[usize::from(row)].push((ext, id));
        }
        if located > ext {
          non_extracted[usize::from(row)].push((located - ext, id));
        }
      }
    }
    let order = |a: &(u64, u32), b: &(u64, u32)| -> Ordering {
      b.0
        .cmp(&a.0)
        .then_with(|| scripts[a.1 as usize].cmp(&scripts[b.1 as usize]))
    };
    let select = |list: &mut Vec<(u64, u32)>, top: usize| {
      if top == 0 {
        list.clear();
        return;
      }
      if list.len() > top {
        list.select_nth_unstable_by(top - 1, order);
        list.truncate(top);
      }
      list.sort_unstable_by(order);
    };
    let holder = |row: RowId, count: u64, entry: u32, want_extracted: bool| {
      let mut utxos = 0;
      let mut last = 0;
      for &id in &self.script_entries[entry as usize].slots {
        let slot = &self.slots[id as usize];
        if slot.extracted == want_extracted && slot.rows.iter().any(|(r, _)| *r == row) {
          utxos += 1;
          last = last.max(slot.utxo.height);
        }
      }
      Holder {
        script_pubkey: scripts[entry as usize].unwrap().clone(),
        count,
        utxos,
        last_received_height: last,
      }
    };
    (0..rows)
      .map(|row| {
        let mut ext = std::mem::take(&mut extracted[row]);
        let mut non = std::mem::take(&mut non_extracted[row]);
        let top = if row < catalog_rows { top } else { year_top };
        select(&mut ext, top);
        select(&mut non, top);
        let row_id = row as RowId;
        (
          ext
            .into_iter()
            .map(|(n, entry)| holder(row_id, n, entry, true))
            .collect(),
          non
            .into_iter()
            .map(|(n, entry)| holder(row_id, n, entry, false))
            .collect(),
        )
      })
      .collect()
  }

  /// Per catalog row: holder counts by the set of years, and by the set of
  /// lucks, each script holds sats of (located, then extracted).
  fn holder_masks(&self) -> Vec<HolderMasks> {
    let rows = self.catalog_rows();
    // Per catalog row: [year located, year extracted, luck located, luck
    // extracted], mask -> scripts.
    let mut counts: Vec<[FxHashMap<u128, u64>; 4]> = vec![Default::default(); rows];
    // Per script: (catalog row, the same four masks).
    let mut masks: Vec<(RowId, [u128; 4])> = Vec::new();
    // A free entry holds no rows.
    for entry in &self.script_entries {
      masks.clear();
      for &(id, held, ext) in &entry.rows {
        let (row, kind, bit) = match split_row(rows, id) {
          TrackedRow::Catalog(_) => continue,
          TrackedRow::Year(row, year) => (row, 0, 1u128 << year),
          TrackedRow::Luck(row, luck) => (row, 2, 1u128 << luck),
        };
        let slot = match masks.iter_mut().find(|(r, _)| *r == row) {
          Some(slot) => slot,
          None => {
            masks.push((row, [0; 4]));
            masks.last_mut().unwrap()
          }
        };
        if held > 0 {
          slot.1[kind] |= bit;
        }
        if ext > 0 {
          slot.1[kind + 1] |= bit;
        }
      }
      for (row, masks) in &masks {
        for (counts, &mask) in counts[usize::from(*row)].iter_mut().zip(masks) {
          if mask != 0 {
            *counts.entry(mask).or_default() += 1;
          }
        }
      }
    }
    fn sorted<M: TryFrom<u128> + Ord>(map: FxHashMap<u128, u64>) -> Vec<(M, u64)> {
      let mut list: Vec<(M, u64)> = map
        .into_iter()
        .filter_map(|(mask, n)| Some((M::try_from(mask).ok()?, n)))
        .collect();
      list.sort_unstable();
      list
    }
    counts
      .into_iter()
      .map(
        |[years, extracted_years, lucks, extracted_lucks]| HolderMasks {
          years: sorted(years),
          extracted_years: sorted(extracted_years),
          lucks: sorted(lucks),
          extracted_lucks: sorted(extracted_lucks),
        },
      )
      .collect()
  }

  pub(super) fn supply_snapshot(&self, top: usize) -> SupplySnapshot {
    let active = self.active_non_extracted();
    let mut holders = if top > 0 {
      self.top_holders(top)
    } else {
      Vec::new()
    };
    holders.resize_with(self.stats.len(), Default::default);
    let mut supplies: Vec<RowSupply> = holders
      .into_iter()
      .enumerate()
      .map(
        |(row, (top_extracted_holders, top_non_extracted_holders))| RowSupply {
          row: row as RowId,
          stats: self.stats[row],
          active_non_extracted: ACTIVE_YEARS
            .iter()
            .copied()
            .zip(active[row].iter().copied())
            .collect(),
          top_extracted_holders,
          top_non_extracted_holders,
        },
      )
      .collect();
    let catalog_rows = self.catalog_rows();
    let other_supplies = supplies.split_off(catalog_rows);
    let mut years = Vec::with_capacity(catalog_rows);
    let mut lucks = Vec::with_capacity(catalog_rows);
    for (row, masks) in self.holder_masks().into_iter().enumerate() {
      years.push(RowYears {
        row: row as RowId,
        cells: Vec::new(),
        holder_masks: masks.years,
        extracted_holder_masks: masks.extracted_years,
      });
      lucks.push(RowLucks {
        row: row as RowId,
        cells: Vec::new(),
        holder_masks: masks.lucks,
        extracted_holder_masks: masks.extracted_lucks,
      });
    }
    for supply in other_supplies {
      if supply.stats == RowStats::default() {
        continue;
      }
      match split_row(catalog_rows, supply.row) {
        TrackedRow::Year(row, year) => years[usize::from(row)].cells.push((year, supply)),
        TrackedRow::Luck(row, luck) => lucks[usize::from(row)].cells.push((luck, supply)),
        TrackedRow::Catalog(_) => unreachable!("catalog rows come first"),
      }
    }
    SupplySnapshot {
      height: self.height,
      hash: self.hash,
      time: self.times.last().copied(),
      rows: supplies,
      years: YearSupply {
        starts: self.blocks.year_starts.clone(),
        rows: years,
      },
      lucks: LuckSupply {
        heights: self.blocks.lucks.clone(),
        rows: lucks,
      },
    }
  }
}

/// One catalog row's holders by the set of years and by the set of lucks
/// they hold (`RowYears::holder_masks`, `RowLucks::holder_masks`).
struct HolderMasks {
  years: Vec<(YearMask, u64)>,
  extracted_years: Vec<(YearMask, u64)>,
  lucks: Vec<(LuckMask, u64)>,
  extracted_lucks: Vec<(LuckMask, u64)>,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn fenwick_grows_and_sums() {
    let mut fenwick = Fenwick::default();
    let mut values = vec![0u64; 3000];
    let mut seed = 7u64;
    for step in 0..5000 {
      seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
      let index = ((seed >> 33) as usize) % (1 + step.min(2999));
      let n = seed % 1000;
      if seed & 1 == 0 || values[index] < n {
        fenwick.add(index, n);
        values[index] += n;
      } else {
        fenwick.sub(index, n);
        values[index] -= n;
      }
      if step % 97 == 0 {
        for i in [0, 1, index, index + 1, 64, 1000, 3000] {
          assert_eq!(
            fenwick.prefix(i),
            values[..i.min(3000)].iter().sum::<u64>(),
            "prefix {i}"
          );
        }
        assert_eq!(fenwick.total(), values.iter().sum::<u64>());
      }
    }
  }
}
