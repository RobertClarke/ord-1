//! Sales and inscriptions on tracked sats, derived per block from the
//! resolved block (inputs carry their sighash flag and inscription
//! envelopes) and the tracker's delta (which inputs carried which tracked
//! pieces, and where they went). Pure: no I/O, no tracker state, so it adds
//! nothing to the fast sync's critical path beyond the few transactions that
//! spend tracked sats. See docs/API.md "Sales and inscriptions".
//!
//! OWNER: core agent (detection). The store/server agent persists
//! `MarketDelta`s and serves them; keep this signature.
//!
//! Only transactions in `delta.moves` (those that spent tracked sats) are
//! looked at:
//! * inscriptions (`inscriptions.rs`): an envelope whose target sat (ord's
//!   pointer rule) is one of the transaction's tracked sats;
//! * sales (`sales.rs`): a tracked input sold by a listing or a fee-only
//!   marketplace shape, with the marketplace from the fee outputs
//!   (`marketplaces.rs`, `data/marketplaces.json`).

mod inscriptions;
pub mod marketplaces;
mod sales;
mod units;
mod view;

#[cfg(test)]
mod tests;

pub use {
  marketplaces::{Marketplace, Marketplaces},
  sales::{LISTING_SIGHASH, UNKNOWN_MARKETPLACE},
  units::{sale_sats, sale_unit},
};

use {
  crate::{
    model::{BlockDelta, MarketDelta, ResolvedBlock},
    sats::SatCatalog,
  },
  std::sync::Arc,
  view::TxView,
};

pub struct MarketContext {
  pub catalog: Arc<SatCatalog>,
  pub marketplaces: Marketplaces,
}

/// Sales and inscriptions in `block`, whose tracker delta is `delta`.
pub fn analyze(block: &ResolvedBlock, delta: &BlockDelta, ctx: &MarketContext) -> MarketDelta {
  let mut market = MarketDelta::default();
  for m in &delta.moves {
    if m.coinbase || m.inputs.is_empty() {
      continue;
    }
    let Some(tx) = block
      .txs
      .get(m.tx_index as usize)
      .filter(|tx| tx.txid == m.txid)
    else {
      continue;
    };
    let view = TxView::new(tx, m, block.height);
    inscriptions::detect(&view, &mut market.inscriptions);
    sales::detect(&view, &ctx.marketplaces, &mut market.sales);
  }
  market
}
