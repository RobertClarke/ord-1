//! Luck rows (DESIGN.md §5.2): the sats of each row by the luck of the
//! block that mined them, and the holders of luck N or more.

use {
  super::*,
  crate::model::{LUCK_SLOTS, LuckMask, luck_row},
};

fn id(tracker: &Tracker, row: &str) -> RowId {
  tracker.catalog().row_id(row).unwrap()
}

/// Luck N or more.
fn at_least(n: usize) -> LuckMask {
  LuckMask::MAX << n
}

#[test]
fn luck_rows_and_holders() {
  let config = TrackerConfig::default();
  let (mut tracker, mut chain) = tracker_at(&config, Some(5_000));
  let reward = subsidy(5_001);
  let lucks = [0u8, 3, 5, 3, 1, 6];
  let mut deltas = Vec::new();
  let mut coinbases = Vec::new();
  // Block i pays its uncommon to owner 1 or 2 (alternately), its black to
  // owner 10 + i.
  for (i, &luck) in lucks.iter().enumerate() {
    let cb = txid(100 + i as u64);
    coinbases.push(cb);
    chain.next_luck = Some(luck);
    let block = chain.block(vec![coinbase(
      cb,
      vec![
        out_to(1, 1 + (i % 2) as u8),
        out(reward - 2),
        out_to(1, 10 + i as u8),
      ],
    )]);
    deltas.push(tracker.apply_block(&block).unwrap());
  }
  // A block of luck 2 pays its sats to owner 1; owner 1 sends the uncommons of
  // blocks 0 (luck 0) and 2 (luck 5) to owner 7 in one 2-sat output.
  chain.next_luck = Some(2);
  let block = chain.block(vec![
    coinbase(txid(300), vec![out(reward)]),
    tx(
      txid(200),
      &[(op(coinbases[0], 0), 1), (op(coinbases[2], 0), 1)],
      vec![out_to(2, 7)],
    ),
  ]);
  deltas.push(tracker.apply_block(&block).unwrap());
  assert_eq!(&tracker.lucks()[5_001..], [0, 3, 5, 3, 1, 6, 2]);
  tracker.verify().unwrap();
  check_lucks(&tracker);
  check_years(&tracker);
  check_months(&tracker);

  let uncommon = id(&tracker, "uncommon");
  let mined = |luck| tracker.luck_stats(uncommon, luck).mined;
  assert_eq!(
    (0..8).map(mined).collect::<Vec<_>>(),
    [1, 1, 1, 2, 0, 1, 1, 0]
  );
  assert_eq!(tracker.luck_stats(uncommon, 3).holders, 1);
  assert_eq!(tracker.luck_stats(id(&tracker, "black"), 3).mined, 2);

  // Holders of luck N or more: owner 1 (lucks 1, and 2: the last
  // coinbase's output), owner 2 (3, 3, 6) and owner 7 (0, 5).
  let supply = tracker.supply(10);
  let rows = tracker.catalog().rows().len();
  let lucks = &supply.lucks.rows[usize::from(uncommon)];
  assert_eq!(
    (0..8)
      .map(|n| lucks.holders(at_least(n), false))
      .collect::<Vec<_>>(),
    [3, 3, 3, 2, 2, 2, 1, 0]
  );
  assert_eq!(lucks.holders(0b11_1000, false), 2);
  assert_eq!(lucks.holders(0b1, false), 1);
  assert_eq!(
    lucks
      .cells
      .iter()
      .map(|(luck, _)| *luck)
      .collect::<Vec<_>>(),
    [0, 1, 2, 3, 5, 6]
  );
  assert_eq!(lucks.cells[3].1.row, luck_row(rows, uncommon, 3));
  assert!(lucks.cells[3].1.top_non_extracted_holders.is_empty());
  assert_eq!(supply.lucks.heights.len(), 5_008);

  // Restored from its snapshot, the tracker has the same supply.
  let restored = Tracker::restore(
    tracker.catalog().clone(),
    config.clone(),
    tracker.snapshot(),
  )
  .unwrap();
  assert_eq!(restored.supply(10), supply);

  // Undone, the luck rows empty again.
  for delta in deltas.iter().rev() {
    tracker.undo_block(delta).unwrap();
    tracker.verify().unwrap();
    check_lucks(&tracker);
  }
  assert_eq!(tracker.lucks().len(), 5_001);
  for luck in 0..LUCK_SLOTS {
    assert_eq!(tracker.luck_stats(uncommon, luck), RowStats::default());
  }
}

#[test]
fn a_piece_across_blocks_counts_each_block_luck() {
  // Block 9's range runs into block 10's uncommon: one piece, two lucks.
  let config = TrackerConfig::default();
  let (mut tracker, mut chain) = tracker_at(&config, Some(8));
  let reward = subsidy(9);
  let mut coinbases = Vec::new();
  for luck in [4u8, 7] {
    let cb = txid(10 + u64::from(luck));
    coinbases.push(cb);
    chain.next_luck = Some(luck);
    let block = chain.block(vec![coinbase(cb, vec![out(reward)])]);
    tracker.apply_block(&block).unwrap();
  }
  // Block 9's last sats and block 10's first in one output: the merged
  // piece spans the two blocks.
  chain.next_luck = Some(0);
  let block = chain.block(vec![
    coinbase(txid(50), vec![out(reward)]),
    tx(
      txid(51),
      &[(op(coinbases[0], 0), reward), (op(coinbases[1], 0), reward)],
      vec![out(reward - 5), out_to(10, 3), out(reward - 5)],
    ),
  ]);
  tracker.apply_block(&block).unwrap();
  let moved = tracker.utxo(&op(txid(51), 1)).unwrap();
  assert_eq!(moved.pieces.len(), 1, "{moved:?}");
  tracker.verify().unwrap();
  check_lucks(&tracker);
  let block9 = id(&tracker, "block9");
  let uncommon = id(&tracker, "uncommon");
  assert_eq!(tracker.luck_stats(block9, 4).located, reward);
  assert_eq!(tracker.luck_stats(block9, 7).located, 0);
  assert_eq!(tracker.luck_stats(uncommon, 7).located, 1);
  assert_eq!(tracker.luck_stats(uncommon, 4).located, 1);
}
