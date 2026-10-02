//! Every kind of history entry, from real tracker deltas.

use {
  super::*,
  crate::{
    model::{Sale, SaleKind, TrackerConfig},
    tracker::{
      Tracker,
      tests::{Chain, coinbase, op, op_return, out_to, script, tracker_at, tx, txid},
    },
  },
  bitcoin::{OutPoint, ScriptBuf},
};

/// Blocks from 840,000 on (2024, so small outputs are extracted) track
/// exactly their first (uncommon; 840,000's is an epic) and last (black)
/// sats.
const H: u32 = 840_000;
const S: u64 = 312_500_000;
const F: u64 = 1_968_750_000_000_000;

fn first(height: u32) -> u64 {
  F + u64::from(height - H) * S
}

fn black(height: u32) -> u64 {
  first(height) + S - 1
}

struct Fixture {
  tracker: Tracker,
  chain: Chain,
}

impl Fixture {
  fn new() -> Self {
    let (tracker, chain) = tracker_at(&TrackerConfig::default(), Some(H - 1));
    Self { tracker, chain }
  }

  /// Applies a block (a coinbase paying `S` to script 99 first) and
  /// returns its entries.
  fn block(
    &mut self,
    txs: Vec<crate::model::ResolvedTx>,
    sales: Vec<Sale>,
  ) -> (BlockDelta, MarketDelta, Vec<HistoryEntry>) {
    self.block_with_coinbase(
      coinbase(txid(1000 + u64::from(self.chain.next)), vec![out_to(S, 99)]),
      txs,
      sales,
    )
  }

  fn block_with_coinbase(
    &mut self,
    cb: crate::model::ResolvedTx,
    txs: Vec<crate::model::ResolvedTx>,
    sales: Vec<Sale>,
  ) -> (BlockDelta, MarketDelta, Vec<HistoryEntry>) {
    let mut all = vec![cb];
    all.extend(txs);
    let block = self.chain.block(all);
    let delta = self.tracker.apply_block(&block).unwrap();
    let market = MarketDelta {
      sales,
      inscriptions: Vec::new(),
    };
    let tracker = &self.tracker;
    let entries = block_entries(&delta, &market, &**tracker.catalog(), &|height| {
      tracker.time(height)
    });
    assert_eq!(
      entries
        .iter()
        .map(|entry| (entry.key, entry.tx_index))
        .collect::<Vec<_>>(),
      block_keys(&delta),
      "block_keys names every entry"
    );
    (delta, market, entries)
  }

  fn row(&self, id: &str) -> RowId {
    self.tracker.catalog().row_id(id).unwrap()
  }
}

fn entry(entries: &[HistoryEntry], owner: u8, tx_index: u32) -> &HistoryEntry {
  let key = history_key(&script(owner));
  entries
    .iter()
    .find(|entry| entry.key == key && entry.tx_index == tx_index)
    .unwrap_or_else(|| panic!("no entry for script {owner} in tx {tx_index}: {entries:#?}"))
}

fn sale(txid: Txid, seller: (OutPoint, &crate::model::TrackedUtxo), buyer: u8) -> Sale {
  Sale {
    txid,
    tx_index: 1,
    vin: 0,
    kind: SaleKind::Listing,
    marketplace: "magisat".into(),
    seller_outpoint: seller.0,
    seller: seller.1.clone(),
    payment_vout: Some(1),
    price: 90_000,
    seller_proceeds: 90_000,
    fees: 0,
    buyer_vout: Some(0),
    buyer_script: Some(script(buyer)),
  }
}

#[test]
fn every_kind() {
  let mut f = Fixture::new();
  let (epic, uncommon, black_row) = (f.row("epic"), f.row("uncommon"), f.row("black"));

  // Block H: the coinbase pays the epic to script 1 in 100,000 sats (not
  // extracted) and the black to script 3 in 1 sat (extracted).
  let cb = txid(1);
  let (_, _, entries) = f.block_with_coinbase(
    coinbase(
      cb,
      vec![out_to(100_000, 1), out_to(S - 100_001, 9), out_to(1, 3)],
    ),
    Vec::new(),
    Vec::new(),
  );
  let mined = entry(&entries, 1, 0);
  assert_eq!(mined.kind, HistoryKind::Mined);
  assert_eq!(mined.direction, Direction::In);
  assert_eq!(mined.received.rows, [(epic, 1)]);
  assert_eq!((mined.received.sats, mined.received.value), (1, 100_000));
  assert!(!mined.extracted_in);
  assert_eq!((mined.isolated, mined.counterparty.clone()), (0, None));
  assert_eq!(mined.lead_sat, Some(F));
  let mined_black = entry(&entries, 3, 0);
  assert_eq!(mined_black.kind, HistoryKind::Mined);
  assert!(mined_black.extracted_in);
  assert_eq!(mined_black.isolated, 0, "a coinbase isolates nothing");
  assert_eq!(entries.len(), 2, "script 9 received no tracked sat");

  // Block H+1, transaction 1: script 1 splits its epic into 546 sats of its
  // own (padding split): an extraction. Transaction 2: script 3 sends its
  // black to script 4: a transfer between extracted outputs. The coinbase
  // pays H+1's uncommon to script 1 in 546 sats.
  let (split, send) = (txid(2), txid(3));
  let (_, _, entries) = f.block_with_coinbase(
    coinbase(
      txid(4),
      vec![out_to(546, 1), out_to(S - 547, 9), out_to(1, 5)],
    ),
    vec![
      tx(
        split,
        &[(op(cb, 0), 100_000)],
        vec![out_to(546, 1), out_to(99_000, 1)],
      ),
      tx(send, &[(op(cb, 2), 1)], vec![out_to(1, 4)]),
    ],
    Vec::new(),
  );
  let extraction = entry(&entries, 1, 1);
  assert_eq!(extraction.kind, HistoryKind::Extraction);
  assert_eq!(extraction.direction, Direction::SelfTransfer);
  assert_eq!((extraction.isolated, extraction.extracted_sats), (1, 1));
  assert!(extraction.extracted_in && !extraction.extracted_out);
  assert_eq!(
    extraction.received.count, 1,
    "the change holds no tracked sat"
  );
  assert_eq!(extraction.received.value, 546);
  assert_eq!(extraction.sent.value, 100_000);
  assert_eq!(extraction.received.rows, [(epic, 1)]);
  assert_eq!(extraction.sent.rows, [(epic, 1)]);
  assert_eq!(
    (extraction.counterparties, extraction.counterparty.clone()),
    (0, None)
  );
  assert_eq!(extraction.txid, split);

  let sender = entry(&entries, 3, 2);
  assert_eq!(
    (sender.kind, sender.direction),
    (HistoryKind::Transfer, Direction::Out)
  );
  assert!(sender.extracted_out && !sender.extracted_in);
  assert_eq!(sender.sent.rows, [(black_row, 1)]);
  assert_eq!(sender.counterparty, Some(script(4)));
  assert_eq!(sender.counterparties, 1);
  let receiver = entry(&entries, 4, 2);
  assert_eq!(
    (receiver.kind, receiver.direction),
    (HistoryKind::Transfer, Direction::In)
  );
  assert_eq!(receiver.isolated, 0, "already isolated");
  assert_eq!(receiver.counterparty, Some(script(3)));
  assert_eq!(receiver.lead_sat, Some(black(H)));

  // Block H+2: script 1 consolidates its two extracted outputs (the epic
  // and H+1's uncommon) into one; script 4 burns the black; script 5
  // sells H+1's black to script 6 (who pays with an untracked input).
  let (merge, burn, buy) = (txid(5), txid(6), txid(7));
  let seller_utxo = f.tracker.utxo(&op(txid(4), 2)).unwrap().clone();
  let (_, _, entries) = f.block(
    vec![
      tx(
        merge,
        &[(op(split, 0), 546), (op(txid(4), 0), 546)],
        vec![out_to(900, 1)],
      ),
      tx(burn, &[(op(send, 0), 1)], vec![op_return(1)]),
      tx(
        buy,
        &[(op(txid(4), 2), 1), (op(txid(900), 0), 100_000)],
        vec![out_to(1, 6), out_to(90_000, 5)],
      ),
    ],
    vec![Sale {
      tx_index: 3,
      ..sale(buy, (op(txid(4), 2), &seller_utxo), 6)
    }],
  );
  let consolidation = entry(&entries, 1, 1);
  assert_eq!(
    (consolidation.kind, consolidation.direction),
    (HistoryKind::Consolidation, Direction::SelfTransfer)
  );
  assert_eq!(
    (consolidation.sent.count, consolidation.received.count),
    (2, 1)
  );
  assert_eq!(consolidation.isolated, 0);
  assert_eq!(consolidation.received.rows, [(uncommon, 1), (epic, 1)]);
  assert_eq!(consolidation.lead_sat, Some(F), "the epic outranks");

  let burner = entry(&entries, 4, 2);
  assert_eq!(
    (burner.kind, burner.direction, burner.burned),
    (HistoryKind::Burn, Direction::Out, 1)
  );
  assert_eq!(
    (burner.counterparties, burner.counterparty.clone()),
    (0, None)
  );
  assert_eq!(entries.iter().filter(|e| e.tx_index == 2).count(), 1);

  let seller = entry(&entries, 5, 3);
  assert_eq!(
    (seller.kind, seller.direction, seller.seller, seller.buyer),
    (HistoryKind::Sale, Direction::Out, true, false)
  );
  assert_eq!(seller.counterparty, Some(script(6)));
  let buyer = entry(&entries, 6, 3);
  assert_eq!(
    (buyer.kind, buyer.direction, buyer.seller, buyer.buyer),
    (HistoryKind::Sale, Direction::In, false, true)
  );
  assert_eq!(buyer.counterparty, Some(script(5)));
  assert_eq!(buyer.sent, Side::default(), "its payment was untracked");
  assert_eq!(
    entries.iter().filter(|e| e.tx_index == 3).count(),
    2,
    "the payment output to the seller holds no tracked sat"
  );

  // Block H+3: script 6 sends the black to itself: a self transfer.
  let (_, _, entries) = f.block(
    vec![tx(txid(8), &[(op(buy, 0), 1)], vec![out_to(1, 6)])],
    Vec::new(),
  );
  let own = entry(&entries, 6, 1);
  assert_eq!(
    (own.kind, own.direction),
    (HistoryKind::Transfer, Direction::SelfTransfer)
  );
  assert_eq!((own.counterparties, own.counterparty.clone()), (0, None));
}

#[test]
fn extraction_out_of_someone_elses_output() {
  // An exchange (script 2) pays a rare sat into a small output of script 7
  // and the change back to itself: script 7 extracted it, the exchange
  // transferred.
  let mut f = Fixture::new();
  let cb = txid(1);
  f.block_with_coinbase(
    coinbase(cb, vec![out_to(1_000_000, 2), out_to(S - 1_000_000, 9)]),
    Vec::new(),
    Vec::new(),
  );
  let pay = txid(2);
  let (_, _, entries) = f.block(
    vec![tx(
      pay,
      &[(op(cb, 0), 1_000_000)],
      vec![out_to(600, 7), out_to(999_000, 2)],
    )],
    Vec::new(),
  );
  let exchange = entry(&entries, 2, 1);
  assert_eq!(
    (exchange.kind, exchange.direction),
    (HistoryKind::Transfer, Direction::Out)
  );
  assert_eq!(exchange.counterparty, Some(script(7)));
  let collector = entry(&entries, 7, 1);
  assert_eq!(
    (collector.kind, collector.direction, collector.isolated),
    (HistoryKind::Extraction, Direction::In, 1)
  );
  assert_eq!(collector.counterparty, Some(script(2)));
}

#[test]
fn main_counterparty_by_sats_then_position() {
  // Script 1's 2 tracked sats (an uncommon and a black, one output) go to
  // scripts 3 and 4, one each: the tie goes to the lower vout.
  let mut f = Fixture::new();
  let cb = txid(1);
  f.block_with_coinbase(coinbase(cb, vec![out_to(S, 1)]), Vec::new(), Vec::new());
  let (_, _, entries) = f.block(
    vec![tx(
      txid(2),
      &[(op(cb, 0), S)],
      vec![out_to(1, 4), out_to(S - 2, 1), out_to(1, 3)],
    )],
    Vec::new(),
  );
  let payer = entry(&entries, 1, 1);
  assert_eq!(payer.direction, Direction::Out, "the change holds nothing");
  assert_eq!(payer.counterparties, 2);
  assert_eq!(payer.counterparty, Some(script(4)));
}

#[test]
fn keys_ignore_unspendable_outputs_and_dedupe() {
  let mut f = Fixture::new();
  let cb = txid(1);
  f.block_with_coinbase(coinbase(cb, vec![out_to(S, 1)]), Vec::new(), Vec::new());
  let (delta, _, entries) = f.block(
    vec![tx(
      txid(2),
      &[(op(cb, 0), S)],
      vec![op_return(1), out_to(S - 2, 1), out_to(1, 1)],
    )],
    Vec::new(),
  );
  assert_eq!(block_keys(&delta).len(), 2, "script 1 once, script 99");
  let own = entry(&entries, 1, 1);
  // It burned the epic and isolated the black: extraction outranks burn.
  assert_eq!((own.burned, own.isolated), (1, 1));
  assert_eq!(own.kind, HistoryKind::Extraction);
  assert_eq!(own.received.count, 1);
}

#[test]
fn history_key_is_pinned() {
  assert_eq!(history_key(&ScriptBuf::new()), 0xe3b0_c442_98fc_1c14);
}
