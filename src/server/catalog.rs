//! What the API needs from the sat catalog, as a trait so the server can be
//! tested with a small fake catalog.

use {
  crate::{
    model::{RowCounts, RowId},
    sats::{SatCatalog, SatType},
  },
  serde::Serialize,
  std::sync::Arc,
};

/// A row definition as the API reports it (type names, not enum values).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RowInfo {
  pub id: String,
  pub label: String,
  pub all_of: Vec<&'static str>,
  pub any_of: Vec<&'static str>,
  pub heights: Option<(u32, u32)>,
  pub range: bool,
}

pub trait Catalog: Send + Sync {
  /// The sat's type names (`getSatTypes` + ranges + bounty3 + Jan–Feb 2009).
  fn types(&self, sat: u64) -> Vec<&'static str>;
  fn is_tracked(&self, sat: u64) -> bool;
  /// In `RowId` order.
  fn rows(&self) -> &[RowInfo];
  /// Per-row counts of the tracked run `[start, start + len)`.
  fn row_counts(&self, start: u64, len: u64) -> RowCounts;
  fn row_total(&self, row: RowId) -> u64;
  fn fingerprint(&self) -> String;

  fn row_id(&self, id: &str) -> Option<RowId> {
    self
      .rows()
      .iter()
      .position(|row| row.id == id)
      .map(|row| row as RowId)
  }

  /// The rows a sat counts toward, ascending; empty for untracked sats.
  fn sat_rows(&self, sat: u64) -> Vec<RowId> {
    if !self.is_tracked(sat) {
      return Vec::new();
    }
    self
      .row_counts(sat, 1)
      .into_iter()
      .filter(|(_, count)| *count > 0)
      .map(|(row, _)| row)
      .collect()
  }
}

/// The real catalog, with row definitions and totals computed once.
pub struct SatCatalogView {
  catalog: Arc<SatCatalog>,
  rows: Vec<RowInfo>,
  totals: Vec<u64>,
}

impl SatCatalogView {
  pub fn new(catalog: Arc<SatCatalog>) -> Self {
    let names = |types: &[SatType]| types.iter().map(|t| t.name()).collect();
    let rows = catalog
      .rows()
      .iter()
      .map(|row| RowInfo {
        id: row.id.clone(),
        label: row.label.clone(),
        all_of: names(&row.all_of),
        any_of: names(&row.any_of),
        heights: row.heights,
        range: row.range,
      })
      .collect::<Vec<RowInfo>>();
    let totals = (0..rows.len())
      .map(|row| catalog.row_total(row as RowId))
      .collect();
    Self {
      catalog,
      rows,
      totals,
    }
  }

  pub fn catalog(&self) -> &Arc<SatCatalog> {
    &self.catalog
  }
}

impl Catalog for SatCatalogView {
  fn types(&self, sat: u64) -> Vec<&'static str> {
    self.catalog.types(sat).names()
  }

  fn is_tracked(&self, sat: u64) -> bool {
    self.catalog.is_tracked(sat)
  }

  fn rows(&self) -> &[RowInfo] {
    &self.rows
  }

  fn row_counts(&self, start: u64, len: u64) -> RowCounts {
    self.catalog.row_counts(start, len)
  }

  fn row_total(&self, row: RowId) -> u64 {
    self
      .totals
      .get(usize::from(row))
      .copied()
      .unwrap_or_default()
  }

  fn fingerprint(&self) -> String {
    self.catalog.fingerprint()
  }

  fn row_id(&self, id: &str) -> Option<RowId> {
    self.catalog.row_id(id)
  }

  fn sat_rows(&self, sat: u64) -> Vec<RowId> {
    self.catalog.sat_rows(sat)
  }
}

#[cfg(test)]
mod tests {
  use {super::*, crate::model::TrackerConfig};

  #[test]
  fn view_of_the_real_catalog() {
    let view = SatCatalogView::new(Arc::new(
      SatCatalog::new(&TrackerConfig::default()).unwrap(),
    ));

    // Block 9's first sat.
    let types = view.types(45_000_000_000);
    for expected in ["uncommon", "alpha", "block9", "450x", "nakamoto"] {
      assert!(types.contains(&expected), "{types:?}");
    }
    assert!(view.is_tracked(45_000_000_000));
    assert!(view.is_tracked(45_000_000_123), "block 9 is tracked whole");
    assert!(!view.is_tracked(5_000_000_001));

    let rows = view.sat_rows(45_000_000_000);
    let ids = rows
      .iter()
      .map(|row| view.rows()[usize::from(*row)].id.as_str())
      .collect::<Vec<&str>>();
    for expected in ["uncommon", "block9", "450x", "block9_alpha"] {
      assert!(ids.contains(&expected), "{ids:?}");
    }
    assert_eq!(view.sat_rows(5_000_000_001), Vec::<RowId>::new());

    let uncommon = view.row_id("uncommon").unwrap();
    assert_eq!(view.rows()[usize::from(uncommon)].all_of, ["uncommon"]);
    assert!(view.row_total(uncommon) > 6_000_000);
    let block9 = view.row_id("block9").unwrap();
    assert!(view.rows()[usize::from(block9)].range);
    assert_eq!(view.row_total(block9), 5_000_000_000);
    assert_eq!(view.row_id("nope"), None);
    assert!(!view.fingerprint().is_empty());
  }
}
