//! Block sources: bitcoind JSON-RPC, the fast initial-sync pipeline and the
//! live (`getblock <hash> 3`) source. Both sources produce identical
//! `ResolvedBlock`s. See docs/DESIGN.md §6.
//!
//! OWNER: source agent. The public API below is the contract used by the
//! indexer; keep the signatures.

use {
  crate::model::ResolvedBlock,
  bitcoin::{BlockHash, block::Header},
  std::path::PathBuf,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcAuth {
  /// bitcoind's `.cookie` file, re-read on authentication failure.
  Cookie(PathBuf),
  UserPass { username: String, password: String },
  None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcConfig {
  /// e.g. `http://100.121.138.22:8332`
  pub url: String,
  pub auth: RpcAuth,
  pub timeout_secs: u64,
}

/// Blocking, thread-safe, cheaply cloneable JSON-RPC client with a keep-alive
/// connection pool; retries transient failures (connection reset, 5xx, work
/// queue full) with backoff.
#[derive(Clone)]
pub struct Rpc {}

impl Rpc {
  pub fn new(_config: RpcConfig) -> anyhow::Result<Self> {
    unimplemented!()
  }

  /// `getblockchaininfo.chain`: "main", "test", "testnet4", "signet", "regtest".
  pub fn chain(&self) -> anyhow::Result<String> {
    unimplemented!()
  }

  pub fn block_count(&self) -> anyhow::Result<u32> {
    unimplemented!()
  }

  pub fn best_block_hash(&self) -> anyhow::Result<BlockHash> {
    unimplemented!()
  }

  pub fn block_hash(&self, _height: u32) -> anyhow::Result<BlockHash> {
    unimplemented!()
  }

  pub fn block_header(&self, _hash: BlockHash) -> anyhow::Result<Header> {
    unimplemented!()
  }

  /// `getblock <hash> 0`, decoded.
  pub fn block_raw(&self, _hash: BlockHash) -> anyhow::Result<Vec<u8>> {
    unimplemented!()
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastSyncOptions {
  /// Last height to deliver (inclusive).
  pub stop_height: u32,
  /// Parallel `getblock` requests.
  pub fetch_threads: usize,
  /// Decoded blocks buffered ahead of the consumer.
  pub buffer_blocks: usize,
  /// Initial capacity of the UTXO value map (entries).
  pub utxo_capacity: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FastSourceStats {
  pub height: Option<u32>,
  pub utxos: u64,
  pub bytes_fetched: u64,
}

/// Genesis-to-`stop_height` pipeline: parallel raw-block fetch and decode,
/// in-order delivery, input values resolved from an in-memory UTXO value
/// map that sees every block. Dropping it stops its threads and frees the map.
pub struct FastSource {}

impl FastSource {
  pub fn start(_rpc: Rpc, _options: FastSyncOptions) -> anyhow::Result<Self> {
    unimplemented!()
  }

  /// The next block in height order, `None` after `stop_height`.
  pub fn next_block(&mut self) -> Option<anyhow::Result<ResolvedBlock>> {
    unimplemented!()
  }

  pub fn stats(&self) -> FastSourceStats {
    unimplemented!()
  }
}

/// `getblock <hash> 3`: prevout values come from Core's undo data, so any
/// height can be resolved without local UTXO state.
pub struct LiveSource {}

impl LiveSource {
  pub fn new(_rpc: Rpc) -> Self {
    unimplemented!()
  }

  pub fn block(&self, _hash: BlockHash, _height: u32) -> anyhow::Result<ResolvedBlock> {
    unimplemented!()
  }

  /// Heights `from..=to` in order, fetched `threads` at a time.
  pub fn blocks(
    &self,
    _from: u32,
    _to: u32,
    _threads: usize,
  ) -> Box<dyn Iterator<Item = anyhow::Result<ResolvedBlock>> + Send + '_> {
    unimplemented!()
  }
}
