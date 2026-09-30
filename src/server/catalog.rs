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
}

/// Every `SatType`, in declaration order (`TypeSet` bit order).
pub const SAT_TYPES: [SatType; 37] = [
  SatType::Mythic,
  SatType::Legendary,
  SatType::Epic,
  SatType::Rare,
  SatType::Uncommon,
  SatType::Paliblock,
  SatType::Black,
  SatType::BlackRare,
  SatType::BlackEpic,
  SatType::Alpha,
  SatType::MegasatAlpha,
  SatType::CentasatAlpha,
  SatType::DecasatAlpha,
  SatType::KilosatAlpha,
  SatType::Omega,
  SatType::MegasatOmega,
  SatType::CentasatOmega,
  SatType::DecasatOmega,
  SatType::KilosatOmega,
  SatType::PalindromesInteger,
  SatType::PalindromesInteger1d,
  SatType::PalindromesInteger2d,
  SatType::PalindromesInteger3d,
  SatType::PalindromesPerfectPalinception,
  SatType::Vintage,
  SatType::Block9,
  SatType::Block450x,
  SatType::Block78,
  SatType::Block286,
  SatType::Block666,
  SatType::Nakamoto,
  SatType::Jpeg,
  SatType::Pizza,
  SatType::Hitman,
  SatType::Sr1stAuction,
  SatType::Bounty3Name,
  SatType::AlphaJanFeb2009,
];

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
    let types = self.catalog.types(sat);
    SAT_TYPES
      .iter()
      .filter(|t| types.contains(**t))
      .map(|t| t.name())
      .collect()
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
}
