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
  assert_eq!(changed, ["uncommon", "epic", "black"]);
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
  // The split's outputs hold nothing but block 9 sats: extracted for the
  // block9 row although they are worth more than 10,000 sats. The coinbase
  // output (1000 block 9 sats in 5e9) is not.
  assert_eq!(block9.extracted, 4_999_999_000);
  assert_eq!(block9.extracted_holders, 3);
  assert_eq!(block9.holders, 3);
  // For the 450x row, output 0 is not all 450x sats.
  let x450 = row(&tracker, "450x");
  assert_eq!((x450.located, x450.extracted), (100_000_000, 0));
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
  let _ = times;
}
