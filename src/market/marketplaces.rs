//! Marketplaces and their fee addresses (`data/marketplaces.json`).
//!
//! ```json
//! { "marketplaces": [{
//!     "id": "magic_eden",
//!     "name": "Magic Eden",
//!     "fee_only_sales": true,
//!     "fee_addresses": [
//!       { "address": "bc1q…", "from_height": 767430, "to_height": 840000 },
//!       { "script_pubkey": "0014…" }
//!     ] }] }
//! ```
//!
//! * `id`: the `Sale::marketplace` value (docs/API.md).
//! * `fee_addresses`: outputs paying one of these, between `from_height`
//!   and `to_height` (inclusive, either open), are the marketplace's fees.
//!   `address` is a mainnet address; `script_pubkey` (hex) also works on
//!   any chain.
//! * `fee_only_sales`: a transaction paying the fee address is a sale of the
//!   tracked sats it moves even without SINGLE|ANYONECANPAY listings (legacy
//!   Magic Eden).
//!
//! Unknown fields are ignored, so the table can grow.

use {
  anyhow::{Context, ensure},
  bitcoin::{Address, Network, Script, ScriptBuf},
  serde::Deserialize,
  std::{collections::HashMap, str::FromStr},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marketplace {
  pub id: String,
  pub name: String,
  pub fee_only_sales: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FeeAddress {
  marketplace: usize,
  from_height: u32,
  to_height: u32,
}

/// Marketplace fee addresses by height range.
#[derive(Debug, Clone, Default)]
pub struct Marketplaces {
  marketplaces: Vec<Marketplace>,
  by_script: HashMap<ScriptBuf, Vec<FeeAddress>>,
}

#[derive(Deserialize)]
struct FileJson {
  #[serde(default)]
  marketplaces: Vec<MarketplaceJson>,
}

#[derive(Deserialize)]
struct MarketplaceJson {
  id: String,
  #[serde(default)]
  name: Option<String>,
  #[serde(default)]
  fee_only_sales: bool,
  #[serde(default)]
  fee_addresses: Vec<FeeAddressJson>,
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
}

/// `data/marketplaces.json`, when the build has one.
const EMBEDDED: &str = include_str!("../../data/marketplaces.json");

impl Marketplaces {
  /// The table embedded at build time (`data/marketplaces.json`); empty if
  /// that file holds none (or does not parse, which a test rules out).
  pub fn embedded() -> Self {
    Self::from_json(EMBEDDED).unwrap_or_default()
  }

  pub fn from_json(json: &str) -> anyhow::Result<Self> {
    let file: FileJson = serde_json::from_str(json).context("marketplaces.json")?;
    let mut table = Self::default();
    for marketplace in file.marketplaces {
      ensure!(!marketplace.id.is_empty(), "marketplace without an id");
      ensure!(
        table.marketplaces.iter().all(|m| m.id != marketplace.id),
        "duplicate marketplace `{}`",
        marketplace.id
      );
      let index = table.marketplaces.len();
      for fee in marketplace.fee_addresses {
        let script = match (&fee.address, &fee.script_pubkey) {
          (Some(address), None) => Address::from_str(address)
            .with_context(|| format!("{}: fee address {address}", marketplace.id))?
            .require_network(Network::Bitcoin)
            .with_context(|| format!("{}: fee address {address}", marketplace.id))?
            .script_pubkey(),
          (None, Some(hex)) => ScriptBuf::from_hex(hex)
            .with_context(|| format!("{}: fee script {hex}", marketplace.id))?,
          _ => anyhow::bail!(
            "{}: each fee address needs exactly one of `address` and `script_pubkey`",
            marketplace.id
          ),
        };
        let range = FeeAddress {
          marketplace: index,
          from_height: fee.from_height.unwrap_or(0),
          to_height: fee.to_height.unwrap_or(u32::MAX),
        };
        ensure!(
          range.from_height <= range.to_height,
          "{}: empty height range",
          marketplace.id
        );
        table.by_script.entry(script).or_default().push(range);
      }
      table.marketplaces.push(Marketplace {
        name: marketplace.name.unwrap_or_else(|| marketplace.id.clone()),
        id: marketplace.id,
        fee_only_sales: marketplace.fee_only_sales,
      });
    }
    Ok(table)
  }

  /// The marketplace whose fees an output paying `script` at `height` is.
  pub fn fee_output(&self, script: &Script, height: u32) -> Option<&Marketplace> {
    self
      .by_script
      .get(script)?
      .iter()
      .find(|fee| fee.from_height <= height && height <= fee.to_height)
      .map(|fee| &self.marketplaces[fee.marketplace])
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

  #[test]
  fn embedded_table_parses() {
    Marketplaces::from_json(EMBEDDED).unwrap();
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
        {{ "id": "magic_eden", "name": "Magic Eden", "fee_only_sales": true,
          "fee_addresses": [
            {{ "address": "{address}", "from_height": 100, "to_height": 200 }},
            {{ "script_pubkey": "51", "from_height": 300 }} ] }},
        {{ "id": "magisat", "fee_addresses": [ {{ "script_pubkey": "52" }} ], "extra": 1 }} ] }}"#
    ))
    .unwrap();
    assert_eq!(table.fee_output(&me, 99), None);
    assert_eq!(table.fee_output(&me, 100).unwrap().id, "magic_eden");
    assert!(table.fee_output(&me, 100).unwrap().fee_only_sales);
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
    assert!(!magisat.fee_only_sales);
    assert_eq!(table.marketplaces().len(), 2);
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
      "not json".into(),
    ] {
      assert!(Marketplaces::from_json(&json).is_err(), "{json}");
    }
    assert!(Marketplaces::from_json("{}").unwrap().is_empty());
  }
}
