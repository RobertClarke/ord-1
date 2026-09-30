use {
  super::{math::*, *},
  crate::model::TrackedRange,
  std::sync::OnceLock,
};

/// The default catalog (block 9 tracked whole), shared by the tests.
pub(crate) fn default_catalog() -> &'static SatCatalog {
  static CATALOG: OnceLock<SatCatalog> = OnceLock::new();
  CATALOG.get_or_init(|| SatCatalog::new(&TrackerConfig::default()).unwrap())
}

/// Individual sats only.
pub(crate) fn rangeless_catalog() -> &'static SatCatalog {
  static CATALOG: OnceLock<SatCatalog> = OnceLock::new();
  CATALOG.get_or_init(|| {
    SatCatalog::new(&TrackerConfig {
      ranges: Vec::new(),
      ..TrackerConfig::default()
    })
    .unwrap()
  })
}

/// A small custom range straddling the boundary of blocks 11 and 12.
pub(crate) fn custom_config() -> TrackerConfig {
  TrackerConfig {
    ranges: vec![TrackedRange {
      id: "custom".into(),
      start: 60_000_000_000 - 1_500_000_000,
      end: 60_000_000_000 + 700_000_000,
    }],
    ..TrackerConfig::default()
  }
}

pub(crate) fn custom_catalog() -> &'static SatCatalog {
  static CATALOG: OnceLock<SatCatalog> = OnceLock::new();
  CATALOG.get_or_init(|| SatCatalog::new(&custom_config()).unwrap())
}

struct Fixture {
  sats: Vec<(u64, TypeSet)>,
  special_sat_types: Vec<String>,
  required_combination_types: Vec<Vec<String>>,
}

fn fixture() -> &'static Fixture {
  static FIXTURE: OnceLock<Fixture> = OnceLock::new();
  FIXTURE.get_or_init(|| {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sat-types.json");
    let json: serde_json::Value =
      serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let names: Vec<SatType> = json["type_names"]
      .as_array()
      .unwrap()
      .iter()
      .map(|name| {
        let name = name.as_str().unwrap();
        SatType::from_name(name).unwrap_or_else(|| panic!("fixture type `{name}` unknown"))
      })
      .collect();
    let sats = json["sats"]
      .as_array()
      .unwrap()
      .iter()
      .map(|entry| {
        let sat = entry[0].as_u64().unwrap();
        let mask = entry[1].as_u64().unwrap();
        let types = (0..names.len())
          .filter(|i| mask >> i & 1 == 1)
          .map(|i| names[i])
          .collect();
        (sat, types)
      })
      .collect();
    Fixture {
      sats,
      special_sat_types: serde_json::from_value(json["special_sat_types"].clone()).unwrap(),
      required_combination_types: serde_json::from_value(
        json["required_combination_types"].clone(),
      )
      .unwrap(),
    }
  })
}

#[test]
fn types_match_js_fixture() {
  let catalog = default_catalog();
  let fixture = fixture();
  assert!(fixture.sats.len() > 200_000);
  let mut mismatches = Vec::new();
  let mut seen = TypeSet::EMPTY;
  for &(sat, expected) in &fixture.sats {
    seen = seen.union(expected);
    let actual = catalog.types(sat);
    if actual != expected {
      mismatches.push(format!("{sat}: rust {actual} js {expected}"));
    }
  }
  assert!(
    mismatches.is_empty(),
    "{} mismatches, first: {:#?}",
    mismatches.len(),
    &mismatches[..mismatches.len().min(20)]
  );
  // The fixture exercises every type.
  assert_eq!(
    seen.len(),
    SatType::ALL.len(),
    "missing {}",
    TypeSet((1 << 37) - 1).difference(seen)
  );
}

#[test]
fn psbt_builder_arrays_have_not_drifted() {
  let fixture = fixture();
  assert_eq!(fixture.special_sat_types, SPECIAL_SAT_TYPES);
  assert_eq!(
    fixture.required_combination_types,
    REQUIRED_COMBINATION_TYPES
      .iter()
      .map(|combo| combo.iter().map(|t| t.to_string()).collect::<Vec<_>>())
      .collect::<Vec<_>>()
  );
  for name in NOT_TRACKED_STANDALONE {
    assert!(SPECIAL_SAT_TYPES.contains(&name), "{name}");
  }
}

/// The tracked rule restated from the JS arrays in the fixture.
fn js_tracked(fixture: &Fixture, types: TypeSet) -> bool {
  let has = |name: &str| SatType::from_name(name).is_some_and(|t| types.contains(t));
  fixture
    .special_sat_types
    .iter()
    .filter(|name| !NOT_TRACKED_STANDALONE.contains(&name.as_str()))
    .any(|name| has(name))
    || fixture
      .required_combination_types
      .iter()
      .any(|combo| combo.iter().all(|name| has(name)))
    || types.contains(SatType::AlphaJanFeb2009)
}

#[test]
fn tracked_set_matches_js_rules() {
  let fixture = fixture();
  let block9 = 45_000_000_000..50_000_000_000;
  let custom = custom_config().ranges[0].start..custom_config().ranges[0].end;
  for &(sat, types) in &fixture.sats {
    let individual = js_tracked(fixture, types);
    assert_eq!(
      default_catalog().is_tracked(sat),
      individual || block9.contains(&sat),
      "{sat} {types}"
    );
    assert_eq!(rangeless_catalog().is_tracked(sat), individual, "{sat}");
    assert_eq!(
      custom_catalog().is_tracked(sat),
      individual || custom.contains(&sat),
      "{sat}"
    );
  }
  assert!(!default_catalog().is_tracked(SUPPLY));
  assert!(!default_catalog().is_tracked(u64::MAX));
}

#[test]
fn documented_quirks() {
  use SatType::*;
  let catalog = default_catalog();
  // Sat 0: no palindrome types (the scanner's generator never yields 0).
  assert_eq!(
    catalog.types(0),
    TypeSet::of(&[
      Mythic,
      Legendary,
      Epic,
      Rare,
      Uncommon,
      Alpha,
      MegasatAlpha,
      Vintage,
      AlphaJanFeb2009
    ])
  );
  // Block 9's first sat: palindromic height, but exotic types drop paliblock.
  let t = catalog.types(45_000_000_000);
  assert!(!t.contains(Paliblock));
  assert!(t.contains_all(TypeSet::of(&[
    Uncommon, Alpha, Block9, Block450x, Nakamoto, Vintage
  ])));
  // One-sat blocks of the last epoch: both boundaries, paliblock once.
  let h = 6_727_276u64;
  let sat = first_sat(h);
  let t = catalog.types(sat);
  assert!(
    t.contains_all(TypeSet::of(&[Uncommon, Black, Paliblock])),
    "{t}"
  );
}

#[test]
fn tracked_runs_match_brute_force() {
  let windows: Vec<(u64, u64)> = {
    let mut w = vec![
      (0, 3000),
      (4_999_999_000, 5_000_001_000),
      (45_000_000_000 - 2000, 45_000_000_000 + 2000),
      (50_000_000_000 - 2000, 50_000_000_000 + 2000),
      (
        custom_config().ranges[0].start - 1500,
        custom_config().ranges[0].start + 1500,
      ),
      (
        custom_config().ranges[0].end - 1500,
        custom_config().ranges[0].end + 1500,
      ),
      (390_000_000_000 - 1000, 390_000_000_000 + 1000),
      (1_430_000_000_000 - 1000, 1_430_000_000_000 + 1000),
      (3_333_333_333_000, 3_333_333_334_000),
      (
        29_620_000_000_000 - 100_000_500,
        29_620_000_000_000 - 99_999_000,
      ),
      (1_050_000_000_000_000 - 700, 1_050_000_000_000_000 + 700),
      (SUPPLY - 3000, SUPPLY + 10),
      (1_000_000_000_000_000 - 1000, 1_000_000_000_000_000 + 1000),
    ];
    // Around fixture sats of every kind.
    for (i, &(sat, _)) in fixture().sats.iter().enumerate() {
      if i % 997 == 0 {
        w.push((sat.saturating_sub(40), sat + 40));
      }
    }
    w
  };
  for catalog in [default_catalog(), rangeless_catalog(), custom_catalog()] {
    for &(a, b) in &windows {
      let runs = catalog.tracked_runs(a, b);
      let mut expected: Vec<(u64, u64)> = Vec::new();
      for sat in a..b.min(SUPPLY) {
        if catalog.is_tracked(sat) {
          match expected.last_mut() {
            Some((s, l)) if *s + *l == sat => *l += 1,
            _ => expected.push((sat, 1)),
          }
        }
      }
      assert_eq!(runs, expected, "[{a}, {b})");
    }
  }
}

/// Every sat of an anchor family in `[a, b)`, found without the catalog's
/// source groups.
fn all_candidates(catalog: &SatCatalog, a: u64, b: u64) -> Vec<u64> {
  let mut out = Vec::new();
  let lists = &catalog.lists;
  for anchor in [
    anchors::Anchor::of(SatType::Uncommon).unwrap(),
    anchors::Anchor::of(SatType::Black).unwrap(),
    anchors::Anchor::of(SatType::Alpha).unwrap(),
    anchors::Anchor::of(SatType::Omega).unwrap(),
    anchors::Anchor::Palindromes,
    anchors::Anchor::List(anchors::ListId::Bounty3),
  ] {
    anchor.for_each(lists, a, b, |sat| out.push(sat));
  }
  out.sort_unstable();
  out.dedup();
  out
}

fn runs_from(catalog: &SatCatalog, a: u64, b: u64) -> Vec<(u64, u64)> {
  let mut points: Vec<(u64, u64)> = all_candidates(catalog, a, b)
    .into_iter()
    .filter(|&sat| catalog.is_tracked(sat))
    .map(|sat| (sat, sat + 1))
    .collect();
  data::for_each_overlap(&catalog.dense, a, b, |s, e| points.push((s, e)));
  points.sort_unstable();
  let mut runs: Vec<(u64, u64)> = Vec::new();
  for (s, e) in points {
    match runs.last_mut() {
      Some((first, len)) if *first + *len >= s => *len = (*len).max(e - *first),
      _ => runs.push((s, e - s)),
    }
  }
  runs
}

#[test]
fn coinbase_runs_match_candidate_enumeration() {
  // Whole subsidy ranges: every tracked sat is an anchor candidate, so the
  // runs must equal the tracked candidates merged with the dense ranges.
  let mut heights: Vec<u64> = vec![
    0, 1, 8, 9, 10, 11, 12, 78, 286, 666, 688, 877, 999, 1000, 1760, 2016, 5923, 5924, 9443,
    24_000, 30_000, 57_043, 91_842, 209_999, 210_000, 419_999, 420_000, 500_000, 630_000, 840_000,
    969_000, 1_049_999, 6_727_276, 6_929_999,
  ];
  // Blocks with pizza, JPEG, hitman and Silk Road sats.
  for t in [
    SatType::Pizza,
    SatType::Jpeg,
    SatType::Hitman,
    SatType::Sr1stAuction,
  ] {
    let ranges = default_catalog().exotic.ranges(t);
    for i in (0..ranges.len()).step_by((ranges.len() / 25).max(1)) {
      heights.push(height_of(ranges[i].0));
    }
  }
  for catalog in [default_catalog(), rangeless_catalog(), custom_catalog()] {
    for &h in &heights {
      let (a, b) = (first_sat(h), first_sat(h) + subsidy(h));
      if h <= 999 && h > 20 && h != 78 && h != 286 && h != 666 && h != 688 && h != 877 {
        // Dense palindrome blocks: keep the debug-mode run time down.
        continue;
      }
      assert_eq!(
        catalog.tracked_runs(a, b),
        runs_from(catalog, a, b),
        "block {h}"
      );
    }
  }
  let runs = default_catalog().tracked_runs(45_000_000_000, 50_000_000_000);
  assert_eq!(runs, [(45_000_000_000, 5_000_000_000)]);
  // Without the range, block 9 is its individual sats: 50k palindromes, 50
  // alpha/omega pairs and the boundaries.
  let runs = rangeless_catalog().tracked_runs(45_000_000_000, 50_000_000_000);
  let sats: u64 = runs.iter().map(|(_, len)| len).sum();
  assert_eq!(sats, 50_000 + 50 + 50);
}

#[test]
fn runs_straddle_block_boundaries() {
  // The black of block 11 and the uncommon of block 12 are adjacent.
  let boundary = first_sat(12);
  let runs = rangeless_catalog().tracked_runs(boundary - 1, boundary + 1);
  assert_eq!(runs, [(boundary - 1, 2)]);
  // The custom range spans the boundary as one run, and the tracked vintage
  // omega just before it and alpha just after it merge into that run.
  let range = &custom_config().ranges[0];
  let merged = (range.start - 1, range.end - range.start + 2);
  assert_eq!(
    custom_catalog().tracked_runs(range.start - 5, range.end + 5),
    [merged]
  );
  assert!(
    custom_catalog()
      .tracked_runs(first_sat(11), first_sat(13))
      .contains(&merged)
  );
}

fn brute_row_counts(catalog: &SatCatalog, start: u64, len: u64) -> RowCounts {
  let mut counts = vec![0u64; catalog.rows().len()];
  for sat in start..start + len {
    let types = catalog.types(sat);
    for (id, row) in catalog.rows().iter().enumerate() {
      let all = TypeSet::of(&row.all_of);
      let any = TypeSet::of(&row.any_of);
      let h = height_of(sat);
      if types.contains_all(all)
        && (any.is_empty() || types.intersects(any))
        && row
          .heights
          .is_none_or(|(from, to)| u64::from(from) <= h && h <= u64::from(to))
      {
        counts[id] += 1;
      }
    }
  }
  counts
    .into_iter()
    .enumerate()
    .filter(|&(_, n)| n > 0)
    .map(|(id, n)| (id as RowId, n))
    .collect()
}

#[test]
fn row_counts_match_brute_force() {
  for catalog in [default_catalog(), rangeless_catalog(), custom_catalog()] {
    // Individual tracked sats of the fixture.
    for (i, &(sat, _)) in fixture().sats.iter().enumerate() {
      if i % 7 == 0 && catalog.is_tracked(sat) {
        assert_eq!(
          catalog.row_counts(sat, 1),
          brute_row_counts(catalog, sat, 1),
          "{sat}"
        );
      }
    }
    // Runs through the dense ranges, long enough for the arithmetic path.
    for &(start, len) in &[
      (45_000_000_000, 40_000),
      (45_099_990_000, 20_000),
      (49_999_970_000, 30_000),
      (46_600_000_000 - 17, 34),
    ] {
      if (start..start + len).all(|sat| catalog.is_tracked(sat)) {
        assert_eq!(
          catalog.row_counts(start, len),
          brute_row_counts(catalog, start, len),
          "[{start}, +{len})"
        );
      }
    }
  }
  let range = &custom_config().ranges[0];
  let catalog = custom_catalog();
  for (start, len) in [
    (range.start, 30_000),
    (60_000_000_000 - 20_000, 40_000),
    (range.end - 25_000, 25_000),
  ] {
    assert_eq!(
      catalog.row_counts(start, len),
      brute_row_counts(catalog, start, len),
      "[{start}, +{len})"
    );
  }
}

#[test]
fn block9_row_counts() {
  let catalog = default_catalog();
  let counts: HashMap<&str, u64> = catalog
    .row_counts(45_000_000_000, 5_000_000_000)
    .into_iter()
    .map(|(id, n)| (catalog.rows()[usize::from(id)].id.as_str(), n))
    .collect();
  let expect = |id: &str| counts.get(id).copied().unwrap_or(0);
  assert_eq!(expect("block9"), 5_000_000_000);
  assert_eq!(expect("450x"), 100_000_000);
  assert_eq!(expect("block9_palindrome"), 50_000);
  assert_eq!(expect("nakamoto_palindrome"), 50_000);
  assert_eq!(expect("block9_alpha"), 50);
  assert_eq!(expect("block9_omega"), 50);
  assert_eq!(expect("vintage_alpha"), 50);
  assert_eq!(expect("alpha_jan_feb_2009"), 50);
  assert_eq!(expect("uncommon"), 1);
  assert_eq!(expect("black"), 1);
  assert_eq!(expect("uncommon_2009"), 1);
  assert_eq!(expect("paliblock"), 0);
  // Split pieces add up.
  let mut sum = vec![0u64; catalog.rows().len()];
  for (start, len) in [
    (45_000_000_000, 1_234_567_891),
    (46_234_567_891, 1),
    (46_234_567_892, 3_765_432_108),
  ] {
    for (id, n) in catalog.row_counts(start, len) {
      sum[usize::from(id)] += n;
    }
  }
  for (id, n) in catalog.row_counts(45_000_000_000, 5_000_000_000) {
    assert_eq!(
      sum[usize::from(id)],
      n,
      "{}",
      catalog.rows()[usize::from(id)].id
    );
  }
}

#[test]
fn row_totals() {
  let catalog = default_catalog();
  let total = |id: &str| catalog.row_total(catalog.row_id(id).unwrap());
  assert_eq!(total("uncommon"), 6_930_000);
  assert_eq!(total("black"), 6_930_000);
  assert_eq!(total("rare"), 3438);
  assert_eq!(total("black_rare"), 3437);
  assert_eq!(total("epic"), 33);
  assert_eq!(total("black_epic"), 33);
  assert_eq!(total("legendary"), 6);
  assert_eq!(total("mythic"), 1);
  assert_eq!(total("alpha_jan_feb_2009"), 296_200);
  assert_eq!(total("megasat_alpha"), 21);
  assert_eq!(total("centasat_alpha"), 210 - 21);
  assert_eq!(total("decasat_alpha"), 2100 - 210);
  assert_eq!(total("kilosat_alpha"), 21_000 - 2100);
  assert_eq!(total("megasat_omega"), 20);
  assert_eq!(total("block9"), 5_000_000_000);
  assert_eq!(total("450x"), 100_000_000);
  assert_eq!(total("block9_palindrome"), 50_000);
  assert_eq!(total("vintage_uncommon"), 1000);
  assert_eq!(total("vintage_alpha"), 50_000);
  assert_eq!(total("vintage_omega"), 50_000);
  assert_eq!(total("nakamoto_uncommon"), 19);
  assert_eq!(total("uncommon_2009"), 32_490);
  assert_eq!(total("rare_2009"), 17);
  assert_eq!(total("black_2009"), 32_490);
  assert_eq!(total("bounty3_name"), 12_450);
  assert_eq!(total("palindrome_1d"), catalog.lists.one_digit.len() as u64);
  assert_eq!(total("palindrome_2d"), catalog.lists.two_digit.len() as u64);
  // Without the range, block 9 counts its individually tracked sats.
  let rangeless = rangeless_catalog();
  assert_eq!(
    rangeless.row_total(rangeless.row_id("block9").unwrap()),
    50_000 + 100
  );
  assert_eq!(
    rangeless.row_total(rangeless.row_id("450x").unwrap()),
    1_000 + 2
  );
  assert_eq!(rangeless.row_total(u16::MAX), 0);
}

#[test]
fn sat_rows() {
  let catalog = default_catalog();
  let ids = |sat: u64| -> Vec<&str> {
    catalog
      .sat_rows(sat)
      .into_iter()
      .map(|id| catalog.rows()[usize::from(id)].id.as_str())
      .collect()
  };
  assert_eq!(
    ids(45_000_000_000),
    [
      "uncommon",
      "alpha_jan_feb_2009",
      "vintage_uncommon",
      "vintage_alpha",
      "block9",
      "450x",
      "block9_alpha",
      "nakamoto_uncommon",
      "nakamoto_alpha",
      "uncommon_2009"
    ]
  );
  assert_eq!(ids(45_000_000_001), ["block9", "450x"]);
  assert!(ids(5_000_000_001).is_empty());
  assert_eq!(
    ids(0),
    [
      "uncommon",
      "rare",
      "epic",
      "legendary",
      "mythic",
      "alpha_jan_feb_2009",
      "megasat_alpha",
      "vintage_uncommon",
      "vintage_alpha",
      "uncommon_2009",
      "rare_2009"
    ]
  );
}

#[test]
fn fingerprint() {
  let a = default_catalog().fingerprint();
  assert_eq!(a.len(), 64);
  assert_eq!(
    a,
    SatCatalog::new(&TrackerConfig::default())
      .unwrap()
      .fingerprint()
  );
  assert_ne!(a, rangeless_catalog().fingerprint());
  assert_ne!(a, custom_catalog().fingerprint());
  let mut rows = default_rows();
  rows[0].label = "Uncommon sats".into();
  assert_ne!(
    a,
    SatCatalog::with_rows(&TrackerConfig::default(), rows)
      .unwrap()
      .fingerprint()
  );
  let config = TrackerConfig {
    extracted_max_value: 10_001,
    ..TrackerConfig::default()
  };
  assert_ne!(a, SatCatalog::new(&config).unwrap().fingerprint());
}

#[test]
fn invalid_rows_and_configs_are_rejected() {
  let row = |id: &str, all_of: &[SatType], range: bool| Row {
    id: id.into(),
    label: id.into(),
    all_of: all_of.to_vec(),
    any_of: Vec::new(),
    heights: None,
    range,
  };
  let config = TrackerConfig::default();
  assert!(SatCatalog::with_rows(&config, vec![row("a", &[SatType::Pizza], false)]).is_err());
  assert!(SatCatalog::with_rows(&config, vec![row("a", &[SatType::Alpha], true)]).is_err());
  assert!(
    SatCatalog::with_rows(
      &config,
      vec![
        row("a", &[SatType::Alpha], false),
        row("a", &[SatType::Omega], false)
      ]
    )
    .is_err()
  );
  assert!(SatCatalog::with_rows(&config, vec![row("", &[SatType::Alpha], false)]).is_err());
  let empty = TrackerConfig {
    ranges: vec![TrackedRange {
      id: "x".into(),
      start: 5,
      end: 5,
    }],
    ..TrackerConfig::default()
  };
  assert!(SatCatalog::new(&empty).is_err());
  // any_of rows count each sat once.
  let catalog = SatCatalog::with_rows(
    &config,
    vec![Row {
      any_of: vec![SatType::Uncommon, SatType::Alpha],
      ..row("either", &[], false)
    }],
  )
  .unwrap();
  assert_eq!(catalog.row_counts(0, 1), [(0, 1)]);
  assert_eq!(catalog.row_counts(45_000_000_000, 5_000_000_000), [(0, 50)]);
  assert_eq!(catalog.row_counts(45_000_000_000, 16), [(0, 1)]);
}

#[test]
fn totals_match_the_fixture_where_it_lists_every_sat() {
  // The fixture holds every 1d/2d palindrome, every palinception, every
  // bounty3 sat and both boundary sats of every palindromic height.
  let catalog = default_catalog();
  let count = |f: &dyn Fn(u64, TypeSet) -> bool| {
    fixture()
      .sats
      .iter()
      .filter(|&&(sat, types)| f(sat, types))
      .count() as u64
  };
  let total = |id: &str| catalog.row_total(catalog.row_id(id).unwrap());
  assert_eq!(
    total("paliblock"),
    count(&|_, t| t.contains(SatType::Paliblock))
  );
  assert_eq!(
    total("palindrome_1d"),
    count(&|_, t| t.contains(SatType::PalindromesInteger1d))
  );
  assert_eq!(
    total("palindrome_2d"),
    count(&|_, t| t.contains(SatType::PalindromesInteger2d))
  );
  assert_eq!(
    total("palindrome_perfect_palinception"),
    count(&|_, t| t.contains(SatType::PalindromesPerfectPalinception))
  );
  assert_eq!(
    total("bounty3_name"),
    count(&|_, t| t.contains(SatType::Bounty3Name))
  );
  assert_eq!(
    total("palindrome_2d_2009"),
    count(&|sat, t| t.contains(SatType::PalindromesInteger2d) && sat < first_sat(32_490))
  );
}

#[test]
fn catalog_is_shareable() {
  fn assert_send_sync<T: Send + Sync>() {}
  assert_send_sync::<SatCatalog>();
}
