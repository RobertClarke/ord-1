//! Sales of tracked sats.
//!
//! Every tracked input of a transaction is tried against the shapes below,
//! in order; the first that matches makes the input a sale. Then the
//! transaction's marketplace fees are attributed to its sales. Each shape
//! is one function, so the rules can be refined one by one as the sales
//! research (docs/SALES-SHAPES.md, docs/MARKETPLACES.md) lands.
//!
//! 1. **Listing**: the input is signed SINGLE|ANYONECANPAY (`0x83`): its
//!    owner pre-signed "this input, for output `vin`", the marketplace
//!    listing pattern, so `outputs[vin]` is the seller's payment. Not a
//!    sale when that output received the sold sats itself.
//! 2. **Fee-only** (legacy Magic Eden): the transaction pays the fee
//!    address of a marketplace marked `fee_only_sales`, the input's sats
//!    went to an output with another script, and another output pays the
//!    input's own script: that output is the seller's payment. Without such
//!    an output the seller cannot be told from a buyer whose padding
//!    happens to hold tracked sats, so nothing is recorded.
//!
//! Price: seller proceeds plus the marketplace fees attributable to the sale
//! (the transaction's fee outputs, split over its sales in proportion to
//! their proceeds). The buyer is the output that received most of the sold
//! sats.

use {
  super::{marketplaces::Marketplaces, view::TxView},
  crate::model::{MoveInput, Sale, SaleKind},
};

/// SIGHASH_SINGLE | SIGHASH_ANYONECANPAY, what marketplace listings sign.
pub const LISTING_SIGHASH: u8 = 0x83;

/// Marketplace id of sales in transactions paying no known fee address.
pub const UNKNOWN_MARKETPLACE: &str = "unknown";

struct Fee<'a> {
  vout: u32,
  value: u64,
  marketplace: &'a str,
  fee_only_sales: bool,
}

pub(super) fn detect(view: &TxView, marketplaces: &Marketplaces, out: &mut Vec<Sale>) {
  let fees: Vec<Fee> = view
    .tx
    .outputs
    .iter()
    .enumerate()
    .filter_map(|(vout, output)| {
      let marketplace = marketplaces.fee_output(&output.script_pubkey, view.height)?;
      Some(Fee {
        vout: vout as u32,
        value: output.value.to_sat(),
        marketplace: &marketplace.id,
        fee_only_sales: marketplace.fee_only_sales,
      })
    })
    .collect();

  let mut sales: Vec<Sale> = view
    .mv
    .inputs
    .iter()
    .filter_map(|input| listing(view, input).or_else(|| fee_only(view, input, &fees)))
    .collect();
  attribute_fees(&mut sales, &fees);
  out.extend(sales);
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

/// Shape 2: a fee-only (legacy Magic Eden) sale.
fn fee_only(view: &TxView, input: &MoveInput, fees: &[Fee]) -> Option<Sale> {
  if !fees.iter().any(|fee| fee.fee_only_sales) {
    return None;
  }
  let seller = &input.utxo.script_pubkey;
  let buyer = view.main_destination(input)?;
  if view.tx.outputs[buyer as usize].script_pubkey == *seller {
    return None;
  }
  let payment = view
    .tx
    .outputs
    .iter()
    .enumerate()
    .map(|(vout, output)| (vout as u32, output))
    .find(|&(vout, output)| {
      vout != buyer && output.script_pubkey == *seller && fees.iter().all(|fee| fee.vout != vout)
    })?
    .0;
  Some(sale(view, input, SaleKind::Unknown, payment, Some(buyer)))
}

/// Splits the transaction's marketplace fees over its sales in proportion
/// to their proceeds (largest remainder; equally when none has proceeds) and
/// names the marketplace that took the most.
fn attribute_fees(sales: &mut [Sale], fees: &[Fee]) {
  if sales.is_empty() || fees.is_empty() {
    return;
  }
  let mut per_marketplace: Vec<(&str, u64)> = Vec::new();
  for fee in fees {
    match per_marketplace
      .iter_mut()
      .find(|(id, _)| *id == fee.marketplace)
    {
      Some((_, total)) => *total += fee.value,
      None => per_marketplace.push((fee.marketplace, fee.value)),
    }
  }
  let marketplace = per_marketplace
    .iter()
    .max_by_key(|&&(_, total)| total)
    .map(|&(id, _)| id)
    .unwrap_or(UNKNOWN_MARKETPLACE);
  let total = u128::from(fees.iter().map(|fee| fee.value).sum::<u64>());
  let weights: Vec<u128> = if sales.iter().all(|sale| sale.seller_proceeds == 0) {
    vec![1; sales.len()]
  } else {
    sales
      .iter()
      .map(|sale| u128::from(sale.seller_proceeds))
      .collect()
  };
  let weight: u128 = weights.iter().sum();
  let mut shares: Vec<u128> = weights.iter().map(|w| total * w / weight).collect();
  let mut left = total - shares.iter().sum::<u128>();
  let mut by_remainder: Vec<usize> = (0..sales.len()).collect();
  by_remainder.sort_by_key(|&i| std::cmp::Reverse(total * weights[i] % weight));
  for i in by_remainder {
    if left == 0 {
      break;
    }
    shares[i] += 1;
    left -= 1;
  }
  for (sale, share) in sales.iter_mut().zip(shares) {
    let share = u64::try_from(share).unwrap_or(u64::MAX);
    sale.fees = share;
    sale.price = sale.seller_proceeds.saturating_add(share);
    sale.marketplace = marketplace.into();
  }
}
