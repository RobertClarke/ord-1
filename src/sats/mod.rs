//! Sat types, the tracked set and rows: an exact port of bitcoin-monitoring's
//! JS rules (`satRarityRules.js`, `exoticSatRanges.js`, `psbtBuilder.js`).
//! See docs/DESIGN.md §1–§3. Pure and deterministic; no I/O besides the
//! embedded data files.
//!
//! OWNER: sats+tracker agent. The public API below is the contract used by
//! the tracker, store and server; keep the signatures.
//!
//! # Types of a sat
//!
//! `types(sat)` reproduces what the runtime's scanner
//! (`rareSatScanner.analyzeSatRanges`) reports for the sat as a point sat:
//!
//! 1. the rule types of `findSpecialSatsInRangesBatch` (block boundaries,
//!    alphas/omegas with their milestone tier, palindromes);
//! 2. `bounty3_name` when the sat is in `data/bounty3-names-map.json`;
//! 3. every `EXOTIC_RANGES` type whose ranges contain the sat;
//! 4. `normalizePaliblockTypes` over that combined list: `paliblock` survives
//!    only next to `uncommon`/`black` and paliblock-compatible types, so any
//!    exotic type or `bounty3_name` drops it (block 9's first sat has no
//!    `paliblock`);
//! 5. the synthetic `alpha_jan_feb_2009` (`isAlpha2009Sat`), added after the
//!    normalization.
//!
//! `common_name`, name palindromes and `inscription_*` types are not tracked
//! and never reported. Where the scanner differs from a bare `getSatTypes`:
//! sat 0 is not a palindrome (`generatePalindromesInRange` never yields 0),
//! and a sat that is both a block's first and last sat (the one-sat blocks of
//! the last epoch) carries `paliblock` once, like the scanner's de-duplicated
//! list. `tests/fixtures/sat-types.json`, generated from the JS by
//! `tools/export-monitoring-data.js`, pins all of this.
//!
//! # The tracked set
//!
//! A sat below the supply is tracked when it lies in a `TrackerConfig` range
//! (default: block 9), or its types include a standalone type
//! (`SPECIAL_SAT_TYPES` minus `NOT_TRACKED_STANDALONE`), or all types of one
//! `REQUIRED_COMBINATION_TYPES` entry, or `alpha_jan_feb_2009`.
//!
//! Every individually tracked sat belongs to a sparse family (an *anchor*:
//! block boundaries, alphas, omegas, palindromes, listed sats), so
//! `tracked_runs` enumerates only those families, each restricted to the
//! exotic ranges its combinations need, and merges the tracked ones with the
//! config ranges into maximal runs.

mod anchors;
mod data;
pub(crate) mod math;
mod rows;
mod types;

#[cfg(test)]
mod tests;

use {
  self::{
    anchors::{Anchor, Lists},
    data::{
      BOUNTY3_NAMES_BIN, Bounty3, EXOTIC_RANGES_BIN, ExoticIndex, for_each_overlap,
      intersect_ranges, merge_ranges, overlap_len, parse_exotic_ranges,
    },
    math::{FIRST_MARCH_2009_SAT, SUPPLY, first_sat},
  },
  crate::model::{RowCounts, RowId, TrackerConfig},
  anyhow::{Context, ensure},
  bitcoin::hashes::{Hash, HashEngine, sha256},
  serde::{Deserialize, Serialize},
  std::{collections::HashMap, sync::OnceLock},
};

pub use rows::{BLOCK9_FIRST_TX, HEIGHTS_2009, default_rows};

/// Bump whenever the port's behaviour or the tracker's aggregate rules
/// change: it changes the fingerprint, so the store refuses indexes built
/// under the old rules and asks for a reindex.
///
/// * 1: the scanner's types, the tracked set and rows.
/// * 2: extraction by budget (`model::is_extracted`, DESIGN.md §5) instead
///   of `value ≤ extracted_max_value || row_count == value` per row.
/// * 3: the pizza, jpeg, hitman and sr_1st_auction ranges end on their last
///   sat (DESIGN.md §2.2): the sat after each range, an uncommon or alpha
///   when the range ends on a block or BTC, is no longer of the type.
pub const RULES_VERSION: &str = "3";

/// `psbtBuilder.js` → `SPECIAL_SAT_TYPES`, verbatim and in order (a test
/// compares it with the array parsed from the JS).
pub const SPECIAL_SAT_TYPES: [&str; 23] = [
  "450x",
  "block9",
  "block78",
  "block286",
  "uncommon",
  "rare",
  "palindromes_perfect_palinception",
  "palindromes_integer_1d",
  "palindromes_integer_2d",
  "bounty3_name",
  "black_rare",
  "black_epic",
  "epic",
  "black",
  "kilosat_alpha",
  "decasat_alpha",
  "centasat_alpha",
  "megasat_alpha",
  "kilosat_omega",
  "decasat_omega",
  "centasat_omega",
  "megasat_omega",
  "requests",
];

/// `psbtBuilder.js` → `REQUIRED_COMBINATION_TYPES`, verbatim and in order.
pub const REQUIRED_COMBINATION_TYPES: [&[&str]; 30] = [
  &["nakamoto", "palindromes_integer"],
  &["palindromes_integer", "pizza"],
  &["jpeg", "palindromes_integer"],
  &["alpha", "pizza"],
  &["alpha", "jpeg"],
  &["omega", "jpeg"],
  &["omega", "pizza"],
  &["alpha", "nakamoto"],
  &["omega", "nakamoto"],
  &["alpha", "vintage"],
  &["omega", "vintage"],
  &["block9", "palindromes_integer"],
  &["block9", "alpha"],
  &["block9", "omega"],
  &["alpha", "block78"],
  &["omega", "block78"],
  &["block78", "palindromes_integer"],
  &["alpha", "block286"],
  &["omega", "block286"],
  &["block286", "palindromes_integer"],
  &["alpha", "hitman"],
  &["omega", "hitman"],
  &["hitman", "palindromes_integer"],
  &["alpha", "sr_1st_auction"],
  &["omega", "sr_1st_auction"],
  &["black", "sr_1st_auction"],
  &[
    "palindromes_integer_3d",
    "palindromes_integer",
    "sr_1st_auction",
  ],
  &["block_666", "palindromes_integer"],
  &["block_666", "alpha"],
  &["block_666", "omega"],
];

/// `SPECIAL_SAT_TYPES` entries that do not make a sat tracked on their own:
/// the whole-range extractions (block 9, including 450x, is tracked whole
/// through the config ranges; the ≥1M-sat block 78 / block 286 range
/// extractions are out of scope, only their combination sats are tracked)
/// and customer `requests`.
pub const NOT_TRACKED_STANDALONE: [&str; 5] = ["450x", "block9", "block78", "block286", "requests"];

/// Runs up to this long get their row counts sat by sat.
const SHORT_RUN: u64 = 16;

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
  /// Sat-number scope `[start, end)`, if any (block 9's first-transaction
  /// sats).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub sats: Option<(u64, u64)>,
  /// Pure range-membership row, counted by overlap length.
  pub range: bool,
  /// A market row: a sale can be priced per its sats (`market::units`).
  /// `false` for a label row naming the same circulating sats as another
  /// row (`block9_1st_tx`: every block 9 sat that ever moved is in it).
  /// Changes no index, so it is not part of the catalog fingerprint.
  #[serde(skip_serializing, default = "market_default")]
  pub market: bool,
}

fn market_default() -> bool {
  true
}

/// Types that `normalizePaliblockTypes` lets `paliblock` coexist with.
fn paliblock_compatible() -> TypeSet {
  use SatType::*;
  TypeSet::of(&[
    Paliblock,
    Uncommon,
    Black,
    Alpha,
    Omega,
    KilosatAlpha,
    DecasatAlpha,
    CentasatAlpha,
    MegasatAlpha,
    KilosatOmega,
    DecasatOmega,
    CentasatOmega,
    MegasatOmega,
  ])
}

fn type_set(names: &[&str]) -> anyhow::Result<TypeSet> {
  names
    .iter()
    .map(|name| SatType::from_name(name).with_context(|| format!("unknown sat type `{name}`")))
    .collect()
}

/// Candidates of one anchor, and where they may be tracked.
struct SourceGroup {
  anchor: Anchor,
  /// Candidates anywhere may be tracked; `Some(true)`: all of them are.
  everywhere: Option<bool>,
  /// Candidates inside these sorted, disjoint ranges are all tracked.
  sure_ranges: Vec<(u64, u64)>,
  /// Candidates in exotic segments covering the set may be tracked
  /// (`true`: are tracked).
  exotic: Vec<(TypeSet, bool)>,
}

struct CompiledRow {
  all: TypeSet,
  any: TypeSet,
  heights: Option<(u64, u64)>,
  sats: Option<(u64, u64)>,
  range: bool,
  /// Sats the row can contain (exotic types, height scope, implied limits).
  domain: Vec<(u64, u64)>,
  /// Per-sat rows: the candidates of these anchors cover every row sat.
  anchors: Vec<Anchor>,
  /// Per-sat rows: every candidate of `anchors[0]` in `domain` is a tracked
  /// row sat, so counts are pure arithmetic.
  sure: bool,
  /// Types under which the row is indexed in `rows_by_key`.
  keys: TypeSet,
}

/// Loaded once per process and shared (`Arc`).
pub struct SatCatalog {
  config: TrackerConfig,
  exotic: ExoticIndex,
  bounty3: Bounty3,
  lists: Lists,
  standalone: TypeSet,
  combos: Vec<TypeSet>,
  /// Config ranges and combinations without a sparse type, merged.
  dense: Vec<(u64, u64)>,
  groups: Vec<SourceGroup>,
  rows: Vec<Row>,
  compiled: Vec<CompiledRow>,
  per_sat_rows: Vec<RowId>,
  range_rows: Vec<RowId>,
  rows_by_key: Vec<Vec<RowId>>,
  row_index: HashMap<String, RowId>,
  totals: Vec<OnceLock<u64>>,
  ranks: OnceLock<Vec<usize>>,
  fingerprint: OnceLock<String>,
}

impl SatCatalog {
  pub fn new(config: &TrackerConfig) -> anyhow::Result<Self> {
    Self::with_rows(config, default_rows())
  }

  /// A catalog with a custom row set (tests, experiments).
  pub fn with_rows(config: &TrackerConfig, rows: Vec<Row>) -> anyhow::Result<Self> {
    let exotic = ExoticIndex::new(parse_exotic_ranges(EXOTIC_RANGES_BIN)?);
    let bounty3 = Bounty3::parse(BOUNTY3_NAMES_BIN)?;
    let lists = Lists {
      one_digit: math::palindromes_with_distinct_digits(1),
      two_digit: math::palindromes_with_distinct_digits(2),
      palinception: math::perfect_palinceptions(),
      bounty3: bounty3.sats.clone(),
    };

    let mut dense = Vec::new();
    for range in &config.ranges {
      ensure!(!range.id.is_empty(), "tracked range without an id");
      ensure!(
        range.start < range.end,
        "tracked range `{}` is empty: [{}, {})",
        range.id,
        range.start,
        range.end
      );
      dense.push((range.start.min(SUPPLY), range.end.min(SUPPLY)));
    }

    let special = type_set(
      &SPECIAL_SAT_TYPES
        .iter()
        .copied()
        .filter(|name| !NOT_TRACKED_STANDALONE.contains(name))
        .collect::<Vec<_>>(),
    )?;
    let combos = REQUIRED_COMBINATION_TYPES
      .iter()
      .map(|combo| type_set(combo))
      .collect::<anyhow::Result<Vec<_>>>()?;

    let mut catalog = Self {
      config: config.clone(),
      exotic,
      bounty3,
      lists,
      standalone: special,
      combos,
      dense: Vec::new(),
      groups: Vec::new(),
      rows: Vec::new(),
      compiled: Vec::new(),
      per_sat_rows: Vec::new(),
      range_rows: Vec::new(),
      rows_by_key: vec![Vec::new(); SatType::ALL.len()],
      row_index: HashMap::new(),
      totals: Vec::new(),
      ranks: OnceLock::new(),
      fingerprint: OnceLock::new(),
    };
    catalog.build_sources(dense)?;
    catalog.compile_rows(rows)?;
    Ok(catalog)
  }

  fn exotic_intersection(&self, set: TypeSet) -> Vec<(u64, u64)> {
    let mut domain = vec![(0, SUPPLY)];
    for t in set.iter() {
      domain = intersect_ranges(&domain, self.exotic.ranges(t));
    }
    domain
  }

  fn build_sources(&mut self, mut dense: Vec<(u64, u64)>) -> anyhow::Result<()> {
    struct Source {
      anchor: Anchor,
      domain: Domain,
      sure: bool,
    }
    enum Domain {
      Everywhere,
      Ranges(Vec<(u64, u64)>),
      Exotic(TypeSet),
    }
    let exotic_types = TypeSet::exotic();
    let mut sources = Vec::new();

    for t in self.standalone.iter() {
      if t.is_exotic() {
        dense.extend(self.exotic.ranges(t));
        continue;
      }
      let anchor = Anchor::of(t).context("standalone type without an anchor")?;
      sources.push(Source {
        anchor,
        domain: Domain::Everywhere,
        sure: anchor.implied().contains(t),
      });
    }

    for &combo in &self.combos {
      if combo.intersects(self.standalone) {
        // Every sat satisfying it has a standalone type.
        continue;
      }
      let exotic = combo.intersection(exotic_types);
      let sparse = combo.difference(exotic_types);
      let Some(anchor) = sparse
        .iter()
        .filter_map(Anchor::of)
        .min_by_key(|a| a.rank())
      else {
        dense.extend(self.exotic_intersection(exotic));
        continue;
      };
      sources.push(Source {
        anchor,
        domain: if exotic.is_empty() {
          Domain::Everywhere
        } else {
          Domain::Exotic(exotic)
        },
        sure: anchor.implied().union(exotic).contains_all(combo),
      });
    }

    // Jan–Feb 2009 alphas.
    sources.push(Source {
      anchor: Anchor::of(SatType::AlphaJanFeb2009).unwrap(),
      domain: Domain::Ranges(vec![(0, FIRST_MARCH_2009_SAT)]),
      sure: true,
    });

    // Group by anchor, dropping anchors whose candidates another anchor
    // already enumerates everywhere as tracked.
    let covers = |a: Anchor, b: Anchor| match (a, b) {
      (
        Anchor::BlockFirst {
          modulus: 1,
          residue: 0,
        },
        Anchor::BlockFirst { .. },
      )
      | (
        Anchor::BlockLast {
          modulus: 1,
          residue: 0,
        },
        Anchor::BlockLast { .. },
      ) => true,
      (
        Anchor::Multiple {
          modulus: m1,
          residue: r1,
        },
        Anchor::Multiple {
          modulus: m2,
          residue: r2,
        },
      ) => m2 % m1 == 0 && r2 % m1 == r1,
      _ => a == b,
    };
    let sure_everywhere: Vec<Anchor> = sources
      .iter()
      .filter(|s| s.sure && matches!(s.domain, Domain::Everywhere))
      .map(|s| s.anchor)
      .collect();
    let mut groups: Vec<SourceGroup> = Vec::new();
    for source in sources {
      if sure_everywhere
        .iter()
        .any(|&a| a != source.anchor && covers(a, source.anchor))
      {
        continue;
      }
      let index = match groups.iter().position(|g| g.anchor == source.anchor) {
        Some(index) => index,
        None => {
          groups.push(SourceGroup {
            anchor: source.anchor,
            everywhere: None,
            sure_ranges: Vec::new(),
            exotic: Vec::new(),
          });
          groups.len() - 1
        }
      };
      let group = &mut groups[index];
      match source.domain {
        Domain::Everywhere => {
          group.everywhere = Some(group.everywhere.unwrap_or(false) | source.sure);
        }
        Domain::Ranges(ranges) => {
          ensure!(source.sure, "range sources must be sure");
          group.sure_ranges.extend(ranges);
          group.sure_ranges = merge_ranges(std::mem::take(&mut group.sure_ranges));
        }
        Domain::Exotic(set) => group.exotic.push((set, source.sure)),
      }
    }
    self.groups = groups;
    self.dense = merge_ranges(dense);
    Ok(())
  }

  fn compile_rows(&mut self, rows: Vec<Row>) -> anyhow::Result<()> {
    ensure!(rows.len() <= usize::from(RowId::MAX), "too many rows");
    let exotic_types = TypeSet::exotic();
    for (index, row) in rows.iter().enumerate() {
      let id = RowId::try_from(index)?;
      ensure!(!row.id.is_empty(), "row {index} has no id");
      ensure!(
        self.row_index.insert(row.id.clone(), id).is_none(),
        "duplicate row id `{}`",
        row.id
      );
      let all = TypeSet::of(&row.all_of);
      let any = TypeSet::of(&row.any_of);
      ensure!(
        !all.is_empty() || !any.is_empty(),
        "row `{}` matches every sat",
        row.id
      );
      let heights = row
        .heights
        .map(|(from, to)| (u64::from(from), u64::from(to)));
      if let Some((from, to)) = heights {
        ensure!(from <= to, "row `{}` has an empty height scope", row.id);
      }
      if let Some((start, end)) = row.sats {
        ensure!(start < end, "row `{}` has an empty sat scope", row.id);
      }

      let mut domain = self.exotic_intersection(all.intersection(exotic_types));
      if !any.is_empty() && exotic_types.contains_all(any) {
        let union = merge_ranges(
          any
            .iter()
            .flat_map(|t| self.exotic.ranges(t).iter().copied())
            .collect(),
        );
        domain = intersect_ranges(&domain, &union);
      }
      if let Some((from, to)) = heights {
        domain = intersect_ranges(&domain, &[(first_sat(from), first_sat(to + 1))]);
      }
      if let Some(scope) = row.sats {
        domain = intersect_ranges(&domain, &[scope]);
      }
      if all.contains(SatType::AlphaJanFeb2009) {
        domain = intersect_ranges(&domain, &[(0, FIRST_MARCH_2009_SAT)]);
      }

      let mut compiled = CompiledRow {
        all,
        any,
        heights,
        sats: row.sats,
        range: row.range,
        domain,
        anchors: Vec::new(),
        sure: false,
        keys: TypeSet::EMPTY,
      };

      if row.range {
        ensure!(
          exotic_types.contains_all(all) && exotic_types.contains_all(any),
          "range row `{}` may only use exotic range types",
          row.id
        );
        self.range_rows.push(id);
      } else {
        let best = all
          .iter()
          .filter_map(|t| Anchor::of(t).map(|a| (t, a)))
          .min_by_key(|(_, a)| a.rank());
        if let Some((key, anchor)) = best {
          compiled.anchors.push(anchor);
          compiled.keys.insert(key);
          let mut implied = anchor.implied().union(all.intersection(exotic_types));
          if implied.contains(SatType::Alpha) && all.contains(SatType::AlphaJanFeb2009) {
            implied.insert(SatType::AlphaJanFeb2009);
          }
          let tracked = implied.intersects(self.standalone)
            || self.combos.iter().any(|c| implied.contains_all(*c))
            || implied.contains(SatType::AlphaJanFeb2009)
            || compiled
              .domain
              .iter()
              .all(|&(s, e)| overlap_len(&self.dense, s, e) == e - s);
          compiled.sure = any.is_empty() && implied.contains_all(all) && tracked;
        } else {
          ensure!(
            !any.is_empty() && any.iter().all(|t| Anchor::of(t).is_some()),
            "per-sat row `{}` needs a sparse type (block boundary, alpha/omega, palindrome, \
             bounty3) in all_of or only sparse types in any_of",
            row.id
          );
          for t in any.iter() {
            let anchor = Anchor::of(t).unwrap();
            if !compiled.anchors.contains(&anchor) {
              compiled.anchors.push(anchor);
            }
            compiled.keys.insert(t);
          }
        }
        for t in compiled.keys.iter() {
          self.rows_by_key[t as usize].push(id);
        }
        self.per_sat_rows.push(id);
      }
      self.compiled.push(compiled);
    }
    self.totals = rows.iter().map(|_| OnceLock::new()).collect();
    self.rows = rows;
    Ok(())
  }

  /// Exactly the JS `getSatTypes(sat)` + exotic range membership +
  /// `bounty3_name` + `alpha_jan_feb_2009`, assembled the way the runtime
  /// scanner types a point sat (see the module docs): `paliblock` is
  /// normalized over the rule, bounty3 and exotic types together, and sat 0
  /// is not a palindrome.
  pub fn types(&self, sat: u64) -> TypeSet {
    use {
      SatType::*,
      math::{
        COIN, CYCLE_INTERVAL, DIFFCHANGE_INTERVAL, Digits, HALVING_INTERVAL, block_of,
        distinct_digits, is_palindrome, is_palindrome_digits, is_perfect_palinception,
      },
    };
    let pos = block_of(sat);
    let h = pos.height;
    let mut types = TypeSet::EMPTY;
    let mut paliblock = false;

    if pos.offset == 0 {
      if sat == 0 {
        types.insert(Mythic);
      }
      if h.is_multiple_of(CYCLE_INTERVAL) {
        types.insert(Legendary);
      }
      if h.is_multiple_of(HALVING_INTERVAL) {
        types.insert(Epic);
      }
      if h.is_multiple_of(DIFFCHANGE_INTERVAL) {
        types.insert(Rare);
      }
      types.insert(Uncommon);
      paliblock |= h != 0
        && !h.is_multiple_of(CYCLE_INTERVAL)
        && !h.is_multiple_of(HALVING_INTERVAL)
        && !h.is_multiple_of(DIFFCHANGE_INTERVAL)
        && is_palindrome(h);
    }

    if pos.offset == pos.subsidy - 1 {
      types.insert(Black);
      paliblock |= h != 0
        && !(h + 1).is_multiple_of(HALVING_INTERVAL)
        && !(h + 1).is_multiple_of(DIFFCHANGE_INTERVAL)
        && is_palindrome(h);
      if (h + 1).is_multiple_of(DIFFCHANGE_INTERVAL) {
        types.insert(BlackRare);
      }
      if (h + 1).is_multiple_of(HALVING_INTERVAL) {
        types.insert(BlackEpic);
      }
    }

    // Milestone tiers are exclusive: the largest one that divides.
    let tier = |n: u64, tiers: [SatType; 4]| {
      [1_000_000, 100_000, 10_000, 1_000]
        .into_iter()
        .zip(tiers)
        .find(|&(btc, _)| n.is_multiple_of(COIN * btc))
        .map(|(_, t)| t)
    };

    if sat.is_multiple_of(COIN) {
      types.insert(Alpha);
      if let Some(t) = tier(
        sat,
        [MegasatAlpha, CentasatAlpha, DecasatAlpha, KilosatAlpha],
      ) {
        types.insert(t);
      }
    }

    if sat % COIN == COIN - 1 {
      types.insert(Omega);
      if let Some(t) = tier(
        sat + 1,
        [MegasatOmega, CentasatOmega, DecasatOmega, KilosatOmega],
      ) {
        types.insert(t);
      }
    }

    if sat != 0 {
      let digits = Digits::new(sat);
      let digits = digits.as_slice();
      if is_palindrome_digits(digits) {
        types.insert(PalindromesInteger);
        match distinct_digits(digits) {
          1 => types.insert(PalindromesInteger1d),
          2 => types.insert(PalindromesInteger2d),
          3 => types.insert(PalindromesInteger3d),
          _ => {}
        }
        if is_perfect_palinception(digits) {
          types.insert(PalindromesPerfectPalinception);
        }
      }
    }

    if self.bounty3.contains(sat) {
      types.insert(Bounty3Name);
    }

    types = types.union(self.exotic.types_at(sat));

    if paliblock && paliblock_compatible().contains_all(types) {
      types.insert(Paliblock);
    }

    if sat < FIRST_MARCH_2009_SAT && sat.is_multiple_of(COIN) {
      types.insert(AlphaJanFeb2009);
    }

    types
  }

  fn in_dense(&self, sat: u64) -> bool {
    let i = self.dense.partition_point(|&(_, end)| end <= sat);
    self.dense.get(i).is_some_and(|&(start, _)| start <= sat)
  }

  fn tracked_by_types(&self, types: TypeSet) -> bool {
    types.intersects(self.standalone)
      || types.contains(SatType::AlphaJanFeb2009)
      || self.combos.iter().any(|combo| types.contains_all(*combo))
  }

  /// DESIGN.md §1.
  pub fn is_tracked(&self, sat: u64) -> bool {
    sat < SUPPLY && (self.in_dense(sat) || self.tracked_by_types(self.types(sat)))
  }

  /// Tracked sats in `[start, end)` as maximal runs `(first_sat, len)`,
  /// ascending. Used for each coinbase's subsidy range; fast for the 5e9-sat
  /// block 9 (one run with the default config) and for any other block
  /// (about 200 ns for a post-2012 coinbase).
  pub fn tracked_runs(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
    let (start, end) = (start.min(SUPPLY), end.min(SUPPLY));
    let mut runs: Vec<(u64, u64)> = Vec::new();
    if start >= end {
      return runs;
    }

    // The parts of [start, end) outside the dense ranges.
    let mut dense_parts = Vec::new();
    for_each_overlap(&self.dense, start, end, |s, e| dense_parts.push((s, e)));
    let mut gaps = Vec::new();
    let mut cursor = start;
    for &(s, e) in &dense_parts {
      if cursor < s {
        gaps.push((cursor, s));
      }
      cursor = e;
    }
    if cursor < end {
      gaps.push((cursor, end));
    }

    let mut candidates = Vec::new();
    for &(a, b) in &gaps {
      self.for_each_individual(a, b, |sat| candidates.push(sat));
    }
    candidates.sort_unstable();
    candidates.dedup();

    let mut add = |s: u64, e: u64| match runs.last_mut() {
      Some((first, len)) if *first + *len >= s => *len = (*len).max(e - *first),
      _ => runs.push((s, e - s)),
    };
    let mut next_dense = 0;
    for sat in candidates {
      while let Some(&(s, e)) = dense_parts.get(next_dense).filter(|(s, _)| *s <= sat) {
        add(s, e);
        next_dense += 1;
      }
      add(sat, sat + 1);
    }
    for &(s, e) in &dense_parts[next_dense..] {
      add(s, e);
    }
    runs
  }

  /// Calls `f` for every sat of `[a, b)` tracked by its own types (not just
  /// by lying in a config range): unordered, possibly more than once.
  fn for_each_individual(&self, a: u64, b: u64, mut f: impl FnMut(u64)) {
    let mut push = |sat: u64, sure: bool| {
      if sure || self.tracked_by_types(self.types(sat)) {
        f(sat);
      }
    };
    let mut segments = Vec::new();
    if self.groups.iter().any(|g| !g.exotic.is_empty()) {
      self
        .exotic
        .for_each_segment(a, b, |s, e, types| segments.push((s, e, types)));
    }
    for group in &self.groups {
      if let Some(sure) = group.everywhere {
        group
          .anchor
          .for_each(&self.lists, a, b, |sat| push(sat, sure));
        if sure {
          continue;
        }
      }
      for_each_overlap(&group.sure_ranges, a, b, |s, e| {
        group
          .anchor
          .for_each(&self.lists, s, e, |sat| push(sat, true))
      });
      if group.exotic.is_empty() {
        continue;
      }
      for &(s, e, types) in &segments {
        let mut matched = None;
        for &(set, sure) in &group.exotic {
          if types.contains_all(set) {
            matched = Some(matched.unwrap_or(false) | sure);
          }
        }
        if let Some(sure) = matched {
          group
            .anchor
            .for_each(&self.lists, s, e, |sat| push(sat, sure));
        }
      }
    }
  }

  /// The sats of `[start, end)` that are special on their own (tracked by
  /// their types, not merely by lying in a config range like block 9),
  /// ascending, at most `cap`. Cheap for long ranges: it stops early.
  pub fn individual_sats(&self, start: u64, end: u64, cap: usize) -> Vec<u64> {
    let end = end.min(SUPPLY);
    let mut sats = Vec::new();
    let mut window = 1u64 << 20;
    let mut a = start;
    while a < end && sats.len() < cap {
      let b = end.min(a.saturating_add(window));
      let mut found = Vec::new();
      self.for_each_individual(a, b, |sat| found.push(sat));
      found.sort_unstable();
      found.dedup();
      sats.extend(found.into_iter().take(cap - sats.len()));
      a = b;
      window = window.saturating_mul(4);
    }
    sats
  }

  /// Rarity rank of a row: 0 for the row with the fewest sats that will ever
  /// exist (`row_total`), ties by `RowId`. Used to pick a sale's primary row.
  pub fn row_rank(&self, row: RowId) -> usize {
    self
      .ranks
      .get_or_init(|| {
        let mut order: Vec<RowId> = (0..self.rows.len() as RowId).collect();
        order.sort_by_key(|&row| (self.row_total(row), row));
        let mut ranks = vec![0; order.len()];
        for (rank, row) in order.into_iter().enumerate() {
          ranks[usize::from(row)] = rank;
        }
        ranks
      })
      .get(usize::from(row))
      .copied()
      .unwrap_or(usize::MAX)
  }

  pub fn rows(&self) -> &[Row] {
    &self.rows
  }

  pub fn row_id(&self, id: &str) -> Option<RowId> {
    self.row_index.get(id).copied()
  }

  fn row_matches(&self, row: &CompiledRow, types: TypeSet, sat: u64) -> bool {
    types.contains_all(row.all)
      && (row.any.is_empty() || types.intersects(row.any))
      && row.heights.is_none_or(|(from, to)| {
        let h = math::height_of(sat);
        from <= h && h <= to
      })
      && row
        .sats
        .is_none_or(|(start, end)| start <= sat && sat < end)
  }

  /// Calls `f` with each per-sat row the sat (with `types`) counts toward.
  fn for_each_sat_row(&self, sat: u64, types: TypeSet, mut f: impl FnMut(RowId)) {
    let mut bits = types.0;
    while bits != 0 {
      let t = SatType::ALL[bits.trailing_zeros() as usize];
      bits &= bits - 1;
      for &id in &self.rows_by_key[t as usize] {
        let row = &self.compiled[usize::from(id)];
        // Multi-key rows count once, under their lowest present key.
        if row.keys.len() > 1 && types.intersection(row.keys).0.trailing_zeros() != t as u32 {
          continue;
        }
        if self.row_matches(row, types, sat) {
          f(id);
        }
      }
    }
  }

  /// Calls `f(row, count)` for the per-row counts of the tracked sats
  /// `[start, start + len)`; a row may be reported more than once.
  pub fn add_row_counts(&self, start: u64, len: u64, mut f: impl FnMut(RowId, u64)) {
    let end = start.saturating_add(len);
    for &id in &self.range_rows {
      let n = overlap_len(&self.compiled[usize::from(id)].domain, start, end);
      if n > 0 {
        f(id, n);
      }
    }
    if len <= SHORT_RUN {
      for sat in start..end {
        self.for_each_sat_row(sat, self.types(sat), |id| f(id, 1));
      }
      return;
    }
    for &id in &self.per_sat_rows {
      let row = &self.compiled[usize::from(id)];
      let mut n = 0;
      for_each_overlap(&row.domain, start, end, |s, e| {
        if row.sure {
          n += row.anchors[0].count(&self.lists, s, e);
        } else if let [anchor] = row.anchors[..] {
          anchor.for_each(&self.lists, s, e, |sat| {
            n += u64::from(self.row_matches(row, self.types(sat), sat));
          });
        } else {
          let mut candidates = Vec::new();
          for anchor in &row.anchors {
            anchor.for_each(&self.lists, s, e, |sat| candidates.push(sat));
          }
          candidates.sort_unstable();
          candidates.dedup();
          n += candidates
            .into_iter()
            .filter(|&sat| self.row_matches(row, self.types(sat), sat))
            .count() as u64;
        }
      });
      if n > 0 {
        f(id, n);
      }
    }
  }

  /// Per-row counts for the tracked sats `[start, start + len)` (every sat
  /// in the run must be tracked).
  pub fn row_counts(&self, start: u64, len: u64) -> RowCounts {
    let mut counts = vec![0u64; self.rows.len()];
    self.add_row_counts(start, len, |id, n| counts[usize::from(id)] += n);
    counts
      .into_iter()
      .enumerate()
      .filter(|&(_, n)| n > 0)
      .map(|(id, n)| (id as RowId, n))
      .collect()
  }

  /// The rows a tracked sat counts toward, ascending; empty for untracked
  /// sats.
  pub fn sat_rows(&self, sat: u64) -> Vec<RowId> {
    if !self.is_tracked(sat) {
      return Vec::new();
    }
    let mut rows = Vec::new();
    self.add_row_counts(sat, 1, |id, _| rows.push(id));
    rows.sort_unstable();
    rows.dedup();
    rows
  }

  /// All sats of the row that will ever exist (through the last block with a
  /// subsidy).
  pub fn row_total(&self, row: RowId) -> u64 {
    let Some(cell) = self.totals.get(usize::from(row)) else {
      return 0;
    };
    *cell.get_or_init(|| {
      let compiled = &self.compiled[usize::from(row)];
      let mut total = 0;
      for &(s, e) in &compiled.domain {
        if compiled.range {
          total += self
            .tracked_runs(s, e)
            .iter()
            .map(|(_, len)| len)
            .sum::<u64>();
        } else if compiled.sure {
          total += compiled.anchors[0].count(&self.lists, s, e);
        } else {
          let mut candidates = Vec::new();
          for anchor in &compiled.anchors {
            anchor.for_each(&self.lists, s, e, |sat| candidates.push(sat));
          }
          if compiled.anchors.len() > 1 {
            candidates.sort_unstable();
            candidates.dedup();
          }
          total += candidates
            .into_iter()
            .filter(|&sat| {
              let types = self.types(sat);
              self.row_matches(compiled, types, sat)
                && (self.in_dense(sat) || self.tracked_by_types(types))
            })
            .count() as u64;
        }
      }
      total
    })
  }

  /// Changes whenever the tracked set or the rows would change (rules
  /// version, embedded data, config); recorded by the store.
  pub fn fingerprint(&self) -> String {
    self
      .fingerprint
      .get_or_init(|| {
        let mut engine = sha256::Hash::engine();
        let mut part = |bytes: &[u8]| {
          engine.input(&(bytes.len() as u64).to_le_bytes());
          engine.input(bytes);
        };
        part(b"ord rare-sat catalog");
        part(RULES_VERSION.as_bytes());
        part(sha256::Hash::hash(EXOTIC_RANGES_BIN).as_byte_array());
        part(sha256::Hash::hash(BOUNTY3_NAMES_BIN).as_byte_array());
        part(
          serde_json::to_string(&SPECIAL_SAT_TYPES)
            .unwrap()
            .as_bytes(),
        );
        part(
          serde_json::to_string(&REQUIRED_COMBINATION_TYPES)
            .unwrap()
            .as_bytes(),
        );
        part(
          serde_json::to_string(&NOT_TRACKED_STANDALONE)
            .unwrap()
            .as_bytes(),
        );
        part(serde_json::to_string(&self.rows).unwrap().as_bytes());
        part(serde_json::to_string(&self.config).unwrap().as_bytes());
        sha256::Hash::from_engine(engine).to_string()
      })
      .clone()
  }

  /// The config the catalog was built for.
  pub fn config(&self) -> &TrackerConfig {
    &self.config
  }

  /// The bounty3 name of a sat, if it has one.
  pub fn bounty3_name(&self, sat: u64) -> Option<&str> {
    self.bounty3.name(sat)
  }

  /// Height of the block that mined `sat` (sats past the supply continue
  /// the last epoch's one-sat blocks, as in the JS).
  pub fn height(&self, sat: u64) -> u64 {
    math::height_of(sat)
  }
}
