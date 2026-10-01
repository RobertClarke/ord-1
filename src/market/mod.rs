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
//! * sales (`sales.rs`): a tracked input sold by a SINGLE|ANYONECANPAY
//!   listing, an accepted Magisat offer, a two-party ALL|ANYONECANPAY PSBT or
//!   a fee-only marketplace shape, priced and attributed by the fee outputs
//!   (`marketplaces.rs`, `data/marketplaces.json`).

mod inscriptions;
pub mod marketplaces;
mod sales;
mod units;
mod view;

#[cfg(test)]
mod tests;

pub use {
  marketplaces::{Marketplace, Marketplaces, PriceRule},
  sales::{
    LISTING_SIGHASH, MAGISAT_MAKER_FEE_HEIGHT, OFFER_SIGHASH, PSBT_SIGHASH, UNKNOWN_MARKETPLACE,
  },
  units::{sale_sats, sale_unit},
};

use {
  crate::{
    model::{BlockDelta, MarketDelta, ResolvedBlock},
    sats::SatCatalog,
    settings::Chain,
  },
  std::sync::Arc,
  view::TxView,
};

pub struct MarketContext {
  pub catalog: Arc<SatCatalog>,
  pub marketplaces: Marketplaces,
  /// Envelopes count as inscriptions from this height on, like ord
  /// (`first_inscription_height`).
  pub first_inscription_height: u32,
}

impl MarketContext {
  pub fn new(catalog: Arc<SatCatalog>, marketplaces: Marketplaces, chain: Chain) -> Self {
    Self {
      catalog,
      marketplaces,
      first_inscription_height: first_inscription_height(chain),
    }
  }
}

/// The height ord starts indexing inscriptions at; earlier envelopes are not
/// inscriptions (the sources report envelopes at every height).
pub fn first_inscription_height(chain: Chain) -> u32 {
  match chain {
    Chain::Mainnet => 767_430,
    Chain::Signet => 112_402,
    Chain::Testnet4 | Chain::Regtest => 0,
  }
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
    if block.height >= ctx.first_inscription_height {
      inscriptions::detect(&view, &mut market.inscriptions);
    }
    sales::detect(&view, &ctx.marketplaces, &mut market.sales);
  }
  market
}
