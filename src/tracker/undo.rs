//! Reverting the tip block from its delta.

use super::*;

impl Tracker {
  pub(super) fn undo(&mut self, delta: &BlockDelta) -> anyhow::Result<()> {
    ensure!(
      self.height == Some(delta.height) && self.hash == Some(delta.hash),
      "block {} {} is not the tip ({:?} {:?})",
      delta.height,
      delta.hash,
      self.height,
      self.hash
    );
    ensure!(
      self.lost_sats == delta.lost_sats_after,
      "lost sats {} do not match the delta's {}",
      self.lost_sats,
      delta.lost_sats_after
    );

    // Validate everything before changing anything.
    for (outpoint, utxo) in &delta.created {
      let id = *self
        .index
        .get(&OutKey(*outpoint))
        .with_context(|| format!("created UTXO {outpoint} is missing"))?;
      ensure!(
        &self.slots[id as usize].utxo == utxo,
        "created UTXO {outpoint} differs from the delta"
      );
    }
    ensure!(
      self.lost.ends_with(&delta.lost),
      "the lost pieces do not end with the delta's"
    );
    ensure!(
      self.overwritten.len() >= delta.overwritten.len()
        && self.overwritten[self.overwritten.len() - delta.overwritten.len()..]
          .iter()
          .zip(&delta.overwritten)
          .all(|(o, (outpoint, utxo))| {
            o.outpoint == *outpoint && o.height == delta.height && o.utxo == *utxo
          }),
      "the overwritten UTXOs do not end with the delta's"
    );
    for (outpoint, _) in &delta.spent {
      ensure!(
        !self.index.contains_key(&OutKey(*outpoint))
          || delta.created.iter().any(|(created, _)| created == outpoint),
        "spent UTXO {outpoint} exists"
      );
    }

    self.begin_rows();
    for (outpoint, _) in delta.created.iter().rev() {
      let id = self.index[&OutKey(*outpoint)];
      self.unlocate_slot(id);
      self.detach(id);
      self.free_slot(id);
    }
    for _ in &delta.lost {
      self.pop_lost()?;
    }
    for _ in &delta.overwritten {
      let overwritten = self.pop_overwritten()?;
      self.insert_utxo(
        overwritten.outpoint,
        overwritten.utxo,
        Some(overwritten.rows),
      )?;
    }
    for (outpoint, utxo) in delta.spent.iter().rev() {
      self.insert_utxo(*outpoint, utxo.clone(), None)?;
    }
    self.lost_sats = delta.lost_sats_before;
    for d in &delta.rows {
      self.touch(d.row).mined = d.before.mined;
    }

    // The inverse operations must land exactly on the block's `before`.
    for d in &delta.rows {
      let stats = self.stats[usize::from(d.row)];
      ensure!(
        stats == d.before,
        "row {} did not revert: {stats:?} instead of {:?}",
        d.row,
        d.before
      );
    }
    for &row in &self.touched {
      if !delta.rows.iter().any(|d| d.row == row) {
        ensure!(
          self.stats[usize::from(row)] == self.before[usize::from(row)],
          "row {row} changed while undoing a block that did not change it"
        );
      }
    }

    self.pop_time();
    self.height = delta.height.checked_sub(1);
    self.hash = self.height.map(|_| delta.header.prev_blockhash);
    Ok(())
  }
}
