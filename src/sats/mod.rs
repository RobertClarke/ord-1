//! Sat types, the tracked set and rows: an exact port of bitcoin-monitoring's
//! JS rules (`satRarityRules.js`, `exoticSatRanges.js`, `psbtBuilder.js`).
//! See docs/DESIGN.md §1–§3. Pure and deterministic; no I/O besides the
//! embedded data files.
//!
//! OWNER: sats+tracker agent. The public API below is the contract used by
//! the tracker, store and server; keep the signatures.

use {
  crate::model::{RowCounts, RowId, TrackerConfig},
  serde::{Deserialize, Serialize},
};

/// One sat type. `name()` is the JS type string (e.g. `"palindromes_integer_2d"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SatType {
  Mythic,
  Legendary,
  Epic,
  Rare,
  Uncommon,
  Paliblock,
  Black,
  BlackRare,
  BlackEpic,
  Alpha,
  MegasatAlpha,
  CentasatAlpha,
  DecasatAlpha,
  KilosatAlpha,
  Omega,
  MegasatOmega,
  CentasatOmega,
  DecasatOmega,
  KilosatOmega,
  PalindromesInteger,
  PalindromesInteger1d,
  PalindromesInteger2d,
  PalindromesInteger3d,
  PalindromesPerfectPalinception,
  Vintage,
  Block9,
  Block450x,
  Block78,
  Block286,
  Block666,
  Nakamoto,
  Jpeg,
  Pizza,
  Hitman,
  Sr1stAuction,
  Bounty3Name,
  AlphaJanFeb2009,
}

impl SatType {
  pub fn name(self) -> &'static str {
    unimplemented!()
  }

  pub fn from_name(_name: &str) -> Option<Self> {
    unimplemented!()
  }
}

/// A set of `SatType`s (bit i = variant i).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TypeSet(pub u64);

impl TypeSet {
  pub fn contains(self, t: SatType) -> bool {
    self.0 & (1 << t as u64) != 0
  }

  pub fn insert(&mut self, t: SatType) {
    self.0 |= 1 << t as u64;
  }

  pub fn is_empty(self) -> bool {
    self.0 == 0
  }
}

/// A reported category (DESIGN.md §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
  /// Stable API id, e.g. `"uncommon"`, `"block9_alpha"`, `"uncommon_2009"`.
  pub id: String,
  pub label: String,
  /// Every type must be present.
  pub all_of: Vec<SatType>,
  /// At least one must be present (ignored when empty).
  pub any_of: Vec<SatType>,
  /// Inclusive block-height scope of the sat's own block, if any.
  pub heights: Option<(u32, u32)>,
  /// Pure range-membership row, counted by overlap length.
  pub range: bool,
}

/// Loaded once per process and shared (`Arc`).
pub struct SatCatalog {}

impl SatCatalog {
  pub fn new(_config: &TrackerConfig) -> anyhow::Result<Self> {
    unimplemented!()
  }

  /// Exactly the JS `getSatTypes(sat)` + exotic range membership +
  /// `bounty3_name` + `alpha_jan_feb_2009`.
  pub fn types(&self, _sat: u64) -> TypeSet {
    unimplemented!()
  }

  /// DESIGN.md §1.
  pub fn is_tracked(&self, _sat: u64) -> bool {
    unimplemented!()
  }

  /// Tracked sats in `[start, end)` as maximal runs `(first_sat, len)`,
  /// ascending. Used for each coinbase's subsidy range; must be fast for a
  /// 5e9-sat vintage block (one run) and for any later block.
  pub fn tracked_runs(&self, _start: u64, _end: u64) -> Vec<(u64, u64)> {
    unimplemented!()
  }

  pub fn rows(&self) -> &[Row] {
    unimplemented!()
  }

  pub fn row_id(&self, _id: &str) -> Option<RowId> {
    unimplemented!()
  }

  /// Per-row counts for the tracked sats `[start, start + len)` (every sat
  /// in the run must be tracked).
  pub fn row_counts(&self, _start: u64, _len: u64) -> RowCounts {
    unimplemented!()
  }

  /// All sats of the row that will ever exist (through the last block with a
  /// subsidy).
  pub fn row_total(&self, _row: RowId) -> u64 {
    unimplemented!()
  }

  /// Changes whenever the tracked set or the rows would change (rules
  /// version, embedded data, config); recorded by the store.
  pub fn fingerprint(&self) -> String {
    unimplemented!()
  }
}
