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
//!   listing, an accepted Magisat offer, a two-party ALL|ANYONECANPAY sale
//!   (Satflow's secure purchase) or a marketplace's offer or passthrough
//!   sale, priced and attributed by the fee outputs and markers
//!   (`marketplaces.rs`, `data/marketplaces.json`). Real transactions of
//!   each shape, and lookalikes that are not sales, are fixtures
//!   (`fixtures.rs`, `tests/fixtures/sales`).

mod inscriptions;
pub mod marketplaces;
mod sales;
mod units;
mod view;

#[cfg(test)]
pub(crate) mod fixtures;
#[cfg(test)]
mod tests;

pub use {
  marketplaces::{Marketplace, Marketplaces, PriceRule, Role},
  sales::{
    LISTING_SIGHASH, MAGISAT_MAKER_FEE_HEIGHT, MIN_OFFER_PAYMENT, OFFER_SIGHASH, PSBT_SIGHASH,
    UNKNOWN_MARKETPLACE,
  },
  units::{pure_rows, sale_pure_rows, sale_sats, sale_unit, unit_row},
};

use {
  crate::{
    model::{BlockDelta, MarketDelta, ResolvedBlock, ResolvedTx},
    sats::SatCatalog,
    settings::Chain,
  },
  bitcoin::OutPoint,
  std::{collections::HashMap, sync::Arc, sync::OnceLock},
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

/// The block a transaction is analyzed in: which transaction spends an
/// output, for sales settled by a second transaction (indexed on first use).
pub(crate) struct BlockView<'a> {
  block: &'a ResolvedBlock,
  spends: OnceLock<HashMap<OutPoint, usize>>,
}

impl<'a> BlockView<'a> {
  pub(crate) fn new(block: &'a ResolvedBlock) -> Self {
    Self {
      block,
      spends: OnceLock::new(),
    }
  }

  /// The transaction of this block that spends `outpoint`.
  pub(crate) fn spender(&self, outpoint: OutPoint) -> Option<&'a ResolvedTx> {
    let spends = self.spends.get_or_init(|| {
      let mut spends = HashMap::new();
      for (index, tx) in self.block.txs.iter().enumerate() {
        for input in &tx.inputs {
          spends.insert(input.previous_output, index);
        }
      }
      spends
    });
    spends.get(&outpoint).map(|&index| &self.block.txs[index])
  }
}

/// Sales and inscriptions in `block`, whose tracker delta is `delta`.
pub fn analyze(block: &ResolvedBlock, delta: &BlockDelta, ctx: &MarketContext) -> MarketDelta {
  let mut market = MarketDelta::default();
  let block_view = BlockView::new(block);
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
    sales::detect(&view, &block_view, &ctx.marketplaces, &mut market.sales);
  }
  market
}
