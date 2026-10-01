//! Sales and inscriptions on real tracker deltas.

use {
  super::*,
  crate::{
    model::{Envelope, InscriptionEvent, ResolvedInput, ResolvedTx, Sale, SaleKind, TrackerConfig},
    tracker::{
      Tracker,
      tests::{Chain, catalog_for, coinbase, op, out_to, script, tracker_at, txid},
    },
  },
  bitcoin::{OutPoint, ScriptBuf, TxOut, Txid},
};

/// Blocks from 840,000 have exactly two tracked sats: the first (uncommon,
/// and epic at 840,000) and the last (black) sat of the subsidy.
const H: u32 = 840_000;
const S: u64 = 312_500_000;
const F: u64 = 1_968_750_000_000_000;

const MAGISAT_FEE: u8 = 50;
const MAGIC_EDEN_FEE: u8 = 51;
const BUYER: u8 = 9;

fn context() -> MarketContext {
  let fee = |n: u8| script(n).to_hex_string();
  MarketContext::new(
    catalog_for(&TrackerConfig::default()),
    Marketplaces::from_json(&format!(
      r#"{{ "marketplaces": [
        {{ "id": "magisat", "fee_addresses": [{{ "script_pubkey": "{}" }}] }},
        {{ "id": "magic_eden", "fee_only_sales": true,
           "fee_addresses": [{{ "script_pubkey": "{}", "to_height": 900000 }}] }} ] }}"#,
      fee(MAGISAT_FEE),
      fee(MAGIC_EDEN_FEE)
    ))
    .unwrap(),
    crate::settings::Chain::Mainnet,
  )
}

fn input(previous_output: OutPoint, value: u64, sighash: Option<u8>) -> ResolvedInput {
  ResolvedInput {
    previous_output,
    value,
    sighash,
    envelopes: Vec::new(),
  }
}

fn inscribing(mut input: ResolvedInput, pointers: &[Option<u64>]) -> ResolvedInput {
  input.envelopes = pointers
    .iter()
    .map(|&pointer| Envelope {
      pointer,
      unrecognized_even_field: false,
    })
    .collect();
  input
}

fn tx(txid: Txid, inputs: Vec<ResolvedInput>, outputs: Vec<TxOut>) -> ResolvedTx {
  ResolvedTx {
    txid,
    inputs,
    outputs,
  }
}

/// A padding UTXO the tracker has never seen.
fn untracked(n: u64, value: u64) -> ResolvedInput {
  input(op(txid(1_000_000 + n), 0), value, Some(0x01))
}

struct Market {
  tracker: Tracker,
  chain: Chain,
  ctx: MarketContext,
}

impl Market {
  /// The tracker after block 840,000 (whose epic first sat is paid away
  /// with the rest of its coinbase).
  fn new() -> Self {
    let (tracker, chain) = tracker_at(&TrackerConfig::default(), Some(H - 1));
    let mut market = Self {
      tracker,
      chain,
      ctx: context(),
    };
    market.apply(Vec::new());
    market
  }

  /// Mines a block whose coinbase pays its uncommon into a 546-sat output
  /// of `owner`: (that output, its value, the uncommon sat).
  fn mine_rare(&mut self, owner: u8) -> (OutPoint, u64, u64) {
    let height = self.chain.next;
    let cb = txid(600 + u64::from(height));
    let block = self.chain.block(vec![coinbase(
      cb,
      vec![out_to(546, owner), out_to(S - 546, 100)],
    )]);
    self.tracker.apply_block(&block).unwrap();
    (op(cb, 0), 546, F + u64::from(height - H) * S)
  }

  /// Applies a block with these transactions (and a coinbase claiming
  /// their fees) and analyzes it.
  fn apply(&mut self, txs: Vec<ResolvedTx>) -> MarketDelta {
    let fees: u64 = txs
      .iter()
      .map(|tx| {
        tx.inputs.iter().map(|i| i.value).sum::<u64>()
          - tx.outputs.iter().map(|o| o.value.to_sat()).sum::<u64>()
      })
      .sum();
    let mut all = vec![coinbase(
      txid(700 + u64::from(self.chain.next)),
      vec![out_to(S + fees, 100)],
    )];
    all.extend(txs);
    let block = self.chain.block(all);
    let delta = self.tracker.apply_block(&block).unwrap();
    analyze(&block, &delta, &self.ctx)
  }
}

#[test]
fn single_listing() {
  let mut market = Market::new();
  let (x, value, sold) = market.mine_rare(1);
  let seller = market.tracker.utxo(&x).unwrap().clone();
  // Two padding inputs, the listing (SINGLE|ANYONECANPAY, vin 2), the
  // buyer's payment; outputs: padding merged, the buyer's ordinal output,
  // the seller's payment (vout 2, paired with vin 2), the fee, change.
  let buy = txid(1);
  let delta = market.apply(vec![tx(
    buy,
    vec![
      untracked(1, 600),
      untracked(2, 600),
      input(x, value, Some(LISTING_SIGHASH)),
      untracked(3, 100_000),
    ],
    vec![
      out_to(1200, BUYER),
      out_to(546, BUYER),
      out_to(45_000, 1),
      out_to(1_000, MAGISAT_FEE),
      out_to(100_000 - 45_000 - 1_000 - 500, BUYER),
    ],
  )]);
  assert_eq!(
    delta.sales,
    [Sale {
      txid: buy,
      tx_index: 1,
      vin: 2,
      kind: SaleKind::Listing,
      marketplace: "magisat".into(),
      seller_outpoint: x,
      seller: seller.clone(),
      payment_vout: Some(2),
      price: 46_000,
      seller_proceeds: 45_000,
      fees: 1_000,
      buyer_vout: Some(1),
      buyer_script: Some(script(BUYER)),
    }]
  );
  assert_eq!(seller.pieces[0].start, sold);
  assert!(delta.inscriptions.is_empty());
  // Block 840,001's first sat is uncommon only: one uncommon for 45,454
  // sats above the output's own 546.
  let (row, count, unit) = sale_unit(&delta.sales[0], &market.ctx.catalog).unwrap();
  assert_eq!(market.ctx.catalog.rows()[usize::from(row)].id, "uncommon");
  assert_eq!((count, unit), (1, 46_000 - 546));
  assert_eq!(sale_sats(&delta.sales[0], &market.ctx.catalog, 100), [sold]);
}

#[test]
fn bulk_buy_of_three_listings() {
  let mut market = Market::new();
  let listings: Vec<(OutPoint, u64)> = (1..=3)
    .map(|owner| {
      let (outpoint, value, _) = market.mine_rare(owner);
      (outpoint, value)
    })
    .collect();
  let buy = txid(2);
  let mut inputs = vec![untracked(1, 600), untracked(2, 600)];
  inputs.extend(
    listings
      .iter()
      .map(|&(outpoint, value)| input(outpoint, value, Some(LISTING_SIGHASH))),
  );
  inputs.push(untracked(3, 300_000));
  let delta = market.apply(vec![tx(
    buy,
    inputs,
    vec![
      out_to(1200, BUYER),
      out_to(3 * 546, BUYER),
      out_to(30_000, 1),
      out_to(20_000, 2),
      out_to(10_000, 3),
      out_to(1_200, MAGISAT_FEE),
      out_to(300_000 - 60_000 - 1_200 - 900, BUYER),
    ],
  )]);
  // (vin, payment, proceeds, fees, price, buyer) per sale.
  for (sale, expected) in delta.sales.iter().zip([
    (2, 30_000, 600, 30_600),
    (3, 20_000, 400, 20_400),
    (4, 10_000, 200, 10_200),
  ]) {
    let (vin, proceeds, fees, price) = expected;
    assert_eq!(
      (
        sale.vin,
        sale.payment_vout,
        sale.seller_proceeds,
        sale.fees,
        sale.price,
        sale.buyer_vout
      ),
      (vin, Some(vin), proceeds, fees, price, Some(1))
    );
  }
  assert_eq!(delta.sales.len(), 3);
  for (sale, &(outpoint, _)) in delta.sales.iter().zip(&listings) {
    assert_eq!(sale.seller_outpoint, outpoint);
    assert_eq!(sale.marketplace, "magisat");
    assert_eq!(sale.kind, SaleKind::Listing);
  }
}

#[test]
fn transfers_are_not_sales() {
  let mut market = Market::new();
  let (x, value, _) = market.mine_rare(1);
  let (y, _, _) = market.mine_rare(2);
  let delta = market.apply(vec![
    // Signed ALL, even with a marketplace fee output: a transfer.
    tx(
      txid(3),
      vec![input(x, value, Some(0x01)), untracked(1, 10_000)],
      vec![
        out_to(546, BUYER),
        out_to(1_000, MAGISAT_FEE),
        out_to(8_000, 1),
      ],
    ),
    // No signature recognized.
    tx(
      txid(4),
      vec![input(y, value, None)],
      vec![out_to(546, BUYER)],
    ),
  ]);
  assert!(delta.sales.is_empty());
  // A SINGLE|ANYONECANPAY input whose own sats land in its paired output is
  // no sale either.
  let (z, _, _) = market.mine_rare(3);
  let delta = market.apply(vec![tx(
    txid(5),
    vec![input(z, value, Some(LISTING_SIGHASH))],
    vec![out_to(546, 3)],
  )]);
  assert!(delta.sales.is_empty());
}

#[test]
fn legacy_magic_eden_sale_by_fee_address() {
  let mut market = Market::new();
  let (x, value, _) = market.mine_rare(1);
  let sale_tx = txid(6);
  // No SINGLE|ANYONECANPAY: the seller signed the whole trade. The fee
  // output names it a Magic Eden sale; the output paying the seller's own
  // script is the payment.
  let delta = market.apply(vec![tx(
    sale_tx,
    vec![input(x, value, Some(0x01)), untracked(1, 50_000)],
    vec![
      out_to(546, BUYER),
      out_to(40_000, 1),
      out_to(1_000, MAGIC_EDEN_FEE),
      out_to(50_000 - 40_000 - 1_000 - 300, BUYER),
    ],
  )]);
  let [sale] = &delta.sales[..] else {
    panic!("one sale expected: {:?}", delta.sales)
  };
  assert_eq!(
    (
      sale.kind,
      sale.marketplace.as_str(),
      sale.payment_vout,
      sale.seller_proceeds,
      sale.fees,
      sale.price,
      sale.buyer_vout
    ),
    (
      SaleKind::Unknown,
      "magic_eden",
      Some(1),
      40_000,
      1_000,
      41_000,
      Some(0)
    )
  );

  // Without an output paying the seller, the seller cannot be told apart:
  // nothing is recorded.
  let (y, _, _) = market.mine_rare(2);
  let delta = market.apply(vec![tx(
    txid(7),
    vec![input(y, value, Some(0x01)), untracked(2, 50_000)],
    vec![
      out_to(546, BUYER),
      out_to(40_000, 3),
      out_to(1_000, MAGIC_EDEN_FEE),
    ],
  )]);
  assert!(delta.sales.is_empty());

  // Without the table entry the fee output means nothing.
  market.ctx.marketplaces = Marketplaces::default();
  let (z, _, _) = market.mine_rare(3);
  let delta = market.apply(vec![tx(
    txid(8),
    vec![input(z, value, Some(0x01)), untracked(3, 50_000)],
    vec![
      out_to(546, BUYER),
      out_to(40_000, 3),
      out_to(1_000, MAGIC_EDEN_FEE),
    ],
  )]);
  assert!(delta.sales.is_empty());
}

#[test]
fn listings_without_a_known_fee_output_are_unknown_marketplace() {
  let mut market = Market::new();
  let (x, value, _) = market.mine_rare(1);
  // The listing is vin 1: its sats follow input 0's 600 into output 0, its
  // paired output 1 pays the seller.
  let delta = market.apply(vec![tx(
    txid(9),
    vec![
      untracked(1, 600),
      input(x, value, Some(LISTING_SIGHASH)),
      untracked(2, 30_000),
    ],
    vec![
      out_to(1_146, BUYER),
      out_to(20_000, 1),
      out_to(30_000 - 20_000 - 200, BUYER),
    ],
  )]);
  let [sale] = &delta.sales[..] else { panic!() };
  assert_eq!(
    (
      sale.marketplace.as_str(),
      sale.price,
      sale.fees,
      sale.payment_vout,
      sale.buyer_vout
    ),
    (UNKNOWN_MARKETPLACE, 20_000, 0, Some(1), Some(0))
  );
}

fn events(market: &MarketDelta) -> Vec<(u64, u32)> {
  market
    .inscriptions
    .iter()
    .map(|&InscriptionEvent { sat, vin, .. }| (sat, vin))
    .collect()
}

#[test]
fn inscriptions_on_tracked_sats() {
  let mut market = Market::new();
  let (x, value, rare) = market.mine_rare(1);
  // The first sat of the revealing input.
  let reveal = txid(10);
  let delta = market.apply(vec![tx(
    reveal,
    vec![inscribing(input(x, value, Some(0x00)), &[None])],
    vec![out_to(546, 1)],
  )]);
  assert_eq!(
    delta.inscriptions,
    [InscriptionEvent {
      sat: rare,
      txid: reveal,
      vin: 0
    }]
  );

  // A pointer from an untracked input onto the tracked sat of the next
  // input; then a reinscription (two envelopes) of the same sat.
  let x = op(reveal, 0);
  let delta = market.apply(vec![tx(
    txid(11),
    vec![
      inscribing(untracked(1, 1_000), &[Some(1_000)]),
      input(x, value, Some(0x00)),
    ],
    vec![out_to(1_546, 1)],
  )]);
  assert_eq!(events(&delta), [(rare, 0)]);
  let x = op(txid(11), 0);
  let delta = market.apply(vec![tx(
    txid(12),
    vec![inscribing(
      input(x, 1_546, Some(0x00)),
      &[Some(1_000), Some(1_000)],
    )],
    vec![out_to(1_546, 1)],
  )]);
  assert_eq!(events(&delta), [(rare, 0), (rare, 0)]);

  // Onto untracked sats: no event. Input 1's envelope points past the
  // outputs, so it falls back to input 1's first sat (offset 700), which is
  // untracked padding of that UTXO (the rare sat is at its offset 1,000).
  let x = op(txid(12), 0);
  let delta = market.apply(vec![tx(
    txid(13),
    vec![
      inscribing(untracked(2, 700), &[Some(1)]),
      inscribing(input(x, 1_546, Some(0x00)), &[Some(5_000)]),
    ],
    vec![out_to(2_246, 1)],
  )]);
  assert_eq!(events(&delta), []);
  // The rare sat is now at offset 1,700: a valid pointer hits it; a pointer
  // at the outputs' end falls back to offset 0, untracked.
  let x = op(txid(13), 0);
  let delta = market.apply(vec![tx(
    txid(14),
    vec![inscribing(
      input(x, 2_246, Some(0x00)),
      &[Some(1_700), Some(2_246)],
    )],
    vec![out_to(2_246, 1)],
  )]);
  assert_eq!(events(&delta), [(rare, 0)]);
}

#[test]
fn ignored_envelopes() {
  let mut market = Market::new();
  let (x, value, rare) = market.mine_rare(1);
  let delta = market.apply(vec![
    // No tracked input: not looked at.
    tx(
      txid(20),
      vec![inscribing(untracked(1, 1_000), &[None])],
      vec![out_to(1_000, 1)],
    ),
    // A zero-value input's envelope is unbound (no sat), even with a
    // pointer onto the tracked sat.
    tx(
      txid(21),
      vec![
        inscribing(untracked(2, 0), &[Some(0), None]),
        input(x, value, Some(0x00)),
      ],
      vec![out_to(546, 1)],
    ),
  ]);
  assert_eq!(events(&delta), []);
  // Valid pointer onto the tracked sat: an event for the input revealing it.
  let x = op(txid(21), 0);
  let delta = market.apply(vec![tx(
    txid(22),
    vec![
      input(x, value, Some(0x00)),
      inscribing(untracked(3, 1_000), &[Some(0)]),
    ],
    vec![out_to(1_546, 1)],
  )]);
  assert_eq!(events(&delta), [(rare, 1)]);
  // An unrecognized even field unbinds the envelope: no sat.
  let x = op(txid(22), 0);
  let mut unbound = inscribing(input(x, 1_546, Some(0x00)), &[None]);
  unbound.envelopes[0].unrecognized_even_field = true;
  let delta = market.apply(vec![tx(txid(23), vec![unbound], vec![out_to(1_546, 1)])]);
  assert_eq!(events(&delta), []);
}

#[test]
fn moves_that_do_not_match_the_block_are_ignored() {
  let mut market = Market::new();
  let (x, value, rare) = market.mine_rare(1);
  // A listing that also inscribes its sat, bought with the usual padding.
  let block = market.chain.block(vec![
    coinbase(txid(30), vec![out_to(S + 154, 100)]),
    tx(
      txid(31),
      vec![
        untracked(1, 600),
        untracked(2, 600),
        inscribing(input(x, value, Some(LISTING_SIGHASH)), &[None]),
        untracked(3, 20_000),
      ],
      vec![
        out_to(1_200, BUYER),
        out_to(546, BUYER),
        out_to(19_000, 1),
        out_to(846, BUYER),
      ],
    ),
  ]);
  let delta = market.tracker.apply_block(&block).unwrap();
  let analyzed = analyze(&block, &delta, &market.ctx);
  assert_eq!(events(&analyzed), [(rare, 2)]);
  assert_eq!(analyzed.sales.len(), 1);
  assert_eq!(analyzed.sales[0].buyer_vout, Some(1));
  // Moves naming another transaction yield nothing.
  let mut other = delta.clone();
  for m in &mut other.moves {
    m.txid = txid(999);
  }
  assert_eq!(analyze(&block, &other, &market.ctx), MarketDelta::default());
}

#[test]
fn sale_units_of_ranges_and_combinations() {
  let ctx = context();
  let catalog = &ctx.catalog;
  let sale = |pieces: Vec<crate::model::Piece>, value: u64, price: u64| Sale {
    txid: txid(1),
    tx_index: 1,
    vin: 0,
    kind: SaleKind::Listing,
    marketplace: "magisat".into(),
    seller_outpoint: op(txid(2), 0),
    seller: crate::model::TrackedUtxo {
      value,
      script_pubkey: ScriptBuf::new(),
      height: 1,
      kind: crate::model::UtxoKind::Spendable,
      pieces,
    },
    payment_vout: Some(0),
    price,
    seller_proceeds: price,
    fees: 0,
    buyer_vout: Some(1),
    buyer_script: None,
  };
  let piece = |offset, start, len| crate::model::Piece { offset, start, len };
  // A 10,000-sat 450x chunk without individually special sats (between the
  // palindromes 45,050,005,054 and 45,050,105,054): per sat.
  let chunk = sale(vec![piece(0, 45_050_006_000, 10_000)], 10_000, 1_010_000);
  let (row, count, unit) = sale_unit(&chunk, catalog).unwrap();
  assert_eq!(catalog.rows()[usize::from(row)].id, "450x");
  assert_eq!((count, unit), (10_000, 100));
  assert!(sale_sats(&chunk, catalog, 100).is_empty());
  // Block 9's first sat is a Nakamoto uncommon: the rarest of its rows.
  let first = sale(vec![piece(0, 45_000_000_000, 1)], 546, 100_546);
  let (row, count, unit) = sale_unit(&first, catalog).unwrap();
  assert_eq!(catalog.rows()[usize::from(row)].id, "nakamoto_uncommon");
  assert_eq!((count, unit), (1, 100_000));
  assert_eq!(sale_sats(&first, catalog, 100), [45_000_000_000]);
  // Price below the output's own value: no premium.
  let cheap = sale(vec![piece(0, F, 1)], 10_000, 9_000);
  assert_eq!(sale_unit(&cheap, catalog).unwrap().2, 0);
  // Whole block 9: the special sats, capped.
  let block9 = sale(
    vec![piece(0, 45_000_000_000, 5_000_000_000)],
    5_000_000_000,
    0,
  );
  assert_eq!(
    sale_sats(&block9, catalog, 3),
    [45_000_000_000, 45_000_000_054, 45_000_100_054]
  );
  // Nothing tracked: no unit.
  assert_eq!(sale_unit(&sale(Vec::new(), 1, 1), catalog), None);
}

#[test]
fn envelopes_before_the_first_inscription_height_are_not_inscriptions() {
  use crate::settings::Chain;
  assert_eq!(first_inscription_height(Chain::Mainnet), 767_430);
  assert_eq!(first_inscription_height(Chain::Regtest), 0);
  let mut market = Market::new();
  let (x, value, rare) = market.mine_rare(1);
  market.ctx.first_inscription_height = market.chain.next + 1;
  let delta = market.apply(vec![tx(
    txid(40),
    vec![inscribing(input(x, value, Some(0x00)), &[None])],
    vec![out_to(546, 1)],
  )]);
  assert_eq!(events(&delta), []);
  market.ctx.first_inscription_height = market.chain.next;
  let delta = market.apply(vec![tx(
    txid(41),
    vec![inscribing(
      input(op(txid(40), 0), value, Some(0x00)),
      &[None],
    )],
    vec![out_to(546, 1)],
  )]);
  assert_eq!(events(&delta), [(rare, 0)]);
}
