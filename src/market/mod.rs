//! Sales and inscriptions on tracked sats, derived per block from the
//! resolved block (inputs carry their sighash flag and inscription
//! envelopes) and the tracker's delta (which inputs carried which tracked
//! pieces, and where they went). Pure: no I/O, no tracker state, so it adds
//! nothing to the fast sync's critical path beyond the few transactions that
//! spend tracked sats. See docs/API.md "Sales and inscriptions".
//!
//! OWNER: core agent (detection). The store/server agent persists
//! `MarketDelta`s and serves them; keep this signature.

use {
  crate::{
    model::{BlockDelta, MarketDelta, ResolvedBlock},
    sats::SatCatalog,
  },
  std::sync::Arc,
};

/// Marketplace fee addresses by height range (data/marketplaces.json).
#[derive(Debug, Clone, Default)]
pub struct Marketplaces {}

impl Marketplaces {
  /// STUB (store agent) until rst/market lands: the embedded
  /// data/marketplaces.json.
  pub fn embedded() -> Self {
    Self::default()
  }
}

pub struct MarketContext {
  pub catalog: Arc<SatCatalog>,
  pub marketplaces: Marketplaces,
}

/// Sales and inscriptions in `block`, whose tracker delta is `delta`.
pub fn analyze(_block: &ResolvedBlock, _delta: &BlockDelta, _ctx: &MarketContext) -> MarketDelta {
  MarketDelta::default()
}
