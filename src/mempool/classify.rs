//! Classifies one pending transaction exactly as a block would: its `Move`
//! (`flow::tx_move`), its sales and inscriptions (`market::analyze`, on a
//! pseudo-block holding the transaction and its pending spenders, at the
//! height it would be mined at) and its address history entries
//! (`history::move_entries`). Pure.

use {
  super::flow,
  crate::{
    history::{self, HistoryEntry},
    market::{self, MarketContext},
    model::{BlockDelta, MarketDelta, Move, MoveInput, ResolvedBlock, ResolvedTx},
  },
  bitcoin::{
    BlockHash, CompactTarget, ScriptBuf, TxMerkleNode, Txid,
    block::{Header, Version},
    hashes::Hash,
  },
  std::collections::HashMap,
};

/// What a pending transaction did to tracked sats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
  /// `tx_index` is 1 (its place in the pseudo-block); the API reports none.
  pub mv: Move,
  pub market: MarketDelta,
  pub entries: Vec<HistoryEntry>,
  /// The script of every entry, by `HistoryEntry::key`.
  pub scripts: HashMap<u64, ScriptBuf>,
}

fn header(time: u32) -> Header {
  Header {
    version: Version::ONE,
    prev_blockhash: BlockHash::all_zeros(),
    merkle_root: TxMerkleNode::all_zeros(),
    time,
    bits: CompactTarget::from_consensus(0x207f_ffff),
    nonce: 0,
  }
}

/// The move of `tx` (spending the tracked `inputs`), with `tx_index` 1.
pub fn tx_move(tx: &ResolvedTx, inputs: Vec<MoveInput>) -> anyhow::Result<Move> {
  flow::tx_move(tx, inputs, 1)
}

/// Classifies `tx`, whose move is `mv`, as if mined at `height` at `time`.
/// `spenders` are pending transactions spending its outputs (a sale settled
/// by a second transaction is attributed through them). `time_of` gives the
/// header time of a height (for spent UTXOs' extraction verdict; pending
/// parents' outputs are at `height`, and should be given `time`).
pub fn classify(
  tx: &ResolvedTx,
  mv: Move,
  spenders: &[&ResolvedTx],
  height: u32,
  time: u32,
  ctx: &MarketContext,
  time_of: &dyn Fn(u32) -> Option<u32>,
) -> Classified {
  let coinbase = ResolvedTx {
    txid: Txid::all_zeros(),
    inputs: Vec::new(),
    outputs: Vec::new(),
  };
  let mut txs = Vec::with_capacity(2 + spenders.len());
  txs.push(coinbase);
  txs.push(tx.clone());
  txs.extend(spenders.iter().map(|tx| (*tx).clone()));
  let block = ResolvedBlock {
    height,
    hash: BlockHash::all_zeros(),
    header: header(time),
    txs,
  };
  let delta = BlockDelta {
    height,
    hash: BlockHash::all_zeros(),
    header: header(time),
    moves: vec![mv],
    spent: Vec::new(),
    created: Vec::new(),
    overwritten: Vec::new(),
    lost: Vec::new(),
    lost_sats_before: 0,
    lost_sats_after: 0,
    rows: Vec::new(),
  };
  let market = market::analyze(&block, &delta, ctx);
  let mv = delta.moves.into_iter().next().expect("one move");
  let entries = history::move_entries(&mv, height, time, &market, &*ctx.catalog, time_of);
  let mut scripts = HashMap::new();
  for script in mv
    .inputs
    .iter()
    .map(|input| &input.utxo.script_pubkey)
    .chain(mv.outputs.iter().map(|output| &output.script_pubkey))
  {
    scripts
      .entry(history::history_key(script))
      .or_insert_with(|| script.clone());
  }
  Classified {
    mv,
    market,
    entries,
    scripts,
  }
}
