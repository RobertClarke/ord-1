//! Hand-written scenarios with exact expected locations.

use {super::*, crate::model::LocationKind};

/// Blocks 840,000..840,319 have exactly two tracked sats each: the first
/// (uncommon) and the last (black) sat of the subsidy.
const H: u32 = 840_000;
const S: u64 = 312_500_000;
const F: u64 = 1_968_750_000_000_000;

fn at_halving() -> (Tracker, Chain) {
  tracker_at(&TrackerConfig::default(), Some(H - 1))
}

fn row(tracker: &Tracker, id: &str) -> RowStats {
  tracker.row_stats(tracker.catalog().row_id(id).unwrap())
}

fn piece(offset: u64, start: u64, len: u64) -> Piece {
  Piece { offset, start, len }
}

fn located(tracker: &Tracker, sat: u64) -> (LocationKind, OutPoint, u64) {
  let location = tracker
    .locate(sat)
    .unwrap_or_else(|| panic!("sat {sat} not located"));
  assert_eq!(
    location.offset - location.piece.offset,
    sat - location.piece.start
  );
  (location.kind, location.outpoint, location.offset)
}

#[test]
fn only_boundary_sats_are_tracked_after_the_halving() {
  let catalog = catalog_for(&TrackerConfig::default());
  for h in [H, H + 1, H + 100, H + 319] {
    let first = first_sat(h.into());
    assert_eq!(
      catalog.tracked_runs(first, first + S),
      [(first, 1), (first + S - 1, 1)],
      "{h}"
    );
  }
  assert_eq!(first_sat(H.into()), F);
}

#[test]
fn coinbase_outputs_place_uncommon_and_black() {
  let (mut tracker, mut chain) = at_halving();
  let cb = txid(1);
  let block = chain.block(vec![coinbase(
    cb,
    vec![out(100_000_000), out_to(212_499_999, 2), out_to(1, 3)],
  )]);
  let delta = tracker.apply_block(&block).unwrap();

  assert_eq!(tracker.utxo(&op(cb, 0)).unwrap().pieces, [piece(0, F, 1)]);
  assert!(tracker.utxo(&op(cb, 1)).is_none());
  assert_eq!(
    tracker.utxo(&op(cb, 2)).unwrap().pieces,
    [piece(0, F + S - 1, 1)]
  );
  assert_eq!(located(&tracker, F), (LocationKind::Unspent, op(cb, 0), 0));
  assert_eq!(
    located(&tracker, F + S - 1),
    (LocationKind::Unspent, op(cb, 2), 0)
  );
  assert!(tracker.locate(F + 1).is_none());

  assert_eq!(delta.created.len(), 2);
  assert_eq!(delta.spent, []);
  assert_eq!(delta.lost_sats_before, 0);
  assert_eq!(delta.lost_sats_after, 0);
  let [cb_move] = &delta.moves[..] else {
    panic!("one move expected")
  };
  assert!(cb_move.coinbase);
  assert_eq!(cb_move.mined, [piece(0, F, 1), piece(S - 1, F + S - 1, 1)]);
  assert_eq!(
    cb_move.outputs.iter().map(|o| o.vout).collect::<Vec<_>>(),
    [0, 2]
  );

  let uncommon = row(&tracker, "uncommon");
  assert_eq!(
    (uncommon.mined, uncommon.located, uncommon.holders),
    (1, 1, 1)
  );
  assert_eq!(row(&tracker, "epic").located, 1);
  let black = row(&tracker, "black");
  // Worth 1 sat, created after 2022-12-14: extracted.
  assert_eq!(
    (black.located, black.extracted, black.extracted_holders),
    (1, 1, 1)
  );
  assert_eq!(uncommon.extracted, 0);
  let changed: Vec<&str> = delta
    .rows
    .iter()
    .map(|d| tracker.catalog().rows()[usize::from(d.row)].id.as_str())
    .collect();
  // Block 840,000's first sat is also an alpha.
  assert_eq!(changed, ["uncommon", "epic", "black", "alpha_uncommon"]);
  tracker.verify().unwrap();
}

#[test]
fn tracked_fee_sats_follow_untracked_fees() {
  let (mut tracker, mut chain) = at_halving();
  let a = txid(1);
  tracker
    .apply_block(&chain.block(vec![coinbase(a, vec![out(S)])]))
    .unwrap();

  let f1 = F + S;
  let (t1, t2, t3, cb) = (txid(11), txid(12), txid(13), txid(14));
  let block = chain.block(vec![
    coinbase(cb, vec![out(S + 1110)]),
    tx(t1, &[(op(txid(900), 0), 1000)], vec![out(900)]),
    tx(t2, &[(op(txid(901), 0), 5000)], vec![out(4000)]),
    tx(t3, &[(op(a, 0), S)], vec![out(S - 10)]),
  ]);
  let delta = tracker.apply_block(&block).unwrap();

  let t3_move = delta.moves.iter().find(|m| m.txid == t3).unwrap();
  assert_eq!(t3_move.fee, [piece(9, F + S - 1, 1)]);
  assert_eq!(t3_move.input_value, S);
  assert_eq!(t3_move.output_value, S - 10);
  assert_eq!(t3_move.outputs[0].pieces, [piece(0, F, 1)]);
  // The coinbase stream: subsidy, then 100 + 1000 sats of untracked fees,
  // then tx 3's fee, whose 10th sat is the black.
  assert_eq!(
    tracker.utxo(&op(cb, 0)).unwrap().pieces,
    [
      piece(0, f1, 1),
      piece(S - 1, f1 + S - 1, 1),
      piece(S + 1100 + 9, F + S - 1, 1)
    ]
  );
  assert_eq!(
    located(&tracker, F + S - 1),
    (LocationKind::Unspent, op(cb, 0), S + 1109)
  );
  assert_eq!(delta.spent.len(), 1);
  assert_eq!(delta.spent[0].0, op(a, 0));
  assert_eq!(delta.moves.len(), 2);
  assert!(delta.moves.last().unwrap().coinbase);
  assert_eq!(
    delta.moves.last().unwrap().mined,
    [piece(0, f1, 1), piece(S - 1, f1 + S - 1, 1)]
  );
  tracker.verify().unwrap();
}

#[test]
fn under_claiming_coinbases_lose_sats_at_ord_offsets() {
  let (mut tracker, mut chain) = at_halving();
  let delta = tracker
    .apply_block(&chain.block(vec![coinbase(txid(1), vec![out(S - 1000)])]))
    .unwrap();
  assert_eq!(delta.lost, [piece(999, F + S - 1, 1)]);
  assert_eq!((delta.lost_sats_before, delta.lost_sats_after), (0, 1000));
  assert_eq!(
    located(&tracker, F + S - 1),
    (LocationKind::Lost, OutPoint::null(), 999)
  );

  // Only an untracked fee sat lost: no lost piece, but the counter moves.
  let delta = tracker
    .apply_block(&chain.block(vec![
      coinbase(txid(2), vec![out(S + 4), out(0)]),
      tx(txid(20), &[(op(txid(900), 1), 10)], vec![out(5)]),
    ]))
    .unwrap();
  assert_eq!(delta.lost, []);
  assert_eq!(tracker.lost_sats(), 1001);

  // Everything unclaimed: both boundary sats lost, in stream order.
  let f2 = F + 2 * S;
  let delta = tracker
    .apply_block(&chain.block(vec![coinbase(txid(3), vec![op_return(0)])]))
    .unwrap();
  assert_eq!(
    delta.lost,
    [piece(1001, f2, 1), piece(1001 + S - 1, f2 + S - 1, 1)]
  );
  assert_eq!(delta.moves[0].lost, delta.lost);
  assert_eq!(tracker.lost_sats(), 1001 + S);
  assert_eq!(
    located(&tracker, f2 + S - 1),
    (LocationKind::Lost, OutPoint::null(), 1001 + S - 1)
  );
  assert_eq!(row(&tracker, "black").lost, 2);
  assert_eq!(row(&tracker, "uncommon").lost, 1);
  assert_eq!(row(&tracker, "uncommon").mined, 3);
  tracker.verify().unwrap();
}

#[test]
fn op_return_and_oversized_outputs_burn_zero_value_outputs_get_nothing() {
  let (mut tracker, mut chain) = at_halving();
  let a = txid(1);
  tracker
    .apply_block(&chain.block(vec![coinbase(a, vec![out(S)])]))
    .unwrap();
  let t = txid(2);
  let delta = tracker
    .apply_block(&chain.block(vec![
      coinbase(txid(3), vec![out(S)]),
      tx(
        t,
        &[(op(a, 0), S)],
        vec![
          op_return(0),
          out(0),
          op_return(1),
          huge_script(10),
          out(S - 11),
        ],
      ),
    ]))
    .unwrap();
  assert!(tracker.utxo(&op(t, 0)).is_none());
  assert!(tracker.utxo(&op(t, 1)).is_none());
  let burned = tracker.utxo(&op(t, 2)).unwrap();
  assert_eq!(burned.kind, UtxoKind::Unspendable);
  assert_eq!(burned.pieces, [piece(0, F, 1)]);
  assert!(tracker.utxo(&op(t, 3)).is_none());
  assert_eq!(
    tracker.utxo(&op(t, 4)).unwrap().pieces,
    [piece(S - 12, F + S - 1, 1)]
  );
  assert_eq!(located(&tracker, F), (LocationKind::Burned, op(t, 2), 0));
  let uncommon = row(&tracker, "uncommon");
  assert_eq!((uncommon.burned, uncommon.located), (1, 1));
  assert!(delta.created.iter().any(|(o, _)| *o == op(t, 2)));

  // A later spend of the burned output is inconsistent input: an error, and
  // the state is untouched (earlier transactions of the block are rolled
  // back too).
  let before = tracker.snapshot();
  let stats: Vec<RowStats> = (0..tracker.catalog().rows().len() as RowId)
    .map(|r| tracker.row_stats(r))
    .collect();
  let bad = chain.block(vec![
    coinbase(txid(4), vec![out(S)]),
    tx(txid(5), &[(op(t, 4), S - 11)], vec![out(S - 11)]),
    tx(txid(6), &[(op(t, 2), 1)], vec![out(1)]),
  ]);
  let err = tracker.apply_block(&bad).unwrap_err();
  assert!(format!("{err:#}").contains("unspendable"), "{err:#}");
  assert_eq!(tracker.snapshot(), before);
  assert_eq!(
    (0..tracker.catalog().rows().len() as RowId)
      .map(|r| tracker.row_stats(r))
      .collect::<Vec<_>>(),
    stats
  );
  tracker.verify().unwrap();
}

#[test]
fn inconsistent_blocks_are_errors_not_panics() {
  let (mut tracker, mut chain) = at_halving();
  let a = txid(1);
  tracker
    .apply_block(&chain.block(vec![coinbase(a, vec![out(S)])]))
    .unwrap();
  let before = tracker.snapshot();
  let next = chain.next;
  let prev = chain.prev;
  let mut attempt = |txs: Vec<ResolvedTx>| {
    let mut c = Chain::at(Some(next - 1));
    c.prev = prev;
    tracker.apply_block(&c.block(txs)).unwrap_err()
  };
  // Outputs exceed inputs.
  attempt(vec![
    coinbase(txid(2), vec![out(S)]),
    tx(txid(3), &[(op(txid(99), 0), 10)], vec![out(11)]),
  ]);
  // The coinbase claims more than subsidy + fees.
  attempt(vec![coinbase(txid(2), vec![out(S + 1)])]);
  // A tracked input with the wrong value.
  attempt(vec![
    coinbase(txid(2), vec![out(S)]),
    tx(txid(3), &[(op(a, 0), S - 1)], vec![out(1)]),
  ]);
  // No inputs; coinbase with inputs; no transactions.
  attempt(vec![
    coinbase(txid(2), vec![out(S)]),
    tx(txid(3), &[], vec![]),
  ]);
  attempt(vec![tx(txid(3), &[(op(txid(99), 0), 1)], vec![])]);
  attempt(vec![]);
  // Overflowing values.
  attempt(vec![
    coinbase(txid(2), vec![out(S)]),
    tx(
      txid(3),
      &[(op(txid(98), 0), u64::MAX), (op(txid(99), 0), 1)],
      vec![],
    ),
  ]);
  assert_eq!(tracker.snapshot(), before);

  // Wrong height, parent or hash.
  let mut wrong = Chain::at(Some(next));
  assert!(
    tracker
      .apply_block(&wrong.block(vec![coinbase(txid(2), vec![])]))
      .is_err()
  );
  let mut orphan = Chain::at(Some(next - 1));
  assert!(
    tracker
      .apply_block(&orphan.block(vec![coinbase(txid(2), vec![])]))
      .is_err()
  );
  let mut good = chain.block(vec![coinbase(txid(2), vec![out(S)])]);
  good.hash = BlockHash::all_zeros();
  assert!(tracker.apply_block(&good).is_err());
  assert_eq!(tracker.snapshot(), before);
  tracker.verify().unwrap();
}

#[test]
fn intra_block_chains() {
  let (mut tracker, mut chain) = at_halving();
  let a = txid(1);
  tracker
    .apply_block(&chain.block(vec![coinbase(a, vec![out(S)])]))
    .unwrap();
  let (b, c, d, cb) = (txid(2), txid(3), txid(4), txid(5));
  let delta = tracker
    .apply_block(&chain.block(vec![
      coinbase(cb, vec![out(S + 100)]),
      tx(b, &[(op(a, 0), S)], vec![out(1000), out(S - 1000)]),
      tx(
        c,
        &[(op(b, 0), 1000), (op(txid(77), 3), 500)],
        vec![out(1500)],
      ),
      tx(d, &[(op(c, 0), 1500)], vec![out_to(1400, 9)]),
    ]))
    .unwrap();
  assert_eq!(
    delta.spent.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
    [op(a, 0)]
  );
  let created: Vec<OutPoint> = delta.created.iter().map(|(o, _)| *o).collect();
  assert_eq!(created, [op(b, 1), op(d, 0), op(cb, 0)]);
  assert!(tracker.utxo(&op(b, 0)).is_none());
  assert!(tracker.utxo(&op(c, 0)).is_none());
  assert_eq!(located(&tracker, F), (LocationKind::Unspent, op(d, 0), 0));
  assert_eq!(
    located(&tracker, F + S - 1),
    (LocationKind::Unspent, op(b, 1), S - 1 - 1000)
  );
  assert_eq!(delta.moves.len(), 4);
  assert_eq!(delta.moves[1].inputs[0].outpoint, op(b, 0));
  assert_eq!(delta.moves[1].inputs[0].vin, 0);
  // Holders follow the uncommon to script 9.
  let uncommon = row(&tracker, "uncommon");
  assert_eq!((uncommon.located, uncommon.holders), (2, 2));

  // Undo restores A, redo reproduces the delta.
  let before = tracker.snapshot();
  let block = chain.block(vec![coinbase(txid(6), vec![out(S)])]);
  let next = tracker.apply_block(&block).unwrap();
  tracker.undo_block(&next).unwrap();
  assert_eq!(tracker.snapshot(), before);
  tracker.undo_block(&delta).unwrap();
  assert_eq!(located(&tracker, F), (LocationKind::Unspent, op(a, 0), 0));
  assert_eq!(tracker.height(), Some(H));
  tracker.verify().unwrap();
}

#[test]
fn bip30_duplicate_coinbase_overwrites() {
  let (mut tracker, mut chain) = at_halving();
  let t = txid(1);
  tracker
    .apply_block(&chain.block(vec![coinbase(t, vec![out(S)])]))
    .unwrap();
  let old = tracker.utxo(&op(t, 0)).unwrap().clone();
  let before = tracker.snapshot();
  let delta = tracker
    .apply_block(&chain.block(vec![coinbase(t, vec![out(S)])]))
    .unwrap();
  assert_eq!(delta.overwritten, [(op(t, 0), old.clone())]);
  assert_eq!(
    located(&tracker, F),
    (LocationKind::Overwritten, op(t, 0), 0)
  );
  assert_eq!(
    located(&tracker, F + S),
    (LocationKind::Unspent, op(t, 0), 0)
  );
  assert_eq!(tracker.utxo(&op(t, 0)).unwrap().height, H + 1);
  let uncommon = row(&tracker, "uncommon");
  assert_eq!(
    (uncommon.located, uncommon.overwritten, uncommon.mined),
    (1, 1, 2)
  );
  assert_eq!(tracker.snapshot().overwritten, [(op(t, 0), H + 1, old)]);
  let restored = Tracker::restore(
    tracker.catalog().clone(),
    tracker.config().clone(),
    tracker.snapshot(),
  )
  .unwrap();
  assert_eq!(restored.snapshot(), tracker.snapshot());
  assert_eq!(located(&restored, F), located(&tracker, F));
  tracker.verify().unwrap();

  tracker.undo_block(&delta).unwrap();
  assert_eq!(tracker.snapshot(), before);
  assert_eq!(located(&tracker, F), (LocationKind::Unspent, op(t, 0), 0));
  tracker.verify().unwrap();
}

#[test]
fn block9_pieces_split_and_merge() {
  let config = TrackerConfig::default();
  let (mut tracker, mut chain) = tracker_at(&config, Some(8));
  let a = txid(1);
  let first = tracker
    .apply_block(&chain.block(vec![coinbase(a, vec![out(5_000_000_000)])]))
    .unwrap();
  let b9 = 45_000_000_000;
  assert_eq!(first.moves[0].mined, [piece(0, b9, 5_000_000_000)]);
  assert_eq!(
    tracker.utxo(&op(a, 0)).unwrap().pieces,
    [piece(0, b9, 5_000_000_000)]
  );
  assert_eq!(row(&tracker, "block9").located, 5_000_000_000);
  assert_eq!(row(&tracker, "450x").located, 100_000_000);
  assert_eq!(row(&tracker, "block9_palindrome").located, 50_000);
  assert_eq!(tracker.piece_count(), 1);

  // Block 10 splits it across three outputs; 1000 sats go to the fee.
  let (split, cb10) = (txid(2), txid(3));
  chain.next_time = Some(1_700_000_000);
  let delta = tracker
    .apply_block(&chain.block(vec![
      coinbase(cb10, vec![out(5_000_001_000)]),
      tx(
        split,
        &[(op(a, 0), 5_000_000_000)],
        vec![
          out(1_000_000_000),
          out_to(2_500_000_000, 2),
          out_to(20_000, 3),
          out(1_499_979_000),
        ],
      ),
    ]))
    .unwrap();
  let pieces = |vout| tracker.utxo(&op(split, vout)).unwrap().pieces.clone();
  assert_eq!(pieces(0), [piece(0, b9, 1_000_000_000)]);
  assert_eq!(pieces(1), [piece(0, b9 + 1_000_000_000, 2_500_000_000)]);
  assert_eq!(pieces(2), [piece(0, b9 + 3_500_000_000, 20_000)]);
  assert_eq!(pieces(3), [piece(0, b9 + 3_500_020_000, 1_499_979_000)]);
  assert_eq!(delta.moves[0].fee, [piece(0, 50_000_000_000 - 1000, 1000)]);
  // The fee lands after block 10's own tracked subsidy sats.
  let cb = tracker.utxo(&op(cb10, 0)).unwrap();
  assert_eq!(
    *cb.pieces.last().unwrap(),
    piece(5_000_000_000, 50_000_000_000 - 1000, 1000)
  );
  assert_eq!(
    located(&tracker, b9 + 3_500_000_123),
    (LocationKind::Unspent, op(split, 2), 123)
  );
  assert_eq!(
    located(&tracker, 49_999_999_999),
    (LocationKind::Unspent, op(cb10, 0), 5_000_000_999)
  );
  let block9 = row(&tracker, "block9");
  assert_eq!(block9.located, 5_000_000_000);
  // The split's outputs hold nothing but block 9 sats, within their budget
  // (run length + 10,000): extracted although worth more than 10,000 sats.
  // The coinbase output (1000 block 9 sats and block 10's individually
  // tracked sats in 5e9) is not.
  assert_eq!(block9.extracted, 4_999_999_000);
  assert_eq!(block9.extracted_holders, 3);
  assert_eq!(block9.holders, 3);
  // Output 0 is extracted as a whole, so for every row it holds: its 450x
  // sats count as extracted too.
  let x450 = row(&tracker, "450x");
  assert_eq!((x450.located, x450.extracted), (100_000_000, 100_000_000));
  assert_eq!(row(&tracker, "uncommon").extracted, 1);
  tracker.verify().unwrap();
  let mut deltas = vec![delta];

  // Spending outputs 0 and 1 in order merges them back into one piece;
  // in the other order they stay two pieces.
  let (merge, swap) = (txid(4), txid(5));
  let before_merge = tracker.snapshot();
  let delta = tracker
    .apply_block(&chain.block(vec![
      coinbase(txid(6), vec![out(5_000_000_000)]),
      tx(
        merge,
        &[(op(split, 0), 1_000_000_000), (op(split, 1), 2_500_000_000)],
        vec![out(3_500_000_000)],
      ),
      tx(
        swap,
        &[(op(split, 3), 1_499_979_000), (op(split, 2), 20_000)],
        vec![out(1_499_999_000)],
      ),
    ]))
    .unwrap();
  assert_eq!(
    tracker.utxo(&op(merge, 0)).unwrap().pieces,
    [piece(0, b9, 3_500_000_000)]
  );
  assert_eq!(
    tracker.utxo(&op(swap, 0)).unwrap().pieces,
    [
      piece(0, b9 + 3_500_020_000, 1_499_979_000),
      piece(1_499_979_000, b9 + 3_500_000_000, 20_000)
    ]
  );
  assert_eq!(
    located(&tracker, b9 + 3_499_999_999),
    (LocationKind::Unspent, op(merge, 0), 3_499_999_999)
  );
  assert_eq!(row(&tracker, "block9").located, 5_000_000_000);
  tracker.verify().unwrap();

  // Undo back to the empty tip at height 8.
  tracker.undo_block(&delta).unwrap();
  assert_eq!(tracker.snapshot(), before_merge);
  tracker.undo_block(&deltas.pop().unwrap()).unwrap();
  tracker.undo_block(&first).unwrap();
  assert_eq!(
    tracker.snapshot(),
    tracker_at(&config, Some(8)).0.snapshot()
  );
  assert_eq!(tracker.utxo_count(), 0);
  assert_eq!(row(&tracker, "block9"), RowStats::default());
  tracker.verify().unwrap();
}

#[test]
fn extraction_uses_block_time_and_value() {
  let (mut tracker, mut chain) = at_halving();
  // Before 2022-12-14: never extracted.
  chain.next_time = Some(1_670_975_999);
  let early = txid(1);
  tracker
    .apply_block(&chain.block(vec![coinbase(early, vec![out(1), out(S - 2), out(1)])]))
    .unwrap();
  assert_eq!(row(&tracker, "uncommon").extracted, 0);
  assert_eq!(row(&tracker, "black").extracted, 0);
  // At the threshold: small outputs are extracted.
  chain.next_time = Some(1_670_976_000);
  tracker
    .apply_block(&chain.block(vec![coinbase(txid(2), vec![out(10_000), out(S - 10_000)])]))
    .unwrap();
  assert_eq!(row(&tracker, "uncommon").extracted, 1);
  assert_eq!(row(&tracker, "black").extracted, 0);
  // A header time below the previous one still counts by its own time.
  chain.next_time = Some(1_670_975_000);
  tracker
    .apply_block(&chain.block(vec![coinbase(txid(3), vec![out(1), out(S - 1)])]))
    .unwrap();
  assert_eq!(row(&tracker, "uncommon").extracted, 1);
  tracker.verify().unwrap();
}

#[test]
fn supply_active_windows_and_holders() {
  let (mut tracker, mut chain) = at_halving();
  let year = 365 * 86_400;
  // Above every synthetic history time, so the running max is ours.
  let base = 1_800_000_000u32;
  // Blocks 8, 6, 4, 2 and 0 years before the tip time, each paying its
  // uncommon to script h and its black to script 100.
  let mut times = Vec::new();
  for (i, age) in [8u32, 6, 4, 2, 0].into_iter().enumerate() {
    let time = base + (8 - age) * year;
    times.push(time);
    chain.next_time = Some(time);
    tracker
      .apply_block(&chain.block(vec![coinbase(
        txid(i as u64 + 1),
        vec![out_to(20_000, i as u8 + 1), out_to(S - 20_000, 100)],
      )]))
      .unwrap();
  }
  let supply = tracker.supply(3);
  assert_eq!(supply.height, Some(H + 4));
  assert_eq!(supply.time, Some(base + 8 * year));
  let uncommon = &supply.rows[usize::from(tracker.catalog().row_id("uncommon").unwrap())];
  assert_eq!(uncommon.stats.located, 5);
  assert_eq!(uncommon.stats.holders, 5);
  // 20,000-sat outputs holding one row sat: not extracted.
  assert_eq!(uncommon.stats.extracted, 0);
  assert_eq!(
    uncommon.active_non_extracted,
    [(1, 1), (2, 2), (3, 2), (4, 3), (5, 3), (7, 4)]
  );
  assert_eq!(uncommon.top_non_extracted_holders.len(), 3);
  assert!(uncommon.top_extracted_holders.is_empty());
  let black = &supply.rows[usize::from(tracker.catalog().row_id("black").unwrap())];
  assert_eq!(black.stats.holders, 1);
  assert_eq!(black.top_non_extracted_holders.len(), 1);
  let holder = &black.top_non_extracted_holders[0];
  assert_eq!(
    (
      holder.script_pubkey.clone(),
      holder.count,
      holder.utxos,
      holder.last_received_height
    ),
    (script(100), 5, 5, H + 4)
  );
  // Ties break by script bytes: scripts 1, 2, 3 of the five.
  assert_eq!(
    uncommon
      .top_non_extracted_holders
      .iter()
      .map(|h| h.script_pubkey.clone())
      .collect::<Vec<_>>(),
    [script(1), script(2), script(3)]
  );
  assert_eq!(tracker.supply(0).rows[0].top_non_extracted_holders, []);
  // Restore rebuilds the same supply.
  let restored = Tracker::restore(
    tracker.catalog().clone(),
    tracker.config().clone(),
    tracker.snapshot(),
  )
  .unwrap();
  assert_eq!(restored.supply(3), tracker.supply(3));
  assert_eq!(times.len(), 5);
}

#[test]
fn reorg_to_a_competing_block() {
  let (mut tracker, mut chain) = at_halving();
  let a = txid(1);
  tracker
    .apply_block(&chain.block(vec![coinbase(a, vec![out(S)])]))
    .unwrap();
  let base = tracker.snapshot();
  let (parent, next) = (chain.prev, chain.next);

  // Block X spends A to script 2 and pays the coinbase to script 3.
  let x = chain.block(vec![
    coinbase(txid(2), vec![out_to(S + 10, 3)]),
    tx(txid(3), &[(op(a, 0), S)], vec![out_to(S - 10, 2)]),
  ]);
  let delta_x = tracker.apply_block(&x).unwrap();
  assert_eq!(located(&tracker, F).1, op(txid(3), 0));

  // The competitor Y at the same height spends A differently.
  tracker.undo_block(&delta_x).unwrap();
  assert_eq!(tracker.snapshot(), base);
  let mut fork = Chain::at(Some(next - 1));
  fork.prev = parent;
  fork.next_time = Some(chain.time_of(next) + 1);
  let y = fork.block(vec![
    coinbase(txid(4), vec![out_to(S - 7, 5)]),
    tx(
      txid(5),
      &[(op(a, 0), S)],
      vec![out_to(1, 6), out_to(S - 1, 7)],
    ),
  ]);
  assert_ne!(x.hash, y.hash);
  let delta_y = tracker.apply_block(&y).unwrap();
  assert_eq!(
    located(&tracker, F),
    (LocationKind::Unspent, op(txid(5), 0), 0)
  );
  assert_eq!(tracker.hash(), Some(y.hash));
  // The coinbase under-claims by 7: the black of Y's block is lost.
  assert_eq!(delta_y.lost.len(), 1);
  assert_eq!(tracker.lost_sats(), 7);
  assert_eq!(row(&tracker, "uncommon").extracted, 1);
  tracker.verify().unwrap();
  // X no longer extends the tip.
  assert!(tracker.apply_block(&x).is_err());
}

#[test]
fn restore_rejects_inconsistent_snapshots() {
  let (tracker, chain) = at_halving();
  let catalog = tracker.catalog().clone();
  let config = tracker.config().clone();
  let base = tracker.snapshot();
  let utxo = |pieces: Vec<Piece>| TrackedUtxo {
    value: S,
    script_pubkey: script(1),
    height: H - 1,
    kind: UtxoKind::Spendable,
    pieces,
  };
  let with = |utxos: Vec<(OutPoint, TrackedUtxo)>| TrackerSnapshot {
    utxos,
    ..base.clone()
  };
  let restore = |snapshot| Tracker::restore(catalog.clone(), config.clone(), snapshot);
  // Sats not mined yet at the tip.
  assert!(restore(with(vec![(op(txid(1), 0), utxo(vec![piece(0, F, 1)]))])).is_err());
  // The same sat twice.
  let sat = F - 1;
  assert!(
    restore(with(vec![
      (op(txid(1), 0), utxo(vec![piece(0, sat, 1)])),
      (op(txid(2), 0), utxo(vec![piece(5, sat, 1)])),
    ]))
    .is_err()
  );
  // Unmerged, overlapping or out-of-value pieces.
  for pieces in [
    vec![piece(0, sat - 1, 1), piece(1, sat, 1)],
    vec![piece(0, sat - 1, 2), piece(1, sat - 10, 1)],
    vec![piece(S, sat, 1)],
    vec![],
  ] {
    assert!(restore(with(vec![(op(txid(1), 0), utxo(pieces))])).is_err());
  }
  // Wrong kind, UTXO above the tip, row count mismatch, times.
  let mut burned = utxo(vec![piece(0, sat, 1)]);
  burned.kind = UtxoKind::Unspendable;
  assert!(restore(with(vec![(op(txid(1), 0), burned)])).is_err());
  let mut future = utxo(vec![piece(0, sat, 1)]);
  future.height = H;
  assert!(restore(with(vec![(op(txid(1), 0), future)])).is_err());
  assert!(
    restore(TrackerSnapshot {
      mined: vec![0],
      ..base.clone()
    })
    .is_err()
  );
  assert!(
    restore(TrackerSnapshot {
      times: vec![0],
      ..base.clone()
    })
    .is_err()
  );
  // A valid one restores.
  let ok = restore(with(vec![(op(txid(1), 0), utxo(vec![piece(7, sat, 1)]))])).unwrap();
  assert_eq!(ok.locate(sat).unwrap().offset, 7);
  assert_eq!(chain.next, H);
}

#[test]
fn tracker_is_send() {
  fn assert_send<T: Send>() {}
  assert_send::<Tracker>();
}

#[test]
fn undo_rejects_foreign_deltas_and_genesis_needs_a_null_parent() {
  let (mut tracker, mut chain) = at_halving();
  let first = tracker
    .apply_block(&chain.block(vec![coinbase(txid(1), vec![out(S)])]))
    .unwrap();
  let second = tracker
    .apply_block(&chain.block(vec![coinbase(txid(2), vec![out(S)])]))
    .unwrap();
  let before = tracker.snapshot();
  // Not the tip.
  assert!(tracker.undo_block(&first).is_err());
  // The tip's height and hash, but other contents.
  let mut forged = second.clone();
  forged.created[0].1.value += 1;
  assert!(tracker.undo_block(&forged).is_err());
  let mut forged = second.clone();
  forged.lost_sats_after += 1;
  assert!(tracker.undo_block(&forged).is_err());
  assert_eq!(tracker.snapshot(), before);
  tracker.undo_block(&second).unwrap();
  tracker.undo_block(&first).unwrap();
  assert_eq!(tracker.height(), Some(H - 1));

  let config = TrackerConfig::default();
  let mut empty = Tracker::new(catalog_for(&config), config);
  let mut not_genesis = Chain::at(Some(0));
  not_genesis.next = 0;
  let block = not_genesis.block(vec![coinbase(txid(3), vec![out(5_000_000_000)])]);
  assert!(empty.apply_block(&block).is_err());
  let mut genesis = Chain::at(None);
  let block = genesis.block(vec![coinbase(txid(3), vec![out(5_000_000_000)])]);
  let delta = empty.apply_block(&block).unwrap();
  assert_eq!(empty.height(), Some(0));
  empty.undo_block(&delta).unwrap();
  assert_eq!((empty.height(), empty.hash()), (None, None));
  assert_eq!(empty.utxo_count(), 0);
  empty.verify().unwrap();
}

/// Extraction by budget (DESIGN.md §5): the UTXO's verdict, per the shared
/// rule, and the row counters it drives.
fn extracted(tracker: &Tracker, outpoint: OutPoint) -> bool {
  let utxo = tracker.utxo(&outpoint).unwrap();
  crate::model::is_extracted(utxo, tracker.time(utxo.height).unwrap(), tracker.config())
}

/// Mines `blocks` blocks paying each uncommon and black into its own
/// 330-sat output (scripts 1 and 3); returns those outputs.
fn mine_padded_sats(tracker: &mut Tracker, chain: &mut Chain, blocks: u64) -> Vec<(OutPoint, u64)> {
  let mut outputs = Vec::new();
  for i in 0..blocks {
    let cb = txid(10_000 + i);
    tracker
      .apply_block(&chain.block(vec![coinbase(
        cb,
        vec![out_to(330, 1), out_to(S - 660, 2), out_to(330, 3)],
      )]))
      .unwrap();
    assert!(extracted(tracker, op(cb, 0)) && extracted(tracker, op(cb, 2)));
    outputs.push((op(cb, 0), 330));
    outputs.push((op(cb, 2), 330));
  }
  outputs
}

#[test]
fn collector_consolidation_is_extracted() {
  // 200 rare sats at 330 sats of padding each, consolidated into 66,000
  // sats (like a collector's address): within the
  // 2,000,000-sat budget, so extracted, where the old per-row rule
  // (value ≤ 10,000 or all sats of the row) said no.
  let (mut tracker, mut chain) = at_halving();
  let inputs = mine_padded_sats(&mut tracker, &mut chain, 100);
  assert_eq!(row(&tracker, "uncommon").extracted, 100);
  let consolidation = txid(1);
  tracker
    .apply_block(&chain.block(vec![
      coinbase(txid(2), vec![out(S)]),
      tx(consolidation, &inputs, vec![out_to(66_000, 7)]),
    ]))
    .unwrap();
  let utxo = tracker.utxo(&op(consolidation, 0)).unwrap();
  // Each block's black merges with the next block's uncommon (consecutive
  // sats at consecutive offsets): 101 pieces of 200 sats. The budget counts
  // sats, not pieces.
  assert_eq!(utxo.pieces.len(), 101);
  assert_eq!(utxo.pieces.iter().map(|p| p.len).sum::<u64>(), 200);
  assert_eq!(
    crate::model::extraction_budget(&utxo.pieces, tracker.config()),
    2_000_000
  );
  assert!(extracted(&tracker, op(consolidation, 0)));
  for id in ["uncommon", "black"] {
    let stats = row(&tracker, id);
    assert_eq!((stats.extracted, stats.extracted_holders), (100, 1), "{id}");
  }
  // The newest coinbase's uncommon sits in a whole-subsidy output.
  assert_eq!(row(&tracker, "uncommon").located, 101);
  let supply = tracker.supply(2);
  let uncommon = &supply.rows[usize::from(tracker.catalog().row_id("uncommon").unwrap())];
  assert_eq!(uncommon.top_extracted_holders[0].script_pubkey, script(7));
  assert_eq!(
    (
      uncommon.top_extracted_holders[0].count,
      uncommon.top_extracted_holders[0].utxos
    ),
    (100, 1)
  );
  assert_eq!(uncommon.active_non_extracted[0], (1, 1));

  // Seven sats in 70,001 sats is one sat over budget.
  let (mut tracker, mut chain) = at_halving();
  let inputs = mine_padded_sats(&mut tracker, &mut chain, 4);
  let over = txid(3);
  tracker
    .apply_block(&chain.block(vec![
      coinbase(txid(4), vec![out(S)]),
      tx(
        over,
        &[&inputs[..7], &[(op(txid(900), 0), 70_001 - 7 * 330)]].concat(),
        vec![out_to(70_001, 7)],
      ),
    ]))
    .unwrap();
  let utxo = tracker.utxo(&op(over, 0)).unwrap();
  assert_eq!(utxo.pieces.iter().map(|p| p.len).sum::<u64>(), 7);
  assert!(!extracted(&tracker, op(over, 0)));
  // The eighth sat, still alone in its 330 sats, is.
  assert!(extracted(&tracker, inputs[7].0));
  assert_eq!(
    row(&tracker, "uncommon").extracted + row(&tracker, "black").extracted,
    1
  );
  tracker.verify().unwrap();
}

#[test]
fn near_pure_block9_output_is_extracted() {
  // 100,000 block 9 sats behind 438 sats of padding: budget
  // 100,000 + 10,000 = 110,000 ≥ 100,438.
  let config = TrackerConfig::default();
  let (mut tracker, mut chain) = tracker_at(&config, Some(8));
  let a = txid(1);
  tracker
    .apply_block(&chain.block(vec![coinbase(a, vec![out(5_000_000_000)])]))
    .unwrap();
  let (split, cb) = (txid(2), txid(3));
  chain.next_time = Some(1_700_000_000);
  tracker
    .apply_block(&chain.block(vec![
      coinbase(cb, vec![out(5_000_000_000)]),
      tx(
        split,
        &[
          (op(txid(900), 0), 438),
          (op(a, 0), 5_000_000_000),
          (op(txid(901), 0), 1_000_000_000),
        ],
        vec![out_to(100_438, 2), out_to(5_999_900_000, 3)],
      ),
    ]))
    .unwrap();
  let chunk = tracker.utxo(&op(split, 0)).unwrap();
  assert_eq!(chunk.pieces, [piece(438, 45_000_000_000, 100_000)]);
  assert!(extracted(&tracker, op(split, 0)));
  // The rest of block 9 shares its output with 1 BTC of other sats: over
  // its 4,999,910,000-sat budget.
  assert!(!extracted(&tracker, op(split, 1)));
  let block9 = row(&tracker, "block9");
  assert_eq!(
    (block9.located, block9.extracted, block9.extracted_holders),
    (5_000_000_000, 100_000, 1)
  );
  assert_eq!(row(&tracker, "450x").extracted, 100_000);
  // Block 9's first sat and the palindrome 45,000,000,054 are in the chunk:
  // every row of it is extracted.
  assert_eq!(row(&tracker, "uncommon").extracted, 1);
  assert_eq!(row(&tracker, "block9_palindrome").extracted, 1);
  assert_eq!(row(&tracker, "block9_palindrome").located, 50_000);
  tracker.verify().unwrap();

  // The same chunk worth 110,001 sats would not be.
  let mut over = tracker.utxo(&op(split, 0)).unwrap().clone();
  over.value = 110_001;
  assert!(!crate::model::is_extracted(&over, 1_700_000_000, &config));
  over.value = 110_000;
  assert!(crate::model::is_extracted(&over, 1_700_000_000, &config));
}

#[test]
fn exchange_output_with_two_uncommons_is_not_extracted() {
  let (mut tracker, mut chain) = at_halving();
  let mut uncommons = Vec::new();
  for i in 0..2 {
    let cb = txid(20 + i);
    tracker
      .apply_block(&chain.block(vec![coinbase(cb, vec![out(1000), out(S - 1000)])]))
      .unwrap();
    uncommons.push((op(cb, 0), 1000));
  }
  assert_eq!(row(&tracker, "uncommon").extracted, 2);
  let exchange = txid(1);
  tracker
    .apply_block(&chain.block(vec![
      coinbase(txid(2), vec![out(S)]),
      tx(
        exchange,
        &[
          uncommons[0],
          uncommons[1],
          (op(txid(900), 0), 100_000_000 - 2000),
        ],
        vec![out_to(100_000_000, 9)],
      ),
    ]))
    .unwrap();
  let utxo = tracker.utxo(&op(exchange, 0)).unwrap();
  assert_eq!(utxo.pieces.len(), 2);
  assert_eq!(
    crate::model::extraction_budget(&utxo.pieces, tracker.config()),
    20_000
  );
  assert!(!extracted(&tracker, op(exchange, 0)));
  let uncommon = row(&tracker, "uncommon");
  assert_eq!(
    (
      uncommon.located,
      uncommon.extracted,
      uncommon.extracted_holders
    ),
    (3, 0, 0)
  );
  tracker.verify().unwrap();
}

#[test]
fn piece_straddling_the_block9_edge() {
  // Block 9's black (range-tracked) and block 10's uncommon (individually
  // tracked) end up as one piece: budget 1 + 10,000 + 10,000.
  let config = TrackerConfig::default();
  let (mut tracker, mut chain) = tracker_at(&config, Some(8));
  let (a9, a10) = (txid(9), txid(10));
  tracker
    .apply_block(&chain.block(vec![coinbase(a9, vec![out(5_000_000_000)])]))
    .unwrap();
  tracker
    .apply_block(&chain.block(vec![coinbase(a10, vec![out(5_000_000_000)])]))
    .unwrap();
  let (split9, split10) = (txid(11), txid(12));
  chain.next_time = Some(1_700_000_000);
  tracker
    .apply_block(&chain.block(vec![
      coinbase(txid(13), vec![out(5_000_000_000)]),
      tx(
        split9,
        &[(op(a9, 0), 5_000_000_000)],
        vec![out(4_999_999_999), out(1)],
      ),
      tx(
        split10,
        &[(op(a10, 0), 5_000_000_000)],
        vec![out(1), out(4_999_999_999)],
      ),
    ]))
    .unwrap();
  let black9 = 50_000_000_000 - 1;
  for (value, expected) in [(20_001, true), (20_002, false)] {
    let (mut tracker, mut chain) = (
      Tracker::restore(
        tracker.catalog().clone(),
        config.clone(),
        tracker.snapshot(),
      )
      .unwrap(),
      Chain {
        next: chain.next,
        prev: chain.prev,
        next_time: Some(1_700_000_600),
      },
    );
    let edge = txid(14);
    tracker
      .apply_block(&chain.block(vec![
        coinbase(txid(15), vec![out(5_000_000_000)]),
        tx(
          edge,
          &[
            (op(txid(900), 0), value - 2),
            (op(split9, 1), 1),
            (op(split10, 0), 1),
          ],
          vec![out_to(value, 4)],
        ),
      ]))
      .unwrap();
    assert_eq!(
      tracker.utxo(&op(edge, 0)).unwrap().pieces,
      [piece(value - 2, black9, 2)]
    );
    assert_eq!(extracted(&tracker, op(edge, 0)), expected, "{value}");
    assert_eq!(
      row(&tracker, "block9").extracted,
      // split9's first output is pure block 9 and extracted too.
      4_999_999_999 + u64::from(expected)
    );
    assert_eq!(row(&tracker, "black").extracted, u64::from(expected));
    tracker.verify().unwrap();
  }
}

#[test]
fn unspendable_outputs_are_never_extracted() {
  let (mut tracker, mut chain) = at_halving();
  let a = txid(1);
  chain.next_time = Some(1_700_000_000);
  tracker
    .apply_block(&chain.block(vec![coinbase(a, vec![op_return(1), out(S - 1)])]))
    .unwrap();
  let burned = tracker.utxo(&op(a, 0)).unwrap();
  assert_eq!(burned.kind, UtxoKind::Unspendable);
  assert!(!extracted(&tracker, op(a, 0)));
  let uncommon = row(&tracker, "uncommon");
  assert_eq!(
    (
      uncommon.burned,
      uncommon.located,
      uncommon.extracted,
      uncommon.extracted_holders
    ),
    (1, 0, 0, 0)
  );
  assert_eq!(
    tracker.supply(1).rows[usize::from(tracker.catalog().row_id("uncommon").unwrap())]
      .top_extracted_holders,
    []
  );
  tracker.verify().unwrap();
}
