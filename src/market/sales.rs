//! Sales of tracked sats, by the transaction shapes measured on chain:
//! Magisat's whole sales history (its public API: 37,749 sales, every
//! decodable one checked against its transaction), and the fee addresses and
//! layouts of the other marketplaces (`data/marketplaces.json`).
//!
//! Every tracked input of a transaction is tried against these shapes, in
//! order; the first that matches makes the input a sale:
//!
//! 1. **Listing**: the input signed SINGLE|ANYONECANPAY (`0x83`); its paired
//!    output `outputs[vin]` pays the seller. Not a sale when `vin` has no
//!    paired output (SIGHASH_SINGLE's out-of-range case), or when the sold
//!    sats do not leave for a buyer: into the paired output, back to the
//!    seller's script (a self-transfer), into the miner fee, or past the
//!    payment into an output not worth the seller's UTXO (the buyer's change
//!    or a fee: a rune listing, whose item moves by runestone, not FIFO).
//!    In a transaction with listings, the other tracked inputs are the
//!    buyer's dummies and funding, never sales.
//! 2. **Magisat offer** (129 / 129 of Magisat's accepted offers): the
//!    seller's input is vin 0, signed ALL (`0x01`) like every other input,
//!    which all come from other scripts; output 0 receives the sold sats,
//!    worth exactly the input, at another script; output 1 pays the seller;
//!    output 2 is a Magisat fee. Magisat's send and transfer tools have the
//!    same shape, so output 1 must not return change to a funder (an input
//!    other than the seller's), pay the item's own address or a tool address
//!    (219 such transactions in Magisat's history, none of them a sale in
//!    its records; all 102 recorded offers keep matching).
//!    **Marked offer** (raresats.net, a `proceeds` marketplace with the
//!    `offer_sighash_all` flow): the same layout, held to the marketplace's
//!    own contract, since its fee alone is paid by transfers too. The
//!    seller's input is vin 0 and every input signs DEFAULT (`0x00`,
//!    taproot) or ALL (`0x01`), none after vin 0 from the seller's script;
//!    output 0 receives the sold sats, worth exactly the input, at another
//!    script; output 1 pays vin 0's own script (the price); output 2 is the
//!    marketplace's fee and the last output its marker, at most one output
//!    (the bidder's change) between them. Fee and marker both, or no sale.
//! 3. **Two-party ALL|ANYONECANPAY sale** (`0x81`; Satflow's secure
//!    purchase): the seller pre-signs `[sats → buyer, payment → seller]` and
//!    the buyer adds inputs signed without ANYONECANPAY. Every `0x81` input
//!    is the seller's (by script) and passes whole (same value) to the output
//!    at its own index, at another script, where its sats land; no other
//!    input is the seller's; the payment is an output to the seller's script,
//!    else the output right after the seller's, if it does not return to a
//!    buyer's script. Exchange batches and consolidations (every input
//!    `0x81`) and custodial withdrawals (the `0x81` input split, a fee
//!    payer signing ALL) never match. Attributed to the marketplace whose fee
//!    the transaction pays (Satflow's legacy form), or whose settlement
//!    (marker or fee) spends the escrow output in the same block; otherwise
//!    unknown.
//! 4. **Offer or passthrough** (Magic Eden's offers and bids, ord.net and
//!    Ordinals Wallet passthrough sales, Satflow's bid acceptances): no
//!    listing in the transaction, a fee output of a marketplace (not
//!    Magisat, whose offers are shape 2, nor one priced by its `proceeds`,
//!    whose offers are marked offers, nor a buying tool) and no input
//!    from a fee address (a marketplace's own wallet); at least two input
//!    scripts; some of the sats leave the seller's script (only those are
//!    sold: a seller may split one sat out of a UTXO); and a payment: the
//!    output of the largest net value (its value less the inputs from its
//!    own script and less the sats the seller's inputs put into it) that
//!    holds none of the seller's tracked sats, does not pay the buyer and is
//!    no fee or marker, worth at least `MIN_OFFER_PAYMENT`. Fees above half
//!    the price reject it. A passthrough sale (payment at the item's
//!    index, on a marketplace with that flow) is a listing, the rest
//!    offers. Three misreadings are ruled out (docs/SALES-SHAPES.md §6.1):
//!    a rune trade (a runestone output: the items move by edict, tracked
//!    sats in the bidders' funds are not sold); an item output fed by
//!    several other parties' inputs (a batch's change, not a delivery);
//!    and a payment that is an earlier input's UTXO passing whole to
//!    another script (that input is the trade's item; the input holding
//!    tracked sats is the buyer's funding). Last, an offer that hands a
//!    whole UTXO over for less than its value is a misreading too (not
//!    applied to split sales, nor to listings, whose price is signed).
//!
//! Not sales either: a marketplace's escrow output (P2WSH) spent by its
//! settlement or a recovery transfer (a transaction carrying the marker of a
//! two-transaction marketplace), and transactions paying a lending or swap
//! address and no marketplace (loans of sats, swaps).
//!
//! **Fee outputs** pay a fee address of the table valid at the height,
//! except addresses that receive the transaction's tracked sats or fund it
//! (by input script hash): a marketplace's own wallet buying or moving sats.
//! A fee output alone is never a sale (transfer and isolation tools pay one
//! too).
//!
//! **Markers** (the table's OP_RETURN outputs) attribute a listing whose
//! transaction pays no fee address (a purchase without its fee output); a
//! marker alone is never a sale either.
//!
//! **Price**:
//! * a listing in a transaction with a Magisat fee output: Magisat's price
//!   rule, `P` = the paired payment, plus the 0.5 % maker fee `⌊P/199⌋` from
//!   block 861,287 (`fees`); the buyer's taker fee is not part of the price
//!   (exact for 97.6 % of Magisat's sales, 100 % before 861,287);
//! * a Magisat offer: output 1 plus the taker fee, half of output 2 (maker
//!   and taker fees are equal since August 2024; 4 of 129 earlier offers
//!   differ);
//! * a listing of a marketplace priced by its `proceeds` (raresats.net): `P`
//!   alone, no `fees`; its fee output is the buyer's taker fee, on top of
//!   the price; a marked offer: output 1 alone (the bidder's fee is extra);
//! * other marketplaces: proceeds (an offer's net payment) plus the
//!   transaction's fee outputs, split over its sales in proportion to their
//!   proceeds; a payment shared by several sales is split by their UTXOs'
//!   values;
//! * UniSat (no fixed fee address; recognized by its two 300-sat dummies
//!   from one script, returned merged to it at output 0): plus the output
//!   after the payment when it pays that script;
//! * no fee output nor marker: the proceeds, marketplace "unknown" (a
//!   fee-less msigner purchase is not attributed to OKX: Magisat purchases
//!   whose fee is below dust have the same layout).
//!
//! A listing a marketplace buys with its own wallet (a fee address marked as
//! its buying wallet) and no fee output nor marker is that marketplace's
//! sale (Magisat: 8 / 8 such sales are in its records, at its price rule).
//!
//! An anonymous marketplace (known only by its fee address) is reported as
//! "unknown", its fees still counted.

use {
  super::{
    BlockView,
    marketplaces::{Marketplace, Marketplaces, PriceRule, Role},
    view::TxView,
  },
  crate::model::{MoveInput, ResolvedTx, Sale, SaleKind, script_hash},
  bitcoin::{OutPoint, Script, TxOut, opcodes},
};

/// SIGHASH_SINGLE | SIGHASH_ANYONECANPAY: marketplace listings.
pub const LISTING_SIGHASH: u8 = 0x83;

/// SIGHASH_ALL | SIGHASH_ANYONECANPAY: two-party sales (and exchange
/// batches).
pub const PSBT_SIGHASH: u8 = 0x81;

/// SIGHASH_ALL, explicit: both sides of a Magisat offer.
pub const OFFER_SIGHASH: u8 = 0x01;

/// SIGHASH_DEFAULT: a taproot key-path signature without a flag byte.
const DEFAULT_SIGHASH: u8 = 0x00;

/// The first block with a Magisat maker fee (2024-09-14).
pub const MAGISAT_MAKER_FEE_HEIGHT: u32 = 861_287;

/// Marketplace id of sales in transactions paying no known fee address.
pub const UNKNOWN_MARKETPLACE: &str = "unknown";

/// The least net value of an offer's payment: smaller outputs are change,
/// postage, or a creator's share of a launchpad mint.
pub const MIN_OFFER_PAYMENT: u64 = 3_000;

const ANYONECANPAY: u8 = 0x80;

/// Flow (`data/marketplaces.json`) of marketplaces settling a seller's
/// ALL|ANYONECANPAY sale with a second transaction.
const SECURE_PURCHASE: &str = "secure_purchase_two_tx";

/// Flow of marketplaces whose listings settle through an escrow shared with
/// the seller, paid at the item's index.
const PASSTHROUGH: &str = "passthrough_2of2";

/// Flow of marketplaces whose accepted offers have Magisat's layout (output
/// 0 the item, 1 the payment, 2 the fee). Changes the detection only for a
/// `proceeds` marketplace (`marked_offer`).
const OFFER_FLOW: &str = "offer_sighash_all";

const UNISAT: &str = "unisat";
const UNISAT_DUMMY: u64 = 300;

struct Fee<'a> {
  vout: u32,
  value: u64,
  marketplace: &'a Marketplace,
}

/// What a transaction's outputs and inputs say about marketplaces.
struct Marks<'a> {
  /// Outputs paying a fee address of a marketplace or buying tool.
  fees: Vec<Fee<'a>>,
  /// Marker outputs.
  markers: Vec<(u32, &'a Marketplace)>,
  /// Outputs paying a marketplace's tool address: never a payment.
  tools: Vec<u32>,
  /// The transaction pays a lending or swap address.
  lending: bool,
  /// An input spends from a fee address: the marketplace's own wallet.
  funded_by_marketplace: bool,
}

impl<'a> Marks<'a> {
  /// The selling marketplace whose marker the transaction carries.
  fn marker(&self) -> Option<&'a Marketplace> {
    self
      .markers
      .iter()
      .map(|&(_, marketplace)| marketplace)
      .find(|marketplace| marketplace.sells())
  }

  fn marks_flow(&self, flow: &str) -> bool {
    self.fees.iter().any(|fee| fee.marketplace.has_flow(flow))
      || self
        .markers
        .iter()
        .any(|(_, marketplace)| marketplace.has_flow(flow))
  }

  /// A fee, marker or tool output: the marketplace's, never a payment.
  fn is_marketplace_output(&self, vout: u32) -> bool {
    self.fees.iter().any(|fee| fee.vout == vout)
      || self.markers.iter().any(|(v, _)| *v == vout)
      || self.tools.contains(&vout)
  }
}

fn marks<'a>(
  tx: &ResolvedTx,
  receivers: &[&Script],
  height: u32,
  marketplaces: &'a Marketplaces,
) -> Marks<'a> {
  let mut marks = Marks {
    fees: Vec::new(),
    markers: Vec::new(),
    tools: Vec::new(),
    lending: false,
    funded_by_marketplace: tx.inputs.iter().any(|input| {
      marketplaces
        .fee_hash(input.script_hash, height)
        .is_some_and(Marketplace::sells)
    }),
  };
  for (vout, output) in tx.outputs.iter().enumerate() {
    let script = output.script_pubkey.as_script();
    if let Some(marketplace) = marketplaces.marker(script, height) {
      marks.markers.push((vout as u32, marketplace));
      continue;
    }
    if marketplaces.tool_output(script, height).is_some() {
      marks.tools.push(vout as u32);
      continue;
    }
    let Some(marketplace) = marketplaces.fee_output(script, height) else {
      continue;
    };
    match marketplace.role {
      Role::Lending | Role::Swap => marks.lending = true,
      Role::Marketplace | Role::BuyingTool => {
        let hash = script_hash(script);
        if !receivers.contains(&script) && tx.inputs.iter().all(|input| input.script_hash != hash) {
          marks.fees.push(Fee {
            vout: vout as u32,
            value: output.value.to_sat(),
            marketplace,
          });
        }
      }
      Role::Unidentified => {}
    }
  }
  marks
}

pub(super) fn detect(
  view: &TxView,
  block: &BlockView,
  marketplaces: &Marketplaces,
  out: &mut Vec<Sale>,
) {
  let receivers: Vec<&Script> = view
    .mv
    .outputs
    .iter()
    .map(|output| output.script_pubkey.as_script())
    .collect();
  let marks = marks(view.tx, &receivers, view.height, marketplaces);
  if marks.lending && marks.fees.is_empty() && marks.markers.is_empty() {
    return;
  }
  let listing_tx = view
    .tx
    .inputs
    .iter()
    .any(|input| input.sighash == Some(LISTING_SIGHASH));
  let mut listings = Vec::new();
  let mut psbts = Vec::new();
  let mut offers = Vec::new();
  for input in &view.mv.inputs {
    if view.tx.inputs[input.vin as usize].sighash == Some(LISTING_SIGHASH) {
      listings.extend(listing(view, input));
    } else if listing_tx || settlement(input, &marks) {
      // The buyer's dummies or funding; an escrow leg's settlement.
    } else if let Some(sale) =
      magisat_offer(view, input, &marks).or_else(|| marked_offer(view, input, &marks))
    {
      out.push(sale);
    } else if let Some(sale) = psbt(view, input, &marks) {
      psbts.push(sale);
    } else if let Some(sale) = offer(view, input, &marks) {
      offers.push(sale);
    }
  }
  price_listings(view, marketplaces, &mut listings, &marks);
  out.append(&mut listings);
  attribute_psbts(view, block, marketplaces, &mut psbts, &marks.fees);
  out.append(&mut psbts);
  price_offers(view, &mut offers, &marks.fees);
  out.append(&mut offers);
  out.sort_by_key(|sale| sale.vin);
}

fn sale(
  view: &TxView,
  input: &MoveInput,
  kind: SaleKind,
  payment_vout: u32,
  buyer_vout: Option<u32>,
) -> Sale {
  let proceeds = view.tx.outputs[payment_vout as usize].value.to_sat();
  Sale {
    txid: view.tx.txid,
    tx_index: view.mv.tx_index,
    vin: input.vin,
    kind,
    marketplace: UNKNOWN_MARKETPLACE.into(),
    seller_outpoint: input.outpoint,
    seller: input.utxo.clone(),
    payment_vout: Some(payment_vout),
    price: proceeds,
    seller_proceeds: proceeds,
    fees: 0,
    buyer_vout,
    buyer_script: buyer_vout.map(|vout| view.tx.outputs[vout as usize].script_pubkey.clone()),
  }
}

/// Shape 1: a SINGLE|ANYONECANPAY listing.
fn listing(view: &TxView, input: &MoveInput) -> Option<Sale> {
  view.tx.outputs.get(input.vin as usize)?;
  let buyer = view.main_destination(input)?;
  let item = &view.tx.outputs[buyer as usize];
  if buyer == input.vin
    || item.script_pubkey == input.utxo.script_pubkey
    || (buyer > input.vin && item.value.to_sat() != input.utxo.value)
  {
    return None;
  }
  Some(sale(view, input, SaleKind::Listing, input.vin, Some(buyer)))
}

/// A two-transaction marketplace's escrow output spent by its settlement (or
/// a recovery transfer): marked like the marketplace's transactions.
fn settlement(input: &MoveInput, marks: &Marks) -> bool {
  input.utxo.script_pubkey.is_p2wsh() && marks.marks_flow(SECURE_PURCHASE)
}

/// Shape 2: an accepted Magisat offer.
fn magisat_offer(view: &TxView, input: &MoveInput, marks: &Marks) -> Option<Sale> {
  let (inputs, outputs) = (&view.tx.inputs, &view.tx.outputs);
  if input.vin != 0
    || inputs.len() < 2
    || inputs.iter().any(|i| i.sighash != Some(OFFER_SIGHASH))
    || inputs[1..]
      .iter()
      .any(|i| i.script_hash == inputs[0].script_hash)
    || outputs.len() < 3
  {
    return None;
  }
  let fee = marks
    .fees
    .iter()
    .find(|fee| fee.vout == 2 && fee.marketplace.price_rule == PriceRule::Magisat)?;
  // Magisat's send and transfer tools have the same shape, but output 1
  // pays a tool address, the item's own address, or change back to a
  // funder (the seller's own address is a real offer's payment).
  let payment = &outputs[1].script_pubkey;
  let payment_hash = script_hash(payment);
  if outputs[0].value.to_sat() != inputs[0].value
    || outputs[0].script_pubkey == input.utxo.script_pubkey
    || view.main_destination(input) != Some(0)
    || marks.tools.contains(&1)
    || *payment == outputs[0].script_pubkey
    || inputs[1..].iter().any(|i| i.script_hash == payment_hash)
  {
    return None;
  }
  let mut sale = sale(view, input, SaleKind::Offer, 1, Some(0));
  sale.marketplace = fee.marketplace.reported_id().into();
  sale.fees = fee.value / 2;
  sale.price = sale.seller_proceeds.saturating_add(sale.fees);
  Some(sale)
}

/// Shape 2, marked: an accepted offer of a `proceeds` marketplace with the
/// offer flow (raresats.net), in its exact layout: `[item → bidder, price →
/// vin 0's script, fee, (change), marker]`. Its fee alone marks no sale
/// (transfers pay it too), so the transaction must also carry its marker,
/// last, and pay vin 0's own script at output 1.
fn marked_offer(view: &TxView, input: &MoveInput, marks: &Marks) -> Option<Sale> {
  let (inputs, outputs) = (&view.tx.inputs, &view.tx.outputs);
  let seller = &input.utxo.script_pubkey;
  if input.vin != 0
    || inputs.len() < 2
    || !(4..=5).contains(&outputs.len())
    || inputs
      .iter()
      .any(|i| !matches!(i.sighash, Some(DEFAULT_SIGHASH | OFFER_SIGHASH)))
    || inputs[1..]
      .iter()
      .any(|i| i.script_hash == inputs[0].script_hash)
  {
    return None;
  }
  let marketplace = marks
    .fees
    .iter()
    .find(|fee| fee.vout == 2)
    .map(|fee| fee.marketplace)
    .filter(|m| m.price_rule == PriceRule::Proceeds && m.has_flow(OFFER_FLOW))?;
  let last = outputs.len() as u32 - 1;
  if !marks
    .markers
    .iter()
    .any(|&(vout, m)| vout == last && m.id == marketplace.id)
    // vin 0's sats fill output 0 exactly (FIFO): the item lands whole.
    || outputs[0].value.to_sat() != inputs[0].value
    || outputs[0].script_pubkey == *seller
    || outputs[1].script_pubkey != *seller
  {
    return None;
  }
  let mut sale = sale(view, input, SaleKind::Offer, 1, Some(0));
  sale.marketplace = marketplace.reported_id().into();
  Some(sale)
}

/// Shape 3: a two-party ALL|ANYONECANPAY sale.
fn psbt(view: &TxView, input: &MoveInput, marks: &Marks) -> Option<Sale> {
  let (inputs, outputs) = (&view.tx.inputs, &view.tx.outputs);
  if inputs[input.vin as usize].sighash != Some(PSBT_SIGHASH) {
    return None;
  }
  let seller = &input.utxo.script_pubkey;
  let seller_hash = inputs[input.vin as usize].script_hash;
  // The seller's inputs: every ALL|ANYONECANPAY one, each passing whole to
  // the output at its index; no other.
  let mut last = 0;
  let mut buyer_hashes = Vec::new();
  for (vin, other) in inputs.iter().enumerate() {
    if other.sighash == Some(PSBT_SIGHASH) {
      if other.script_hash != seller_hash
        || outputs
          .get(vin)
          .is_none_or(|output| output.value.to_sat() != other.value)
      {
        return None;
      }
      last = vin;
    } else if other.script_hash == seller_hash {
      return None;
    } else {
      buyer_hashes.push(other.script_hash);
    }
  }
  // The buyer's inputs, signed without ANYONECANPAY.
  if !inputs
    .iter()
    .any(|other| other.sighash.is_some_and(|flag| flag & ANYONECANPAY == 0))
  {
    return None;
  }
  let buyer = input.vin;
  let buyer_script = &outputs[buyer as usize].script_pubkey;
  if view.main_destination(input) != Some(buyer) || buyer_script == seller {
    return None;
  }
  let payment = payment_to(outputs, seller, buyer).or_else(|| {
    let vout = last + 1;
    let output = outputs.get(vout)?;
    (output.value.to_sat() > 0
      && output.script_pubkey != *buyer_script
      && !marks.is_marketplace_output(vout as u32)
      && !buyer_hashes.contains(&script_hash(&output.script_pubkey)))
    .then_some(vout as u32)
  })?;
  Some(sale(view, input, SaleKind::Listing, payment, Some(buyer)))
}

/// The first output paying `seller` other than the buyer's.
fn payment_to(outputs: &[TxOut], seller: &Script, buyer: u32) -> Option<u32> {
  outputs
    .iter()
    .enumerate()
    .find(|&(vout, output)| vout as u32 != buyer && output.script_pubkey.as_script() == seller)
    .map(|(vout, _)| vout as u32)
}

/// A fee that can mark an offer or passthrough sale (shape 4): that of a
/// marketplace priced proceeds plus fees, not Magisat's (its offers are
/// shape 2), a `proceeds` marketplace's (transfers pay it too: its offers
/// are marked offers, shape 2, with its marker) or a buying tool's.
fn offer_fee(fee: &Fee) -> bool {
  fee.marketplace.role == Role::Marketplace
    && fee.marketplace.price_rule == PriceRule::ProceedsPlusFees
}

/// Shape 4: a marketplace's accepted offer or passthrough sale.
fn offer(view: &TxView, input: &MoveInput, marks: &Marks) -> Option<Sale> {
  let fees: Vec<&Fee> = marks.fees.iter().filter(|fee| offer_fee(fee)).collect();
  let marketplace = main_marketplace(fees.iter().copied())?;
  if marks.funded_by_marketplace {
    return None;
  }
  let (inputs, outputs) = (&view.tx.inputs, &view.tx.outputs);
  let seller_hash = inputs[input.vin as usize].script_hash;
  if inputs.iter().all(|other| other.script_hash == seller_hash) {
    return None;
  }
  // A rune trade (the transaction carries a runestone): its items are runes
  // and move by edict, and the bidders' escrowed funds pay for them, so
  // tracked sats in those funds reach the payment or another bidder's
  // change by FIFO without being sold. Magic Eden's rune bids (1-of-2 P2WSH
  // escrows, a runestone at output 0) were 9 of the fork's offers, every
  // one such a misreading, 4 below the UTXO's value; no other offer in
  // the fork's history carries a runestone.
  if outputs
    .iter()
    .any(|output| is_runestone(&output.script_pubkey))
  {
    return None;
  }
  // The sats that leave the seller's script, and the output that took most
  // of them (a seller may split one sat out of a UTXO and keep the rest).
  let seller = &input.utxo.script_pubkey;
  let item = view
    .destinations(input)
    .into_iter()
    .filter_map(|(vout, n)| Some((vout?, n)))
    .filter(|&(vout, _)| outputs[vout as usize].script_pubkey != *seller)
    .max_by_key(|&(vout, n)| (n, std::cmp::Reverse(vout)))
    .map(|(vout, _)| vout)?;
  // A two-party trade delivers the item in an output fed by the seller and
  // at most one other party (the buyer's padding or funding). An output
  // merging the inputs of several other parties is a batch's (an exchange
  // payout's change, a coinjoin), not an item: e.g. a 43 BTC exchange batch
  // of five 10 BTC inputs, two holding tracked sats, that also paid a
  // marketplace's fee address.
  let (item_start, item_end) = view.output_range(item as usize);
  let mut feeders: Vec<u32> = Vec::new();
  for (vin, other) in inputs.iter().enumerate() {
    let (start, end) = view.input_range(vin);
    if other.script_hash != seller_hash
      && start < item_end
      && item_start < end
      && !feeders.contains(&other.script_hash)
    {
      feeders.push(other.script_hash);
    }
  }
  if feeders.len() > 1 {
    return None;
  }
  // The outputs the seller's tracked sats went to, which the payment is
  // not (its change and fee inputs are netted out below).
  let items: Vec<u32> = view
    .mv
    .inputs
    .iter()
    .filter(|other| inputs[other.vin as usize].script_hash == seller_hash)
    .flat_map(|other| view.destinations(other))
    .filter_map(|(vout, _)| vout)
    .collect();
  // The payment: to someone other than the buyer, of the largest net value:
  // less every input from its script (a party's change nets out) and the
  // sats the seller's own inputs put into it (a seller is not paid with
  // its own sats, e.g. a buyer's funding that happens to hold tracked sats).
  let buyer = &outputs[item as usize].script_pubkey;
  let seller_ranges: Vec<(u64, u64)> = inputs
    .iter()
    .enumerate()
    .filter(|(_, other)| other.script_hash == seller_hash)
    .map(|(vin, _)| view.input_range(vin))
    .collect();
  let from_seller = |vout: usize| -> u64 {
    let (start, end) = view.output_range(vout);
    seller_ranges
      .iter()
      .map(|&(from, to)| to.min(end).saturating_sub(from.max(start)))
      .sum()
  };
  let own = |vout: usize, except: Option<u32>| -> u64 {
    let hash = script_hash(&outputs[vout].script_pubkey);
    inputs
      .iter()
      .enumerate()
      .filter(|&(vin, other)| other.script_hash == hash && Some(vin as u32) != except)
      .map(|(_, other)| other.value)
      .sum()
  };
  // An output taking exactly the sats of an input ahead of the seller's,
  // whole, at another script is that input's UTXO passing to its buyer: the
  // trade's item (offers put the item first, its funding after), not a
  // payment. Then the "seller" is the buyer's funding holding tracked sats,
  // whose sats went on to pay for that item: 11 of the fork's offers, each
  // "sold" for the item's value (10,000 sats and the like) a UTXO worth up
  // to 3.5 BTC; no other offer was paid by such an output.
  let passes_whole = |vout: usize| -> bool {
    let range = view.output_range(vout);
    let hash = script_hash(&outputs[vout].script_pubkey);
    (0..input.vin as usize)
      .any(|vin| inputs[vin].script_hash != hash && view.input_range(vin) == range)
  };
  let (payment, net) = outputs
    .iter()
    .enumerate()
    .filter(|&(vout, output)| {
      !items.contains(&(vout as u32))
        && !marks.is_marketplace_output(vout as u32)
        && !output.script_pubkey.is_op_return()
        && output.script_pubkey != *buyer
        && !passes_whole(vout)
    })
    .map(|(vout, output)| {
      (
        vout as u32,
        output
          .value
          .to_sat()
          .saturating_sub(own(vout, None))
          .saturating_sub(from_seller(vout)),
      )
    })
    .max_by_key(|&(vout, net)| (net, std::cmp::Reverse(vout)))?;
  if net < MIN_OFFER_PAYMENT {
    return None;
  }
  // The proceeds keep the sold UTXO's postage when the payment returns to
  // its script, as a listing's payment does.
  let proceeds = outputs[payment as usize]
    .value
    .to_sat()
    .saturating_sub(own(payment as usize, Some(input.vin)));
  let kind = if payment == input.vin && marketplace.has_flow(PASSTHROUGH) {
    SaleKind::Listing
  } else {
    SaleKind::Offer
  };
  let mut sale = sale(view, input, kind, payment, Some(item));
  sale.seller_proceeds = proceeds;
  sale.price = proceeds;
  // Only what left the seller was sold.
  sale.seller.pieces = view.pieces_to(input, |vout| {
    outputs[vout as usize].script_pubkey != *seller
  });
  Some(sale)
}

/// The selling marketplace that took the most fees in the transaction.
fn main_marketplace<'a, 'b>(fees: impl Iterator<Item = &'b Fee<'a>>) -> Option<&'a Marketplace>
where
  'a: 'b,
{
  let mut totals: Vec<(&Marketplace, u64)> = Vec::new();
  for fee in fees {
    match totals.iter_mut().find(|(m, _)| m.id == fee.marketplace.id) {
      Some((_, total)) => *total += fee.value,
      None => totals.push((fee.marketplace, fee.value)),
    }
  }
  totals
    .into_iter()
    .max_by_key(|&(_, total)| total)
    .map(|(m, _)| m)
}

/// Prices the transaction's listings by its marketplace's rule: the
/// marketplace whose fee the transaction pays, else whose marker it carries,
/// else the one buying with its own wallet (or UniSat, by its dummies).
fn price_listings(view: &TxView, marketplaces: &Marketplaces, sales: &mut [Sale], marks: &Marks) {
  if sales.is_empty() {
    return;
  }
  let fees = &marks.fees;
  let marketplace = match main_marketplace(fees.iter()).or_else(|| marks.marker()) {
    Some(marketplace) => marketplace,
    None => {
      // A marketplace buying a listing with its own wallet pays itself no
      // fee: its sale all the same.
      let buyer = sales[0].buyer_script.as_deref();
      if let Some(marketplace) = buyer
        .and_then(|script| marketplaces.buying_wallet(script, view.height))
        .filter(|_| {
          sales
            .iter()
            .all(|sale| sale.buyer_script.as_deref() == buyer)
        })
      {
        marketplace
      } else {
        if let (Some(unisat), Some(fee)) = (marketplaces.by_id(UNISAT), unisat_fee(view)) {
          let fee = fee.map(|vout| view.tx.outputs[vout as usize].value.to_sat());
          attribute(sales, fee.unwrap_or(0), unisat);
        }
        return;
      }
    }
  };
  match marketplace.price_rule {
    PriceRule::Magisat => {
      for sale in sales.iter_mut() {
        let maker = if view.height >= MAGISAT_MAKER_FEE_HEIGHT {
          sale.seller_proceeds / 199
        } else {
          0
        };
        sale.marketplace = marketplace.reported_id().into();
        sale.fees = maker;
        sale.price = sale.seller_proceeds.saturating_add(maker);
      }
    }
    PriceRule::ProceedsPlusFees => {
      attribute(sales, fees.iter().map(|fee| fee.value).sum(), marketplace)
    }
    // The fee output is the buyer's, on top of the price.
    PriceRule::Proceeds => attribute(sales, 0, marketplace),
  }
}

/// UniSat's layout: two 300-sat dummies from one script at vin 0 and 1,
/// returned merged to it at output 0, the seller at vin 2; `Some(fee)` with
/// the fee output (the output after the payment, to the same script) if
/// any.
fn unisat_fee(view: &TxView) -> Option<Option<u32>> {
  let (inputs, outputs) = (&view.tx.inputs, &view.tx.outputs);
  let dummies = inputs.get(..2)?;
  let hash = dummies[0].script_hash;
  if dummies
    .iter()
    .any(|dummy| dummy.value != UNISAT_DUMMY || dummy.script_hash != hash)
    || inputs.get(2)?.sighash != Some(LISTING_SIGHASH)
    || outputs.first()?.value.to_sat() != 2 * UNISAT_DUMMY
    || script_hash(&outputs[0].script_pubkey) != hash
  {
    return None;
  }
  Some(
    outputs
      .get(3)
      .filter(|fee| script_hash(&fee.script_pubkey) == hash)
      .map(|_| 3),
  )
}

/// Attributes the transaction's two-party sales: by a fee in the
/// transaction, else by the settlement spending the escrow output in the
/// same block.
fn attribute_psbts(
  view: &TxView,
  block: &BlockView,
  marketplaces: &Marketplaces,
  sales: &mut [Sale],
  fees: &[Fee],
) {
  split_payments(sales);
  if sales.is_empty() {
    return;
  }
  if let Some(marketplace) = main_marketplace(fees.iter()) {
    attribute(sales, fees.iter().map(|fee| fee.value).sum(), marketplace);
    return;
  }
  for sale in sales.iter_mut() {
    let Some(escrow) = sale.buyer_vout else {
      continue;
    };
    if !view.tx.outputs[escrow as usize].script_pubkey.is_p2wsh() {
      continue;
    }
    let Some(settlement) = block.spender(OutPoint::new(view.tx.txid, escrow)) else {
      continue;
    };
    let marks = marks(settlement, &[], view.height, marketplaces);
    let fees: Vec<&Fee> = marks
      .fees
      .iter()
      .filter(|fee| fee.marketplace.has_flow(SECURE_PURCHASE))
      .collect();
    let marketplace = main_marketplace(fees.iter().copied()).or_else(|| {
      marks
        .markers
        .iter()
        .map(|(_, marketplace)| *marketplace)
        .find(|marketplace| marketplace.has_flow(SECURE_PURCHASE))
    });
    if let Some(marketplace) = marketplace {
      attribute(
        std::slice::from_mut(sale),
        fees.iter().map(|fee| fee.value).sum(),
        marketplace,
      );
    }
  }
}

/// Prices the transaction's offers: a payment shared by several sales split
/// by their UTXOs' values, the fees by their proceeds; none when the fees
/// are more than half the price, nor an offer whose seller handed over a
/// whole UTXO for less than its value.
fn price_offers(view: &TxView, sales: &mut Vec<Sale>, fees: &[Fee]) {
  split_payments(sales);
  let fees: Vec<&Fee> = fees.iter().filter(|fee| offer_fee(fee)).collect();
  let Some(marketplace) = main_marketplace(fees.iter().copied()) else {
    sales.clear();
    return;
  };
  let total: u64 = fees.iter().map(|fee| fee.value).sum();
  let proceeds: u64 = sales.iter().map(|sale| sale.seller_proceeds).sum();
  if u128::from(total) * 2 > u128::from(proceeds) + u128::from(total) {
    sales.clear();
    return;
  }
  attribute(sales, total, marketplace);
  // Last line, after the shape rules (rune trades, merged items, an earlier
  // input's item taken for the payment), which account for every offer the
  // fork found below its UTXO's value: an offer's payment is inferred, not
  // signed for, and a seller does not hand over a whole UTXO for less than
  // the sats in it, so such a reading has the wrong payment. Not applied to
  // split sales (the seller keeps the rest of the UTXO, so `seller.value`
  // is not what was sold) nor to listings, whose price the seller signed
  // (SINGLE|ANYONECANPAY), however low.
  let before = sales.len();
  sales.retain(|sale| sale.price >= sale.seller.value || !left_whole(view, sale));
  if sales.len() < before {
    attribute(sales, total, marketplace);
  }
}

/// No output to the seller's script took any of the sold UTXO's sats.
fn left_whole(view: &TxView, sale: &Sale) -> bool {
  let (start, end) = view.input_range(sale.vin as usize);
  view
    .tx
    .outputs
    .iter()
    .enumerate()
    .filter(|(_, output)| output.script_pubkey == sale.seller.script_pubkey)
    .all(|(vout, _)| {
      let (from, to) = view.output_range(vout);
      to <= start || end <= from
    })
}

/// An OP_RETURN output whose next opcode is OP_PUSHNUM_13: a runestone.
fn is_runestone(script: &Script) -> bool {
  script.as_bytes().starts_with(&[
    opcodes::all::OP_RETURN.to_u8(),
    opcodes::all::OP_PUSHNUM_13.to_u8(),
  ])
}

/// Splits one payment among the sales it pays (several seller inputs paid
/// by one output), in proportion to their UTXOs' values.
fn split_payments(sales: &mut [Sale]) {
  let mut payments: Vec<Option<u32>> = sales.iter().map(|sale| sale.payment_vout).collect();
  payments.sort_unstable();
  payments.dedup();
  for payment in payments {
    let group: Vec<usize> = (0..sales.len())
      .filter(|&i| sales[i].payment_vout == payment)
      .collect();
    if group.len() < 2 {
      continue;
    }
    // Each sale of the group holds the whole payment so far.
    let total = u128::from(sales[group[0]].seller_proceeds);
    let weights: Vec<u128> = group
      .iter()
      .map(|&i| u128::from(sales[i].seller.value))
      .collect();
    for (&i, share) in group.iter().zip(proportional(total, &weights)) {
      sales[i].seller_proceeds = share;
      sales[i].price = share;
    }
  }
}

/// Splits `fees` over the sales in proportion to their proceeds (equally
/// when none has any) and names the marketplace.
fn attribute(sales: &mut [Sale], fees: u64, marketplace: &Marketplace) {
  if sales.is_empty() {
    return;
  }
  let weights: Vec<u128> = if sales.iter().all(|sale| sale.seller_proceeds == 0) {
    vec![1; sales.len()]
  } else {
    sales
      .iter()
      .map(|sale| u128::from(sale.seller_proceeds))
      .collect()
  };
  for (sale, share) in sales
    .iter_mut()
    .zip(proportional(u128::from(fees), &weights))
  {
    sale.fees = share;
    sale.price = sale.seller_proceeds.saturating_add(share);
    sale.marketplace = marketplace.reported_id().into();
  }
}

/// `total` split in proportion to `weights`, largest remainder first.
fn proportional(total: u128, weights: &[u128]) -> Vec<u64> {
  let weight: u128 = weights.iter().sum();
  if weight == 0 {
    return vec![0; weights.len()];
  }
  let mut shares: Vec<u128> = weights.iter().map(|w| total * w / weight).collect();
  let mut left = total - shares.iter().sum::<u128>();
  let mut order: Vec<usize> = (0..weights.len()).collect();
  order.sort_by_key(|&i| std::cmp::Reverse(total * weights[i] % weight));
  for i in order {
    if left == 0 {
      break;
    }
    shares[i] += 1;
    left -= 1;
  }
  shares
    .into_iter()
    .map(|share| u64::try_from(share).unwrap_or(u64::MAX))
    .collect()
}
