//! Real transactions as test fixtures (`tests/fixtures/sales/*.json`), the
//! ignored builder that makes them, and the ignored measurement of the
//! detector over the fork's moves window. Both ignored tests run on a box
//! with bitcoind and the live fork:
//!
//! ```sh
//! set -a; source ~/.config/ord-sats/bitcoind.env; set +a  # ORD_BITCOIN_RPC_*
//! FORK_URL=http://127.0.0.1:8090 MAGISAT_SALES=~/work/magisat/sales.jsonl \
//!   cargo test --release --lib market::fixtures::build -- --ignored --nocapture
//!   # builds the missing fixtures; FIXTURES_REBUILD=1 refreshes every one's
//!   # transactions, keeping its tracked inputs
//! FORK_URL=http://127.0.0.1:8090 MEASURE_OUT=market.jsonl \
//!   cargo test --release --lib market::fixtures::measure -- --ignored --nocapture
//! ```
//!
//! A fixture is one transaction as the live source resolves it (`getblock
//! 3`: sighash flags, script hashes and envelopes as production decodes
//! them), plus, for two-transaction sales, the transaction of the same block
//! that spends its escrow output, and its tracked inputs, from:
//! * `fork`: the fork's recorded move (blocks in its moves window);
//! * `fork_sales`: the sold pieces of the sales the fork recorded for the
//!   transaction (`/sales/tx`), when the fork lists it;
//! * `magisat`: Magisat's sale records, the main sat at its offset in the
//!   sold UTXO (the sale's shape is real, the piece stands for the sats);
//! * `fork_output`: the tracked sats the buyer's output still holds today
//!   (the fork's `/output`), mapped back to the seller's input by FIFO.
//!
//! The tracker's outputs are recomputed by FIFO; the builder checks them
//! against the fork's move.
//!
//! A marketplace with no sales on chain yet (raresats.net, from block
//! 969,480) is tested on synthesized fixtures in its layout instead:
//! synthetic txids and scripts, its own fee address and marker.

use {
  super::{
    LISTING_SIGHASH, MarketContext, Marketplaces, PSBT_SIGHASH, UNKNOWN_MARKETPLACE, analyze,
    sale_unit,
  },
  crate::{
    model::{
      BlockDelta, Envelope, Move, MoveInput, MoveOutput, Piece, ResolvedBlock, ResolvedInput,
      ResolvedTx, SaleKind,
      SaleKind::{Listing, Offer},
      TrackedUtxo, TrackerConfig, UtxoKind, script_hash,
    },
    settings::Chain,
    tracker::tests::{catalog_for, out_to, script},
  },
  bitcoin::{
    Address, Amount, BlockHash, CompactTarget, Network, OutPoint, ScriptBuf, TxMerkleNode, TxOut,
    Txid,
    block::{Header, Version},
    hashes::Hash,
  },
  ordinals::Height,
  serde::{Deserialize, Serialize},
  serde_json::Value,
  std::{collections::BTreeMap, str::FromStr},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Fixture {
  name: String,
  txid: Txid,
  height: u32,
  tx_index: u32,
  pieces_from: String,
  tracked: Vec<FixtureTracked>,
  inputs: Vec<FixtureInput>,
  outputs: Vec<FixtureOutput>,
  /// The transaction of the same block spending the escrow output (output
  /// 0) of a two-transaction sale.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  spender: Option<FixtureTx>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FixtureTx {
  txid: Txid,
  inputs: Vec<FixtureInput>,
  outputs: Vec<FixtureOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FixtureInput {
  outpoint: OutPoint,
  value: u64,
  #[serde(default)]
  script_hash: u32,
  sighash: Option<u8>,
  /// (pointer, unrecognized_even_field) of each envelope.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  envelopes: Vec<(Option<u64>, bool)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FixtureOutput {
  value: u64,
  script_pubkey: ScriptBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FixtureTracked {
  vin: u32,
  outpoint: OutPoint,
  value: u64,
  script_pubkey: ScriptBuf,
  height: u32,
  pieces: Vec<Piece>,
}

fn dir() -> std::path::PathBuf {
  std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sales")
}

fn header() -> Header {
  Header {
    version: Version::ONE,
    prev_blockhash: BlockHash::all_zeros(),
    merkle_root: TxMerkleNode::all_zeros(),
    time: 0,
    bits: CompactTarget::from_consensus(0x207f_ffff),
    nonce: 0,
  }
}

fn delta(height: u32, hash: BlockHash, moves: Vec<Move>) -> BlockDelta {
  BlockDelta {
    height,
    hash,
    header: header(),
    moves,
    spent: Vec::new(),
    created: Vec::new(),
    overwritten: Vec::new(),
    lost: Vec::new(),
    lost_sats_before: 0,
    lost_sats_after: 0,
    rows: Vec::new(),
  }
}

fn resolved(txid: Txid, inputs: &[FixtureInput], outputs: &[FixtureOutput]) -> ResolvedTx {
  ResolvedTx {
    txid,
    inputs: inputs
      .iter()
      .map(|input| ResolvedInput {
        previous_output: input.outpoint,
        value: input.value,
        script_hash: input.script_hash,
        sighash: input.sighash,
        envelopes: input
          .envelopes
          .iter()
          .map(|&(pointer, unrecognized_even_field)| Envelope {
            pointer,
            unrecognized_even_field,
          })
          .collect(),
      })
      .collect(),
    outputs: outputs
      .iter()
      .map(|output| TxOut {
        value: Amount::from_sat(output.value),
        script_pubkey: output.script_pubkey.clone(),
      })
      .collect(),
  }
}

fn fixture_tx(tx: &ResolvedTx) -> FixtureTx {
  FixtureTx {
    txid: tx.txid,
    inputs: tx
      .inputs
      .iter()
      .map(|input| FixtureInput {
        outpoint: input.previous_output,
        value: input.value,
        script_hash: input.script_hash,
        sighash: input.sighash,
        envelopes: input
          .envelopes
          .iter()
          .map(|envelope| (envelope.pointer, envelope.unrecognized_even_field))
          .collect(),
      })
      .collect(),
    outputs: tx
      .outputs
      .iter()
      .map(|output| FixtureOutput {
        value: output.value.to_sat(),
        script_pubkey: output.script_pubkey.clone(),
      })
      .collect(),
  }
}

/// Offsets of each input's first sat, and of each output's.
fn offsets(tx: &ResolvedTx) -> (Vec<u64>, Vec<u64>, u64) {
  let mut inputs = Vec::with_capacity(tx.inputs.len());
  let mut total = 0;
  for input in &tx.inputs {
    inputs.push(total);
    total += input.value;
  }
  let mut outputs = Vec::with_capacity(tx.outputs.len());
  let mut end = 0;
  for output in &tx.outputs {
    outputs.push(end);
    end += output.value.to_sat();
  }
  (inputs, outputs, end)
}

/// Where `inputs`' pieces land by FIFO: tracked outputs, sorted by vout,
/// their pieces sorted by offset and merged.
fn fifo(tx: &ResolvedTx, inputs: &[MoveInput]) -> Vec<MoveOutput> {
  let (base, starts, _) = offsets(tx);
  let ends: Vec<u64> = starts
    .iter()
    .zip(&tx.outputs)
    .map(|(start, output)| start + output.value.to_sat())
    .collect();
  let mut pieces: BTreeMap<u32, Vec<Piece>> = BTreeMap::new();
  for input in inputs {
    for piece in &input.utxo.pieces {
      let first = base[input.vin as usize] + piece.offset;
      let mut offset = first;
      let stop = first + piece.len;
      while offset < stop {
        let vout = ends.partition_point(|&e| e <= offset);
        let Some(&output_end) = ends.get(vout) else {
          break;
        };
        let next = stop.min(output_end);
        pieces.entry(vout as u32).or_default().push(Piece {
          offset: offset - starts[vout],
          start: piece.start + (offset - first),
          len: next - offset,
        });
        offset = next;
      }
    }
  }
  pieces
    .into_iter()
    .map(|(vout, pieces)| {
      let output = &tx.outputs[vout as usize];
      MoveOutput {
        vout,
        value: output.value.to_sat(),
        script_pubkey: output.script_pubkey.clone(),
        kind: UtxoKind::Spendable,
        pieces: merged(pieces),
      }
    })
    .collect()
}

fn merged(mut pieces: Vec<Piece>) -> Vec<Piece> {
  pieces.sort_by_key(|piece| piece.offset);
  let mut out: Vec<Piece> = Vec::new();
  for piece in pieces {
    match out.last_mut() {
      Some(last) if last.offset_end() == piece.offset && last.start + last.len == piece.start => {
        last.len += piece.len
      }
      _ => out.push(piece),
    }
  }
  out
}

fn move_of(tx: &ResolvedTx, tx_index: u32, inputs: Vec<MoveInput>) -> Move {
  Move {
    txid: tx.txid,
    tx_index,
    coinbase: false,
    input_value: tx.inputs.iter().map(|input| input.value).sum(),
    output_value: tx.outputs.iter().map(|output| output.value.to_sat()).sum(),
    outputs: fifo(tx, &inputs),
    inputs,
    fee: Vec::new(),
    mined: Vec::new(),
    lost: Vec::new(),
  }
}

impl Fixture {
  fn load(name: &str) -> Self {
    let path = dir().join(format!("{name}.json"));
    let json = std::fs::read_to_string(&path)
      .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
    serde_json::from_str(&json).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
  }

  /// The transaction (and its spender) at its height, after a placeholder
  /// coinbase, and the tracker's delta for them.
  fn block_and_delta(&self) -> (ResolvedBlock, BlockDelta) {
    let tx = resolved(self.txid, &self.inputs, &self.outputs);
    let inputs: Vec<MoveInput> = self
      .tracked
      .iter()
      .map(|tracked| MoveInput {
        vin: tracked.vin,
        outpoint: tracked.outpoint,
        utxo: TrackedUtxo {
          value: tracked.value,
          script_pubkey: tracked.script_pubkey.clone(),
          height: tracked.height,
          kind: UtxoKind::Spendable,
          pieces: tracked.pieces.clone(),
        },
      })
      .collect();
    let mut moves = vec![move_of(&tx, 1, inputs)];
    let coinbase = ResolvedTx {
      txid: Txid::all_zeros(),
      inputs: Vec::new(),
      outputs: Vec::new(),
    };
    let mut txs = vec![coinbase, tx];
    if let Some(spender) = &self.spender {
      let spender = resolved(spender.txid, &spender.inputs, &spender.outputs);
      let inputs: Vec<MoveInput> = spender
        .inputs
        .iter()
        .enumerate()
        .filter_map(|(vin, input)| {
          let output = moves[0]
            .outputs
            .iter()
            .find(|output| input.previous_output == OutPoint::new(self.txid, output.vout))?;
          Some(MoveInput {
            vin: vin as u32,
            outpoint: input.previous_output,
            utxo: TrackedUtxo {
              value: output.value,
              script_pubkey: output.script_pubkey.clone(),
              height: self.height,
              kind: UtxoKind::Spendable,
              pieces: output.pieces.clone(),
            },
          })
        })
        .collect();
      moves.push(move_of(&spender, 2, inputs));
      txs.push(spender);
    }
    let block = ResolvedBlock {
      height: self.height,
      hash: BlockHash::all_zeros(),
      header: header(),
      txs,
    };
    (block, delta(self.height, BlockHash::all_zeros(), moves))
  }
}

fn context() -> MarketContext {
  MarketContext::new(
    catalog_for(&TrackerConfig::default()),
    Marketplaces::embedded(),
    Chain::Mainnet,
  )
}

/// The fixtures, by name, and their transactions.
const FIXTURES: &[(&str, &str)] = &[
  // Magisat listing purchases, from its sales records.
  (
    "magisat_single_2023",
    "3854c0ba0ea36ac5993be75f366395a21bc77c5621887f0a9e59a77d07e5c500",
  ),
  (
    "magisat_no_fee_output_2023",
    "90c55758af26e17c646a146adce237f92b70fb9228eef485ba213c61b0c4fd7d",
  ),
  (
    "magisat_f2_period_2024",
    "d2d35be449ec5b76f451f8c4ac4f8902473e5013fdc36aff0901ed1b3a3485bd",
  ),
  (
    "magisat_bulk_variant_2024",
    "844265bdfba89e3fc21058807c778ec0ab5d227b35b94927a82043f26212b746",
  ),
  (
    "magisat_first_maker_fee",
    "c4d60e298dcc8b3c59606c82a46a2dbd9f3a93a57f9032b2e28812e1e4147bbf",
  ),
  (
    "magisat_maker_fee_f2",
    "72cb8faa3f6806fad40fff7e2bfadfc701d1d320796d5b67932d7adb67c1b954",
  ),
  (
    "magisat_f1_2025",
    "d52437e0272fe3fcb91b8bae542c93836aa3e9b7cfed170363996690a6c5508c",
  ),
  (
    "magisat_0_1_btc",
    "a67fb7f7a12730a47d43902e9388856c928c4e791167cb193931db61963d95b5",
  ),
  (
    "magisat_bulk_2",
    "09d718c6d49d20d2014359374248de91586ddf8454663df425a1ea803b7eb4f2",
  ),
  (
    "magisat_buys_with_f1",
    "53f61c21954cb7ca60a1f58af512b38725bd3b72fba2bf11244d5a1160d867c0",
  ),
  // Magisat's accepted offers.
  (
    "magisat_offer_2025",
    "19df449751a6bc87d75195f282a3886d14f256541e052d4239d7e3ca590097dc",
  ),
  (
    "magisat_offer_2024_minimums",
    "750c5e1e1f180823f4a267c5e7b27685d0e3e2d7ac8bc7c467ac3d0a44dc42e3",
  ),
  (
    "magisat_offer_2024_03_no_taker_fee",
    "39a640d232ebcab2326d6f4b3f295c3ead0808545a0e74c1117bc82633f08b6f",
  ),
  (
    "magisat_offer_zero_fee",
    "b1a71ed6e428c95c8d35f3a899b4665758477738dd7c90a86df6417cb0360b74",
  ),
  (
    "magisat_offer_not_in_api",
    "82d5b938c1d5019a73f33d39f9f0bcede7e8be24de1f7cf2ee3eae4bed935c20",
  ),
  (
    "magisat_offer_paid_to_the_item_address",
    "b04178fb6ad59b6eb9aef64a26604c86544efa686d7e4ecaeeb5b85f1e6bd4f0",
  ),
  (
    "magisat_buys_with_own_wallet_no_fee",
    "9a64e88da4d426392b54449143ee58cd8784d743f7524d6e5f01bf2745425b6d",
  ),
  // Magic Eden (closed 2026-03), sats still where the buyer received them.
  (
    "me_listing_2023",
    "73261d7da965e33a12724d6414b430c25c86de3295454a22310ae44201c31c96",
  ),
  (
    "me_offer_2023",
    "3b404b83eef9796704539fb934b0f253e05d40bfb2bfe80ef2b482dae14de76f",
  ),
  (
    "me_bid_in_listing_layout",
    "95292641ce3a2b2254fc00c4f348491720e7c5fd39512fc5d0056fdcaa531876",
  ),
  (
    "me_transfer_tool",
    "0517a568cb79a3010f307ed98295a6f27260e2fe46698b66fddb6af27f0c64f2",
  ),
  (
    "me_launchpad_mint",
    "a37a2f59d14cd5a3025376ede36cb47d3a1df2a3ecb1ea106aa2e55e58184544",
  ),
  // Other venues, from the fork's moves window.
  (
    "psbt_0x81_sale",
    "0c47e0acc1b4f2de35cab4a8deaf278fe47f0d9a707135dd8e2488a228a8ec86",
  ),
  (
    "psbt_0x81_separate_payment",
    "96e080d3531f4c59893c83a7c242260c0fae0b45a806d6fd7959c32acb4a0e45",
  ),
  (
    "psbt_0x81_service_fee",
    "acd7eeb412db74682b8526e11480602ef650d62e08e6a8bce82b63d020185928",
  ),
  (
    "satflow_bid_acceptance",
    "1844d9d5ad89822dc72b90bf675bd00da45d57c5d4d713c1c880508ad682aa2f",
  ),
  (
    "ord_net_passthrough",
    "43de4b0c81215cecd4e180f8d09617588b96414860549c899cf8a7ed738dc701",
  ),
  (
    "ord_net_offer",
    "c170b60002a3da0df8dd8d897dda9bed05488b4704736f0abdf60cececbcf777",
  ),
  (
    "ordinals_wallet_passthrough",
    "bcb36935d355b88239aad9c66288450560a557fb63e9fbc6049c9d9c0bc7e974",
  ),
  (
    "unisat_listing",
    "b569cb2e5a816a3d666aae88a4616de7b237871f7f053c584da705b45a667802",
  ),
  (
    "listing_fee_33htobl",
    "4496989b62716ff0df954a29b43fe9ac96a153658801392f5ff25fe2d705aa21",
  ),
  (
    "unknown_33hto_split_sale",
    "32dcc1958f11e119898ff74ae29c02559fb3984d0777de433ebf66aa79b0a5c2",
  ),
  (
    "listing_tx_dev_fill",
    "f1c500f81c331d31415902755278e2a599311d8121ac9582bc63ad981efa48fb",
  ),
  (
    "otc_swap_0x83",
    "ce4dc92baaba478327bbd6b7e182576366f79334c72b40e46f5c54551df7c0cb",
  ),
  // Not sales.
  (
    "exchange_batch_0x81",
    "cf061ead6b1288ecaab6512ea1b074504844fbe63506d9d410a3a8ad1463e935",
  ),
  (
    "consolidation_0x81",
    "06d243eb5e28a746c93d81fe1d365fb80d9d98b46e4d483702a5cf9664e93104",
  ),
  (
    "custodial_withdrawal_0x81",
    "671ec9c1a370151fe299e64f73f7392355029e9765fe332901815324373f29a5",
  ),
  (
    "sweep_0x82",
    "f810a3252a4bccf33f3a4552a6d72970995cd3918b9682d02639dee63023358a",
  ),
  (
    "magisat_safe_transfer",
    "c421d418dc00729c7f0dad77dcbf36b222771014b7f72b55c5d862e633f23471",
  ),
  (
    "magisat_isolation",
    "6de0f2f080d5a7b97c4262c00e78b09aa7a2e408899195eb2ca438aad920b2d4",
  ),
  (
    "magisat_send",
    "8c4d350491d327282f2b212d01f2202c89ba03d86021d5268bb5900fc219f39b",
  ),
  (
    "liquidium_loan",
    "56711f668e018fa8ad81293400fe9b38db86f101239895eecdebe5a41ca0b1a5",
  ),
  (
    "magisat_tool_change_back",
    "21151f083d134d844f7e95182ae1d082883baf3cc5e25e895b1d859369051d5f",
  ),
  (
    "magisat_tool_same_address",
    "92e00e8d7b9bd39d0f3d71735b437bfe21ee6e590072d780c131e2d137306d05",
  ),
  (
    "magisat_tool_address",
    "4220297ed1ddef1e6a398d3956f1380fa3600d76e754b09b3e27ebe954a2b210",
  ),
];

/// Fixtures whose escrow output is spent by a settlement in the same block
/// (`Fixture::spender`).
const SETTLED: &[&str] = &["psbt_0x81_sale", "psbt_0x81_separate_payment"];

/// A sale as the fixture tests compare it: vin, kind, marketplace, payment
/// vout, buyer vout, price, proceeds, fees.
type Seen = (u32, SaleKind, String, u32, u32, u64, u64, u64);

fn sales_of(name: &str) -> Vec<Seen> {
  sales(&Fixture::load(name))
}

/// The sales the embedded table finds in the fixture, checked against it.
fn sales(fixture: &Fixture) -> Vec<Seen> {
  let name = &fixture.name;
  let (block, delta) = fixture.block_and_delta();
  let market = analyze(&block, &delta, &context());
  market
    .sales
    .iter()
    .map(|sale| {
      assert_eq!(
        sale.txid, fixture.txid,
        "{name}: only the fixture's own sales"
      );
      let tracked = fixture
        .tracked
        .iter()
        .find(|tracked| tracked.vin == sale.vin)
        .unwrap();
      assert_eq!(sale.seller_outpoint, tracked.outpoint);
      // The sold pieces: the input's, or the part of them that left the
      // seller.
      for piece in &sale.seller.pieces {
        assert!(
          tracked.pieces.iter().any(|t| t.offset <= piece.offset
            && piece.offset_end() <= t.offset_end()
            && piece.start - t.start == piece.offset - t.offset),
          "{name}: {piece:?}"
        );
      }
      assert_eq!(
        sale.buyer_script.as_ref(),
        sale
          .buyer_vout
          .map(|vout| &fixture.outputs[vout as usize].script_pubkey)
      );
      (
        sale.vin,
        sale.kind,
        sale.marketplace.clone(),
        sale.payment_vout.unwrap(),
        sale.buyer_vout.unwrap(),
        sale.price,
        sale.seller_proceeds,
        sale.fees,
      )
    })
    .collect()
}

fn sold(
  vin: u32,
  kind: SaleKind,
  marketplace: &str,
  (payment, buyer): (u32, u32),
  (price, proceeds, fees): (u64, u64, u64),
) -> Seen {
  (
    vin,
    kind,
    marketplace.into(),
    payment,
    buyer,
    price,
    proceeds,
    fees,
  )
}

/// The fixture's transaction pays a fee address of `id` valid at its
/// height.
fn pays(name: &str, id: &str) -> bool {
  paid(&Fixture::load(name), id)
}

fn paid(fixture: &Fixture, id: &str) -> bool {
  let marketplaces = Marketplaces::embedded();
  fixture.outputs.iter().any(|output| {
    marketplaces
      .fee_output(&output.script_pubkey, fixture.height)
      .is_some_and(|marketplace| marketplace.id == id)
  })
}

/// The fixture's transaction carries a marker of `id` valid at its height.
fn marked(fixture: &Fixture, id: &str) -> bool {
  let marketplaces = Marketplaces::embedded();
  fixture.outputs.iter().any(|output| {
    marketplaces
      .marker(&output.script_pubkey, fixture.height)
      .is_some_and(|marketplace| marketplace.id == id)
  })
}

#[test]
fn magisat_listings() {
  // 2023-10, F1: the price is the payment (no maker fee before 861,287);
  // the 875-sat fee output is the buyer's taker fee, not part of the price.
  assert_eq!(
    sales_of("magisat_single_2023"),
    [sold(2, Listing, "magisat", (2, 1), (35_000, 35_000, 0))]
  );
  // 2023-09: 2.5 % of 15,000 is below dust, so no fee output.
  assert!(!pays("magisat_no_fee_output_2023", "magisat"));
  assert_eq!(
    sales_of("magisat_no_fee_output_2023"),
    [sold(
      2,
      Listing,
      UNKNOWN_MARKETPLACE,
      (2, 1),
      (15_000, 15_000, 0)
    )]
  );
  // 2024-03, the F2-only period.
  assert_eq!(
    sales_of("magisat_f2_period_2024"),
    [sold(2, Listing, "magisat", (2, 1), (30_000, 30_000, 0))]
  );
  // The 2024 variant layout: one padding input, sellers at vin 1..=5, every
  // sold sat lands in output 0.
  assert_eq!(
    sales_of("magisat_bulk_variant_2024"),
    (1..=5)
      .map(|vin| sold(vin, Listing, "magisat", (vin, 0), (122_000, 122_000, 0)))
      .collect::<Vec<Seen>>()
  );
  // Block 861,287 has the first maker fees: ⌊89,550 / 199⌋ = 450.
  assert_eq!(
    sales_of("magisat_first_maker_fee"),
    [
      sold(3, Listing, "magisat", (3, 1), (90_000, 89_550, 450)),
      sold(4, Listing, "magisat", (4, 2), (90_000, 89_550, 450)),
    ]
  );
  // 2024-10, F2 12,950 = 2,450 maker + 10,500 taker.
  assert_eq!(
    sales_of("magisat_maker_fee_f2"),
    [sold(
      2,
      Listing,
      "magisat",
      (2, 1),
      (490_000, 487_550, 2_450)
    )]
  );
  // 2025, F1 2,957 = 157 maker + 2,800 taker.
  assert_eq!(
    sales_of("magisat_f1_2025"),
    [sold(2, Listing, "magisat", (2, 1), (31_500, 31_343, 157))]
  );
  // A 0.1 BTC rare sat: F2 150,007 = 50,002 maker + 100,005 taker.
  assert_eq!(
    sales_of("magisat_0_1_btc"),
    [sold(
      2,
      Listing,
      "magisat",
      (2, 1),
      (10_000_546, 9_950_544, 50_002)
    )]
  );
  // Two alpha uncommons in one buy, as the fork recorded the move: one fee
  // output of 3,254 = 2 × 227 maker + 2,800 taker.
  assert_eq!(
    sales_of("magisat_bulk_2"),
    [
      sold(3, Listing, "magisat", (3, 1), (45_546, 45_319, 227)),
      sold(4, Listing, "magisat", (4, 2), (45_546, 45_319, 227)),
    ]
  );
}

#[test]
fn magisat_buying_with_its_own_wallet() {
  // F1 buys 50 block 9 sats from one seller. Its 103 outputs (the bought
  // sats, padding, change) receive tracked sats or return its funds, so they
  // are not fees; the fee goes to F2 at vout 101 (2k + 1). Prices as
  // Magisat's records: 30 × 1,146, 13 × 930 and 7 × 1,046.
  let sales = sales_of("magisat_buys_with_f1");
  assert_eq!(sales.len(), 50);
  let mut prices = BTreeMap::new();
  for (vin, kind, marketplace, payment, buyer, price, proceeds, fees) in sales {
    assert_eq!(
      (kind, marketplace.as_str(), payment, buyer),
      (Listing, "magisat", vin, vin - 50)
    );
    assert_eq!((price, fees), (proceeds + proceeds / 199, proceeds / 199));
    *prices.entry(price).or_insert(0) += 1;
  }
  assert_eq!(prices, BTreeMap::from([(930, 13), (1_046, 7), (1_146, 30)]));
  // With no fee output at all (none is due): still Magisat's sales, at its
  // records' prices.
  assert_eq!(
    sales_of("magisat_buys_with_own_wallet_no_fee")
      .iter()
      .map(|sale| (sale.0, sale.2.as_str(), sale.5, sale.7))
      .collect::<Vec<_>>(),
    [
      (8, "magisat", 30_000, 150),
      (9, "magisat", 30_216, 151),
      (10, "magisat", 30_216, 151),
      (11, "magisat", 30_216, 151),
      (12, "magisat", 30_216, 151),
      (13, "magisat", 169_546, 847),
      (14, "magisat", 178_000, 890),
    ]
  );
}

#[test]
fn magisat_offers() {
  // 2025, 1 % + 1 %: 3,960,000 to the seller, 80,000 fee.
  assert_eq!(
    sales_of("magisat_offer_2025"),
    [sold(
      0,
      Offer,
      "magisat",
      (1, 0),
      (4_000_000, 3_960_000, 40_000)
    )]
  );
  // 2024, 15,000 minimums on each side.
  assert_eq!(
    sales_of("magisat_offer_2024_minimums"),
    [sold(
      0,
      Offer,
      "magisat",
      (1, 0),
      (1_000_000, 985_000, 15_000)
    )]
  );
  // In none of Magisat's feeds (a collection offer?), from the fork's move.
  assert_eq!(
    sales_of("magisat_offer_not_in_api"),
    [sold(
      0,
      Offer,
      "magisat",
      (1, 0),
      (5_200_000, 5_148_000, 52_000)
    )]
  );
  // The seller paid at the item's own (input) address: still an offer, at
  // Magisat's price (Magisat's tools pay their change back instead).
  let fixture = Fixture::load("magisat_offer_paid_to_the_item_address");
  assert_eq!(
    fixture.outputs[1].script_pubkey,
    fixture.tracked[0].script_pubkey
  );
  assert_eq!(
    sales_of("magisat_offer_paid_to_the_item_address"),
    [sold(0, Offer, "magisat", (1, 0), (100_000, 85_000, 15_000))]
  );
  // Known deviation: in 2024-03 offers paid a 2.5 % maker fee and no taker
  // fee, so Magisat's price is 2,500,000; half the fee output is added.
  assert_eq!(
    sales_of("magisat_offer_2024_03_no_taker_fee"),
    [sold(
      0,
      Offer,
      "magisat",
      (1, 0),
      (2_531_250, 2_500_000, 31_250)
    )]
  );
  // Known miss: zero-fee offers (since 2025-11) have no fee output and look
  // like a send with change to the sender.
  assert!(!pays("magisat_offer_zero_fee", "magisat"));
  assert_eq!(sales_of("magisat_offer_zero_fee"), []);
}

#[test]
fn magic_eden_sales() {
  // 2023: the msigner layout, the item padded to 10,000 sats at vout 1; the
  // seller's payment at vout 2 and the 2.5 % fee at vout 3.
  let [(vin, kind, marketplace, payment, buyer, price, proceeds, fees)] =
    &sales_of("me_listing_2023")[..]
  else {
    panic!()
  };
  assert_eq!(
    (*vin, *kind, marketplace.as_str(), *payment, *buyer),
    (2, Listing, "magic_eden", 2, 1)
  );
  assert_eq!(*price, proceeds + fees);
  assert!(*fees > 0);
  // An accepted offer: 1,568,546 to the seller (98 % of 1,600,000 plus the
  // UTXO's 546), a 40,000 fee.
  assert_eq!(
    sales_of("me_offer_2023"),
    [sold(
      0,
      Offer,
      "magic_eden",
      (1, 0),
      (1_608_546, 1_568_546, 40_000)
    )]
  );
  // A bid accepted inside the listing layout: the seller at vin 2 signing
  // the default sighash, paid at vout 2.
  let sales = sales_of("me_bid_in_listing_layout");
  assert_eq!(
    sales
      .iter()
      .map(|sale| (sale.0, sale.1, sale.2.as_str(), sale.3))
      .collect::<Vec<_>>(),
    [(2, Offer, "magic_eden", 2)]
  );
  // The transfer tool (every input the seller's) and a launchpad mint (the
  // creator paid ~547): not sales.
  assert!(pays("me_transfer_tool", "magic_eden"));
  assert_eq!(sales_of("me_transfer_tool"), []);
  assert!(pays("me_launchpad_mint", "magic_eden"));
  assert_eq!(sales_of("me_launchpad_mint"), []);
}

#[test]
fn passthrough_and_bid_sales() {
  // ord.net: the item spent from the seller's 2-of-2 passthrough at vin 2,
  // paid at vout 2: a listing; its offers pay at vout 1.
  let check = |name: &str, kind: SaleKind, marketplace: &str, (vin, payment): (u32, u32)| {
    let sales = sales_of(name);
    assert_eq!(
      sales
        .iter()
        .map(|sale| (sale.0, sale.1, sale.2.as_str(), sale.3))
        .collect::<Vec<_>>(),
      [(vin, kind, marketplace, payment)],
      "{name}"
    );
    let (.., price, proceeds, fees) = &sales[0];
    assert!(*fees > 0 && *price == proceeds + fees, "{name}");
  };
  check("ord_net_passthrough", Listing, "ord_net", (2, 2));
  check("ord_net_offer", Offer, "ord_net", (0, 1));
  // Ordinals Wallet: the escrow at vin 1, paid at vout 1.
  check(
    "ordinals_wallet_passthrough",
    Listing,
    "ordinals_wallet",
    (1, 1),
  );
  // Satflow bid acceptance: 488,046 to the seller, 7,500 to its fee address.
  assert_eq!(
    sales_of("satflow_bid_acceptance"),
    [sold(0, Offer, "satflow", (1, 0), (495_546, 488_046, 7_500))]
  );
  // The unnamed rare-sat venue splitting one sat out of the seller's UTXO:
  // 35,000 to the seller's payment address, 13,500 in fees.
  let sales = sales_of("unknown_33hto_split_sale");
  assert_eq!(
    sales
      .iter()
      .map(|sale| (sale.0, sale.1, sale.2.as_str(), sale.3, sale.6, sale.7))
      .collect::<Vec<_>>(),
    [(0, Offer, UNKNOWN_MARKETPLACE, 2, 35_000, 13_500)]
  );
}

#[test]
fn satflow_secure_purchases() {
  // Transaction A pays the seller and puts the item in a 2-of-3 escrow; B,
  // in the same block, pays the buyer, the fees and the marker. A is the
  // sale, attributed by B; B is no sale.
  for (name, payment) in [
    ("psbt_0x81_sale", 1_970_999),
    ("psbt_0x81_separate_payment", 1_973_999),
  ] {
    let fixture = Fixture::load(name);
    assert_eq!(
      fixture
        .inputs
        .iter()
        .map(|input| input.sighash.is_some_and(|flag| flag & 0x80 == 0))
        .collect::<Vec<_>>(),
      [false, true],
      "{name}: a seller signing ALL|ANYONECANPAY, a buyer signing ALL"
    );
    assert_eq!(fixture.inputs[0].sighash, Some(PSBT_SIGHASH));
    assert!(fixture.spender.is_some(), "{name}");
    let sales = sales_of(name);
    let [(vin, kind, marketplace, vout, buyer, sale_price, proceeds, fees)] = &sales[..] else {
      panic!("{name}: {sales:?}")
    };
    assert_eq!(
      (*vin, *kind, marketplace.as_str(), *vout, *buyer, *proceeds),
      (0, Listing, "satflow", 1, 0, payment),
      "{name}"
    );
    assert!(*fees > 0 && *sale_price == proceeds + fees, "{name}");
  }
  // The legacy form pays a 1,000-sat fee to ordinals.market's address in A.
  assert_eq!(
    sales_of("psbt_0x81_service_fee"),
    [sold(0, Listing, "satflow", (1, 0), (8_217, 7_217, 1_000))]
  );
  // A without its settlement in the block: a sale of an unknown venue.
  let mut fixture = Fixture::load("psbt_0x81_sale");
  fixture.spender = None;
  let (block, delta) = fixture.block_and_delta();
  let market = analyze(&block, &delta, &context());
  assert_eq!(
    market
      .sales
      .iter()
      .map(|sale| (sale.marketplace.as_str(), sale.price))
      .collect::<Vec<_>>(),
    [(UNKNOWN_MARKETPLACE, 1_970_999)]
  );
  // Its unit: a 999-sat UTXO holding a 450x sat.
  let ctx = context();
  let (row, count, unit) = sale_unit(&market.sales[0], &ctx.catalog).unwrap();
  assert_eq!(ctx.catalog.rows()[usize::from(row)].id, "450x");
  assert_eq!((count, unit), (1, 1_970_000));
}

#[test]
fn other_venues() {
  // UniSat: two 300-sat dummies from one script, returned merged to it.
  assert_eq!(
    sales_of("unisat_listing")
      .iter()
      .map(|sale| (sale.0, sale.1, sale.2.as_str(), sale.3, sale.4))
      .collect::<Vec<_>>(),
    [(2, Listing, "unisat", 2, 1)]
  );
  // The unnamed venue's listing: its 1,000-sat fee counted, its name not.
  assert_eq!(
    sales_of("listing_fee_33htobl"),
    [sold(
      1,
      Listing,
      UNKNOWN_MARKETPLACE,
      (1, 0),
      (4_000, 3_000, 1_000)
    )]
  );
  // A buying tool filling a listing without the platform's fee.
  assert_eq!(
    sales_of("listing_tx_dev_fill"),
    [sold(2, Listing, "tx_dev", (2, 1), (77_093, 67_093, 10_000))]
  );
  // No fee output at all.
  assert_eq!(
    sales_of("otc_swap_0x83"),
    [sold(
      1,
      Listing,
      UNKNOWN_MARKETPLACE,
      (1, 0),
      (5_980_000, 5_980_000, 0)
    )]
  );
}

#[test]
fn lookalikes_are_not_sales() {
  let flags = |name: &str| -> Vec<Option<u8>> {
    Fixture::load(name)
      .inputs
      .iter()
      .map(|input| input.sighash)
      .collect()
  };
  // ALL|ANYONECANPAY on every input: an exchange batch and a consolidation.
  for name in ["exchange_batch_0x81", "consolidation_0x81"] {
    assert!(flags(name).iter().all(|&flag| flag == Some(PSBT_SIGHASH)));
    assert_eq!(sales_of(name), [], "{name}");
  }
  // A custodial withdrawal: the wallet signs ALL|ANYONECANPAY, a fee payer's
  // input ALL; the 0x81 input does not pass whole to output 0.
  assert_eq!(
    flags("custodial_withdrawal_0x81"),
    [Some(PSBT_SIGHASH), Some(0x01)]
  );
  assert_eq!(sales_of("custodial_withdrawal_0x81"), []);
  // A NONE|ANYONECANPAY sweep.
  let fixture = Fixture::load("sweep_0x82");
  assert_eq!(
    fixture.inputs[fixture.tracked[0].vin as usize].sighash,
    Some(0x82)
  );
  assert_eq!(sales_of("sweep_0x82"), []);
  // Magisat's tools pay its fee addresses without selling anything.
  for name in ["magisat_safe_transfer", "magisat_isolation", "magisat_send"] {
    assert!(pays(name, "magisat"), "{name}");
    assert_eq!(sales_of(name), [], "{name}");
  }
  // A loan: collateral to the lender's vault, the principal to the borrower.
  assert!(pays("liquidium_loan", "liquidium"));
  assert_eq!(sales_of("liquidium_loan"), []);
  // Magisat's send and transfer tools, in the offer's shape with a Magisat
  // fee: output 1 is change back to the funder, the item's own address, or
  // Magisat's tool address.
  for name in [
    "magisat_tool_change_back",
    "magisat_tool_same_address",
    "magisat_tool_address",
  ] {
    assert!(pays(name, "magisat"), "{name}");
    assert_eq!(sales_of(name), [], "{name}");
  }
  let fixture = Fixture::load("magisat_tool_address");
  assert!(
    Marketplaces::embedded()
      .tool_output(&fixture.outputs[1].script_pubkey, fixture.height)
      .is_some()
  );
}

/// raresats.net, from block 969,480: its fee address and its OP_RETURN
/// "raresats.net" marker.
const RARESATS_FEE: &str = "bc1p4hefchfnn06cn2xmk6fq6xcw5ew260q2dptc4c35fcazgedmzlcqkxf057";
const RARESATS_MARKER: &str = "6a0c72617265736174732e6e6574";
const RARESATS_FROM: u32 = 969_480;

/// The buyer of synthesized purchases (`script(BUYER)`).
const BUYER: u8 = 9;

fn raresats_fee(value: u64) -> TxOut {
  TxOut {
    value: Amount::from_sat(value),
    script_pubkey: Address::from_str(RARESATS_FEE)
      .unwrap()
      .require_network(Network::Bitcoin)
      .unwrap()
      .script_pubkey(),
  }
}

fn raresats_marker() -> TxOut {
  TxOut {
    value: Amount::ZERO,
    script_pubkey: ScriptBuf::from_hex(RARESATS_MARKER).unwrap(),
  }
}

/// The uncommon first sat of block `height`.
fn uncommon(height: u32) -> u64 {
  Height(height).starting_sat().n()
}

/// Owners from `TAPROOT` up hold P2TR scripts in synthesized transactions,
/// the others `script(owner)` (P2WPKH).
const TAPROOT: u8 = 0x80;

/// The script of a synthesized transaction's `owner`.
fn owner_script(owner: u8) -> ScriptBuf {
  if owner < TAPROOT {
    return script(owner);
  }
  let mut bytes = vec![0x51, 0x20];
  bytes.extend([owner; 32]);
  ScriptBuf::from_bytes(bytes)
}

/// An output of `value` to a synthesized transaction's `owner`.
fn owner_out(value: u64, owner: u8) -> TxOut {
  TxOut {
    value: Amount::from_sat(value),
    script_pubkey: owner_script(owner),
  }
}

/// An input of a synthesized transaction: its owner (`owner_script(owner)`),
/// value, sighash flag, and the tracked sat at its offset 0, if any.
type SynthInput = (u8, u64, Option<u8>, Option<u64>);

/// A synthesized transaction at `height`, as a fixture: its txid and its
/// inputs' outpoints are hashes of its name, its scripts
/// `owner_script(owner)`.
fn synthesized(name: &str, height: u32, inputs: &[SynthInput], outputs: Vec<TxOut>) -> Fixture {
  let txid = |what: &str| Txid::hash(format!("{name} {what}").as_bytes());
  let mut fixture = Fixture {
    name: name.into(),
    txid: txid("tx"),
    height,
    tx_index: 1,
    pieces_from: "synthesized".into(),
    tracked: Vec::new(),
    inputs: Vec::new(),
    outputs: outputs
      .into_iter()
      .map(|output| FixtureOutput {
        value: output.value.to_sat(),
        script_pubkey: output.script_pubkey,
      })
      .collect(),
    spender: None,
  };
  for (vin, &(owner, value, sighash, sat)) in inputs.iter().enumerate() {
    let outpoint = OutPoint::new(txid(&format!("input {vin}")), 0);
    let script_pubkey = owner_script(owner);
    fixture.inputs.push(FixtureInput {
      outpoint,
      value,
      script_hash: script_hash(&script_pubkey),
      sighash,
      envelopes: Vec::new(),
    });
    if let Some(start) = sat {
      fixture.tracked.push(FixtureTracked {
        vin: vin as u32,
        outpoint,
        value,
        script_pubkey,
        height: height - 1,
        pieces: vec![Piece {
          offset: 0,
          start,
          len: 1,
        }],
      });
    }
  }
  fixture
}

/// A synthesized raresats.net purchase of the listings paying `payments`,
/// in Magisat's standard layout (docs/SALES-SHAPES.md §3): the buyer's k + 1
/// 600-sat padding inputs, the sellers' 546-sat UTXOs (owners 20.., an
/// uncommon sat each) signed SINGLE|ANYONECANPAY at vin k+1..=2k, the
/// buyer's funding; outputs: the merged padding, the items at 1..=k, the
/// payments (to owners 40..) at the sellers' indices, `fee` at 2k + 1, k + 1
/// new 600-sat padding outputs, the change, and the marker last.
fn raresats_purchase(
  name: &str,
  height: u32,
  payments: &[u64],
  fee: Option<u64>,
  marker: bool,
) -> Fixture {
  let k = payments.len() as u64;
  let padding = 600 * (k + 1);
  let mut inputs: Vec<SynthInput> = (0..=k).map(|_| (BUYER, 600, Some(0x01), None)).collect();
  for (i, _) in (0u8..).zip(payments) {
    let sat = uncommon(969_000 + u32::from(i));
    inputs.push((20 + i, 546, Some(LISTING_SIGHASH), Some(sat)));
  }
  // The funding leaves 10,000 sats of change and a 1,000-sat miner fee.
  let spent: u64 = payments.iter().sum::<u64>() + fee.unwrap_or(0);
  inputs.push((BUYER, spent + padding + 11_000, Some(0x01), None));
  let mut outputs = vec![out_to(padding, BUYER)];
  outputs.extend(payments.iter().map(|_| out_to(546, BUYER)));
  outputs.extend(
    (0u8..)
      .zip(payments)
      .map(|(i, &payment)| out_to(payment, 40 + i)),
  );
  outputs.extend(fee.map(raresats_fee));
  outputs.extend((0..=k).map(|_| out_to(600, BUYER)));
  outputs.push(out_to(10_000, BUYER));
  outputs.extend(marker.then(raresats_marker));
  synthesized(name, height, &inputs, outputs)
}

#[test]
fn raresats_purchases() {
  // One listing of 45,000 sats: the 2.5 % fee (1,125) at vout 3, the marker
  // last. The price is the seller's payment; the fee is the buyer's, on top.
  let one = raresats_purchase("one", RARESATS_FROM, &[45_000], Some(1_125), true);
  assert_eq!(one.outputs[3].script_pubkey, raresats_fee(0).script_pubkey);
  assert!(paid(&one, "raresats") && marked(&one, "raresats"));
  assert_eq!(
    sales(&one),
    [sold(2, Listing, "raresats", (2, 1), (45_000, 45_000, 0))]
  );
  // Three listings: one fee for all, 2.5 % of 60,000, at vout 7 (2k + 1).
  let three = raresats_purchase(
    "three",
    RARESATS_FROM + 1,
    &[30_000, 20_000, 10_000],
    Some(1_500),
    true,
  );
  assert_eq!(
    three.outputs[7].script_pubkey,
    raresats_fee(0).script_pubkey
  );
  assert_eq!(
    sales(&three),
    [
      sold(4, Listing, "raresats", (4, 1), (30_000, 30_000, 0)),
      sold(5, Listing, "raresats", (5, 2), (20_000, 20_000, 0)),
      sold(6, Listing, "raresats", (6, 3), (10_000, 10_000, 0)),
    ]
  );
  // The 546-sat minimum fee (2.5 % of 10,000 is 250), the fee without the
  // marker, and the marker alone (a purchase without its fee output).
  for (name, payments, fee, marker) in [
    ("minimum fee", &[10_000][..], Some(546), true),
    ("fee only", &[10_000], Some(546), false),
    ("marker only", &[10_000], None, true),
    ("marker only, three", &[30_000, 20_000, 10_000], None, true),
  ] {
    let fixture = raresats_purchase(name, RARESATS_FROM, payments, fee, marker);
    assert_eq!(
      (paid(&fixture, "raresats"), marked(&fixture, "raresats")),
      (fee.is_some(), marker),
      "{name}"
    );
    let k = payments.len() as u32;
    assert_eq!(
      sales(&fixture),
      (1..)
        .zip(payments)
        .map(|(i, &payment)| sold(
          k + i,
          Listing,
          "raresats",
          (k + i, i),
          (payment, payment, 0)
        ))
        .collect::<Vec<Seen>>(),
      "{name}"
    );
  }
  // Before block 969,480 neither is raresats.net's (it has no earlier
  // sales): the sale of an unknown venue, at the payment.
  let early = raresats_purchase("early", RARESATS_FROM - 1, &[45_000], Some(1_125), true);
  assert!(!paid(&early, "raresats") && !marked(&early, "raresats"));
  assert_eq!(
    sales(&early),
    [sold(
      2,
      Listing,
      UNKNOWN_MARKETPLACE,
      (2, 1),
      (45_000, 45_000, 0)
    )]
  );
}

#[test]
fn raresats_lookalikes_are_not_sales() {
  let sat = Some(uncommon(969_100));
  // A transfer in an accepted offer's shape: the seller's UTXO (owner 20)
  // signed ALL, another party's funding (owner 30); the item to owner 31,
  // 40,000 sats to owner 32, the fee at vout 2 (Magisat's offer layout), the
  // funder's change. A raresats.net offer pays the seller's own script at
  // vout 1 (`raresats_offers`): no sale, with its fee, its marker or both.
  let transfer = |name: &str, fee: bool, marker: bool| {
    let mut outputs = vec![out_to(546, 31), out_to(40_000, 32)];
    outputs.extend(fee.then(|| raresats_fee(1_000)));
    outputs.push(out_to(if fee { 8_000 } else { 9_000 }, 30));
    outputs.extend(marker.then(raresats_marker));
    synthesized(
      name,
      RARESATS_FROM,
      &[(20, 546, Some(0x01), sat), (30, 50_000, Some(0x01), None)],
      outputs,
    )
  };
  for (name, fee, marker) in [
    ("fee", true, false),
    ("fee and marker", true, true),
    ("marker", false, true),
  ] {
    let fixture = transfer(name, fee, marker);
    assert_eq!(
      (paid(&fixture, "raresats"), marked(&fixture, "raresats")),
      (fee, marker),
      "{name}"
    );
    assert_eq!(sales(&fixture), [], "{name}");
  }
  // Were its fee in the price (proceeds plus fees, the default), the first
  // would be its offer: the price rule is what keeps such transfers out.
  let table = Marketplaces::from_json(&include_str!("../../data/marketplaces.json").replace(
    r#""price_rule": "proceeds""#,
    r#""price_rule": "proceeds_plus_fees""#,
  ))
  .unwrap();
  let (block, delta) = transfer("fee", true, false).block_and_delta();
  let ctx = MarketContext::new(
    catalog_for(&TrackerConfig::default()),
    table,
    Chain::Mainnet,
  );
  assert_eq!(
    analyze(&block, &delta, &ctx)
      .sales
      .iter()
      .map(|sale| (sale.kind, sale.marketplace.as_str(), sale.price))
      .collect::<Vec<_>>(),
    [(Offer, "raresats", 41_000)]
  );

  // A wallet moving its item to a new address of its own, paying the fee
  // and the marker (a tool's transfer): no sale.
  let tool = synthesized(
    "tool",
    RARESATS_FROM,
    &[(20, 546, Some(0x01), sat), (20, 5_000, Some(0x01), None)],
    vec![
      out_to(546, 21),
      raresats_fee(1_000),
      out_to(3_500, 20),
      raresats_marker(),
    ],
  );
  assert!(paid(&tool, "raresats") && marked(&tool, "raresats"));
  assert_eq!(sales(&tool), []);

  // The fee address receiving a listing it buys is no fee: without the
  // marker, the sale of an unknown venue.
  let bought = synthesized(
    "bought",
    RARESATS_FROM,
    &[
      (BUYER, 600, Some(0x01), None),
      (BUYER, 600, Some(0x01), None),
      (20, 546, Some(LISTING_SIGHASH), sat),
      (BUYER, 50_000, Some(0x01), None),
    ],
    vec![
      out_to(1_200, BUYER),
      raresats_fee(546),
      out_to(30_000, 40),
      out_to(600, BUYER),
      out_to(600, BUYER),
      out_to(50_000 - 30_000 - 1_200 - 1_000, BUYER),
    ],
  );
  assert!(paid(&bought, "raresats"));
  assert_eq!(
    sales(&bought),
    [sold(
      2,
      Listing,
      UNKNOWN_MARKETPLACE,
      (2, 1),
      (30_000, 30_000, 0)
    )]
  );
}

/// The seller (P2TR) of synthesized raresats.net offers, and its bidder.
const OFFER_SELLER: u8 = TAPROOT + 1;
const OFFER_BIDDER: u8 = TAPROOT + 2;

/// A synthesized raresats.net accepted offer, in its layout: the seller's
/// UTXO of `value` (an uncommon sat at offset 0) at vin 0, signed
/// `sighash`; the bidder's funding at vin 1.. (`funders`, each signed
/// `bidder_sighash`); outputs: the item, whole, to the bidder at vout 0,
/// `price` to the seller's own script at vout 1, the fee (2.5 % of the
/// price, at least 546 sats) at vout 2, the bidder's `change` at vout 3 if
/// any, the marker last. The funding covers a 1,000-sat miner fee.
#[allow(clippy::too_many_arguments)]
fn raresats_offer(
  name: &str,
  seller: u8,
  value: u64,
  sighash: u8,
  bidder: u8,
  funders: usize,
  bidder_sighash: u8,
  price: u64,
  change: Option<u64>,
) -> Fixture {
  let fee = (price / 40).max(546);
  let funding = price + fee + change.unwrap_or(0) + 1_000;
  let mut inputs: Vec<SynthInput> = vec![(seller, value, Some(sighash), Some(uncommon(969_200)))];
  for i in 0..funders as u64 {
    let share = funding / funders as u64 + u64::from(i == 0) * (funding % funders as u64);
    inputs.push((bidder, share, Some(bidder_sighash), None));
  }
  let mut outputs = vec![
    owner_out(value, bidder),
    owner_out(price, seller),
    raresats_fee(fee),
  ];
  outputs.extend(change.map(|change| owner_out(change, bidder)));
  outputs.push(raresats_marker());
  synthesized(name, RARESATS_FROM, &inputs, outputs)
}

/// The P2TR seller's offer: a 546-sat UTXO sold for 100,000 sats (a 2,500
/// fee) to a P2TR bidder, every input signed SIGHASH_DEFAULT, no change.
fn taproot_offer(name: &str) -> Fixture {
  raresats_offer(
    name,
    OFFER_SELLER,
    546,
    0x00,
    OFFER_BIDDER,
    1,
    0x00,
    100_000,
    None,
  )
}

#[test]
fn raresats_offers() {
  // The P2TR seller: the price is output 1, the bidder's fee is on top.
  let taproot = taproot_offer("taproot offer");
  assert!(taproot.inputs[0].script_hash == script_hash(&owner_script(OFFER_SELLER)));
  assert!(owner_script(OFFER_SELLER).is_p2tr());
  assert_eq!(taproot.outputs[2].value, 2_500);
  assert!(paid(&taproot, "raresats") && marked(&taproot, "raresats"));
  assert_eq!(
    sales(&taproot),
    [sold(0, Offer, "raresats", (1, 0), (100_000, 100_000, 0))]
  );
  // A P2WPKH seller signing ALL, a 10,000-sat UTXO sold for 20,000 (the
  // 546-sat minimum fee), to a P2WPKH bidder funding from two inputs, with
  // change at vout 3.
  let segwit = raresats_offer(
    "segwit offer",
    21,
    10_000,
    0x01,
    31,
    2,
    0x01,
    20_000,
    Some(7_000),
  );
  assert!(owner_script(21).is_p2wpkh());
  assert_eq!(
    (
      segwit.inputs.len(),
      segwit.outputs.len(),
      segwit.outputs[2].value
    ),
    (3, 5, 546)
  );
  assert_eq!(
    sales(&segwit),
    [sold(0, Offer, "raresats", (1, 0), (20_000, 20_000, 0))]
  );
  // A taproot bidder signing ALL explicitly for a P2WPKH seller.
  let mixed = raresats_offer(
    "mixed offer",
    21,
    546,
    0x01,
    OFFER_BIDDER,
    1,
    0x01,
    50_000,
    Some(3_000),
  );
  assert_eq!(
    sales(&mixed),
    [sold(0, Offer, "raresats", (1, 0), (50_000, 50_000, 0))]
  );
}

#[test]
fn raresats_offer_lookalikes_are_not_sales() {
  let offer = || taproot_offer("lookalike");
  let mut lookalikes: Vec<(&str, Fixture)> = Vec::new();

  // The fee without the marker (a transfer paying the fee), with or
  // without change after it.
  let mut fee_only = offer();
  fee_only.outputs.pop();
  lookalikes.push(("fee only", fee_only.clone()));
  fee_only.outputs.push(FixtureOutput {
    value: 500,
    script_pubkey: owner_script(OFFER_BIDDER),
  });
  lookalikes.push(("fee only, change", fee_only));
  // The marker without the fee: the bidder's change in its place.
  let mut marker_only = offer();
  marker_only.outputs[2].script_pubkey = owner_script(OFFER_BIDDER);
  lookalikes.push(("marker only", marker_only));
  // Neither.
  let mut neither = offer();
  neither.outputs.pop();
  neither.outputs[2].script_pubkey = owner_script(OFFER_BIDDER);
  lookalikes.push(("neither", neither));

  // Transfers to self: the "bidder" funding from the seller's own script,
  // or the item back to the seller's script.
  let mut self_funded = offer();
  self_funded.inputs[1].script_hash = script_hash(&owner_script(OFFER_SELLER));
  lookalikes.push(("self-funded", self_funded));
  let mut item_to_self = offer();
  item_to_self.outputs[0].script_pubkey = owner_script(OFFER_SELLER);
  lookalikes.push(("item to self", item_to_self));

  // Output 1 not the seller's script: a third party, the bidder's own
  // address (change back to the funder: a transfer paying the fee and the
  // marker), a P2WPKH address of the same owner number.
  for (name, script) in [
    ("paid to another", owner_script(TAPROOT + 9)),
    ("paid to the bidder", owner_script(OFFER_BIDDER)),
    ("paid to a lookalike script", script(OFFER_SELLER & 0x7f)),
  ] {
    let mut fixture = offer();
    fixture.outputs[1].script_pubkey = script;
    lookalikes.push((name, fixture));
  }

  // The sats not landing whole in vout 0: the item output smaller than the
  // UTXO (the sat, at offset 400, falls into the payment), or larger (the
  // bidder's sats merged into it).
  let mut short = offer();
  short.outputs[0].value = 330;
  short.outputs[1].value += 216;
  lookalikes.push(("item short", tracked_at(short, 0, 400)));
  let mut long = offer();
  long.outputs[0].value = 1_000;
  long.outputs[1].value -= 454;
  lookalikes.push(("item padded", long));

  // The layout out of order: fee and change swapped, the marker before the
  // change, an extra output, the seller not at vin 0.
  let segwit_offer = || {
    raresats_offer(
      "lookalike change",
      21,
      10_000,
      0x01,
      31,
      1,
      0x01,
      20_000,
      Some(7_000),
    )
  };
  let mut swapped = segwit_offer();
  swapped.outputs.swap(2, 3);
  lookalikes.push(("fee after the change", swapped));
  let mut marker_early = segwit_offer();
  marker_early.outputs.swap(3, 4);
  lookalikes.push(("marker before the change", marker_early));
  let mut extra = segwit_offer();
  extra.outputs.insert(
    4,
    FixtureOutput {
      value: 1_000,
      script_pubkey: script(32),
    },
  );
  lookalikes.push(("extra output", extra));
  let mut late = offer();
  late.inputs.swap(0, 1);
  late.tracked[0].vin = 1;
  lookalikes.push(("seller at vin 1", late));

  // Other sighash flags: the seller's or the bidder's ANYONECANPAY or
  // SINGLE, an unrecognized signature.
  for (name, vin, flag) in [
    ("seller SINGLE|ANYONECANPAY", 0, Some(0x83)),
    ("seller NONE", 0, Some(0x02)),
    ("bidder ALL|ANYONECANPAY", 1, Some(0x81)),
    ("bidder unsigned", 1, None),
  ] {
    let mut fixture = offer();
    fixture.inputs[vin].sighash = flag;
    lookalikes.push((name, fixture));
  }

  // Before raresats.net's launch, neither the fee nor the marker is its.
  let mut early = offer();
  early.height = RARESATS_FROM - 1;
  lookalikes.push(("before launch", early));

  for (name, mut fixture) in lookalikes {
    fixture.name = name.into();
    assert_eq!(sales(&fixture), [], "{name}");
  }
}

/// A two-party ALL|ANYONECANPAY sale (Satflow's secure-purchase form,
/// docs/SALES-SHAPES.md): the seller's 546-sat UTXO (owner 20) signed
/// `0x81` at vin 0, the buyer's funding (owner 30) signed ALL; the item to
/// the buyer (owner 31) at vout 0, 60,000 sats to the seller's script at
/// vout 1, then `extra` (fees, a marker), the buyer's change last.
fn psbt_sale(name: &str, extra: Vec<TxOut>) -> Fixture {
  let spent: u64 = extra.iter().map(|output| output.value.to_sat()).sum();
  let mut outputs = vec![out_to(546, 31), out_to(60_000, 20)];
  outputs.extend(extra);
  outputs.push(out_to(9_000, 30));
  synthesized(
    name,
    RARESATS_FROM,
    &[
      (20, 546, Some(PSBT_SIGHASH), Some(uncommon(969_300))),
      (30, 60_000 + spent + 9_000 + 1_000, Some(0x01), None),
    ],
    outputs,
  )
}

#[test]
fn raresats_fee_marks_no_two_party_sale() {
  // raresats.net never sells in this shape: its fee (with or without its
  // marker) neither attributes the sale nor adds to its price. The sale is
  // an unknown venue's, at the payment.
  for (name, extra) in [
    ("psbt, raresats fee", vec![raresats_fee(1_500)]),
    (
      "psbt, raresats fee and marker",
      vec![raresats_fee(1_500), raresats_marker()],
    ),
    ("psbt, raresats marker", vec![raresats_marker()]),
  ] {
    let fixture = psbt_sale(name, extra);
    assert_eq!(
      sales(&fixture),
      [sold(
        0,
        Listing,
        UNKNOWN_MARKETPLACE,
        (1, 0),
        (60_000, 60_000, 0)
      )],
      "{name}"
    );
  }
  // With another marketplace's fee too, the sale is that marketplace's, at
  // the payment plus its fee only.
  let both = psbt_sale(
    "psbt, satflow and raresats fees",
    vec![pay_to(SATFLOW_FEE_2, 1_200), raresats_fee(1_500)],
  );
  assert!(paid(&both, "satflow") && paid(&both, "raresats"));
  assert_eq!(
    sales(&both),
    [sold(0, Listing, "satflow", (1, 0), (61_200, 60_000, 1_200))]
  );
}

/// Every recorded fixture's sales are the same with or without raresats'
/// offer flow: marked offers change nothing else (and none pays its fee).
#[test]
fn raresats_offers_change_no_other_sale() {
  let without = Marketplaces::from_json(&include_str!("../../data/marketplaces.json").replace(
    "\"listing_0x83_msigner\",\n        \"offer_sighash_all\"\n      ],\n      \"notes\": \"Rare-sat marketplace",
    "\"listing_0x83_msigner\"\n      ],\n      \"notes\": \"Rare-sat marketplace",
  ))
  .unwrap();
  assert!(
    !without
      .by_id("raresats")
      .unwrap()
      .has_flow("offer_sighash_all")
  );
  assert!(
    Marketplaces::embedded()
      .by_id("raresats")
      .unwrap()
      .has_flow("offer_sighash_all")
  );
  let ctx = MarketContext::new(
    catalog_for(&TrackerConfig::default()),
    without,
    Chain::Mainnet,
  );
  for (name, _) in FIXTURES {
    // None pays raresats.net's fee, so ignoring it in two-party sales
    // changes none of them either.
    assert!(!pays(name, "raresats"), "{name}");
    let (block, delta) = Fixture::load(name).block_and_delta();
    assert_eq!(
      analyze(&block, &delta, &context()).sales,
      analyze(&block, &delta, &ctx).sales,
      "{name}"
    );
  }
  // And without it, the offers are no sale.
  let (block, delta) = taproot_offer("taproot offer").block_and_delta();
  assert_eq!(analyze(&block, &delta, &ctx).sales, []);
}

/// Magic Eden's fee address of 2023-2026 (`bc1qcq2…`, to block 940,150).
const MAGIC_EDEN_FEE: &str = "bc1qcq2uv5nk6hec6kvag3wyevp6574qmsm9scjxc2";

/// Satflow's second fee address, from block 958,599.
const SATFLOW_FEE_2: &str = "bc1qzwwj9elnghu55g0uwuj8lju4z3yzp7nsatfl0w";

fn pay_to(address: &str, value: u64) -> TxOut {
  TxOut {
    value: Amount::from_sat(value),
    script_pubkey: Address::from_str(address)
      .unwrap()
      .require_network(Network::Bitcoin)
      .unwrap()
      .script_pubkey(),
  }
}

/// Moves the tracked sat of input `vin` of a synthesized fixture to offset
/// `offset` of that input.
fn tracked_at(mut fixture: Fixture, vin: u32, offset: u64) -> Fixture {
  let tracked = fixture
    .tracked
    .iter_mut()
    .find(|tracked| tracked.vin == vin)
    .unwrap();
  assert!(offset < tracked.value);
  tracked.pieces[0].offset = offset;
  fixture
}

/// A marketplace offer whose seller is not the input holding tracked sats:
/// the item at vin 0 (owner 20, 10,000 sats) passes whole to the buyer's
/// ordinals address (owner 31) at vout 0; the buyer's funding at vin 1
/// (owner 30, 1,187,791 sats, a tracked sat at offset 16,321) pays the
/// item's seller 460,000 (owner 21) at vout 1 and Magic Eden's 579-sat
/// floor fee at vout 2, and gets its change at vout 3. FIFO puts the
/// funding's tracked sat into the payment: shaped like the funding's own
/// sale, paid by the 10,000-sat item (Magic Eden, 2023-2024: 11 such
/// readings, 3,885 to 11,023 sats for UTXOs of up to 3.5 BTC).
fn funding_pays_for_an_earlier_item(name: &str, item: Option<u64>) -> Fixture {
  let funding = 1_187_791;
  let fixture = synthesized(
    name,
    900_000,
    &[
      (20, 10_000, Some(0x01), item),
      (30, funding, Some(0x01), Some(uncommon(899_000))),
    ],
    vec![
      out_to(10_000, 31),
      out_to(460_000, 21),
      pay_to(MAGIC_EDEN_FEE, 579),
      out_to(funding - 460_000 - 579 - 1_000, 30),
    ],
  );
  tracked_at(fixture, 1, 16_321)
}

/// A Magic Eden rune-bid fill: a runestone at vout 0; two bidders' escrows
/// (owners 50 and 51) and the seller's 546-sat rune UTXOs (owner 60); the
/// runes to the bidders (owners 70, 71), the seller's rune change, each
/// escrow's change, the fee, and the seller's payment (owner 61). Bidder
/// 50's escrow holds a tracked sat at offset 10,697,918, which FIFO puts in
/// bidder 51's change (4 such readings below the UTXO's value, 5 above).
fn rune_bid_fill(name: &str, runestone: bool) -> Fixture {
  let mut outputs = Vec::new();
  if runestone {
    outputs.push(TxOut {
      value: Amount::ZERO,
      script_pubkey: ScriptBuf::from_hex("6a5d0b00c0a23303e8070100").unwrap(),
    });
  }
  outputs.extend([
    out_to(546, 70),
    out_to(546, 71),
    out_to(546, 60),
    out_to(9_997_643 + 3_276 - 1_638, 50),
    out_to(740_183, 51),
    pay_to(MAGIC_EDEN_FEE, 76_701),
  ]);
  let total_in: u64 = 11_998_188 + 2_740_929 + 3 * 546;
  let spent: u64 = outputs.iter().map(|output| output.value.to_sat()).sum();
  outputs.push(out_to(total_in - spent - 2_000, 61));
  let fixture = synthesized(
    name,
    900_000,
    &[
      (50, 11_998_188, Some(0x01), Some(uncommon(899_100))),
      (51, 2_740_929, Some(0x01), None),
      (60, 546, Some(0x00), None),
      (60, 546, Some(0x00), None),
      (60, 546, Some(0x00), None),
    ],
    outputs,
  );
  tracked_at(fixture, 0, 10_697_918)
}

/// An exchange's batch payout that also pays a marketplace's fee address:
/// five 10 BTC inputs of five scripts (owners 80..=84), input 1 holding a
/// tracked sat at offset 924,559,322; payouts to owners 100.., the fee
/// address, and the change (owner 99), fed by inputs 1-4, which receives
/// the tracked sat (Satflow's second fee address, first paid by such a
/// batch at block 958,599: 2 readings, 10 BTC "sold" for 3.08 BTC each).
fn exchange_batch(name: &str) -> Fixture {
  let mut outputs: Vec<TxOut> = (0..9)
    .map(|i| out_to(100_000 + i * 1_000, 100 + i as u8))
    .collect();
  outputs.push(out_to(608_613_711, 109));
  outputs.push(pay_to(SATFLOW_FEE_2, 6_900_000));
  let spent: u64 = outputs.iter().map(|output| output.value.to_sat()).sum();
  outputs.push(out_to(5_000_000_000 - spent - 50_000, 99));
  let fixture = synthesized(
    name,
    958_599,
    &[
      (80, 1_000_000_000, Some(0x01), None),
      (81, 1_000_000_000, Some(0x01), Some(uncommon(958_000))),
      (82, 1_000_000_000, Some(0x01), None),
      (83, 1_000_000_000, Some(0x01), None),
      (84, 1_000_000_000, Some(0x01), None),
    ],
    outputs,
  );
  tracked_at(fixture, 1, 924_559_322)
}

#[test]
fn offer_misreadings_are_not_sales() {
  // The funding is no seller: the output taking exactly the earlier item's
  // sats is that item's delivery, not a payment.
  let fixture = funding_pays_for_an_earlier_item("funding", None);
  assert!(paid(&fixture, "magic_eden"));
  assert_eq!(sales(&fixture), []);
  // When the item itself holds tracked sats, it is the one sale: paid
  // 460,000 at vout 1, the whole fee its own.
  let fixture = funding_pays_for_an_earlier_item("item and funding", Some(uncommon(899_001)));
  assert_eq!(
    sales(&fixture),
    [sold(
      0,
      Offer,
      "magic_eden",
      (1, 0),
      (460_579, 460_000, 579)
    )]
  );

  // A rune trade: none of the bidders' sats are sold. Without the runestone
  // the same transaction reads as bidder 50's sale, which the rule is what
  // prevents.
  let fixture = rune_bid_fill("rune bid", true);
  assert!(paid(&fixture, "magic_eden"));
  assert_eq!(sales(&fixture), []);
  let without = rune_bid_fill("rune bid without runestone", false);
  assert_eq!(
    sales(&without)
      .iter()
      .map(|sale| (sale.0, sale.1, sale.2.as_str(), sale.4))
      .collect::<Vec<_>>(),
    [(0, Offer, "magic_eden", 4)]
  );

  // An exchange batch: the tracked sat lands in the change, fed by four
  // parties' inputs; no item, no sale.
  let fixture = exchange_batch("exchange batch");
  assert!(paid(&fixture, "satflow"));
  assert_eq!(sales(&fixture), []);
}

#[test]
fn offers_below_the_sold_utxo_value() {
  // The last line: a seller handing over a whole 100,000-sat UTXO (owner 20,
  // an uncommon at offset 0) to the buyer (owner 31) for a 50,000-sat
  // payment (owner 21) funded by the buyer (owner 30) is a misreading.
  let whole = synthesized(
    "whole",
    900_000,
    &[
      (20, 100_000, Some(0x01), Some(uncommon(899_200))),
      (30, 80_000, Some(0x01), None),
    ],
    vec![
      out_to(100_000, 31),
      out_to(50_000, 21),
      pay_to(MAGIC_EDEN_FEE, 1_250),
      out_to(80_000 - 50_000 - 1_250 - 1_000, 30),
    ],
  );
  assert_eq!(sales(&whole), []);
  // The same price for one sat split out of that UTXO, the rest back to the
  // seller: a sale (the seller keeps 99,454 sats).
  let split = synthesized(
    "split",
    900_000,
    &[
      (20, 100_000, Some(0x01), Some(uncommon(899_200))),
      (30, 80_000, Some(0x01), None),
    ],
    vec![
      out_to(546, 31),
      out_to(99_454, 20),
      out_to(50_000, 21),
      pay_to(MAGIC_EDEN_FEE, 1_250),
      out_to(80_000 - 50_000 - 1_250 - 1_000, 30),
    ],
  );
  assert_eq!(
    sales(&split),
    [sold(
      0,
      Offer,
      "magic_eden",
      (2, 0),
      (51_250, 50_000, 1_250)
    )]
  );
  // Paid at least the UTXO's value: a sale.
  let fair = synthesized(
    "fair",
    900_000,
    &[
      (20, 100_000, Some(0x01), Some(uncommon(899_200))),
      (30, 200_000, Some(0x01), None),
    ],
    vec![
      out_to(100_000, 31),
      out_to(150_000, 21),
      pay_to(MAGIC_EDEN_FEE, 3_750),
      out_to(200_000 - 150_000 - 3_750 - 1_000, 30),
    ],
  );
  assert_eq!(
    sales(&fair),
    [sold(
      0,
      Offer,
      "magic_eden",
      (1, 0),
      (153_750, 150_000, 3_750)
    )]
  );
}

#[test]
fn every_fixture_is_listed() {
  let mut files: Vec<String> = std::fs::read_dir(dir())
    .unwrap()
    .map(|entry| {
      let name = entry.unwrap().file_name().into_string().unwrap();
      name.strip_suffix(".json").unwrap().to_string()
    })
    .collect();
  files.sort();
  let mut names: Vec<String> = FIXTURES.iter().map(|(name, _)| name.to_string()).collect();
  names.sort();
  assert_eq!(files, names);
  for (name, txid) in FIXTURES {
    let fixture = Fixture::load(name);
    assert_eq!(fixture.name, *name);
    assert_eq!(fixture.txid.to_string(), *txid);
    assert!(!fixture.tracked.is_empty(), "{name}");
    assert_eq!(fixture.spender.is_some(), SETTLED.contains(name), "{name}");
  }
}

/// Builds the fixtures (ignored: needs bitcoind, the fork and Magisat's
/// sales records).
#[test]
#[ignore]
fn build() {
  let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
  let (rpc, live) = live_source();
  let fork = Fork::new(env("FORK_URL"));
  // Magisat's records by buyer txid: (sold UTXO, main sat, count, offset).
  type Record = (OutPoint, u64, u64, u64);
  let mut magisat: BTreeMap<String, Vec<Record>> = BTreeMap::new();
  for line in std::fs::read_to_string(env("MAGISAT_SALES"))
    .unwrap()
    .lines()
  {
    let record: Value = serde_json::from_str(line).unwrap();
    let snapshot = &record["data"]["snapshot"];
    let main = &snapshot["mainSatoshi"];
    let (Some(buyer), Some(utxo), Some(start)) = (
      snapshot["buyerTxId"].as_str(),
      snapshot["utxo"].as_str(),
      main["rangeStart"].as_str(),
    ) else {
      continue;
    };
    let number = |value: &Value| {
      value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
    };
    let outpoint = OutPoint::from_str(utxo).unwrap();
    let entry = magisat.entry(buyer.into()).or_default();
    if entry.iter().all(|(o, ..)| *o != outpoint) {
      entry.push((
        outpoint,
        start.parse().unwrap(),
        number(&main["count"]).unwrap_or(1).max(1),
        number(&main["offset"]).unwrap_or(0),
      ));
    }
  }
  let moves_from = fork.get("/status")["moves_from"].as_u64().unwrap() as u32;
  std::fs::create_dir_all(dir()).unwrap();
  let rebuild = std::env::var_os("FIXTURES_REBUILD").is_some();
  for &(name, txid) in FIXTURES {
    let existing = dir()
      .join(format!("{name}.json"))
      .exists()
      .then(|| Fixture::load(name));
    if existing.is_some() && !rebuild {
      continue;
    }
    let raw: Value = rpc
      .call("getrawtransaction", &[txid.into(), 2.into()])
      .unwrap();
    let hash = BlockHash::from_str(raw["blockhash"].as_str().unwrap()).unwrap();
    let header: Value = rpc
      .call("getblockheader", &[hash.to_string().into()])
      .unwrap();
    let height = header["height"].as_u64().unwrap() as u32;
    let block = live.block(hash, height).unwrap();
    let txid = Txid::from_str(txid).unwrap();
    let (tx_index, tx) = block
      .txs
      .iter()
      .enumerate()
      .find(|(_, tx)| tx.txid == txid)
      .unwrap();
    let tracked_input = |vin: usize, pieces: Vec<Piece>| {
      let prevout = &raw["vin"][vin]["prevout"];
      FixtureTracked {
        vin: vin as u32,
        outpoint: tx.inputs[vin].previous_output,
        value: tx.inputs[vin].value,
        script_pubkey: ScriptBuf::from_hex(prevout["scriptPubKey"]["hex"].as_str().unwrap())
          .unwrap(),
        height: prevout["height"].as_u64().unwrap() as u32,
        pieces,
      }
    };
    let recorded = (existing.is_none() && height >= moves_from).then(|| {
      fork
        .moves(height)
        .into_iter()
        .find(|m| m.txid == txid)
        .unwrap_or_else(|| panic!("{name}: the fork has no move for {txid}"))
    });
    let (tracked, pieces_from): (Vec<FixtureTracked>, String) = if let Some(existing) = &existing {
      (existing.tracked.clone(), existing.pieces_from.clone())
    } else if let Some(m) = &recorded {
      let tracked = m
        .inputs
        .iter()
        .map(|input| FixtureTracked {
          vin: input.vin,
          outpoint: input.outpoint,
          value: input.utxo.value,
          script_pubkey: input.utxo.script_pubkey.clone(),
          height: input.utxo.height,
          pieces: input.utxo.pieces.clone(),
        })
        .collect();
      (tracked, "fork".into())
    } else if let Some(sold) = fork
      .try_get(&format!("/sales/tx/{txid}"))
      .map(|json| json["sales"].as_array().cloned().unwrap_or_default())
      .filter(|sales| !sales.is_empty())
    {
      let tracked = sold
        .iter()
        .map(|sale| {
          let vin = sale["vin"].as_u64().unwrap() as usize;
          let pieces = sale["pieces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|piece| Piece {
              offset: piece["offset"].as_u64().unwrap(),
              start: piece["start"].as_u64().unwrap(),
              len: piece["len"].as_u64().unwrap(),
            })
            .collect();
          tracked_input(vin, pieces)
        })
        .collect();
      (tracked, "fork_sales".into())
    } else if let Some(records) = magisat.get(&txid.to_string()) {
      let mut tracked: Vec<FixtureTracked> = records
        .iter()
        .map(|&(outpoint, start, count, offset)| {
          let vin = tx
            .inputs
            .iter()
            .position(|input| input.previous_output == outpoint)
            .unwrap();
          tracked_input(
            vin,
            vec![Piece {
              offset,
              start,
              len: count.min(tx.inputs[vin].value - offset),
            }],
          )
        })
        .collect();
      tracked.sort_by_key(|tracked| tracked.vin);
      (tracked, "magisat".into())
    } else {
      // The sats the outputs still hold, mapped back to the inputs.
      let (base, starts, _) = offsets(tx);
      let mut by_vin: BTreeMap<usize, Vec<Piece>> = BTreeMap::new();
      for (vout, &output_start) in starts.iter().enumerate() {
        let Some(json) = fork.try_get(&format!("/output/{txid}:{vout}")) else {
          continue;
        };
        for piece in json["pieces"].as_array().unwrap() {
          let offset = output_start + piece["offset"].as_u64().unwrap();
          let vin = base.partition_point(|&b| b <= offset) - 1;
          let len = piece["len"]
            .as_u64()
            .unwrap()
            .min(base[vin] + tx.inputs[vin].value - offset);
          by_vin.entry(vin).or_default().push(Piece {
            offset: offset - base[vin],
            start: piece["start"].as_u64().unwrap(),
            len,
          });
        }
      }
      let tracked: Vec<FixtureTracked> = by_vin
        .into_iter()
        .map(|(vin, pieces)| tracked_input(vin, merged(pieces)))
        .collect();
      assert!(!tracked.is_empty(), "{name}: no tracked sats in place");
      (tracked, "fork_output".into())
    };
    let spender = SETTLED.contains(&name).then(|| {
      let escrow = OutPoint::new(txid, 0);
      let spender = block
        .txs
        .iter()
        .find(|tx| {
          tx.inputs
            .iter()
            .any(|input| input.previous_output == escrow)
        })
        .unwrap_or_else(|| panic!("{name}: output 0 is not spent in the same block"));
      fixture_tx(spender)
    });
    let FixtureTx {
      inputs, outputs, ..
    } = fixture_tx(tx);
    let fixture = Fixture {
      name: name.into(),
      txid,
      height,
      tx_index: tx_index as u32,
      pieces_from,
      tracked,
      inputs,
      outputs,
      spender,
    };
    let (block, built) = fixture.block_and_delta();
    if let Some(m) = &recorded {
      let outputs = |outputs: &[MoveOutput]| -> Vec<(u32, Vec<Piece>)> {
        outputs
          .iter()
          .map(|output| (output.vout, merged(output.pieces.clone())))
          .collect()
      };
      assert_eq!(
        outputs(&built.moves[0].outputs),
        outputs(&m.outputs),
        "{name}: FIFO differs from the fork's move"
      );
    }
    std::fs::write(
      dir().join(format!("{name}.json")),
      serde_json::to_string_pretty(&fixture).unwrap() + "\n",
    )
    .unwrap();
    let market = analyze(&block, &built, &context());
    eprintln!(
      "{name}: height {height}, {} in, {} out, tracked vins {:?} ({}), sales {:?}",
      fixture.inputs.len(),
      fixture.outputs.len(),
      fixture
        .tracked
        .iter()
        .map(|tracked| tracked.vin)
        .collect::<Vec<_>>(),
      fixture.pieces_from,
      market
        .sales
        .iter()
        .map(|sale| (
          sale.vin,
          sale.kind,
          &sale.marketplace,
          sale.payment_vout,
          sale.buyer_vout,
          sale.price,
          sale.seller_proceeds,
          sale.fees
        ))
        .collect::<Vec<_>>()
    );
  }
}

/// Re-runs the detector on transactions the fork recorded sales for, with
/// the pieces it recorded (ignored: needs bitcoind and the fork):
/// `REVALIDATE_TXIDS` (a file, one txid per line) to `REVALIDATE_OUT` (one
/// JSON line per transaction: its height and the sales found now).
#[test]
#[ignore]
fn revalidate() {
  use std::io::Write;
  let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
  let (rpc, live) = live_source();
  let fork = Fork::new(env("FORK_URL"));
  let txids = std::fs::read_to_string(env("REVALIDATE_TXIDS")).unwrap();
  let mut out = std::io::BufWriter::new(std::fs::File::create(env("REVALIDATE_OUT")).unwrap());
  let ctx = context();
  for txid in txids.lines().map(str::trim).filter(|line| !line.is_empty()) {
    let raw: Value = rpc
      .call("getrawtransaction", &[txid.into(), 2.into()])
      .unwrap();
    let hash = BlockHash::from_str(raw["blockhash"].as_str().unwrap()).unwrap();
    let header: Value = rpc
      .call("getblockheader", &[hash.to_string().into()])
      .unwrap();
    let height = header["height"].as_u64().unwrap() as u32;
    let block = live.block(hash, height).unwrap();
    let txid = Txid::from_str(txid).unwrap();
    let (tx_index, tx) = block
      .txs
      .iter()
      .enumerate()
      .find(|(_, tx)| tx.txid == txid)
      .unwrap();
    let sold = fork
      .try_get(&format!("/sales/tx/{txid}"))
      .map(|json| json["sales"].as_array().cloned().unwrap_or_default())
      .unwrap_or_default();
    let tracked = sold
      .iter()
      .map(|sale| {
        let vin = sale["vin"].as_u64().unwrap() as usize;
        let prevout = &raw["vin"][vin]["prevout"];
        FixtureTracked {
          vin: vin as u32,
          outpoint: tx.inputs[vin].previous_output,
          value: tx.inputs[vin].value,
          script_pubkey: ScriptBuf::from_hex(prevout["scriptPubKey"]["hex"].as_str().unwrap())
            .unwrap(),
          height: prevout["height"].as_u64().unwrap() as u32,
          pieces: sale["pieces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|piece| Piece {
              offset: piece["offset"].as_u64().unwrap(),
              start: piece["start"].as_u64().unwrap(),
              len: piece["len"].as_u64().unwrap(),
            })
            .collect(),
        }
      })
      .collect();
    let FixtureTx {
      inputs, outputs, ..
    } = fixture_tx(tx);
    let fixture = Fixture {
      name: txid.to_string(),
      txid,
      height,
      tx_index: tx_index as u32,
      pieces_from: "fork_sales".into(),
      tracked,
      inputs,
      outputs,
      spender: None,
    };
    let (block, delta) = fixture.block_and_delta();
    let market = analyze(&block, &delta, &ctx);
    let sales: Vec<Value> = market
      .sales
      .iter()
      .map(|sale| {
        serde_json::json!({
          "vin": sale.vin,
          "kind": sale.kind,
          "marketplace": sale.marketplace,
          "price": sale.price,
          "seller_proceeds": sale.seller_proceeds,
          "fees": sale.fees,
          "payment_vout": sale.payment_vout,
          "buyer_vout": sale.buyer_vout,
        })
      })
      .collect();
    serde_json::to_writer(
      &mut out,
      &serde_json::json!({ "txid": txid, "height": height, "sales": sales }),
    )
    .unwrap();
    writeln!(out).unwrap();
  }
  out.flush().unwrap();
}

fn live_source() -> (crate::source::Rpc, crate::source::LiveSource) {
  use crate::source::{LiveSource, Rpc, RpcAuth, RpcConfig};
  let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
  let rpc = Rpc::new(RpcConfig {
    url: env("ORD_BITCOIN_RPC_URL"),
    auth: RpcAuth::UserPass {
      username: env("ORD_BITCOIN_RPC_USERNAME"),
      password: env("ORD_BITCOIN_RPC_PASSWORD"),
    },
    timeout_secs: 600,
  })
  .unwrap();
  (rpc.clone(), LiveSource::new(rpc))
}

/// The live fork's JSON API.
struct Fork {
  url: String,
  client: reqwest::blocking::Client,
}

impl Fork {
  fn new(url: String) -> Self {
    Self {
      url,
      client: reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .unwrap(),
    }
  }

  fn get(&self, path: &str) -> Value {
    self
      .try_get(path)
      .unwrap_or_else(|| panic!("{path}: not found"))
  }

  /// `None` on 404.
  fn try_get(&self, path: &str) -> Option<Value> {
    let response = self
      .client
      .get(format!("{}{path}", self.url))
      .send()
      .unwrap();
    if response.status() == reqwest::StatusCode::NOT_FOUND {
      return None;
    }
    assert!(
      response.status().is_success(),
      "{path}: {}",
      response.status()
    );
    Some(response.json().unwrap())
  }

  /// `/block/{height}/moves` as the tracker's moves.
  fn moves(&self, height: u32) -> Vec<Move> {
    let pieces = |pieces: &Value| -> Vec<Piece> {
      pieces
        .as_array()
        .unwrap()
        .iter()
        .map(|piece| Piece {
          offset: piece["offset"].as_u64().unwrap(),
          start: piece["start"].as_u64().unwrap(),
          len: piece["len"].as_u64().unwrap(),
        })
        .collect()
    };
    let script = |value: &Value| ScriptBuf::from_hex(value.as_str().unwrap()).unwrap();
    self.get(&format!("/block/{height}/moves"))["moves"]
      .as_array()
      .unwrap()
      .iter()
      .map(|m| Move {
        txid: Txid::from_str(m["txid"].as_str().unwrap()).unwrap(),
        tx_index: m["tx_index"].as_u64().unwrap() as u32,
        coinbase: m["coinbase"].as_bool().unwrap(),
        input_value: m["input_value"].as_u64().unwrap(),
        output_value: m["output_value"].as_u64().unwrap(),
        inputs: m["inputs"]
          .as_array()
          .unwrap()
          .iter()
          .map(|input| MoveInput {
            vin: input["vin"].as_u64().unwrap() as u32,
            outpoint: OutPoint::from_str(input["outpoint"].as_str().unwrap()).unwrap(),
            utxo: TrackedUtxo {
              value: input["value"].as_u64().unwrap(),
              script_pubkey: script(&input["script_pubkey"]),
              height: input["created_height"].as_u64().unwrap() as u32,
              kind: UtxoKind::Spendable,
              pieces: pieces(&input["pieces"]),
            },
          })
          .collect(),
        outputs: m["outputs"]
          .as_array()
          .unwrap()
          .iter()
          .map(|output| MoveOutput {
            vout: output["vout"].as_u64().unwrap() as u32,
            value: output["value"].as_u64().unwrap(),
            script_pubkey: script(&output["script_pubkey"]),
            kind: if output["kind"] == "spendable" {
              UtxoKind::Spendable
            } else {
              UtxoKind::Unspendable
            },
            pieces: pieces(&output["pieces"]),
          })
          .collect(),
        fee: Vec::new(),
        mined: Vec::new(),
        lost: Vec::new(),
      })
      .collect()
  }
}

/// Runs the detector over the fork's moves window (ignored: needs bitcoind
/// and the fork). Writes one line per transaction that spent tracked sats
/// to `MEASURE_OUT` (its inputs' flags, the table entries its outputs pay,
/// its sales) and prints the tallies.
#[test]
#[ignore]
fn measure() {
  use std::io::Write;
  let env = |name: &str| std::env::var(name).ok();
  let fork = Fork::new(env("FORK_URL").expect("set FORK_URL"));
  let status = fork.get("/status");
  let from = env("MEASURE_FROM").map_or(status["moves_from"].as_u64().unwrap() as u32, |from| {
    from.parse().unwrap()
  });
  let to = env("MEASURE_TO").map_or(status["height"].as_u64().unwrap() as u32, |to| {
    to.parse().unwrap()
  });
  let threads = env("MEASURE_THREADS").map_or(4, |threads| threads.parse().unwrap());
  let mut out = std::io::BufWriter::new(
    std::fs::File::create(env("MEASURE_OUT").expect("set MEASURE_OUT")).unwrap(),
  );
  let (_, live) = live_source();
  let ctx = context();
  let mut tally: BTreeMap<String, u64> = BTreeMap::new();
  let mut count = |key: String| *tally.entry(key).or_default() += 1;
  let started = std::time::Instant::now();
  let mut analyze_time = std::time::Duration::ZERO;
  let mut blocks = 0u32;
  for block in live.blocks(from, to, threads) {
    let block = block.unwrap();
    let moves = fork.moves(block.height);
    let delta = delta(block.height, block.hash, moves);
    let start = std::time::Instant::now();
    let market = analyze(&block, &delta, &ctx);
    analyze_time += start.elapsed();
    blocks += 1;
    for sale in &market.sales {
      count(format!("sales {:?} {}", sale.kind, sale.marketplace));
    }
    for _ in &market.inscriptions {
      count("inscription events".into());
    }
    for m in &delta.moves {
      if m.coinbase || m.inputs.is_empty() {
        continue;
      }
      let tx = &block.txs[m.tx_index as usize];
      assert_eq!(tx.txid, m.txid);
      let flags: Vec<Option<u8>> = tx.inputs.iter().map(|input| input.sighash).collect();
      let mut paid: Vec<String> = tx
        .outputs
        .iter()
        .filter_map(|output| {
          let script = &output.script_pubkey;
          ctx
            .marketplaces
            .fee_output(script, block.height)
            .or_else(|| ctx.marketplaces.marker(script, block.height))
            .map(|marketplace| marketplace.id.clone())
        })
        .collect();
      paid.sort();
      paid.dedup();
      let sales: Vec<Value> = market
        .sales
        .iter()
        .filter(|sale| sale.txid == m.txid)
        .map(|sale| {
          let unit = sale_unit(sale, &ctx.catalog).map(|(row, count, unit)| {
            (ctx.catalog.rows()[usize::from(row)].id.clone(), count, unit)
          });
          serde_json::json!({
            "vin": sale.vin,
            "kind": sale.kind,
            "marketplace": sale.marketplace,
            "payment_vout": sale.payment_vout,
            "buyer_vout": sale.buyer_vout,
            "price": sale.price,
            "seller_proceeds": sale.seller_proceeds,
            "fees": sale.fees,
            "unit": unit,
          })
        })
        .collect();
      serde_json::to_writer(
        &mut out,
        &serde_json::json!({
          "height": block.height,
          "txid": m.txid,
          "flags": flags,
          "tracked_vins": m.inputs.iter().map(|input| input.vin).collect::<Vec<_>>(),
          "paid": paid,
          "sales": sales,
        }),
      )
      .unwrap();
      writeln!(out).unwrap();
    }
  }
  out.flush().unwrap();
  eprintln!(
    "blocks {from}..={to} ({blocks}) in {:.0?}; analyze {:.1?} in total, {:.1} us per block",
    started.elapsed(),
    analyze_time,
    analyze_time.as_secs_f64() * 1e6 / f64::from(blocks.max(1)),
  );
  for (key, n) in tally {
    eprintln!("{key}: {n}");
  }
}

/// Every fixture file's name, block and delta (as the tests here build
/// them), for other modules' tests: the mempool overlay classifies each
/// transaction as pending and must find the same sales.
pub(crate) fn fixture_blocks() -> Vec<(String, ResolvedBlock, BlockDelta)> {
  let mut names: Vec<String> = std::fs::read_dir(dir())
    .unwrap()
    .filter_map(|entry| {
      let name = entry.unwrap().file_name().into_string().unwrap();
      name.strip_suffix(".json").map(str::to_owned)
    })
    .collect();
  names.sort();
  names
    .into_iter()
    .map(|name| {
      let (block, delta) = Fixture::load(&name).block_and_delta();
      (name, block, delta)
    })
    .collect()
}

/// The market context the fixtures are analyzed with.
pub(crate) fn fixture_context() -> MarketContext {
  context()
}
