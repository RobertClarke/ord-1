//! Sale JSON (docs/API.md "Sales and inscriptions"): the stored `Sale` plus
//! the fields computed for the dashboard.
//!
//! The pure rows and the unit row follow `market::units` (ranked by catalog
//! total, ties to the lower id); the individually special sats are found by
//! bisecting the sold pieces with the catalog's row counts (range rows such
//! as block 9 do not count).

use {
  super::json::{PieceJson, Render, RowMap},
  crate::{
    market::{pure_rows, unit_row},
    model::{Piece, RowId, SaleKind},
    store::StoredSale,
  },
  bitcoin::{OutPoint, Script, Txid},
  serde::Serialize,
};

/// `sats` holds at most this many sats.
pub const MAX_SALE_SATS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PartyJson {
  pub address: Option<String>,
  pub script_pubkey: String,
  pub outpoint: OutPoint,
  /// `null` for a buyer output the store could not value.
  pub value: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SaleSatJson {
  pub sat: u64,
  /// The block that mined it.
  pub height: u32,
  pub time: Option<u32>,
  /// Inscribed when it was sold.
  pub inscribed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnitJson {
  pub row: String,
  pub count: u64,
  pub price: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SaleJson {
  pub txid: Txid,
  pub height: u32,
  pub time: Option<u32>,
  pub tx_index: u32,
  pub vin: u32,
  pub marketplace: String,
  pub kind: SaleKind,
  pub seller: PartyJson,
  pub buyer: Option<PartyJson>,
  /// The output paying the seller, when identified.
  pub payment_vout: Option<u32>,
  pub price: u64,
  pub seller_proceeds: u64,
  pub fees: u64,
  /// `price` − `seller.value`.
  pub premium: i64,
  pub pieces: Vec<PieceJson>,
  pub sats: Vec<SaleSatJson>,
  pub rows: RowMap,
  pub unit: Option<UnitJson>,
  /// The rows every sold sat belongs to, rarest first.
  pub pure_rows: Vec<String>,
  pub inscribed: bool,
}

impl Render<'_> {
  fn party(&self, script: &Script, outpoint: OutPoint, value: Option<u64>) -> PartyJson {
    PartyJson {
      address: self.address(script),
      script_pubkey: script.to_hex_string(),
      outpoint,
      value,
    }
  }

  /// A row's rank from the rarest: fewest sats that will ever exist, ties to
  /// the lower id.
  fn rank(&self, row: RowId) -> (u64, RowId) {
    (self.catalog.row_total(row), row)
  }

  /// Whether `[start, start + len)` holds a sat of a non-range row.
  fn has_special(&self, start: u64, len: u64) -> bool {
    self
      .catalog
      .row_counts(start, len)
      .iter()
      .any(|(row, count)| {
        *count > 0
          && self
            .catalog
            .rows()
            .get(usize::from(*row))
            .is_some_and(|row| !row.range)
      })
  }

  /// The individually special sats of `piece` (sats of non-range rows), in
  /// sat order, until `out` holds `MAX_SALE_SATS`.
  fn special_sats(&self, start: u64, len: u64, out: &mut Vec<u64>) {
    if out.len() >= MAX_SALE_SATS || len == 0 || !self.has_special(start, len) {
      return;
    }
    if len == 1 {
      out.push(start);
      return;
    }
    let half = len / 2;
    self.special_sats(start, half, out);
    self.special_sats(start + half, len - half, out);
  }

  /// The sats of `pieces` that carried an inscription before the
  /// transaction at `(height, tx_index)` (an inscription later in the same
  /// block, e.g. by the buyer, does not count).
  fn inscribed_at(
    &self,
    pieces: &[Piece],
    height: u32,
    tx_index: u32,
  ) -> anyhow::Result<std::collections::BTreeSet<u64>> {
    Ok(
      self
        .reader
        .inscribed_in_pieces(pieces)?
        .into_iter()
        .filter(|(_, inscribed)| {
          (inscribed.first_height, inscribed.first_tx_index) < (height, tx_index)
        })
        .map(|(sat, _)| sat)
        .collect(),
    )
  }

  pub fn sale(&self, stored: &StoredSale) -> anyhow::Result<SaleJson> {
    let sale = &stored.sale;
    let pieces = &sale.seller.pieces;

    let mut counts = Vec::new();
    self.count(&mut counts, pieces);
    let sold = pieces.iter().map(|piece| piece.len).sum();
    let premium = i128::from(sale.price) - i128::from(sale.seller.value);
    let premium = i64::try_from(premium).unwrap_or(if premium < 0 { i64::MIN } else { i64::MAX });
    let unit = unit_row(
      &counts,
      sold,
      |row| self.rank(row),
      |row| {
        self
          .catalog
          .rows()
          .get(usize::from(row))
          .is_some_and(|row| row.market)
      },
    )
    .map(|row| {
      let count = counts[usize::from(row)];
      UnitJson {
        row: self.row_name(row),
        count,
        price: premium / i64::try_from(count).unwrap_or(i64::MAX),
      }
    });
    let pure_rows = pure_rows(&counts, sold, |row| self.rank(row))
      .into_iter()
      .map(|row| self.row_name(row))
      .collect();

    let inscribed = self.inscribed_at(pieces, stored.height, sale.tx_index)?;

    let mut special = Vec::new();
    for piece in pieces {
      self.special_sats(piece.start, piece.len, &mut special);
    }
    let mut sats = Vec::with_capacity(special.len());
    for sat in special {
      let height = ordinals::Sat(sat).height().n();
      sats.push(SaleSatJson {
        sat,
        height,
        time: self.time(height)?,
        inscribed: inscribed.contains(&sat),
      });
    }

    let buyer = match (sale.buyer_vout, &sale.buyer_script) {
      (Some(vout), Some(script)) => Some(self.party(
        script,
        OutPoint {
          txid: sale.txid,
          vout,
        },
        stored.buyer_value,
      )),
      _ => None,
    };

    Ok(SaleJson {
      txid: sale.txid,
      height: stored.height,
      time: self.time(stored.height)?,
      tx_index: sale.tx_index,
      vin: sale.vin,
      marketplace: sale.marketplace.clone(),
      kind: sale.kind,
      seller: self.party(
        &sale.seller.script_pubkey,
        sale.seller_outpoint,
        Some(sale.seller.value),
      ),
      buyer,
      payment_vout: sale.payment_vout,
      price: sale.price,
      seller_proceeds: sale.seller_proceeds,
      fees: sale.fees,
      premium,
      pieces: self.pieces(pieces),
      sats,
      rows: self.row_map(&counts),
      unit,
      pure_rows,
      inscribed: !inscribed.is_empty(),
    })
  }

  /// Whether the sale sold sats of `row`.
  pub fn sale_has_row(&self, stored: &StoredSale, row: RowId) -> bool {
    stored.sale.seller.pieces.iter().any(|piece| {
      self
        .catalog
        .row_counts(piece.start, piece.len)
        .iter()
        .any(|(r, count)| *r == row && *count > 0)
    })
  }
}
