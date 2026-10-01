//! Pricing helpers for the API layer.
//!
//! A sale is priced per sat of one row. A row is **pure** for a sale when
//! every tracked sat it sold belongs to that row: a block 9 chunk is pure
//! for `block9` (and `block9_1st_tx`, `450x` when inside them), a single
//! 2009 alpha uncommon for `uncommon`, `alpha_uncommon`, `uncommon_2009`, …
//! The sale's unit row is its rarest pure row, so a chunk holding one
//! palindrome is still priced per block 9 sat, not as the palindrome; a
//! mixed bundle, pure for no row, falls back to its rarest row.

use crate::{
  model::{RowId, Sale},
  sats::SatCatalog,
};

/// The rows every sold sat belongs to, rarest first: `counts` holds the
/// sale's sats per row (by `RowId`), `sold` how many tracked sats it sold,
/// `rank` orders rows from the rarest.
pub fn pure_rows<K: Ord>(counts: &[u64], sold: u64, rank: impl Fn(RowId) -> K) -> Vec<RowId> {
  let mut rows: Vec<RowId> = counts
    .iter()
    .enumerate()
    .filter(|&(_, &count)| sold > 0 && count == sold)
    .map(|(row, _)| row as RowId)
    .collect();
  rows.sort_by_key(|&row| rank(row));
  rows
}

/// The row a sale is priced by: its rarest pure row, or, for a mixed bundle,
/// its rarest row with sold sats; `None` when it sold no row's sats.
pub fn unit_row<K: Ord>(counts: &[u64], sold: u64, rank: impl Fn(RowId) -> K) -> Option<RowId> {
  pure_rows(counts, sold, &rank).first().copied().or_else(|| {
    counts
      .iter()
      .enumerate()
      .filter(|&(_, &count)| count > 0)
      .map(|(row, _)| row as RowId)
      .min_by_key(|&row| rank(row))
  })
}

/// The sale's sats per row and how many tracked sats it sold.
fn sold_counts(sale: &Sale, catalog: &SatCatalog) -> (Vec<u64>, u64) {
  let mut counts = vec![0u64; catalog.rows().len()];
  let mut sold = 0;
  for piece in &sale.seller.pieces {
    sold += piece.len;
    catalog.add_row_counts(piece.start, piece.len, |row, n| {
      counts[usize::from(row)] += n
    });
  }
  (counts, sold)
}

/// The sale's pure rows, rarest first (`row_rank`).
pub fn sale_pure_rows(sale: &Sale, catalog: &SatCatalog) -> Vec<RowId> {
  let (counts, sold) = sold_counts(sale, catalog);
  pure_rows(&counts, sold, |row| catalog.row_rank(row))
}

/// The sale's unit row (`unit_row`), how many of that row's sats it sold
/// and the price per sat of it: the premium, `price − seller.value` (what
/// was paid above the output's own sats), split over the row's sats.
pub fn sale_unit(sale: &Sale, catalog: &SatCatalog) -> Option<(RowId, u64, u64)> {
  let (counts, sold) = sold_counts(sale, catalog);
  let row = unit_row(&counts, sold, |row| catalog.row_rank(row))?;
  let count = counts[usize::from(row)];
  let premium = sale.price.saturating_sub(sale.seller.value);
  Some((row, count, premium / count))
}

/// The individually special sats the sale sold, ascending, at most `cap`
/// (sats of a tracked range, like a block 9 chunk, are in its pieces).
pub fn sale_sats(sale: &Sale, catalog: &SatCatalog, cap: usize) -> Vec<u64> {
  let mut sats = Vec::new();
  for piece in &sale.seller.pieces {
    if sats.len() >= cap {
      break;
    }
    sats.extend(catalog.individual_sats(piece.start, piece.end(), cap - sats.len()));
  }
  sats.sort_unstable();
  sats
}
