//! Sales of tracked sats, by the shapes measured on Magisat's whole sales
//! history (docs/SALES-SHAPES.md on rst/sales-research: 37,749 sales, every
//! decodable one checked on chain).
//!
//! Every tracked input of a transaction is tried against these shapes, in
//! order; the first that matches makes the input a sale:
//!
//! 1. **Listing**: the input signed SINGLE|ANYONECANPAY (`0x83`); its paired
//!    output `outputs[vin]` pays the seller. Not a sale when `vin` has no
//!    paired output (SIGHASH_SINGLE's out-of-range case) or when the sold
//!    sats land in that very output (no counterparty). 35,221 / 35,221
//!    Magisat listing sales; 70 / 70 tracked-input `0x83` transactions in
//!    the fork's retained moves were sales.
//! 2. **Offer** (Magisat's accepted offers, 129 / 129): the seller's input
//!    is vin 0, signed ALL (`0x01`) like every other input; output 0
//!    receives the sold sats, worth exactly the input, at another script;
//!    output 1 pays the seller; output 2 is a Magisat fee.
//! 3. **Two-party PSBT** (`0x81`, ALL|ANYONECANPAY): the seller pre-signs
//!    `[sats → buyer, payment → seller]` and the buyer adds inputs signed
//!    without ANYONECANPAY. Every `0x81` input must be a tracked input of
//!    the seller's script that passes whole (same value) to the output at
//!    its own index, at another script, where its sats land; at least one
//!    input must sign without ANYONECANPAY. The payment is an output to the
//!    seller's script, else the output right after the paired ones (sellers
//!    often get paid at a separate address). Exchange batches and
//!    consolidations (every input `0x81`) never match, nor do custodial
//!    withdrawals whose `0x81` inputs are split (a fee payer's input
//!    signs ALL). Reported as a listing of an unknown marketplace. Over the
//!    fork's moves window (blocks 965,417 to 969,448): 84 / 84 two-party
//!    transactions, 0 of 2,459 all-`0x81` ones, 0 of 8 custodial ones.
//! 4. **Fee-only** (marketplaces marked `fee_only_sales`, e.g. legacy Magic
//!    Eden): the transaction pays the marketplace's fee address, the sats
//!    leave the seller's script and another output pays it.
//!
//! **Fee outputs** pay a fee address of the table valid at the height,
//! except addresses that receive the transaction's tracked sats or fund it
//! through a tracked input: Magisat's own wallet buying or moving sats, not
//! fees. A fee output alone is never a sale (Magisat's transfer and
//! isolation tools pay one too).
//!
//! **Price**:
//! * a listing in a transaction with a Magisat fee output: Magisat's price
//!   rule, `P` = the paired payment, plus the 0.5 % maker fee `⌊P/199⌋` from
//!   block 861,287 (`fees`); the buyer's taker fee is not part of the price
//!   (exact for 97.6 % of Magisat's sales, 100 % before 861,287, and for the
//!   120 / 120 in the moves window);
//! * an offer: output 1 plus the taker fee, half of output 2 (maker and taker
//!   fees are equal since August 2024; 4 of 129 earlier offers differ);
//! * a two-party PSBT: the payment, split over the seller's inputs by value
//!   when one output pays several;
//! * other marketplaces: proceeds plus the transaction's fee outputs, split
//!   over its sales in proportion to their proceeds;
//! * no fee output: the proceeds, marketplace "unknown".

use {
  super::{
    marketplaces::{Marketplace, Marketplaces, PriceRule},
    view::TxView,
  },
  crate::model::{MoveInput, Sale, SaleKind},
  bitcoin::Script,
};

/// SIGHASH_SINGLE | SIGHASH_ANYONECANPAY: marketplace listings.
pub const LISTING_SIGHASH: u8 = 0x83;

/// SIGHASH_ALL | SIGHASH_ANYONECANPAY: two-party PSBT sales (and exchange
/// batches).
pub const PSBT_SIGHASH: u8 = 0x81;

/// SIGHASH_ALL, explicit: both sides of a Magisat offer.
pub const OFFER_SIGHASH: u8 = 0x01;

/// The first block with a Magisat maker fee (2024-09-14).
pub const MAGISAT_MAKER_FEE_HEIGHT: u32 = 861_287;

/// Marketplace id of sales in transactions paying no known fee address.
pub const UNKNOWN_MARKETPLACE: &str = "unknown";

const ANYONECANPAY: u8 = 0x80;

struct Fee<'a> {
  vout: u32,
  value: u64,
  marketplace: &'a Marketplace,
}

pub(super) fn detect(view: &TxView, marketplaces: &Marketplaces, out: &mut Vec<Sale>) {
  let fees = fee_outputs(view, marketplaces);
  let mut listings = Vec::new();
  let mut fee_only = Vec::new();
  let mut psbts = Vec::new();
  for input in &view.mv.inputs {
    if let Some(sale) = listing(view, input) {
      listings.push(sale);
    } else if let Some(sale) = offer(view, input, &fees) {
      out.push(sale);
    } else if let Some(sale) = psbt(view, input, &fees) {
      psbts.push(sale);
    } else if let Some(sale) = fee_only_sale(view, input, &fees) {
      fee_only.push(sale);
    }
  }
  price_listings(&mut listings, &fees, view.height);
  out.append(&mut listings);
  split_payments(&mut psbts);
  out.append(&mut psbts);
  attribute_fees(&mut fee_only, &fees);
  out.append(&mut fee_only);
  out.sort_by_key(|sale| sale.vin);
}

/// The transaction's marketplace fee outputs.
fn fee_outputs<'a>(view: &TxView, marketplaces: &'a Marketplaces) -> Vec<Fee<'a>> {
  let not_a_fee = |script: &Script| {
    view
      .mv
      .outputs
      .iter()
      .any(|output| output.script_pubkey.as_script() == script)
      || view
        .mv
        .inputs
        .iter()
        .any(|input| input.utxo.script_pubkey.as_script() == script)
  };
  view
    .tx
    .outputs
    .iter()
    .enumerate()
    .filter_map(|(vout, output)| {
      let marketplace = marketplaces.fee_output(&output.script_pubkey, view.height)?;
      (!not_a_fee(&output.script_pubkey)).then_some(Fee {
        vout: vout as u32,
        value: output.value.to_sat(),
        marketplace,
      })
    })
    .collect()
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
  if view.tx.inputs[input.vin as usize].sighash != Some(LISTING_SIGHASH) {
    return None;
  }
  view.tx.outputs.get(input.vin as usize)?;
  let buyer = view.main_destination(input);
  if buyer == Some(input.vin) {
    return None;
  }
  Some(sale(view, input, SaleKind::Listing, input.vin, buyer))
}

/// Shape 2: an accepted Magisat offer.
fn offer(view: &TxView, input: &MoveInput, fees: &[Fee]) -> Option<Sale> {
  let (inputs, outputs) = (&view.tx.inputs, &view.tx.outputs);
  if input.vin != 0
    || inputs.len() < 2
    || inputs.iter().any(|i| i.sighash != Some(OFFER_SIGHASH))
    || outputs.len() < 3
  {
    return None;
  }
  let fee = fees
    .iter()
    .find(|fee| fee.vout == 2 && fee.marketplace.price_rule == PriceRule::Magisat)?;
  if outputs[0].value.to_sat() != inputs[0].value
    || outputs[0].script_pubkey == input.utxo.script_pubkey
    || view.main_destination(input) != Some(0)
  {
    return None;
  }
  let mut sale = sale(view, input, SaleKind::Offer, 1, Some(0));
  sale.marketplace = fee.marketplace.id.clone();
  sale.fees = fee.value / 2;
  sale.price = sale.seller_proceeds.saturating_add(sale.fees);
  Some(sale)
}

/// Shape 3: a two-party ALL|ANYONECANPAY sale.
fn psbt(view: &TxView, input: &MoveInput, fees: &[Fee]) -> Option<Sale> {
  let (inputs, outputs) = (&view.tx.inputs, &view.tx.outputs);
  if inputs[input.vin as usize].sighash != Some(PSBT_SIGHASH) {
    return None;
  }
  let seller = &input.utxo.script_pubkey;
  // The seller's inputs: tracked, of one script, each passing whole to the
  // output at its index.
  let mut last = 0;
  for (vin, other) in inputs.iter().enumerate() {
    if other.sighash != Some(PSBT_SIGHASH) {
      continue;
    }
    if view
      .tracked(vin as u32)
      .is_none_or(|tracked| tracked.utxo.script_pubkey != *seller)
      || outputs
        .get(vin)
        .is_none_or(|output| output.value.to_sat() != other.value)
    {
      return None;
    }
    last = vin;
  }
  // The buyer's inputs.
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
  let payment = payment_to(view, seller, buyer, fees).or_else(|| {
    let vout = last + 1;
    let output = outputs.get(vout)?;
    (output.value.to_sat() > 0
      && output.script_pubkey != *buyer_script
      && fees.iter().all(|fee| fee.vout != vout as u32))
    .then_some(vout as u32)
  })?;
  Some(sale(view, input, SaleKind::Listing, payment, Some(buyer)))
}

/// Shape 4: a fee-only (legacy Magic Eden) sale.
fn fee_only_sale(view: &TxView, input: &MoveInput, fees: &[Fee]) -> Option<Sale> {
  if !fees.iter().any(|fee| fee.marketplace.fee_only_sales) {
    return None;
  }
  let seller = &input.utxo.script_pubkey;
  let buyer = view.main_destination(input)?;
  if view.tx.outputs[buyer as usize].script_pubkey == *seller {
    return None;
  }
  let payment = payment_to(view, seller, buyer, fees)?;
  Some(sale(view, input, SaleKind::Unknown, payment, Some(buyer)))
}

/// The first output paying `seller` that is neither the buyer's nor a fee.
fn payment_to(view: &TxView, seller: &Script, buyer: u32, fees: &[Fee]) -> Option<u32> {
  view
    .tx
    .outputs
    .iter()
    .enumerate()
    .map(|(vout, output)| (vout as u32, output))
    .find(|&(vout, output)| {
      vout != buyer
        && output.script_pubkey.as_script() == seller
        && fees.iter().all(|fee| fee.vout != vout)
    })
    .map(|(vout, _)| vout)
}

/// The marketplace that took the most fees in the transaction.
fn main_marketplace<'a>(fees: &[Fee<'a>]) -> Option<&'a Marketplace> {
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

/// Prices the transaction's listings by its marketplace's rule.
fn price_listings(sales: &mut [Sale], fees: &[Fee], height: u32) {
  let Some(marketplace) = main_marketplace(fees) else {
    return;
  };
  match marketplace.price_rule {
    PriceRule::Magisat => {
      for sale in sales.iter_mut() {
        let maker = if height >= MAGISAT_MAKER_FEE_HEIGHT {
          sale.seller_proceeds / 199
        } else {
          0
        };
        sale.marketplace = marketplace.id.clone();
        sale.fees = maker;
        sale.price = sale.seller_proceeds.saturating_add(maker);
      }
    }
    PriceRule::ProceedsPlusFees => attribute_fees(sales, fees),
  }
}

/// Splits one payment among the sales it pays (several seller inputs of a
/// PSBT sale), in proportion to the inputs' values.
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

/// Splits the transaction's fee outputs over its sales in proportion to
/// their proceeds (equally when none has any), naming the marketplace that
/// took the most.
fn attribute_fees(sales: &mut [Sale], fees: &[Fee]) {
  let Some(marketplace) = main_marketplace(fees) else {
    return;
  };
  if sales.is_empty() {
    return;
  }
  let total = u128::from(fees.iter().map(|fee| fee.value).sum::<u64>());
  let weights: Vec<u128> = if sales.iter().all(|sale| sale.seller_proceeds == 0) {
    vec![1; sales.len()]
  } else {
    sales
      .iter()
      .map(|sale| u128::from(sale.seller_proceeds))
      .collect()
  };
  for (sale, share) in sales.iter_mut().zip(proportional(total, &weights)) {
    sale.fees = share;
    sale.price = sale.seller_proceeds.saturating_add(share);
    sale.marketplace = marketplace.id.clone();
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
