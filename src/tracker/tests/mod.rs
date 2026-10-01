//! Tracker tests: hand-written scenarios, a differential test against a naive
//! full sat-range model, undo/snapshot round trips and a benchmark.

mod bench;
mod differential;
mod mainnet;
mod scenarios;

use {
  super::*,
  crate::{
    model::{ResolvedInput, ResolvedTx},
    sats::math::{first_sat, subsidy},
  },
  bitcoin::{
    Amount, CompactTarget, ScriptBuf, TxMerkleNode, TxOut, Txid,
    block::{Header, Version},
    hashes::Hash,
  },
  std::sync::OnceLock,
};

pub(crate) fn catalog_for(config: &TrackerConfig) -> Arc<SatCatalog> {
  static DEFAULT: OnceLock<Arc<SatCatalog>> = OnceLock::new();
  static RANGELESS: OnceLock<Arc<SatCatalog>> = OnceLock::new();
  static CUSTOM: OnceLock<Arc<SatCatalog>> = OnceLock::new();
  let cell = if *config == TrackerConfig::default() {
    &DEFAULT
  } else if config.ranges.is_empty() {
    &RANGELESS
  } else {
    &CUSTOM
  };
  let catalog = cell.get_or_init(|| Arc::new(SatCatalog::new(config).unwrap()));
  assert_eq!(
    catalog.config(),
    config,
    "one custom config per test binary"
  );
  catalog.clone()
}

pub(crate) fn rangeless_config() -> TrackerConfig {
  TrackerConfig {
    ranges: Vec::new(),
    ..TrackerConfig::default()
  }
}

/// A small range straddling the boundary of blocks 11 and 12.
pub(crate) fn custom_config() -> TrackerConfig {
  TrackerConfig {
    ranges: vec![crate::model::TrackedRange {
      id: "custom".into(),
      start: 60_000_000_000 - 1_500_000_000,
      end: 60_000_000_000 + 700_000_000,
    }],
    ..TrackerConfig::default()
  }
}

/// An empty tracker whose tip is `height` (so the next block is `height + 1`).
pub(crate) fn tracker_at(config: &TrackerConfig, height: Option<u32>) -> (Tracker, Chain) {
  let catalog = catalog_for(config);
  let rows = catalog.rows().len();
  let chain = Chain::at(height);
  let snapshot = TrackerSnapshot {
    height,
    hash: height.map(|_| chain.prev),
    times: height.map_or(Vec::new(), |h| (0..=h).map(|i| chain.time_of(i)).collect()),
    lost_sats: 0,
    utxos: Vec::new(),
    lost: Vec::new(),
    overwritten: Vec::new(),
    mined: vec![0; rows],
  };
  (
    Tracker::restore(catalog, config.clone(), snapshot).unwrap(),
    chain,
  )
}

pub(crate) fn txid(n: u64) -> Txid {
  let mut bytes = [0u8; 32];
  bytes[..8].copy_from_slice(&n.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes());
  bytes[8..16].copy_from_slice(&n.to_le_bytes());
  Txid::from_byte_array(bytes)
}

/// A P2WPKH-shaped script unique to `n`.
pub(crate) fn script(n: u8) -> ScriptBuf {
  let mut bytes = vec![0x00, 0x14];
  bytes.extend([n; 20]);
  ScriptBuf::from_bytes(bytes)
}

pub(crate) fn out(value: u64) -> TxOut {
  out_to(value, 1)
}

pub(crate) fn out_to(value: u64, owner: u8) -> TxOut {
  TxOut {
    value: Amount::from_sat(value),
    script_pubkey: script(owner),
  }
}

pub(crate) fn op_return(value: u64) -> TxOut {
  TxOut {
    value: Amount::from_sat(value),
    script_pubkey: ScriptBuf::from_bytes(vec![0x6a, 0x04, 1, 2, 3, 4]),
  }
}

/// Unspendable because it is longer than 10,000 bytes.
pub(crate) fn huge_script(value: u64) -> TxOut {
  TxOut {
    value: Amount::from_sat(value),
    script_pubkey: ScriptBuf::from_bytes(vec![0x51; 10_001]),
  }
}

pub(crate) fn coinbase(txid: Txid, outputs: Vec<TxOut>) -> ResolvedTx {
  ResolvedTx {
    txid,
    inputs: Vec::new(),
    outputs,
  }
}

pub(crate) fn tx(txid: Txid, inputs: &[(OutPoint, u64)], outputs: Vec<TxOut>) -> ResolvedTx {
  ResolvedTx {
    txid,
    inputs: inputs
      .iter()
      .map(|&(previous_output, value)| ResolvedInput {
        previous_output,
        value,
        script_hash: 0,
        sighash: None,
        envelopes: Vec::new(),
      })
      .collect(),
    outputs,
  }
}

pub(crate) fn op(txid: Txid, vout: u32) -> OutPoint {
  OutPoint { txid, vout }
}

/// Builds consecutive blocks with consistent heights, hashes and times.
pub(crate) struct Chain {
  pub next: u32,
  pub prev: BlockHash,
  /// Overrides for the next block's time.
  pub next_time: Option<u32>,
}

impl Chain {
  /// Continues after `height` (a synthetic parent hash stands in for it).
  pub fn at(height: Option<u32>) -> Self {
    match height {
      None => Self {
        next: 0,
        prev: BlockHash::all_zeros(),
        next_time: None,
      },
      Some(h) => Self {
        next: h + 1,
        prev: BlockHash::from_byte_array([0xab; 32]),
        next_time: None,
      },
    }
  }

  /// Ten minutes per block from the genesis time.
  pub fn time_of(&self, height: u32) -> u32 {
    1_231_006_505 + height * 600
  }

  pub fn block(&mut self, txs: Vec<ResolvedTx>) -> ResolvedBlock {
    let height = self.next;
    let time = self.next_time.take().unwrap_or(self.time_of(height));
    let mut merkle = [0u8; 32];
    merkle[..4].copy_from_slice(&height.to_le_bytes());
    merkle[4..12].copy_from_slice(&(txs.len() as u64).to_le_bytes());
    if let Some(first) = txs.first() {
      merkle[12..].copy_from_slice(&first.txid.as_byte_array()[..20]);
    }
    let header = Header {
      version: Version::ONE,
      prev_blockhash: self.prev,
      merkle_root: TxMerkleNode::from_byte_array(merkle),
      time,
      bits: CompactTarget::from_consensus(0x207f_ffff),
      nonce: height,
    };
    let hash = header.block_hash();
    self.prev = hash;
    self.next += 1;
    ResolvedBlock {
      height,
      hash,
      header,
      txs,
    }
  }
}
