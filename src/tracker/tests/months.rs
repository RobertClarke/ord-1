//! Month rows (DESIGN.md §5.3): the sats of each row by mint month, across
//! a month's boundary inside a year.

use {
  super::*,
  crate::model::{month_index_of_time, month_row},
};

/// Blocks ten minutes apart, the first of the month starting at `start`
/// mined at height `boundary`.
fn time_of(boundary: u32, start: u32, height: u32) -> u32 {
  if height < boundary {
    start - (boundary - height) * 600
  } else {
    start + (height - boundary) * 600
  }
}

fn id(tracker: &Tracker, row: &str) -> RowId {
  tracker.catalog().row_id(row).unwrap()
}

#[test]
fn month_rows_follow_the_new_month() {
  // 2012-03-01T00:00:00Z, after a leap day.
  let (boundary, start) = (170_000, 1_330_560_000);
  let config = TrackerConfig::default();
  let catalog = catalog_for(&config);
  let tip = boundary - 4;
  let mut chain = Chain::at(Some(tip));
  let snapshot = TrackerSnapshot {
    height: Some(tip),
    hash: Some(chain.prev),
    times: (0..=tip).map(|h| time_of(boundary, start, h)).collect(),
    lucks: vec![0; tip as usize + 1],
    mined: vec![0; crate::model::tracked_row_ids(catalog.rows().len())],
    ..TrackerSnapshot::default()
  };
  let mut tracker = Tracker::restore(catalog, config, snapshot).unwrap();
  let (feb, mar) = (month_index_of_time(start - 1), month_index_of_time(start));
  assert_eq!((feb, mar), (37, 38));
  assert_eq!(tracker.month_starts().len(), feb + 1);

  // Six blocks across the boundary, each paying its uncommon to owner
  // 1 + i % 3 and its black to owner 10 + i; then owner 1 sends its two
  // uncommons (one of each month) to owner 7 in one 2-sat output.
  let reward = subsidy(u64::from(boundary));
  let mut deltas = Vec::new();
  let mut coinbases = Vec::new();
  for i in 0..6u8 {
    let cb = txid(100 + u64::from(i));
    coinbases.push(cb);
    chain.next_time = Some(time_of(boundary, start, chain.next));
    let block = chain.block(vec![coinbase(
      cb,
      vec![out_to(1, 1 + i % 3), out(reward - 2), out_to(1, 10 + i)],
    )]);
    deltas.push(tracker.apply_block(&block).unwrap());
  }
  chain.next_time = Some(time_of(boundary, start, chain.next));
  let block = chain.block(vec![
    coinbase(txid(300), vec![out(reward)]),
    tx(
      txid(200),
      &[(op(coinbases[0], 0), 1), (op(coinbases[3], 0), 1)],
      vec![out_to(2, 7)],
    ),
  ]);
  deltas.push(tracker.apply_block(&block).unwrap());
  assert_eq!(tracker.month_starts().len(), mar + 1);
  assert_eq!(tracker.month_starts()[mar], boundary);
  tracker.verify().unwrap();
  check_years(&tracker);
  check_months(&tracker);

  let uncommon = id(&tracker, "uncommon");
  let old = tracker.month_stats(uncommon, feb);
  let new = tracker.month_stats(uncommon, mar);
  // February's three uncommons (the blocks below the boundary) are owner
  // 7's, 2's and 3's; March's four owner 7's, 2's, 3's and 1's.
  assert_eq!((old.mined, old.located, old.holders), (3, 3, 3));
  assert_eq!((new.mined, new.located, new.holders), (4, 4, 4));
  let year = tracker.year_stats(uncommon, 3);
  assert_eq!((year.mined, year.located, year.holders), (7, 7, 4));

  let supply = tracker.supply(10);
  let months = &supply.months.rows[usize::from(uncommon)];
  assert_eq!(
    months.cells.iter().map(|(m, _)| *m).collect::<Vec<_>>(),
    [feb, mar]
  );
  let (_, cell) = &months.cells[1];
  assert_eq!(
    cell.row,
    month_row(tracker.catalog().rows().len(), uncommon, mar)
  );
  assert_eq!(cell.top_non_extracted_holders.len(), 4);
  // Owner 7 holds one of each month: in both months' top holders.
  for (_, cell) in &months.cells {
    assert!(
      cell
        .top_non_extracted_holders
        .iter()
        .any(|holder| holder.script_pubkey == script(7) && holder.count == 1 && holder.utxos == 1)
    );
  }

  // Undone, the month and its rows go away.
  for delta in deltas.iter().rev() {
    tracker.undo_block(delta).unwrap();
    tracker.verify().unwrap();
    check_months(&tracker);
  }
  assert_eq!(tracker.month_starts().len(), feb + 1);
  assert_eq!(tracker.month_stats(uncommon, mar), RowStats::default());
  assert_eq!(tracker.month_stats(uncommon, feb), RowStats::default());
}
