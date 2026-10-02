//! Year rows (DESIGN.md §5.1): the sats of each row by mint year, across a
//! new year's boundary.

use {
  super::*,
  crate::model::{FIRST_YEAR, year_row},
};

/// Blocks ten minutes apart, the first of `new_year` at height `boundary`
/// (2010 starts at 32,490 on mainnet, 2023 at 769,787).
fn time_of(boundary: u32, new_year: u32, height: u32) -> u32 {
  if height < boundary {
    new_year - (boundary - height) * 600
  } else {
    new_year + (height - boundary) * 600
  }
}

/// A tracker at `boundary - 4` whose header times follow `time_of`.
fn tracker_before(boundary: u32, new_year: u32) -> (Tracker, Chain) {
  let config = TrackerConfig::default();
  let catalog = catalog_for(&config);
  let tip = boundary - 4;
  let chain = Chain::at(Some(tip));
  let snapshot = TrackerSnapshot {
    height: Some(tip),
    hash: Some(chain.prev),
    times: (0..=tip).map(|h| time_of(boundary, new_year, h)).collect(),
    mined: vec![0; crate::model::tracked_row_ids(catalog.rows().len())],
    ..TrackerSnapshot::default()
  };
  (Tracker::restore(catalog, config, snapshot).unwrap(), chain)
}

fn next_block(
  chain: &mut Chain,
  boundary: u32,
  new_year: u32,
  txs: Vec<ResolvedTx>,
) -> ResolvedBlock {
  chain.next_time = Some(time_of(boundary, new_year, chain.next));
  chain.block(txs)
}

fn id(tracker: &Tracker, row: &str) -> RowId {
  tracker.catalog().row_id(row).unwrap()
}

/// Six blocks across the boundary (three in each year). Each coinbase
/// pays its uncommon to owner 1 + i % 3 and its black to owner 10 + i in
/// 1-sat outputs; then a seventh block (new year) pays its uncommon and
/// black to owner 1 in one large output, while owner 1 sends its two
/// 1-sat uncommons (one of each year) to owner 7 in one 2-sat output,
/// which leaves owner 7 a holder of both years. Returns the deltas,
/// oldest first.
fn cross_the_boundary(
  tracker: &mut Tracker,
  chain: &mut Chain,
  boundary: u32,
  new_year: u32,
) -> Vec<BlockDelta> {
  let reward = subsidy(u64::from(boundary));
  let mut deltas = Vec::new();
  let mut coinbases = Vec::new();
  for i in 0..6u8 {
    let cb = txid(100 + u64::from(i));
    coinbases.push(cb);
    let block = next_block(
      chain,
      boundary,
      new_year,
      vec![coinbase(
        cb,
        vec![out_to(1, 1 + i % 3), out(reward - 2), out_to(1, 10 + i)],
      )],
    );
    deltas.push(tracker.apply_block(&block).unwrap());
  }
  // Owner 1 got the uncommons of blocks 0 and 3: one of each year.
  let spend = txid(200);
  let block = next_block(
    chain,
    boundary,
    new_year,
    vec![
      coinbase(txid(300), vec![out(reward)]),
      tx(
        spend,
        &[(op(coinbases[0], 0), 1), (op(coinbases[3], 0), 1)],
        vec![out_to(2, 7)],
      ),
    ],
  );
  deltas.push(tracker.apply_block(&block).unwrap());
  deltas
}

fn year_index(year: u32) -> usize {
  (year - u32::from(FIRST_YEAR)) as usize
}

#[test]
fn year_rows_follow_the_new_year() {
  let (boundary, new_year) = (32_490, 1_262_304_000);
  let (mut tracker, mut chain) = tracker_before(boundary, new_year);
  assert_eq!(tracker.year_starts(), [0]);
  let deltas = cross_the_boundary(&mut tracker, &mut chain, boundary, new_year);
  assert_eq!(tracker.year_starts(), [0, boundary]);
  tracker.verify().unwrap();
  check_years(&tracker);

  let uncommon = id(&tracker, "uncommon");
  let black = id(&tracker, "black");
  let (y2009, y2010) = (year_index(2009), year_index(2010));
  let old = tracker.year_stats(uncommon, y2009);
  let new = tracker.year_stats(uncommon, y2010);
  // 2009's three uncommons are owner 7's, 2's and 3's; 2010's four owner
  // 7's, 2's, 3's and 1's (owner 7 holds one of each year).
  assert_eq!((old.mined, old.located, old.holders), (3, 3, 3));
  assert_eq!((new.mined, new.located, new.holders), (4, 4, 4));
  let all = tracker.row_stats(uncommon);
  assert_eq!((all.mined, all.located, all.holders), (7, 7, 4));
  assert_eq!(tracker.year_stats(black, y2010).mined, 4);

  // The 2009 rows are the year rows of 2009, counter for counter.
  for (row, year_of) in [
    ("uncommon_2009", "uncommon"),
    ("black_2009", "black"),
    ("rare_2009", "rare"),
    ("palindrome_2d_2009", "palindrome_2d"),
  ] {
    assert_eq!(
      tracker.row_stats(id(&tracker, row)),
      tracker.year_stats(id(&tracker, year_of), y2009),
      "{row}"
    );
  }

  // The holders of any set of years, from the masks.
  let supply = tracker.supply(10);
  let years = &supply.years.rows[usize::from(uncommon)];
  assert_eq!(years.holders(1 << y2009, false), 3);
  assert_eq!(years.holders(1 << y2010, false), 4);
  assert_eq!(years.holders((1 << y2009) | (1 << y2010), false), 4);
  assert_eq!(
    years
      .cells
      .iter()
      .map(|(year, _)| *year)
      .collect::<Vec<_>>(),
    [y2009, y2010]
  );
  let (_, cell) = &years.cells[1];
  assert_eq!(
    cell.row,
    year_row(tracker.catalog().rows().len(), uncommon, y2010)
  );
  assert_eq!(cell.top_non_extracted_holders.len(), 4);

  // Undone, the year and its rows go away.
  for delta in deltas.iter().rev() {
    tracker.undo_block(delta).unwrap();
    tracker.verify().unwrap();
    check_years(&tracker);
  }
  assert_eq!(tracker.year_starts(), [0]);
  assert_eq!(tracker.year_stats(uncommon, y2010), RowStats::default());
  assert_eq!(tracker.year_stats(uncommon, y2009), RowStats::default());
}

#[test]
fn extracted_holders_by_year() {
  // 2023 starts at 769,787 on mainnet, after the ordinals era's start:
  // 1-sat outputs are extracted.
  let (boundary, new_year) = (769_787, 1_672_531_200);
  let (mut tracker, mut chain) = tracker_before(boundary, new_year);
  assert_eq!(tracker.year_starts().len(), year_index(2022) + 1);
  cross_the_boundary(&mut tracker, &mut chain, boundary, new_year);
  assert_eq!(tracker.year_starts().len(), year_index(2023) + 1);
  assert_eq!(tracker.year_starts()[year_index(2023)], boundary);
  tracker.verify().unwrap();
  check_years(&tracker);

  let uncommon = id(&tracker, "uncommon");
  let (y2022, y2023) = (year_index(2022), year_index(2023));
  let old = tracker.year_stats(uncommon, y2022);
  let new = tracker.year_stats(uncommon, y2023);
  // Owner 7's 2-sat output of two uncommons is extracted too; owner 1's
  // whole-subsidy output is not.
  assert_eq!(
    (old.located, old.extracted, old.extracted_holders),
    (3, 3, 3)
  );
  assert_eq!(
    (new.located, new.extracted, new.extracted_holders),
    (4, 3, 3)
  );
  let supply = tracker.supply(10);
  let years = &supply.years.rows[usize::from(uncommon)];
  assert_eq!(years.holders(1 << y2023, false), 4);
  assert_eq!(years.holders((1 << y2022) | (1 << y2023), true), 3);
  assert_eq!(tracker.row_stats(uncommon).extracted_holders, 3);
  let (_, cell) = &years.cells[1];
  assert_eq!(cell.top_extracted_holders.len(), 3);
  assert_eq!(cell.top_non_extracted_holders.len(), 1);
}

#[test]
fn a_restored_tracker_has_the_same_years() {
  let (boundary, new_year) = (32_490, 1_262_304_000);
  let (mut tracker, mut chain) = tracker_before(boundary, new_year);
  cross_the_boundary(&mut tracker, &mut chain, boundary, new_year);
  let restored = Tracker::restore(
    tracker.catalog().clone(),
    tracker.config().clone(),
    tracker.snapshot(),
  )
  .unwrap();
  assert_eq!(restored.year_starts(), tracker.year_starts());
  assert_eq!(restored.supply(10), tracker.supply(10));
}
