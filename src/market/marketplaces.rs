//! Marketplaces, their fee addresses and markers (`data/marketplaces.json`).
//!
//! ```json
//! { "marketplaces": [{
//!     "id": "magisat",
//!     "name": "Magisat",
//!     "role": "marketplace",
//!     "price_rule": "magisat",
//!     "flows": ["listing_0x83_msigner", "offer_sighash_all"],
//!     "fee_addresses": [
//!       { "address": "bc1p…", "from_height": 793428, "to_height": 831296 },
//!       { "script_pubkey": "0014…" }
//!     ],
//!     "markers": [{ "op_return": "6a07…", "from_height": 855890 }] }] }
//! ```
//!
//! A bare array of marketplaces is accepted too.
//!
//! * `id`: the `Sale::marketplace` value (docs/API.md), unless `anonymous`.
//! * `role`: `marketplace` (the default) and `buying_tool` (fills other
//!   marketplaces' listings) attribute sales; transactions paying a
//!   `lending` or `swap` address are not sales (unless they also pay a
//!   marketplace: a financed purchase); `unidentified` entries document
//!   fee-like clusters and are never loaded.
//! * `fee_addresses`: outputs paying one of these, between `from_height`
//!   and `to_height` (inclusive, either open), are the entry's fees.
//!   `address` is a mainnet address; `script_pubkey` (hex) also works on
//!   any chain. `buying_wallet: true` marks a fee address the marketplace
//!   also buys with: a listing it buys with no fee output is its own sale.
//! * `tool_addresses`: the marketplace's service addresses (same shape),
//!   paid by its transfer and send tools: an output to one is never a
//!   payment.
//! * `markers`: `op_return` (the output script, hex) outputs that mark the
//!   marketplace's transactions, between the same kind of heights. A listing
//!   in a transaction with a marker and no fee output is the marketplace's
//!   sale.
//! * `price_rule`: how a listing's price is derived (`PriceRule`):
//!   `proceeds_plus_fees` (the default), `proceeds` (the payment alone: the
//!   fee is the buyer's, on top of the price, and transfers pay it too, so
//!   it alone never marks an offer) or `magisat` (its maker-fee rule; it
//!   also enables the Magisat offer shape).
//! * `anonymous`: a venue known only by its fee address: its sales are found
//!   and its fees counted, but they are reported as `"unknown"`.
//! * `flows`: the transaction shapes the marketplace uses; three change the
//!   detection: `secure_purchase_two_tx` (a seller's ALL|ANYONECANPAY sale
//!   into an escrow output, settled by a second transaction carrying the
//!   marketplace's marker or fee), `passthrough_2of2` (the seller's item
//!   is spent from an escrow shared with the marketplace and paid at the
//!   same index: a listing, not an offer) and, for a `proceeds`
//!   marketplace, `offer_sighash_all` (its accepted offers in Magisat's
//!   layout, recognized only with both its fee at output 2 and its marker
//!   last: `sales::marked_offer`).
//!
//! Unknown fields (evidence, notes, statistics) are ignored, so the table
//! can grow.

use {
  super::sales::UNKNOWN_MARKETPLACE,
  crate::model::script_hash,
  anyhow::{Context, ensure},
  bitcoin::{Address, Network, Script, ScriptBuf},
  serde::Deserialize,
  std::{collections::HashMap, str::FromStr},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marketplace {
  pub id: String,
  pub name: String,
  pub role: Role,
  pub price_rule: PriceRule,
  pub anonymous: bool,
  pub flows: Vec<String>,
}

impl Marketplace {
  /// The `Sale::marketplace` value of its sales.
  pub fn reported_id(&self) -> &str {
    if self.anonymous {
      UNKNOWN_MARKETPLACE
    } else {
      &self.id
    }
  }

  /// Whether a sale can be attributed to it.
  pub fn sells(&self) -> bool {
    matches!(self.role, Role::Marketplace | Role::BuyingTool)
  }

  pub fn has_flow(&self, flow: &str) -> bool {
    self.flows.iter().any(|f| f == flow)
  }
}

/// What a table entry is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
  #[default]
  Marketplace,
  /// Fills other marketplaces' listings, with its own fee.
  BuyingTool,
  /// Loans against sats: its transactions are not sales.
  Lending,
  /// Swaps: not sales.
  Swap,
  /// Fee-like clusters of no confirmed owner: documentation only.
  Unidentified,
}

/// How a marketplace's listing sales are priced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceRule {
  /// Seller proceeds plus the transaction's fee outputs, split over its
  /// sales in proportion to their proceeds.
  #[default]
  ProceedsPlusFees,
  /// The paired payment `P` alone: the fee output is the buyer's taker fee,
  /// on top of the listing price, and the seller pays none. Transfers pay
  /// the fee too, so it alone never marks an offer: with the
  /// `offer_sighash_all` flow, an offer in the marketplace's exact layout,
  /// also carrying its marker, is priced at its payment (output 1) alone.
  Proceeds,
  /// Magisat: the paired payment `P`, plus the 0.5 % maker fee `⌊P/199⌋`
  /// from block 861,287; the buyer's taker fee in the fee output is not
  /// part of the price.
  Magisat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Range {
  marketplace: usize,
  from_height: u32,
  to_height: u32,
}

impl Range {
  fn contains(&self, height: u32) -> bool {
    self.from_height <= height && height <= self.to_height
  }
}

/// Marketplace fee addresses and markers by height range.
#[derive(Debug, Clone, Default)]
pub struct Marketplaces {
  marketplaces: Vec<Marketplace>,
  fees: HashMap<ScriptBuf, Vec<Range>>,
  /// `fees` by `script_hash`, to recognize inputs spending from them.
  fee_hashes: HashMap<u32, Vec<Range>>,
  /// Fee addresses the marketplace buys with.
  buying_wallets: HashMap<ScriptBuf, Vec<Range>>,
  tools: HashMap<ScriptBuf, Vec<Range>>,
  markers: HashMap<ScriptBuf, Vec<Range>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FileJson {
  Object {
    #[serde(default)]
    marketplaces: Vec<MarketplaceJson>,
  },
  Array(Vec<MarketplaceJson>),
}

#[derive(Deserialize)]
struct MarketplaceJson {
  id: String,
  #[serde(default)]
  name: Option<String>,
  #[serde(default)]
  role: Role,
  #[serde(default)]
  price_rule: PriceRule,
  #[serde(default)]
  anonymous: bool,
  #[serde(default)]
  flows: Vec<String>,
  #[serde(default)]
  fee_addresses: Vec<FeeAddressJson>,
  #[serde(default)]
  tool_addresses: Vec<FeeAddressJson>,
  #[serde(default)]
  markers: Vec<MarkerJson>,
}

#[derive(Deserialize)]
struct FeeAddressJson {
  #[serde(default)]
  address: Option<String>,
  #[serde(default)]
  script_pubkey: Option<String>,
  #[serde(default)]
  from_height: Option<u32>,
  #[serde(default)]
  to_height: Option<u32>,
  #[serde(default)]
  buying_wallet: bool,
}

impl FeeAddressJson {
  fn script(&self, id: &str) -> anyhow::Result<ScriptBuf> {
    Ok(match (&self.address, &self.script_pubkey) {
      (Some(address), None) => Address::from_str(address)
        .with_context(|| format!("{id}: address {address}"))?
        .require_network(Network::Bitcoin)
        .with_context(|| format!("{id}: address {address}"))?
        .script_pubkey(),
      (None, Some(hex)) => {
        ScriptBuf::from_hex(hex).with_context(|| format!("{id}: script {hex}"))?
      }
      _ => anyhow::bail!("{id}: each address needs exactly one of `address` and `script_pubkey`"),
    })
  }
}

#[derive(Deserialize)]
struct MarkerJson {
  op_return: String,
  #[serde(default)]
  from_height: Option<u32>,
  #[serde(default)]
  to_height: Option<u32>,
}

/// `data/marketplaces.json`, when the build has one.
const EMBEDDED: &str = include_str!("../../data/marketplaces.json");

fn range(
  id: &str,
  marketplace: usize,
  from_height: Option<u32>,
  to_height: Option<u32>,
) -> anyhow::Result<Range> {
  let range = Range {
    marketplace,
    from_height: from_height.unwrap_or(0),
    to_height: to_height.unwrap_or(u32::MAX),
  };
  ensure!(
    range.from_height <= range.to_height,
    "{id}: empty height range"
  );
  Ok(range)
}

impl Marketplaces {
  /// The table embedded at build time (`data/marketplaces.json`); empty if
  /// that file holds none (or does not parse, which a test rules out).
  pub fn embedded() -> Self {
    Self::from_json(EMBEDDED).unwrap_or_default()
  }

  pub fn from_json(json: &str) -> anyhow::Result<Self> {
    let file: FileJson = serde_json::from_str(json).context("marketplaces.json")?;
    let entries = match file {
      FileJson::Object { marketplaces } | FileJson::Array(marketplaces) => marketplaces,
    };
    let mut table = Self::default();
    for marketplace in entries {
      let id = marketplace.id;
      ensure!(!id.is_empty(), "marketplace without an id");
      ensure!(
        table.marketplaces.iter().all(|m| m.id != id),
        "duplicate marketplace `{id}`"
      );
      if marketplace.role == Role::Unidentified {
        continue;
      }
      let index = table.marketplaces.len();
      for fee in marketplace.fee_addresses {
        let script = fee.script(&id)?;
        let range = range(&id, index, fee.from_height, fee.to_height)?;
        table
          .fee_hashes
          .entry(script_hash(&script))
          .or_default()
          .push(range);
        if fee.buying_wallet {
          table
            .buying_wallets
            .entry(script.clone())
            .or_default()
            .push(range);
        }
        table.fees.entry(script).or_default().push(range);
      }
      for tool in marketplace.tool_addresses {
        let script = tool.script(&id)?;
        let range = range(&id, index, tool.from_height, tool.to_height)?;
        table.tools.entry(script).or_default().push(range);
      }
      for marker in marketplace.markers {
        let script = ScriptBuf::from_hex(&marker.op_return)
          .with_context(|| format!("{id}: marker {}", marker.op_return))?;
        ensure!(
          script.is_op_return(),
          "{id}: marker {} is not an OP_RETURN script",
          marker.op_return
        );
        let range = range(&id, index, marker.from_height, marker.to_height)?;
        table.markers.entry(script).or_default().push(range);
      }
      table.marketplaces.push(Marketplace {
        name: marketplace.name.unwrap_or_else(|| id.clone()),
        id,
        role: marketplace.role,
        price_rule: marketplace.price_rule,
        anonymous: marketplace.anonymous,
        flows: marketplace.flows,
      });
    }
    Ok(table)
  }

  fn find<'a>(
    &'a self,
    map: &'a HashMap<ScriptBuf, Vec<Range>>,
    script: &Script,
    height: u32,
  ) -> Option<&'a Marketplace> {
    map
      .get(script)?
      .iter()
      .find(|range| range.contains(height))
      .map(|range| &self.marketplaces[range.marketplace])
  }

  /// The entry (of any role) whose fee address an output paying `script` at
  /// `height` pays.
  pub fn fee_output(&self, script: &Script, height: u32) -> Option<&Marketplace> {
    self.find(&self.fees, script, height)
  }

  /// The marketplace an output with `script` at `height` marks.
  pub fn marker(&self, script: &Script, height: u32) -> Option<&Marketplace> {
    self.find(&self.markers, script, height)
  }

  /// The marketplace whose tool address `script` is at `height`.
  pub fn tool_output(&self, script: &Script, height: u32) -> Option<&Marketplace> {
    self.find(&self.tools, script, height)
  }

  /// The marketplace that buys with the fee address `script` at `height`.
  pub fn buying_wallet(&self, script: &Script, height: u32) -> Option<&Marketplace> {
    self.find(&self.buying_wallets, script, height)
  }

  /// The entry (of any role) one of whose fee addresses has `script_hash`
  /// at `height`: an input with that script hash spends from it (hashes can
  /// collide, so this is a hint, not proof).
  pub fn fee_hash(&self, script_hash: u32, height: u32) -> Option<&Marketplace> {
    self
      .fee_hashes
      .get(&script_hash)?
      .iter()
      .find(|range| range.contains(height))
      .map(|range| &self.marketplaces[range.marketplace])
  }

  pub fn by_id(&self, id: &str) -> Option<&Marketplace> {
    self.marketplaces.iter().find(|m| m.id == id)
  }

  pub fn marketplaces(&self) -> &[Marketplace] {
    &self.marketplaces
  }

  pub fn is_empty(&self) -> bool {
    self.marketplaces.is_empty()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn script(address: &str) -> ScriptBuf {
    Address::from_str(address)
      .unwrap()
      .require_network(Network::Bitcoin)
      .unwrap()
      .script_pubkey()
  }

  #[test]
  fn embedded_table_parses() {
    let table = Marketplaces::from_json(EMBEDDED).unwrap();
    let id = |address: &str, height| {
      table
        .fee_output(&script(address), height)
        .map(|m| m.id.as_str())
    };
    // Magisat's two fee addresses, by the heights they were used at.
    let f1 = "bc1pan37k3yyvpnazh4zsz284hsyvsrucxzjgs5td8h22regpfzhgu9sg4xlc6";
    let f2 = "3Ke21osfhEbEryUeqdwAuAY8VKxm5B9uB2";
    assert_eq!(id(f1, 793_427), None);
    assert_eq!(id(f1, 804_700), Some("magisat"));
    assert_eq!(id(f1, 831_279), Some("magisat"));
    assert_eq!(id(f1, 840_000), None);
    assert_eq!(id(f1, 852_779), Some("magisat"));
    assert_eq!(id(f2, 831_279), None);
    assert_eq!(id(f2, 831_297), Some("magisat"));
    assert_eq!(id(f2, 969_000), Some("magisat"));
    assert_eq!(
      table.fee_output(&script(f1), 900_000).unwrap().price_rule,
      PriceRule::Magisat
    );
    // Magic Eden's three, closed in 2026; the second is an exchange deposit
    // address after May 2024.
    for (address, from, to) in [
      (
        "bc1qcq2uv5nk6hec6kvag3wyevp6574qmsm9scjxc2",
        781_237,
        940_150,
      ),
      ("3P4WqXDbSLRhzo2H6MT6YFbvBKBDPLbVtQ", 820_193, 843_661),
      (
        "bc1qp8j9sx6609h7llqufurxjgrwsqwt020tqzn0gs",
        855_139,
        940_150,
      ),
    ] {
      assert_eq!(id(address, from - 1), None);
      assert_eq!(id(address, from), Some("magic_eden"));
      assert_eq!(id(address, to), Some("magic_eden"));
      assert_eq!(id(address, to + 1), None);
    }
    // Every role, and the venue reported as unknown.
    let entry = |address: &str| table.fee_output(&script(address), 969_000).unwrap();
    assert_eq!(
      entry("bc1qxma2dwmutht4vxyl6u395slew5ectfpn35ug9l").role,
      Role::BuyingTool
    );
    assert_eq!(
      entry("bc1qhwdela6q8kyg2l664663jdw92hgkejcxh8raxzdvl8pmtcwwk4vq4swknz").role,
      Role::Lending
    );
    assert_eq!(
      entry("bc1px06vxyj5xy6z3unfj20fhusr0j2yndup585v7zl302d0a9qqv39qf7fuck").role,
      Role::Swap
    );
    let anonymous = entry("33HToBLb5xGhkoHbYCR7KZP7hy57ZnrCJW");
    assert!(anonymous.anonymous && anonymous.sells());
    assert_eq!(anonymous.reported_id(), UNKNOWN_MARKETPLACE);
    // Unidentified clusters are not loaded.
    assert_eq!(
      table.fee_output(
        &script("bc1pndths0lnsvem2a0n2c6t49u462xam0n4krjl6kpfwn0nknc0cd7qmnxhf8"),
        900_000
      ),
      None
    );
    assert!(table.marketplaces().iter().all(|m| m.id != "unidentified"));
    // Satflow's OP_RETURN marker.
    let marker = ScriptBuf::from_hex("6a07534154464c4f57").unwrap();
    assert_eq!(table.marker(&marker, 855_889), None);
    let satflow = table.marker(&marker, 969_000).unwrap();
    assert_eq!(satflow.id, "satflow");
    assert!(satflow.has_flow("secure_purchase_two_tx"));
    assert!(
      table
        .fee_output(
          &script("bc1pgkfga880836f5kp3m9vvya4m0whva80ddm58r7fyltzp9q8t08rs0rdnet"),
          969_000
        )
        .unwrap()
        .has_flow("passthrough_2of2")
    );
    // raresats.net's fee address and OP_RETURN "raresats.net" marker, from
    // its launch at block 969,480; its price is the payment alone.
    let fee = "bc1p4hefchfnn06cn2xmk6fq6xcw5ew260q2dptc4c35fcazgedmzlcqkxf057";
    let marker = ScriptBuf::from_hex("6a0c72617265736174732e6e6574").unwrap();
    assert_eq!(&marker.as_bytes()[2..], b"raresats.net");
    assert_eq!(id(fee, 969_479), None);
    assert_eq!(table.marker(&marker, 969_479), None);
    for height in [969_480, 1_000_000] {
      assert_eq!(id(fee, height), Some("raresats"));
      assert_eq!(
        table.marker(&marker, height).map(|m| m.id.as_str()),
        Some("raresats")
      );
    }
    let raresats = table.by_id("raresats").unwrap();
    assert_eq!(
      (
        raresats.name.as_str(),
        raresats.role,
        raresats.price_rule,
        raresats.reported_id()
      ),
      (
        "raresats.net",
        Role::Marketplace,
        PriceRule::Proceeds,
        "raresats"
      )
    );
    assert!(raresats.has_flow("listing_0x83_msigner"));
    assert!(raresats.has_flow("offer_sighash_all"));
    assert_eq!(table.buying_wallet(&script(fee), 969_480), None);
  }

  fn address(network: Network) -> (String, ScriptBuf) {
    let script = ScriptBuf::from_hex("0014c0b5c6527efb63db5e74563ee2fae1b56a3b1f5d").unwrap();
    (
      Address::from_script(&script, network).unwrap().to_string(),
      script,
    )
  }

  #[test]
  fn fee_addresses_by_height() {
    let (address, me) = address(Network::Bitcoin);
    let table = Marketplaces::from_json(&format!(
      r#"{{ "marketplaces": [
        {{ "id": "magic_eden", "name": "Magic Eden",
          "fee_addresses": [
            {{ "address": "{address}", "from_height": 100, "to_height": 200 }},
            {{ "script_pubkey": "51", "from_height": 300 }} ] }},
        {{ "id": "magisat", "fee_addresses": [ {{ "script_pubkey": "52" }} ], "extra": 1 }},
        {{ "id": "loans", "role": "lending", "fee_addresses": [ {{ "script_pubkey": "53" }} ] }},
        {{ "id": "clusters", "role": "unidentified",
          "fee_addresses": [ {{ "script_pubkey": "54" }} ] }} ] }}"#
    ))
    .unwrap();
    assert_eq!(table.fee_output(&me, 99), None);
    assert_eq!(table.fee_output(&me, 100).unwrap().id, "magic_eden");
    assert_eq!(table.fee_output(&me, 201), None);
    let op_true = ScriptBuf::from_bytes(vec![0x51]);
    assert_eq!(table.fee_output(&op_true, 299), None);
    assert_eq!(
      table.fee_output(&op_true, u32::MAX).unwrap().id,
      "magic_eden"
    );
    let magisat = table
      .fee_output(&ScriptBuf::from_bytes(vec![0x52]), 0)
      .unwrap();
    assert_eq!(
      (magisat.id.as_str(), magisat.name.as_str()),
      ("magisat", "magisat")
    );
    assert_eq!(
      (magisat.role, magisat.anonymous, magisat.sells()),
      (Role::Marketplace, false, true)
    );
    let loans = table
      .fee_output(&ScriptBuf::from_bytes(vec![0x53]), 0)
      .unwrap();
    assert!(!loans.sells());
    assert_eq!(
      table.fee_output(&ScriptBuf::from_bytes(vec![0x54]), 0),
      None
    );
    assert_eq!(table.marketplaces().len(), 3);
    assert_eq!(
      table
        .fee_hash(script_hash(&ScriptBuf::from_bytes(vec![0x53])), 7)
        .unwrap()
        .id,
      "loans"
    );
    assert_eq!(table.fee_hash(script_hash(&me), 201), None);
    assert_eq!(table.by_id("magisat").unwrap().id, "magisat");
    assert_eq!(table.by_id("clusters"), None);

    // A bare array, with a marker.
    let table = Marketplaces::from_json(
      r#"[{ "id": "m", "markers": [{ "op_return": "6a0101", "to_height": 10 }] }]"#,
    )
    .unwrap();
    let marker = ScriptBuf::from_hex("6a0101").unwrap();
    assert_eq!(table.marker(&marker, 10).unwrap().id, "m");
    assert_eq!(table.marker(&marker, 11), None);
  }

  #[test]
  fn bad_tables_are_rejected() {
    let (testnet, _) = address(Network::Testnet);
    for json in [
      r#"{ "marketplaces": [{ "id": "" }] }"#.to_string(),
      r#"{ "marketplaces": [{ "id": "a" }, { "id": "a" }] }"#.into(),
      r#"{ "marketplaces": [{ "id": "a", "fee_addresses": [{}] }] }"#.into(),
      r#"{ "marketplaces": [{ "id": "a", "fee_addresses": [{ "script_pubkey": "zz" }] }] }"#.into(),
      format!(r#"{{ "marketplaces": [{{ "id": "a", "fee_addresses": [{{ "address": "{testnet}" }}] }}] }}"#),
      r#"{ "marketplaces": [{ "id": "a", "fee_addresses": [{ "script_pubkey": "51", "from_height": 5, "to_height": 4 }] }] }"#.into(),
      r#"{ "marketplaces": [{ "id": "a", "markers": [{ "op_return": "51" }] }] }"#.into(),
      r#"{ "marketplaces": [{ "id": "a", "role": "exchange" }] }"#.into(),
      "not json".into(),
    ] {
      assert!(Marketplaces::from_json(&json).is_err(), "{json}");
    }
    assert!(Marketplaces::from_json("{}").unwrap().is_empty());
    assert!(Marketplaces::from_json("[]").unwrap().is_empty());
  }
}
