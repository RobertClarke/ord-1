//! Block sources: bitcoind JSON-RPC, the fast initial-sync pipeline and the
//! live (`getblock <hash> 3`) source. Both sources produce identical
//! `ResolvedBlock`s. See docs/DESIGN.md §6.
//!
//! OWNER: source agent. The public API below is the contract used by the
//! indexer; keep the signatures.
//!
//! Everything here blocks: `Rpc` uses `reqwest::blocking`, which panics when
//! it is created, used or dropped on a thread that is driving an async
//! runtime. Call it from plain threads (or `tokio::task::spawn_blocking`).
//!
//! ```ignore
//! let rpc = Rpc::new(RpcConfig { url, auth: RpcAuth::Cookie(path), timeout_secs: 120 })?;
//! let tip = rpc.block_count()?;
//! let mut fast = FastSource::start(rpc.clone(), FastSyncOptions::recommended(tip - 12))?;
//! while let Some(block) = fast.next_block() {
//!   tracker.apply_block(&block?)?;
//! }
//! drop(fast); // frees the ~11 GiB UTXO value map
//!
//! let live = LiveSource::new(rpc);
//! for block in live.blocks(tip - 11, new_tip, 4) {
//!   tracker.apply_block(&block?)?;
//! }
//! ```
//!
//! Errors from bitcoind carry an `RpcError` (`error.downcast_ref()`), e.g.
//! -5 for an unknown block hash, -8 for a height above the tip.

use {
  bitcoin::{ScriptBuf, TxOut},
  std::path::PathBuf,
};

pub use {
  fast::{FastSource, FastSourceDiagnostics, FastSourceStats, FastSyncOptions, decode_block},
  live::LiveSource,
  rpc::{RetryPolicy, Rpc, RpcError},
  witness::{input_envelopes, input_sighash, is_der_signature},
};

mod fast;
mod hex;
mod live;
mod rpc;
mod utxo;
mod witness;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcAuth {
  /// bitcoind's `.cookie` file, re-read on authentication failure.
  Cookie(PathBuf),
  UserPass {
    username: String,
    password: String,
  },
  None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcConfig {
  /// e.g. `http://100.121.138.22:8332`
  pub url: String,
  pub auth: RpcAuth,
  /// Per-request timeout, including reading the response body; 0 disables
  /// it.
  pub timeout_secs: u64,
}

/// Longest script Core keeps in its UTXO set (`MAX_SCRIPT_SIZE`).
pub const MAX_SCRIPT_SIZE: usize = 10_000;

/// Core's `CScript::IsUnspendable`: the script starts with `OP_RETURN` or is
/// longer than `MAX_SCRIPT_SIZE`. Such outputs never enter the UTXO set.
pub fn is_unspendable(script_pubkey: &ScriptBuf) -> bool {
  script_pubkey.is_op_return() || script_pubkey.len() > MAX_SCRIPT_SIZE
}

/// Whether `output` enters the UTXO set: every output except unspendable
/// ones and the genesis coinbase's (which Core never adds).
pub(crate) fn enters_utxo_set(height: u32, tx_index: usize, output: &TxOut) -> bool {
  !(height == 0 && tx_index == 0) && !is_unspendable(&output.script_pubkey)
}

#[cfg(test)]
mod tests {
  use {super::*, bitcoin::Amount};

  #[test]
  fn unspendable_scripts() {
    assert!(!is_unspendable(&ScriptBuf::new()));
    assert!(is_unspendable(&ScriptBuf::from_bytes(vec![0x6a])));
    assert!(is_unspendable(&ScriptBuf::from_bytes(vec![
      0x6a, 0x01, 0xff
    ])));
    assert!(!is_unspendable(&ScriptBuf::from_bytes(vec![0x51, 0x6a])));
    assert!(!is_unspendable(&ScriptBuf::from_bytes(vec![0x51; 10_000])));
    assert!(is_unspendable(&ScriptBuf::from_bytes(vec![0x51; 10_001])));
  }

  #[test]
  fn genesis_coinbase_never_enters_the_utxo_set() {
    let output = TxOut {
      value: Amount::from_sat(50 * 100_000_000),
      script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
    };
    assert!(!enters_utxo_set(0, 0, &output));
    assert!(enters_utxo_set(0, 1, &output));
    assert!(enters_utxo_set(1, 0, &output));
    let op_return = TxOut {
      value: Amount::ZERO,
      script_pubkey: ScriptBuf::from_bytes(vec![0x6a]),
    };
    assert!(!enters_utxo_set(1, 0, &op_return));
  }
}
