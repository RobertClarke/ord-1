//! Pricing helpers for the API layer.

use crate::{
  model::{RowId, Sale},
  sats::SatCatalog,
};

/// The sale's primary row, how many of that row's sats it sold and the
/// price per sat of it: the primary row is the rarest row (`row_rank`)
/// among the sold sats, the premium is `price − seller.value` (what was paid
/// above the output's own sats), split over the row's sats. `None` when the
/// sold pieces hold no row's sats.
pub fn sale_unit(sale: &Sale, catalog: &SatCatalog) -> Option<(RowId, u64, u64)> {
  let mut counts = vec![0u64; catalog.rows().len()];
  for piece in &sale.seller.pieces {
    catalog.add_row_counts(piece.start, piece.len, |row, n| {
      counts[usize::from(row)] += n
    });
  }
  let (row, count) = counts
    .iter()
    .enumerate()
    .filter(|&(_, &count)| count > 0)
    .map(|(row, &count)| (row as RowId, count))
    .min_by_key(|&(row, _)| catalog.row_rank(row))?;
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
