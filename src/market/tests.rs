//! Sales and inscriptions on real tracker deltas.

use {
  super::*,
  crate::{
    model::{
      Envelope, InscriptionEvent, ResolvedInput, ResolvedTx, Sale, SaleKind, TrackerConfig,
      script_hash,
    },
    tracker::{
      Tracker,
      tests::{Chain, catalog_for, coinbase, op, out_to, script, tracker_at, txid},
    },
  },
  bitcoin::{Amount, OutPoint, ScriptBuf, TxOut, Txid, consensus},
};

/// Blocks from 840,000 have exactly two tracked sats: the first (uncommon,
/// and epic at 840,000) and the last (black) sat of the subsidy.
const H: u32 = 840_000;
const S: u64 = 312_500_000;
const F: u64 = 1_968_750_000_000_000;

const MAGISAT_FEE: u8 = 50;
const MAGIC_EDEN_FEE: u8 = 51;
const SATFLOW_FEE: u8 = 52;
const PASSTHROUGH_FEE: u8 = 53;
const LENDING_FEE: u8 = 54;
const ANONYMOUS_FEE: u8 = 55;
const TOOL_FEE: u8 = 56;
const BUYER: u8 = 9;

/// Satflow's OP_RETURN "SATFLOW".
const MARKER: &str = "6a07534154464c4f57";

fn marketplaces() -> Marketplaces {
  let fee = |n: u8| script(n).to_hex_string();
  Marketplaces::from_json(&format!(
    r#"{{ "marketplaces": [
      {{ "id": "magisat", "price_rule": "magisat",
         "fee_addresses": [{{ "script_pubkey": "{}" }}] }},
      {{ "id": "magic_eden",
         "fee_addresses": [{{ "script_pubkey": "{}", "to_height": 900000 }}] }},
      {{ "id": "satflow", "flows": ["secure_purchase_two_tx"],
         "fee_addresses": [{{ "script_pubkey": "{}" }}],
         "markers": [{{ "op_return": "{MARKER}" }}] }},
      {{ "id": "ord_net", "flows": ["passthrough_2of2"],
         "fee_addresses": [{{ "script_pubkey": "{}" }}] }},
      {{ "id": "liquidium", "role": "lending",
         "fee_addresses": [{{ "script_pubkey": "{}" }}] }},
      {{ "id": "unknown_venue", "anonymous": true,
         "fee_addresses": [{{ "script_pubkey": "{}" }}] }},
      {{ "id": "tx_dev", "role": "buying_tool",
         "fee_addresses": [{{ "script_pubkey": "{}" }}] }},
      {{ "id": "unisat" }} ] }}"#,
    fee(MAGISAT_FEE),
    fee(MAGIC_EDEN_FEE),
    fee(SATFLOW_FEE),
    fee(PASSTHROUGH_FEE),
    fee(LENDING_FEE),
    fee(ANONYMOUS_FEE),
    fee(TOOL_FEE),
  ))
  .unwrap()
}

fn context() -> MarketContext {
  MarketContext::new(
    catalog_for(&TrackerConfig::default()),
    marketplaces(),
    crate::settings::Chain::Mainnet,
  )
}

/// An input of an owner nobody else shares (its outpoint stands for its
/// script); `Market::apply` gives tracked inputs their real script's hash.
fn input(previous_output: OutPoint, value: u64, sighash: Option<u8>) -> ResolvedInput {
  ResolvedInput {
    previous_output,
    value,
    script_hash: script_hash(&ScriptBuf::from_bytes(consensus::serialize(
      &previous_output,
    ))),
    sighash,
    envelopes: Vec::new(),
  }
}

/// `input` spent from `owner`'s script.
fn owned(mut input: ResolvedInput, owner: u8) -> ResolvedInput {
  input.script_hash = script_hash(&script(owner));
  input
}

/// A 2-of-3 style escrow output (P2WSH) of `n`.
fn escrow(value: u64, n: u8) -> TxOut {
  let mut bytes = vec![0x00, 0x20];
  bytes.extend([n; 32]);
  TxOut {
    value: Amount::from_sat(value),
    script_pubkey: ScriptBuf::from_bytes(bytes),
  }
}

fn marker() -> TxOut {
  TxOut {
    value: Amount::ZERO,
    script_pubkey: ScriptBuf::from_hex(MARKER).unwrap(),
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
    Self::after(H)
  }

  /// The tracker after block `height` (840,000 to 840,000 + 2015, where
  /// blocks carry exactly their first and last sats).
  fn after(height: u32) -> Self {
    let (tracker, chain) = tracker_at(&TrackerConfig::default(), Some(height - 1));
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

  /// Mines a block whose coinbase pays its whole subsidy, with its uncommon
  /// first sat and black last sat, to `owner`: (that output, its value, the
  /// two sats).
  fn mine_whole(&mut self, owner: u8) -> (OutPoint, u64, [u64; 2]) {
    let height = self.chain.next;
    let cb = txid(800 + u64::from(height));
    let block = self.chain.block(vec![coinbase(cb, vec![out_to(S, owner)])]);
    self.tracker.apply_block(&block).unwrap();
    let first = F + u64::from(height - H) * S;
    (op(cb, 0), S, [first, first + S - 1])
  }

  /// Applies a block with these transactions (and a coinbase claiming
  /// their fees) and analyzes it.
  fn apply(&mut self, mut txs: Vec<ResolvedTx>) -> MarketDelta {
    // Inputs spending tracked outputs carry their script's hash.
    for tx in &mut txs {
      for input in &mut tx.inputs {
        if let Some(utxo) = self.tracker.utxo(&input.previous_output) {
          input.script_hash = script_hash(&utxo.script_pubkey);
        }
      }
    }
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
      // Magisat before block 861,287: the price is the payment, no maker
      // fee; the 1,000-sat fee output is the buyer's taker fee.
      price: 45_000,
      seller_proceeds: 45_000,
      fees: 0,
      buyer_vout: Some(1),
      buyer_script: Some(script(BUYER)),
    }]
  );
  assert_eq!(seller.pieces[0].start, sold);
  assert!(delta.inscriptions.is_empty());
  // Block 840,001's first sat is uncommon only: one uncommon for 44,454
  // sats above the output's own 546.
  let (row, count, unit) = sale_unit(&delta.sales[0], &market.ctx.catalog).unwrap();
  assert_eq!(market.ctx.catalog.rows()[usize::from(row)].id, "uncommon");
  assert_eq!((count, unit), (1, 45_000 - 546));
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
  // Magisat before 861,287: each price is its payment; the shared fee
  // output is the buyer's taker fee.
  for (sale, expected) in delta.sales.iter().zip([
    (2, 30_000, 0, 30_000),
    (3, 20_000, 0, 20_000),
    (4, 10_000, 0, 10_000),
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

/// A Magic Eden accepted offer: the seller's UTXO (owner 1) signed
/// `seller_sighash` to the buyer at vout 0, the payment at vout 1, the fee
/// at vout 2, and change to the funder of the other input.
fn me_offer(
  market: &mut Market,
  n: u64,
  seller_sighash: Option<u8>,
  (payment, fee): (TxOut, TxOut),
  (funder, funding): (u8, u64),
) -> MarketDelta {
  let (x, value, _) = market.mine_rare(1);
  let change = funding - payment.value.to_sat() - fee.value.to_sat() - 300;
  market.apply(vec![tx(
    txid(6_000 + n),
    vec![
      input(x, value, seller_sighash),
      owned(untracked(6_000 + n, funding), funder),
    ],
    vec![out_to(value, BUYER), payment, fee, out_to(change, funder)],
  )])
}

/// vin, kind, marketplace, payment vout, proceeds, fees, price, buyer vout.
type Summary<'a> = (
  u32,
  SaleKind,
  &'a str,
  Option<u32>,
  u64,
  u64,
  u64,
  Option<u32>,
);

fn summary(sales: &[Sale]) -> Vec<Summary<'_>> {
  sales
    .iter()
    .map(|sale| {
      (
        sale.vin,
        sale.kind,
        sale.marketplace.as_str(),
        sale.payment_vout,
        sale.seller_proceeds,
        sale.fees,
        sale.price,
        sale.buyer_vout,
      )
    })
    .collect()
}

#[test]
fn marketplace_offers() {
  let mut market = Market::new();
  // No SINGLE|ANYONECANPAY: the fee output names the marketplace, the
  // payment is the largest output of another party's funds.
  let delta = me_offer(
    &mut market,
    1,
    Some(0x01),
    (out_to(40_000, 1), out_to(1_000, MAGIC_EDEN_FEE)),
    (BUYER, 50_000),
  );
  assert_eq!(
    summary(&delta.sales),
    [(
      0,
      SaleKind::Offer,
      "magic_eden",
      Some(1),
      40_000,
      1_000,
      41_000,
      Some(0)
    )]
  );

  // An escrowed bid whose seller pays the miner fee from the address that
  // receives the payment: the payment is net of that input.
  let (x, value, _) = market.mine_rare(1);
  let delta = market.apply(vec![tx(
    txid(6_010),
    vec![
      input(x, value, Some(0x00)),
      owned(untracked(2, 100_000), BUYER),
      owned(untracked(3, 5_000), 2),
    ],
    vec![
      out_to(value, BUYER),
      out_to(65_000, 2),
      out_to(1_500, MAGIC_EDEN_FEE),
      out_to(100_000 + 5_000 - 65_000 - 1_500 - 400, BUYER),
    ],
  )]);
  assert_eq!(
    summary(&delta.sales),
    [(
      0,
      SaleKind::Offer,
      "magic_eden",
      Some(1),
      60_000,
      1_500,
      61_500,
      Some(0)
    )]
  );

  // A passthrough sale paid at the item's index on a marketplace with that
  // flow is a listing.
  let (x, value, _) = market.mine_rare(1);
  let delta = market.apply(vec![tx(
    txid(6_011),
    vec![
      untracked(4, 639),
      untracked(5, 1_000),
      input(x, value, Some(0x00)),
      owned(untracked(6, 300_000), BUYER),
    ],
    vec![
      out_to(639, PASSTHROUGH_FEE),
      out_to(1_000 + value, BUYER),
      out_to(250_000, 3),
      out_to(5_000, PASSTHROUGH_FEE),
      out_to(300_000 - 250_000 - 5_000 - 1_000, BUYER),
    ],
  )]);
  assert_eq!(
    summary(&delta.sales),
    [(
      2,
      SaleKind::Listing,
      "ord_net",
      Some(2),
      250_000,
      5_639,
      255_639,
      Some(1)
    )]
  );
}

#[test]
fn offer_lookalikes_are_not_sales() {
  let me = |payment: u64| (out_to(payment, 3), out_to(1_000, MAGIC_EDEN_FEE));
  type Case = (&'static str, Option<u8>, (TxOut, TxOut), (u8, u64));
  let cases: Vec<Case> = vec![
    (
      "the seller funds it too: a transfer",
      Some(0x01),
      me(40_000),
      (1, 50_000),
    ),
    (
      "no payment of 3,000 sats (a launchpad mint pays its creator ~547)",
      Some(0x01),
      me(547),
      (BUYER, 50_000),
    ),
    (
      "the payment returns to the funder: change",
      Some(0x01),
      (out_to(40_000, 7), out_to(1_000, MAGIC_EDEN_FEE)),
      (7, 50_000),
    ),
    (
      "fees above half the price",
      Some(0x01),
      (out_to(4_000, 3), out_to(5_000, MAGIC_EDEN_FEE)),
      (BUYER, 50_000),
    ),
    (
      "a fee address funds it: the marketplace's own wallet",
      Some(0x01),
      me(40_000),
      (MAGISAT_FEE, 50_000),
    ),
    (
      "Magisat's fee, not in its own offer shape (a taproot default signature)",
      Some(0x00),
      (out_to(40_000, 3), out_to(1_000, MAGISAT_FEE)),
      (BUYER, 50_000),
    ),
    (
      "a buying tool's fee outside a listing: a transfer",
      Some(0x01),
      (out_to(40_000, 3), out_to(10_000, TOOL_FEE)),
      (BUYER, 60_000),
    ),
    (
      "an output to an address not in the table is no fee",
      Some(0x01),
      (out_to(40_000, 3), out_to(1_000, 77)),
      (BUYER, 50_000),
    ),
  ];
  for (n, (case, sighash, outputs, funding)) in cases.into_iter().enumerate() {
    let mut market = Market::new();
    let delta = me_offer(&mut market, 20 + n as u64, sighash, outputs, funding);
    assert!(delta.sales.is_empty(), "{case}: {:?}", delta.sales);
  }
  // Without the table entry the fee output means nothing.
  let mut market = Market::new();
  market.ctx.marketplaces = Marketplaces::default();
  let delta = me_offer(&mut market, 40, Some(0x01), me(40_000), (BUYER, 50_000));
  assert!(delta.sales.is_empty());
  // Nor after Magic Eden's last height.
  let mut market = Market::after(900_001);
  let delta = me_offer(&mut market, 41, Some(0x01), me(40_000), (BUYER, 50_000));
  assert!(delta.sales.is_empty());
  // The same with a valid height: a sale.
  let mut market = Market::new();
  let delta = me_offer(&mut market, 42, Some(0x01), me(40_000), (BUYER, 50_000));
  assert_eq!(delta.sales.len(), 1);

  // A transfer tool moving a wallet's UTXOs to a new address, the fees
  // paid from another: the largest moved UTXO is no payment, it goes to the
  // same new address as the sats.
  let mut market = Market::new();
  let (x, value, _) = market.mine_rare(1);
  let delta = market.apply(vec![tx(
    txid(6_050),
    vec![
      input(x, value, Some(0x01)),
      owned(untracked(6_050, 5_000), 1),
      owned(untracked(6_051, 20_000), 8),
    ],
    vec![
      out_to(value, 4),
      out_to(5_000, 4),
      out_to(1_000, SATFLOW_FEE),
      marker(),
      out_to(20_000 - 1_000 - 400, 8),
    ],
  )]);
  assert!(delta.sales.is_empty(), "{:?}", delta.sales);

  // A buyer's funding holding a tracked sat, in a sale of untracked sats:
  // its sats pay the seller and the fee and come back as change; it is no
  // sale, even though one output (the change) is not the buyer's item.
  let mut market = Market::new();
  let (z, funding, _) = market.mine_whole(BUYER);
  let delta = market.apply(vec![tx(
    txid(6_051),
    vec![
      owned(untracked(6_052, 9_112), 1),
      input(z, funding, Some(0x00)),
    ],
    vec![
      out_to(9_112, BUYER),
      out_to(funding - 100_000, 1),
      out_to(27_300, PASSTHROUGH_FEE),
      out_to(100_000 - 27_300 - 500, 7),
    ],
  )]);
  assert!(delta.sales.is_empty(), "{:?}", delta.sales);

  // The buyer's funding holds tracked sats too: they reach the seller's
  // payment and the buyer's change, but the buyer sold nothing.
  let mut market = Market::new();
  let (x, value, _) = market.mine_rare(1);
  let (z, funding, _) = market.mine_whole(BUYER);
  let delta = market.apply(vec![tx(
    txid(6_052),
    vec![input(x, value, Some(0x01)), input(z, funding, Some(0x01))],
    vec![
      out_to(value, BUYER),
      out_to(40_000, 1),
      out_to(1_000, MAGIC_EDEN_FEE),
      out_to(funding - 40_000 - 1_000 - 500, BUYER),
    ],
  )]);
  assert_eq!(
    summary(&delta.sales),
    [(
      0,
      SaleKind::Offer,
      "magic_eden",
      Some(1),
      40_000,
      1_000,
      41_000,
      Some(0)
    )]
  );
}

#[test]
fn split_sales_sell_only_what_leaves_the_seller() {
  // The seller's UTXO holds an uncommon (first sat) and a black (last sat);
  // one sat goes to the buyer, the rest back to the seller.
  let mut market = Market::new();
  let (x, value, [uncommon, _black]) = market.mine_whole(1);
  let delta = market.apply(vec![tx(
    txid(6_600),
    vec![
      input(x, value, Some(0x01)),
      owned(untracked(80, 100_000), BUYER),
    ],
    vec![
      out_to(1, BUYER),
      out_to(value - 1, 1),
      out_to(35_000, 3),
      out_to(3_500, ANONYMOUS_FEE),
      out_to(10_000, ANONYMOUS_FEE),
      out_to(100_000 - 35_000 - 13_500 - 500, BUYER),
    ],
  )]);
  assert_eq!(
    summary(&delta.sales),
    [(
      0,
      SaleKind::Offer,
      UNKNOWN_MARKETPLACE,
      Some(2),
      35_000,
      13_500,
      48_500,
      Some(0)
    )]
  );
  assert_eq!(
    sale_sats(&delta.sales[0], &market.ctx.catalog, 10),
    [uncommon]
  );
  assert_eq!(delta.sales[0].seller.value, value);
}

#[test]
fn loans_and_swaps_are_not_sales() {
  // Collateral into a lender's vault, the principal to the borrower, a
  // lending fee: shaped like an offer, not a sale.
  let mut market = Market::new();
  let (x, value, _) = market.mine_rare(1);
  let loan = |market: &mut Market, n: u64, x: OutPoint, fee: Option<TxOut>| {
    let mut outputs = vec![
      escrow(value, 8),
      out_to(500_000, 1),
      out_to(5_000, LENDING_FEE),
    ];
    outputs.extend(fee);
    market.apply(vec![tx(
      txid(6_100 + n),
      vec![
        input(x, value, Some(0x01)),
        owned(untracked(30 + n, 600_000), 8),
      ],
      outputs,
    )])
  };
  assert!(loan(&mut market, 0, x, None).sales.is_empty());
  // A purchase financed by a loan pays a marketplace too: a sale.
  let (y, _, _) = market.mine_rare(1);
  let delta = loan(&mut market, 1, y, Some(out_to(2_000, MAGIC_EDEN_FEE)));
  assert_eq!(
    summary(&delta.sales),
    [(
      0,
      SaleKind::Offer,
      "magic_eden",
      Some(1),
      500_000,
      2_000,
      502_000,
      Some(0)
    )]
  );
}

#[test]
fn anonymous_venues_count_fees_as_unknown() {
  let mut market = Market::new();
  let (x, value, _) = market.mine_rare(1);
  let delta = market.apply(vec![tx(
    txid(6_200),
    vec![
      untracked(40, 600),
      input(x, value, Some(LISTING_SIGHASH)),
      untracked(41, 10_000),
    ],
    vec![
      out_to(600 + value, BUYER),
      out_to(3_000, 1),
      out_to(1_000, ANONYMOUS_FEE),
      out_to(10_000 - 3_000 - 1_000 - 300, BUYER),
    ],
  )]);
  assert_eq!(
    summary(&delta.sales),
    [(
      1,
      SaleKind::Listing,
      UNKNOWN_MARKETPLACE,
      Some(1),
      3_000,
      1_000,
      4_000,
      Some(0)
    )]
  );
  // A buying tool's fill of a listing is the tool's sale.
  let (y, value, _) = market.mine_rare(1);
  let delta = market.apply(vec![tx(
    txid(6_201),
    vec![
      untracked(42, 600),
      untracked(43, 600),
      input(y, value, Some(LISTING_SIGHASH)),
      untracked(44, 100_000),
    ],
    vec![
      out_to(1_200, BUYER),
      out_to(value, BUYER),
      out_to(67_093, 1),
      out_to(100_000 - 67_093 - 10_000 - 500, BUYER),
      out_to(10_000, TOOL_FEE),
    ],
  )]);
  assert_eq!(
    summary(&delta.sales),
    [(
      2,
      SaleKind::Listing,
      "tx_dev",
      Some(2),
      67_093,
      10_000,
      77_093,
      Some(1)
    )]
  );
}

#[test]
fn unisat_listings_by_their_dummies() {
  // Two 300-sat dummies from the per-trade address, returned merged to it
  // at output 0; the fee to the same address right after the payment.
  const TRADE: u8 = 70;
  let mut market = Market::new();
  let unisat = |market: &mut Market, n: u64, fee: bool, dummy: u64| {
    let (x, value, _) = market.mine_rare(1);
    let mut outputs = vec![
      out_to(2 * dummy, TRADE),
      out_to(value, BUYER),
      out_to(60_000, 1),
    ];
    if fee {
      outputs.push(out_to(1_080, TRADE));
    }
    outputs.push(out_to(1_000, BUYER));
    market.apply(vec![tx(
      txid(6_300 + n),
      vec![
        owned(untracked(50 + 2 * n, dummy), TRADE),
        owned(untracked(51 + 2 * n, dummy), TRADE),
        input(x, value, Some(LISTING_SIGHASH)),
        untracked(90 + n, 62_500),
      ],
      outputs,
    )])
  };
  assert_eq!(
    summary(&unisat(&mut market, 0, true, 300).sales),
    [(
      2,
      SaleKind::Listing,
      "unisat",
      Some(2),
      60_000,
      1_080,
      61_080,
      Some(1)
    )]
  );
  assert_eq!(
    summary(&unisat(&mut market, 1, false, 300).sales),
    [(
      2,
      SaleKind::Listing,
      "unisat",
      Some(2),
      60_000,
      0,
      60_000,
      Some(1)
    )]
  );
  // 600-sat dummies: some other venue.
  assert_eq!(
    unisat(&mut market, 2, true, 600).sales[0].marketplace,
    UNKNOWN_MARKETPLACE
  );
}

#[test]
fn satflow_secure_purchases() {
  // A: the seller signs ALL|ANYONECANPAY over [item → escrow, payment →
  // seller], the buyer funds exactly. B: the escrow pays the buyer, the
  // fees and the marker. A is the sale, attributed by B in the same block;
  // B is not a sale.
  let mut market = Market::new();
  let (x, value, _) = market.mine_rare(1);
  let a_txid = txid(6_400);
  let a = tx(
    a_txid,
    vec![
      input(x, value, Some(PSBT_SIGHASH)),
      untracked(60, 1_000_000 - value),
    ],
    vec![escrow(value, 8), out_to(990_000, 1)],
  );
  let mut leg = input(op(a_txid, 0), value, Some(0x01));
  leg.script_hash = script_hash(&escrow(value, 8).script_pubkey);
  let b = tx(
    txid(6_401),
    vec![leg, untracked(61, 30_000)],
    vec![
      out_to(value, BUYER),
      out_to(15_000, SATFLOW_FEE),
      out_to(10_000, 4),
      marker(),
      out_to(30_000 - 15_000 - 10_000 - 500, BUYER),
    ],
  );
  let delta = market.apply(vec![a.clone(), b.clone()]);
  assert_eq!(
    summary(&delta.sales),
    [(
      0,
      SaleKind::Listing,
      "satflow",
      Some(1),
      990_000,
      15_000,
      1_005_000,
      Some(0)
    )]
  );
  assert_eq!(delta.sales[0].txid, a_txid);

  // Settled in a later block: A is still a sale, of an unknown marketplace;
  // B is not.
  let mut market = Market::new();
  let (x, _, _) = market.mine_rare(1);
  let mut a = a.clone();
  a.inputs[0].previous_output = x;
  let a_txid = txid(6_402);
  a.txid = a_txid;
  let delta = market.apply(vec![a]);
  assert_eq!(
    summary(&delta.sales),
    [(
      0,
      SaleKind::Listing,
      UNKNOWN_MARKETPLACE,
      Some(1),
      990_000,
      0,
      990_000,
      Some(0)
    )]
  );
  let mut b = b;
  b.inputs[0].previous_output = op(a_txid, 0);
  let delta = market.apply(vec![b]);
  assert!(delta.sales.is_empty(), "{:?}", delta.sales);

  // The legacy form pays its 1,000-sat fee in A itself.
  let mut market = Market::new();
  let (x, value, _) = market.mine_rare(1);
  let delta = market.apply(vec![tx(
    txid(6_403),
    vec![input(x, value, Some(PSBT_SIGHASH)), untracked(62, 20_000)],
    vec![
      out_to(value, BUYER),
      out_to(15_546, 3),
      out_to(1_000, SATFLOW_FEE),
    ],
  )]);
  assert_eq!(
    summary(&delta.sales),
    [(
      0,
      SaleKind::Listing,
      "satflow",
      Some(1),
      15_546,
      1_000,
      16_546,
      Some(0)
    )]
  );
}

#[test]
fn listings_that_sell_nothing_tracked() {
  let mut market = Market::new();
  // A rune listing: no dummies, so the seller's sats land past its payment
  // in the buyer's change (or in the miner fee), not in the item output.
  for (n, change) in [(0, 20_000), (1, 0)] {
    let (x, value, _) = market.mine_rare(1);
    let mut outputs = vec![
      out_to(546, BUYER),
      out_to(30_000, 1),
      out_to(600, MAGIC_EDEN_FEE),
    ];
    if change > 0 {
      outputs.push(out_to(change, BUYER));
    }
    // Change: the seller's sats are its last ones; none: past the outputs.
    let funding = if change > 0 {
      546 + 30_000 + 600 + change - value
    } else {
      546 + 30_000 + 600 + 400
    };
    let delta = market.apply(vec![tx(
      txid(6_500 + n),
      vec![
        untracked(70 + n, funding),
        input(x, value, Some(LISTING_SIGHASH)),
      ],
      outputs,
    )]);
    assert!(delta.sales.is_empty(), "{n}: {:?}", delta.sales);
  }
  // A SINGLE|ANYONECANPAY self-transfer: the sats stay with the seller.
  let (x, value, _) = market.mine_rare(1);
  let delta = market.apply(vec![tx(
    txid(6_502),
    vec![untracked(72, 600), input(x, value, Some(LISTING_SIGHASH))],
    vec![out_to(600 + value, 1), out_to(0, 1)],
  )]);
  assert!(delta.sales.is_empty(), "{:?}", delta.sales);
  // The buyer's tracked dummy in a listing purchase is not sold.
  let (x, _, _) = market.mine_rare(BUYER);
  let (y, value, _) = market.mine_rare(1);
  let delta = market.apply(vec![tx(
    txid(6_503),
    vec![
      input(x, 546, Some(0x01)),
      untracked(73, 600),
      input(y, value, Some(LISTING_SIGHASH)),
      untracked(74, 50_000),
    ],
    vec![
      out_to(1_146, 4),
      out_to(value, 4),
      out_to(40_000, 1),
      out_to(1_000, MAGISAT_FEE),
      out_to(50_000 - 40_000 - 1_000 - 300, 4),
    ],
  )]);
  assert_eq!(
    delta
      .sales
      .iter()
      .map(|sale| (sale.vin, sale.marketplace.as_str()))
      .collect::<Vec<_>>(),
    [(2, "magisat")]
  );
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

/// A Magisat-style buy of one listing: two padding inputs, the listing at
/// vin 2 paid at vout 2, the buyer's ordinal output at vout 1.
fn buy_listing(
  market: &mut Market,
  (x, value): (OutPoint, u64),
  payment: u64,
  fee: u64,
  buyer: u8,
) -> MarketDelta {
  market.apply(vec![tx(
    txid(1_000 + u64::from(market.chain.next)),
    vec![
      untracked(1, 600),
      untracked(2, 600),
      input(x, value, Some(LISTING_SIGHASH)),
      untracked(3, payment + fee + 10_000),
    ],
    vec![
      out_to(1200, buyer),
      out_to(value, buyer),
      out_to(payment, 1),
      out_to(fee, MAGISAT_FEE),
      out_to(9_000, buyer),
    ],
  )])
}

#[test]
fn magisat_maker_fee_from_block_861287() {
  // 487,550 paid to the seller is a 490,000 price with a 2,450 maker fee
  // from block 861,287 (e.g. 72cb8faa…); before it, the payment is the price.
  for (after, maker) in [(861_287 - 4, 0), (861_287, 2_450)] {
    let mut market = Market::after(after);
    let (x, value, _) = market.mine_rare(1);
    let height = market.chain.next;
    let delta = buy_listing(&mut market, (x, value), 487_550, 12_950, BUYER);
    assert_eq!(height >= MAGISAT_MAKER_FEE_HEIGHT, maker > 0);
    let [sale] = &delta.sales[..] else { panic!() };
    assert_eq!(
      (
        sale.marketplace.as_str(),
        sale.seller_proceeds,
        sale.fees,
        sale.price
      ),
      ("magisat", 487_550, maker, 487_550 + maker)
    );
  }
}

#[test]
fn magisat_buying_with_its_own_wallet_is_no_fee() {
  // The fee address receives the bought sats: it is the buyer, not a fee.
  let mut market = Market::new();
  let (x, value, _) = market.mine_rare(1);
  let delta = buy_listing(&mut market, (x, value), 30_000, 1_000, MAGISAT_FEE);
  let [sale] = &delta.sales[..] else { panic!() };
  assert_eq!(
    (
      sale.marketplace.as_str(),
      sale.price,
      sale.fees,
      sale.buyer_script.clone()
    ),
    (UNKNOWN_MARKETPLACE, 30_000, 0, Some(script(MAGISAT_FEE)))
  );
}

#[test]
fn listing_shape_edge_cases() {
  let mut market = Market::new();
  // vin past the outputs (SIGHASH_SINGLE's out-of-range case): no sale.
  let (x, value, _) = market.mine_rare(1);
  let delta = market.apply(vec![tx(
    txid(50),
    vec![untracked(1, 600), input(x, value, Some(LISTING_SIGHASH))],
    vec![out_to(1_146, BUYER)],
  )]);
  assert!(delta.sales.is_empty());
}

/// An accepted Magisat offer: the seller's UTXO at vin 0 and the buyer's
/// funding all signed ALL; out0 = the sold UTXO's value to the buyer, out1
/// = the seller's proceeds, out2 = the Magisat fee (maker + taker), change.
fn offer_tx(
  (x, value): (OutPoint, u64),
  sighashes: [Option<u8>; 2],
  out0: u64,
  fee_script: u8,
) -> ResolvedTx {
  tx(
    txid(60),
    vec![
      input(x, value, sighashes[0]),
      input(op(txid(1_000_001), 0), 5_000_000, sighashes[1]),
    ],
    vec![
      out_to(out0, BUYER),
      out_to(3_960_000, 2),
      out_to(80_000, fee_script),
      out_to(5_000_000 + value - out0 - 3_960_000 - 80_000 - 1_000, BUYER),
    ],
  )
}

#[test]
fn magisat_offers() {
  // 19df4497…: a 4,000,000 offer: 3,960,000 to the seller, 80,000 fee =
  // 40,000 maker + 40,000 taker.
  let mut market = Market::new();
  let (x, value, _) = market.mine_rare(1);
  let seller = market.tracker.utxo(&x).unwrap().clone();
  let delta = market.apply(vec![offer_tx(
    (x, value),
    [Some(0x01), Some(0x01)],
    value,
    MAGISAT_FEE,
  )]);
  assert_eq!(
    delta.sales,
    [Sale {
      txid: txid(60),
      tx_index: 1,
      vin: 0,
      kind: SaleKind::Offer,
      marketplace: "magisat".into(),
      seller_outpoint: x,
      seller,
      payment_vout: Some(1),
      price: 4_000_000,
      seller_proceeds: 3_960_000,
      fees: 40_000,
      buyer_vout: Some(0),
      buyer_script: Some(script(BUYER)),
    }]
  );
  // Not offers: a taproot default (0x00) signature, output 0 worth other
  // than the input, no Magisat fee at output 2.
  for (sighashes, out0, fee) in [
    ([Some(0x00), Some(0x01)], 546, MAGISAT_FEE),
    ([Some(0x01), Some(0x00)], 546, MAGISAT_FEE),
    ([Some(0x01), Some(0x01)], 600, MAGISAT_FEE),
    ([Some(0x01), Some(0x01)], 546, 3),
  ] {
    let mut market = Market::new();
    let (x, value, _) = market.mine_rare(1);
    let delta = market.apply(vec![offer_tx((x, value), sighashes, out0, fee)]);
    assert!(delta.sales.is_empty(), "{sighashes:?} {out0} {fee}");
  }
}

#[test]
fn two_party_psbt_sales() {
  // 0c47e0ac…-like: the seller signs ALL|ANYONECANPAY over [sats → buyer,
  // payment → seller], the buyer adds an input signed ALL.
  let mut market = Market::new();
  let (x, value, _) = market.mine_rare(1);
  let delta = market.apply(vec![tx(
    txid(70),
    vec![
      input(x, value, Some(PSBT_SIGHASH)),
      input(op(txid(1_000_001), 0), 100_000, Some(0x01)),
    ],
    vec![
      out_to(value, BUYER),
      out_to(80_000, 1),
      out_to(100_000 - 80_000 - 1_000, BUYER),
    ],
  )]);
  let [sale] = &delta.sales[..] else {
    panic!("{:?}", delta.sales)
  };
  assert_eq!(
    (
      sale.kind,
      sale.marketplace.as_str(),
      sale.payment_vout,
      sale.price,
      sale.buyer_vout
    ),
    (
      SaleKind::Listing,
      UNKNOWN_MARKETPLACE,
      Some(1),
      80_000,
      Some(0)
    )
  );

  // 96e080d3…-like: the payment goes to the seller's separate payment
  // address, the output right after the sats.
  let mut market = Market::new();
  let (x, value, _) = market.mine_rare(1);
  let delta = market.apply(vec![tx(
    txid(72),
    vec![
      input(x, value, Some(PSBT_SIGHASH)),
      input(op(txid(1_000_001), 0), 80_600, Some(0x00)),
    ],
    vec![out_to(value, BUYER), out_to(80_000, 3)],
  )]);
  assert_eq!(
    delta
      .sales
      .iter()
      .map(|s| (s.vin, s.payment_vout, s.price, s.buyer_vout))
      .collect::<Vec<_>>(),
    [(0, Some(1), 80_000, Some(0))]
  );

  // Two chunks of the same seller paid by one output share it by value.
  let mut market = Market::new();
  let (x, _, _) = market.mine_rare(1);
  let (y, _, _) = market.mine_rare(1);
  let delta = market.apply(vec![tx(
    txid(71),
    vec![
      input(x, 546, Some(PSBT_SIGHASH)),
      input(y, 546, Some(PSBT_SIGHASH)),
      input(op(txid(1_000_001), 0), 100_000, Some(0x00)),
    ],
    vec![
      out_to(546, BUYER),
      out_to(546, BUYER),
      out_to(80_001, 1),
      out_to(100_000 - 80_001 - 1_000, BUYER),
    ],
  )]);
  assert_eq!(
    delta
      .sales
      .iter()
      .map(|s| (s.vin, s.payment_vout, s.seller_proceeds, s.price))
      .collect::<Vec<_>>(),
    [(0, Some(2), 40_001, 40_001), (1, Some(2), 40_000, 40_000)]
  );
}

type BuildTx = Box<dyn Fn((OutPoint, u64)) -> ResolvedTx>;

#[test]
fn psbt_lookalikes_are_not_sales() {
  let cases: Vec<(&str, BuildTx)> = vec![
    (
      "exchange batch: every input ALL|ANYONECANPAY",
      Box::new(|(x, value)| {
        tx(
          txid(80),
          vec![
            input(x, value, Some(PSBT_SIGHASH)),
            input(op(txid(1_000_001), 0), 100_000, Some(PSBT_SIGHASH)),
          ],
          vec![out_to(value, BUYER), out_to(99_000, 1)],
        )
      }),
    ),
    (
      "an untracked ALL|ANYONECANPAY input of unknown owner",
      Box::new(|(x, value)| {
        tx(
          txid(81),
          vec![
            input(x, value, Some(PSBT_SIGHASH)),
            input(op(txid(1_000_001), 0), 1_000, Some(PSBT_SIGHASH)),
            input(op(txid(1_000_002), 0), 100_000, Some(0x01)),
          ],
          vec![
            out_to(value, BUYER),
            out_to(80_000, 1),
            out_to(20_000, BUYER),
          ],
        )
      }),
    ),
    (
      "the payment would go back to the buyer",
      Box::new(|(x, value)| {
        tx(
          txid(82),
          vec![
            input(x, value, Some(PSBT_SIGHASH)),
            input(op(txid(1_000_001), 0), 100_000, Some(0x01)),
          ],
          vec![out_to(value, BUYER), out_to(99_000, BUYER)],
        )
      }),
    ),
    (
      "a custodial withdrawal: the 0x81 input is split, a fee payer signs ALL",
      Box::new(|(x, value)| {
        tx(
          txid(85),
          vec![
            input(x, value, Some(PSBT_SIGHASH)),
            input(op(txid(1_000_001), 0), 2_000, Some(0x01)),
          ],
          vec![out_to(300, BUYER), out_to(value - 300, 1), out_to(1_000, 7)],
        )
      }),
    ),
    (
      "no output after the sats",
      Box::new(|(x, value)| {
        tx(
          txid(86),
          vec![
            input(x, value, Some(PSBT_SIGHASH)),
            input(op(txid(1_000_001), 0), 2_000, Some(0x01)),
          ],
          vec![out_to(value, BUYER)],
        )
      }),
    ),
    (
      "the sats stay at the seller's script",
      Box::new(|(x, value)| {
        tx(
          txid(83),
          vec![
            input(x, value, Some(PSBT_SIGHASH)),
            input(op(txid(1_000_001), 0), 100_000, Some(0x01)),
          ],
          vec![out_to(value, 1), out_to(99_000, 1)],
        )
      }),
    ),
    (
      "NONE|ANYONECANPAY sweep",
      Box::new(|(x, value)| {
        tx(
          txid(84),
          vec![
            input(x, value, Some(0x82)),
            input(op(txid(1_000_001), 0), 100_000, Some(0x01)),
          ],
          vec![out_to(value, BUYER), out_to(99_000, 1)],
        )
      }),
    ),
  ];
  for (case, build) in cases {
    let mut market = Market::new();
    let (x, value, _) = market.mine_rare(1);
    let delta = market.apply(vec![build((x, value))]);
    assert!(delta.sales.is_empty(), "{case}: {:?}", delta.sales);
  }
}
