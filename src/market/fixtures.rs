//! Real transactions as test fixtures (`tests/fixtures/sales/*.json`, the
//! txids of docs/SALES-SHAPES.md), the ignored builder that makes them, and
//! the ignored measurement of the detector over the fork's moves window.
//! Both ignored tests run on a box with bitcoind and the live fork:
//!
//! ```sh
//! set -a; source ~/.config/ord-sats/bitcoind.env; set +a  # ORD_BITCOIN_RPC_*
//! FORK_URL=http://127.0.0.1:8090 MAGISAT_SALES=~/work/magisat/sales.jsonl \
//!   cargo test --release --lib market::fixtures::build -- --ignored --nocapture
//!   # builds the missing fixtures; FIXTURES_REBUILD=1 rebuilds every one
//! FORK_URL=http://127.0.0.1:8090 MEASURE_OUT=market.jsonl \
//!   cargo test --release --lib market::fixtures::measure -- --ignored --nocapture
//! ```
//!
//! A fixture is one transaction as the live source resolves it (`getblock
//! 3`: sighash flags and envelopes as production decodes them) plus its
//! tracked inputs: the fork's recorded move when the block is within its
//! moves window (`pieces_from: "fork"`), otherwise the main sat of each sale
//! in Magisat's records at its offset in the sold UTXO (`"magisat"`: the
//! sale's shape is real, the piece stands for the sats). The tracker's
//! outputs are recomputed by FIFO; the builder checks them against the
//! fork's move.

use {
  super::{MarketContext, Marketplaces, PSBT_SIGHASH, UNKNOWN_MARKETPLACE, analyze, sale_unit},
  crate::{
    model::{
      BlockDelta, Envelope, Move, MoveInput, MoveOutput, Piece, ResolvedBlock, ResolvedInput,
      ResolvedTx, SaleKind,
      SaleKind::{Listing, Offer},
      TrackedUtxo, TrackerConfig, UtxoKind,
    },
    settings::Chain,
    tracker::tests::catalog_for,
  },
  bitcoin::{
    Amount, BlockHash, CompactTarget, OutPoint, ScriptBuf, TxMerkleNode, TxOut, Txid,
    block::{Header, Version},
    hashes::Hash,
  },
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FixtureInput {
  outpoint: OutPoint,
  value: u64,
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

/// Where `inputs`' pieces land by FIFO: tracked outputs, sorted by vout,
/// their pieces sorted by offset and merged.
fn fifo(tx: &ResolvedTx, inputs: &[MoveInput]) -> Vec<MoveOutput> {
  let mut base = Vec::with_capacity(tx.inputs.len());
  let mut total = 0;
  for input in &tx.inputs {
    base.push(total);
    total += input.value;
  }
  let mut ends = Vec::with_capacity(tx.outputs.len());
  let mut starts = Vec::with_capacity(tx.outputs.len());
  let mut end = 0;
  for output in &tx.outputs {
    starts.push(end);
    end += output.value.to_sat();
    ends.push(end);
  }
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

impl Fixture {
  fn load(name: &str) -> Self {
    let path = dir().join(format!("{name}.json"));
    let json = std::fs::read_to_string(&path)
      .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
    serde_json::from_str(&json).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
  }

  fn tx(&self) -> ResolvedTx {
    ResolvedTx {
      txid: self.txid,
      inputs: self
        .inputs
        .iter()
        .map(|input| ResolvedInput {
          previous_output: input.outpoint,
          value: input.value,
          script_hash: 0,
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
      outputs: self
        .outputs
        .iter()
        .map(|output| TxOut {
          value: Amount::from_sat(output.value),
          script_pubkey: output.script_pubkey.clone(),
        })
        .collect(),
    }
  }

  /// The transaction at its height, after a placeholder coinbase, and the
  /// tracker's delta for it.
  fn block_and_delta(&self) -> (ResolvedBlock, BlockDelta) {
    let tx = self.tx();
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
    let m = Move {
      txid: self.txid,
      tx_index: 1,
      coinbase: false,
      input_value: tx.inputs.iter().map(|input| input.value).sum(),
      output_value: tx.outputs.iter().map(|output| output.value.to_sat()).sum(),
      outputs: fifo(&tx, &inputs),
      inputs,
      fee: Vec::new(),
      mined: Vec::new(),
      lost: Vec::new(),
    };
    let coinbase = ResolvedTx {
      txid: Txid::all_zeros(),
      inputs: Vec::new(),
      outputs: Vec::new(),
    };
    let block = ResolvedBlock {
      height: self.height,
      hash: BlockHash::all_zeros(),
      header: header(),
      txs: vec![coinbase, tx],
    };
    (block, delta(self.height, BlockHash::all_zeros(), vec![m]))
  }
}

fn context() -> MarketContext {
  MarketContext::new(
    catalog_for(&TrackerConfig::default()),
    Marketplaces::embedded(),
    Chain::Mainnet,
  )
}

/// The fixtures, by name, and their transactions (docs/SALES-SHAPES.md).
const FIXTURES: &[(&str, &str)] = &[
  // Magisat listing purchases (§3, §7).
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
  // Accepted offers (§4).
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
  // Other venues and lookalikes in the fork's moves window (§6).
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
    "custodial_withdrawal_0x81",
    "671ec9c1a370151fe299e64f73f7392355029e9765fe332901815324373f29a5",
  ),
  (
    "exchange_batch_0x81",
    "cf061ead6b1288ecaab6512ea1b074504844fbe63506d9d410a3a8ad1463e935",
  ),
  (
    "consolidation_0x81",
    "06d243eb5e28a746c93d81fe1d365fb80d9d98b46e4d483702a5cf9664e93104",
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
    "listing_fee_33htobl",
    "4496989b62716ff0df954a29b43fe9ac96a153658801392f5ff25fe2d705aa21",
  ),
  (
    "listing_tx_dev_fill",
    "f1c500f81c331d31415902755278e2a599311d8121ac9582bc63ad981efa48fb",
  ),
  (
    "otc_swap_0x83",
    "ce4dc92baaba478327bbd6b7e182576366f79334c72b40e46f5c54551df7c0cb",
  ),
];

/// A sale as the fixture tests compare it: vin, kind, marketplace, payment
/// vout, buyer vout, price, proceeds, fees.
type Seen = (u32, SaleKind, &'static str, u32, u32, u64, u64, u64);

fn sales_of(name: &str) -> Vec<Seen> {
  let fixture = Fixture::load(name);
  let (block, delta) = fixture.block_and_delta();
  let market = analyze(&block, &delta, &context());
  assert!(market.inscriptions.is_empty(), "{name}");
  market
    .sales
    .iter()
    .map(|sale| {
      assert_eq!(sale.txid, fixture.txid);
      let tracked = fixture
        .tracked
        .iter()
        .find(|tracked| tracked.vin == sale.vin)
        .unwrap();
      assert_eq!(sale.seller_outpoint, tracked.outpoint);
      assert_eq!(sale.seller.pieces, tracked.pieces);
      assert_eq!(
        sale.buyer_script.as_ref(),
        sale
          .buyer_vout
          .map(|vout| &fixture.outputs[vout as usize].script_pubkey)
      );
      let marketplace = match sale.marketplace.as_str() {
        "magisat" => "magisat",
        UNKNOWN_MARKETPLACE => UNKNOWN_MARKETPLACE,
        other => panic!("{name}: marketplace {other}"),
      };
      (
        sale.vin,
        sale.kind,
        marketplace,
        sale.payment_vout.unwrap(),
        sale.buyer_vout.unwrap(),
        sale.price,
        sale.seller_proceeds,
        sale.fees,
      )
    })
    .collect()
}

/// The fixture's transaction pays a Magisat fee address valid at its
/// height.
fn pays_magisat(name: &str) -> bool {
  let fixture = Fixture::load(name);
  let marketplaces = Marketplaces::embedded();
  fixture.outputs.iter().any(|output| {
    marketplaces
      .fee_output(&output.script_pubkey, fixture.height)
      .is_some_and(|marketplace| marketplace.id == "magisat")
  })
}

#[test]
fn magisat_listings() {
  // 2023-10, F1: the price is the payment (no maker fee before 861,287);
  // the 875-sat fee output is the buyer's taker fee, not part of the price.
  assert_eq!(
    sales_of("magisat_single_2023"),
    [(2, Listing, "magisat", 2, 1, 35_000, 35_000, 0)]
  );
  // 2023-09: 2.5 % of 15,000 is below dust, so no fee output.
  assert!(!pays_magisat("magisat_no_fee_output_2023"));
  assert_eq!(
    sales_of("magisat_no_fee_output_2023"),
    [(2, Listing, UNKNOWN_MARKETPLACE, 2, 1, 15_000, 15_000, 0)]
  );
  // 2024-03, the F2-only period.
  assert_eq!(
    sales_of("magisat_f2_period_2024"),
    [(2, Listing, "magisat", 2, 1, 30_000, 30_000, 0)]
  );
  // The 2024 variant layout: one padding input, sellers at vin 1..=5, every
  // sold sat lands in output 0.
  assert_eq!(
    sales_of("magisat_bulk_variant_2024"),
    (1..=5)
      .map(|vin| (vin, Listing, "magisat", vin, 0, 122_000, 122_000, 0))
      .collect::<Vec<Seen>>()
  );
  // Block 861,287 has the first maker fees: ⌊89,550 / 199⌋ = 450.
  assert_eq!(
    sales_of("magisat_first_maker_fee"),
    [
      (3, Listing, "magisat", 3, 1, 90_000, 89_550, 450),
      (4, Listing, "magisat", 4, 2, 90_000, 89_550, 450),
    ]
  );
  // 2024-10, F2 12,950 = 2,450 maker + 10,500 taker.
  assert_eq!(
    sales_of("magisat_maker_fee_f2"),
    [(2, Listing, "magisat", 2, 1, 490_000, 487_550, 2_450)]
  );
  // 2025, F1 2,957 = 157 maker + 2,800 taker.
  assert_eq!(
    sales_of("magisat_f1_2025"),
    [(2, Listing, "magisat", 2, 1, 31_500, 31_343, 157)]
  );
  // A 0.1 BTC rare sat: F2 150,007 = 50,002 maker + 100,005 taker.
  assert_eq!(
    sales_of("magisat_0_1_btc"),
    [(2, Listing, "magisat", 2, 1, 10_000_546, 9_950_544, 50_002)]
  );
  // Two alpha uncommons in one buy, as the fork recorded the move: one fee
  // output of 3,254 = 2 × 227 maker + 2,800 taker.
  assert_eq!(
    sales_of("magisat_bulk_2"),
    [
      (3, Listing, "magisat", 3, 1, 45_546, 45_319, 227),
      (4, Listing, "magisat", 4, 2, 45_546, 45_319, 227),
    ]
  );
}

#[test]
fn magisat_buying_with_its_own_wallet() {
  // F1 buys 50 block 9 sats from one seller. Its 103 outputs (the bought
  // sats, padding, change) receive tracked sats, so they are not fees; the
  // fee goes to F2 at vout 101 (2k + 1). Prices as Magisat's records: 30 ×
  // 1,146, 13 × 930 and 7 × 1,046.
  let sales = sales_of("magisat_buys_with_f1");
  assert_eq!(sales.len(), 50);
  let mut prices = BTreeMap::new();
  for (vin, kind, marketplace, payment, buyer, price, proceeds, fees) in sales {
    assert_eq!(
      (kind, marketplace, payment, buyer),
      (Listing, "magisat", vin, vin - 50)
    );
    assert_eq!((price, fees), (proceeds + proceeds / 199, proceeds / 199));
    *prices.entry(price).or_insert(0) += 1;
  }
  assert_eq!(prices, BTreeMap::from([(930, 13), (1_046, 7), (1_146, 30)]));
}

#[test]
fn magisat_offers() {
  // 2025, 1 % + 1 %: 3,960,000 to the seller, 80,000 fee.
  assert_eq!(
    sales_of("magisat_offer_2025"),
    [(0, Offer, "magisat", 1, 0, 4_000_000, 3_960_000, 40_000)]
  );
  // 2024, 15,000 minimums on each side.
  assert_eq!(
    sales_of("magisat_offer_2024_minimums"),
    [(0, Offer, "magisat", 1, 0, 1_000_000, 985_000, 15_000)]
  );
  // In none of Magisat's feeds (a collection offer?), from the fork's move.
  assert_eq!(
    sales_of("magisat_offer_not_in_api"),
    [(0, Offer, "magisat", 1, 0, 5_200_000, 5_148_000, 52_000)]
  );
  // Known deviation: in 2024-03 offers paid a 2.5 % maker fee and no taker
  // fee, so Magisat's price is 2,500,000; half the fee output is added.
  assert_eq!(
    sales_of("magisat_offer_2024_03_no_taker_fee"),
    [(0, Offer, "magisat", 1, 0, 2_531_250, 2_500_000, 31_250)]
  );
  // Known miss: zero-fee offers (since 2025-11) have no fee output and look
  // like a send with change to the sender.
  assert!(!pays_magisat("magisat_offer_zero_fee"));
  assert_eq!(sales_of("magisat_offer_zero_fee"), []);
}

#[test]
fn other_venues() {
  // A two-party ALL|ANYONECANPAY sale: a 999-sat UTXO holding a 450x sat for
  // 1,970,999 paid to the seller's own script.
  let fixture = Fixture::load("psbt_0x81_sale");
  assert_eq!(
    fixture
      .inputs
      .iter()
      .map(|input| input.sighash)
      .collect::<Vec<_>>(),
    [Some(PSBT_SIGHASH), Some(0x00)]
  );
  assert_eq!(
    sales_of("psbt_0x81_sale"),
    [(
      0,
      Listing,
      UNKNOWN_MARKETPLACE,
      1,
      0,
      1_970_999,
      1_970_999,
      0
    )]
  );
  let (block, delta) = fixture.block_and_delta();
  let ctx = context();
  let sale = &analyze(&block, &delta, &ctx).sales[0];
  let (row, count, unit) = sale_unit(sale, &ctx.catalog).unwrap();
  assert_eq!(ctx.catalog.rows()[usize::from(row)].id, "450x");
  assert_eq!((count, unit), (1, 1_970_000));
  // Most such sellers are paid at a separate address: the output after the
  // sats (60 of the window's 84).
  assert_eq!(
    sales_of("psbt_0x81_separate_payment"),
    [(
      0,
      Listing,
      UNKNOWN_MARKETPLACE,
      1,
      0,
      1_973_999,
      1_973_999,
      0
    )]
  );
  // With a 1,000-sat fee to a service whose address is not in the table.
  assert_eq!(
    sales_of("psbt_0x81_service_fee"),
    [(0, Listing, UNKNOWN_MARKETPLACE, 1, 0, 7_217, 7_217, 0)]
  );
  // SINGLE|ANYONECANPAY listings on other venues: their fee addresses are not
  // in the table, so the price is the payment.
  assert_eq!(
    sales_of("listing_fee_33htobl"),
    [(1, Listing, UNKNOWN_MARKETPLACE, 1, 0, 3_000, 3_000, 0)]
  );
  assert_eq!(
    sales_of("listing_tx_dev_fill"),
    [(2, Listing, UNKNOWN_MARKETPLACE, 2, 1, 67_093, 67_093, 0)]
  );
  assert_eq!(
    sales_of("otc_swap_0x83"),
    [(
      1,
      Listing,
      UNKNOWN_MARKETPLACE,
      1,
      0,
      5_980_000,
      5_980_000,
      0
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
    assert!(pays_magisat(name), "{name}");
    assert_eq!(sales_of(name), [], "{name}");
  }
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
    // The moves window moves on: keep fixtures built from it.
    if !rebuild && dir().join(format!("{name}.json")).exists() {
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
    let recorded = (height >= moves_from).then(|| {
      fork
        .moves(height)
        .into_iter()
        .find(|m| m.txid == txid)
        .unwrap_or_else(|| panic!("{name}: the fork has no move for {txid}"))
    });
    let tracked: Vec<FixtureTracked> = match &recorded {
      Some(m) => m
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
        .collect(),
      None => {
        let mut tracked: Vec<FixtureTracked> = magisat
          .get(&txid.to_string())
          .unwrap_or_else(|| panic!("{name}: no Magisat record"))
          .iter()
          .map(|&(outpoint, start, count, offset)| {
            let vin = tx
              .inputs
              .iter()
              .position(|input| input.previous_output == outpoint)
              .unwrap();
            let prevout = &raw["vin"][vin]["prevout"];
            FixtureTracked {
              vin: vin as u32,
              outpoint,
              value: tx.inputs[vin].value,
              script_pubkey: ScriptBuf::from_hex(prevout["scriptPubKey"]["hex"].as_str().unwrap())
                .unwrap(),
              height: prevout["height"].as_u64().unwrap() as u32,
              pieces: vec![Piece {
                offset,
                start,
                len: count.min(tx.inputs[vin].value - offset),
              }],
            }
          })
          .collect();
        tracked.sort_by_key(|tracked| tracked.vin);
        tracked
      }
    };
    let fixture = Fixture {
      name: name.into(),
      txid,
      height,
      tx_index: tx_index as u32,
      pieces_from: if recorded.is_some() {
        "fork"
      } else {
        "magisat"
      }
      .into(),
      tracked,
      inputs: tx
        .inputs
        .iter()
        .map(|input| FixtureInput {
          outpoint: input.previous_output,
          value: input.value,
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
    };
    let (_, built) = fixture.block_and_delta();
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
    let market = analyze(
      &block,
      &delta(height, hash, recorded.into_iter().collect()),
      &context(),
    );
    eprintln!(
      "{name}: height {height}, {} in, {} out, tracked vins {:?} ({}), sales in the block {:?}",
      fixture.inputs.len(),
      fixture.outputs.len(),
      fixture
        .tracked
        .iter()
        .map(|tracked| tracked.vin)
        .collect::<Vec<_>>(),
      fixture.pieces_from,
      market.sales
    );
  }
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
    let response = self
      .client
      .get(format!("{}{path}", self.url))
      .send()
      .unwrap();
    assert!(
      response.status().is_success(),
      "{path}: {}",
      response.status()
    );
    response.json().unwrap()
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
/// to `MEASURE_OUT` and prints the tallies.
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
  for block in live.blocks(from, to, threads) {
    let block = block.unwrap();
    let moves = fork.moves(block.height);
    let delta = delta(block.height, block.hash, moves);
    let start = std::time::Instant::now();
    let market = analyze(&block, &delta, &ctx);
    analyze_time += start.elapsed();
    count("blocks".into());
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
      let tracked: Vec<Option<u8>> = m
        .inputs
        .iter()
        .map(|input| flags[input.vin as usize])
        .collect();
      let class = if tracked.contains(&Some(0x83)) {
        "0x83"
      } else if tracked.contains(&Some(0x81)) {
        if flags.iter().all(|flag| *flag == Some(0x81)) {
          "0x81 every input"
        } else {
          "0x81 mixed"
        }
      } else if tracked.contains(&Some(0x82)) {
        "0x82"
      } else {
        "other"
      };
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
      count(format!(
        "txs {class}: {}",
        if sales.is_empty() { "no sale" } else { "sale" }
      ));
      serde_json::to_writer(
        &mut out,
        &serde_json::json!({
          "height": block.height,
          "txid": m.txid,
          "class": class,
          "flags": flags,
          "tracked_vins": m.inputs.iter().map(|input| input.vin).collect::<Vec<_>>(),
          "sales": sales,
        }),
      )
      .unwrap();
      writeln!(out).unwrap();
    }
  }
  out.flush().unwrap();
  eprintln!(
    "blocks {from}..={to} in {:.0?}, analyze {:.1?} in total",
    started.elapsed(),
    analyze_time
  );
  for (key, n) in tally {
    eprintln!("{key}: {n}");
  }
}
