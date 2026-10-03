//! Applying a block: the sat flow of DESIGN.md §4.

use {
  super::*,
  crate::{
    model::{Move, MoveInput, MoveOutput, ResolvedTx},
    sats::math,
  },
  bitcoin::hashes::Hash,
};

/// Output index of fee parts.
const FEE: u32 = u32::MAX;

/// Everything a block did so far, to build its delta or roll it back.
struct BlockCtx {
  seq: u64,
  moves: Vec<Move>,
  /// Tracked UTXOs that existed before the block and were spent in it.
  spent: Vec<(OutPoint, TrackedUtxo)>,
  /// Outpoints of tracked UTXOs created in the block.
  created: Vec<OutPoint>,
  overwritten: Vec<(OutPoint, TrackedUtxo)>,
  lost: Vec<Piece>,
  /// Tracked fee sats, at coinbase input stream offsets.
  fee_pieces: Vec<Piece>,
  /// Fees of the transactions processed so far.
  fees: u64,
  lost_sats_before: u64,
}

#[derive(Clone, Copy)]
struct Input {
  vin: u32,
  slot: u32,
  /// Offset of the input's first sat in the transaction's input stream.
  base: u64,
}

/// Where an input piece went.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fate {
  /// Whole, into this output.
  Intact(u32),
  /// Whole, into the fee.
  Fee,
  /// Cut by an output boundary.
  Split,
}

/// A piece of an input piece that landed in one output (or the fee).
#[derive(Clone, Copy)]
struct Part {
  out: u32,
  /// Offset within the output (or the fee).
  piece: Piece,
  input: usize,
  orig: usize,
  intact: bool,
}

/// A tracked output being created.
struct NewOutput {
  vout: u32,
  pieces: Vec<Piece>,
  rows: RowCounts,
  /// Input whose slot the output takes over.
  reuse: Option<usize>,
  /// Per piece: it is exactly an intact piece of the reused input, so its
  /// locator entry stays valid.
  kept: Vec<bool>,
}

impl Tracker {
  pub(super) fn apply(&mut self, block: &ResolvedBlock) -> anyhow::Result<BlockDelta> {
    let expected = match self.height {
      Some(height) => height.checked_add(1).context("height overflow")?,
      None => 0,
    };
    ensure!(
      block.height == expected,
      "expected block {expected}, got block {}",
      block.height
    );
    let prev = self.hash.unwrap_or_else(BlockHash::all_zeros);
    ensure!(
      block.header.prev_blockhash == prev,
      "block {} builds on {}, not on {prev}",
      block.height,
      block.header.prev_blockhash
    );
    ensure!(
      block.header.block_hash() == block.hash,
      "block {} hash {} does not match its header",
      block.height,
      block.hash
    );
    let coinbase = block.txs.first().context("block without a coinbase")?;
    ensure!(
      coinbase.inputs.is_empty(),
      "block {}: the coinbase has inputs",
      block.height
    );

    self.begin_rows();
    self.push_time(
      block.header.time,
      crate::model::luck_slot(&block.hash, block.header.bits),
    );
    let mut ctx = BlockCtx {
      seq: self.block_seq,
      moves: Vec::new(),
      spent: Vec::new(),
      created: Vec::new(),
      overwritten: Vec::new(),
      lost: Vec::new(),
      fee_pieces: Vec::new(),
      fees: 0,
      lost_sats_before: self.lost_sats,
    };
    match self.apply_transactions(block, &mut ctx) {
      Ok(()) => Ok(self.finish(block, ctx)),
      Err(err) => {
        self.rollback(ctx);
        Err(err.context(format!("block {} {}", block.height, block.hash)))
      }
    }
  }

  fn apply_transactions(
    &mut self,
    block: &ResolvedBlock,
    ctx: &mut BlockCtx,
  ) -> anyhow::Result<()> {
    for (tx_index, tx) in block.txs.iter().enumerate().skip(1) {
      ensure!(
        !tx.inputs.is_empty(),
        "transaction {} has no inputs",
        tx.txid
      );
      let mut input_value = 0u64;
      let mut tracked = false;
      for input in &tx.inputs {
        input_value = input_value
          .checked_add(input.value)
          .context("input values overflow")?;
        tracked = tracked || self.index.contains_key(&OutKey(input.previous_output));
      }
      let mut output_value = 0u64;
      for output in &tx.outputs {
        output_value = output_value
          .checked_add(output.value.to_sat())
          .context("output values overflow")?;
      }
      ensure!(
        output_value <= input_value,
        "transaction {} spends {input_value} sats but creates {output_value}",
        tx.txid
      );
      if tracked {
        self.apply_tracked(
          block.height,
          tx_index as u32,
          tx,
          input_value,
          output_value,
          ctx,
        )?;
      }
      ctx.fees = ctx
        .fees
        .checked_add(input_value - output_value)
        .context("fees overflow")?;
    }
    self.apply_coinbase(block, ctx)
  }

  /// A transaction that spends at least one tracked UTXO.
  fn apply_tracked(
    &mut self,
    height: u32,
    tx_index: u32,
    tx: &ResolvedTx,
    input_value: u64,
    output_value: u64,
    ctx: &mut BlockCtx,
  ) -> anyhow::Result<()> {
    let mut inputs: Vec<Input> = Vec::new();
    let mut base = 0u64;
    for (vin, input) in tx.inputs.iter().enumerate() {
      if let Some(&slot) = self.index.get(&OutKey(input.previous_output)) {
        let utxo = &self.slots[slot as usize].utxo;
        ensure!(
          utxo.value == input.value,
          "{}:{vin} spends {} as {} sats, but it holds {}",
          tx.txid,
          input.previous_output,
          input.value,
          utxo.value
        );
        ensure!(
          utxo.kind == UtxoKind::Spendable,
          "{}:{vin} spends unspendable output {}",
          tx.txid,
          input.previous_output
        );
        ensure!(
          inputs.iter().all(|i| i.slot != slot),
          "{} spends {} twice",
          tx.txid,
          input.previous_output
        );
        inputs.push(Input {
          vin: vin as u32,
          slot,
          base,
        });
      }
      base += input.value;
    }

    // The outputs' offset ranges.
    let mut out_end = Vec::with_capacity(tx.outputs.len());
    let mut end = 0u64;
    for output in &tx.outputs {
      end += output.value.to_sat();
      out_end.push(end);
    }

    // Tracked outputs must be new outpoints (BIP30 duplicates are coinbases).
    // Checked before anything is detached.
    let mut parts: Vec<Part> = Vec::new();
    let mut fates: Vec<Vec<Fate>> = Vec::with_capacity(inputs.len());
    {
      let mut j = 0usize;
      for (k, input) in inputs.iter().enumerate() {
        let pieces = &self.slots[input.slot as usize].utxo.pieces;
        let mut input_fates = Vec::with_capacity(pieces.len());
        for (orig, piece) in pieces.iter().enumerate() {
          let mut offset = input.base + piece.offset;
          let mut sat = piece.start;
          let mut left = piece.len;
          let mut fate = Fate::Split;
          while left > 0 {
            while j < out_end.len() && out_end[j] <= offset {
              j += 1;
            }
            if j == out_end.len() {
              let intact = left == piece.len;
              parts.push(Part {
                out: FEE,
                piece: Piece {
                  offset: offset - output_value,
                  start: sat,
                  len: left,
                },
                input: k,
                orig,
                intact,
              });
              if intact {
                fate = Fate::Fee;
              }
              break;
            }
            let out_start = out_end[j] - tx.outputs[j].value.to_sat();
            let take = left.min(out_end[j] - offset);
            let intact = take == piece.len;
            parts.push(Part {
              out: j as u32,
              piece: Piece {
                offset: offset - out_start,
                start: sat,
                len: take,
              },
              input: k,
              orig,
              intact,
            });
            if intact {
              fate = Fate::Intact(j as u32);
            }
            offset += take;
            sat += take;
            left -= take;
          }
          input_fates.push(fate);
        }
        fates.push(input_fates);
      }
    }

    // Group the output parts into new outputs. `stays[k][orig]`: input piece
    // `orig` of input `k` stays, as is, in the slot its output takes over,
    // so its locator entry stays valid.
    let mut outputs: Vec<NewOutput> = Vec::new();
    let mut stays: Vec<Vec<bool>> = fates.iter().map(|f| vec![false; f.len()]).collect();
    let mut reused = vec![false; inputs.len()];
    let mut intact_counts = vec![0usize; inputs.len()];
    let mut i = 0;
    while i < parts.len() && parts[i].out != FEE {
      let vout = parts[i].out;
      let mut end = i;
      while end < parts.len() && parts[end].out == vout {
        end += 1;
      }
      let group = &parts[i..end];
      let outpoint = OutPoint {
        txid: tx.txid,
        vout,
      };
      ensure!(
        !self.index.contains_key(&OutKey(outpoint)),
        "{outpoint} already holds a tracked UTXO"
      );

      intact_counts.iter_mut().for_each(|n| *n = 0);
      for part in group {
        if part.intact {
          intact_counts[part.input] += 1;
        }
      }
      let reuse = (0..inputs.len())
        .filter(|&k| !reused[k] && intact_counts[k] > 0)
        .max_by_key(|&k| (intact_counts[k], std::cmp::Reverse(k)));
      if let Some(k) = reuse {
        reused[k] = true;
      }

      let mut pieces: Vec<Piece> = Vec::with_capacity(group.len());
      let mut kept: Vec<bool> = Vec::with_capacity(group.len());
      let mut merged_into = Vec::with_capacity(group.len());
      for part in group {
        let before = pieces.len();
        push_merged(&mut pieces, part.piece);
        if pieces.len() > before {
          kept.push(reuse == Some(part.input) && part.intact);
        } else {
          *kept.last_mut().unwrap() = false;
        }
        merged_into.push(pieces.len() - 1);
      }
      for (part, &merged) in group.iter().zip(&merged_into) {
        if kept[merged] {
          stays[part.input][part.orig] = true;
        }
      }

      // Row counts: start from the reused input's cached counts and correct
      // for what did not come along intact, so untouched pieces cost nothing.
      if let Some(k) = reuse {
        let slot = &self.slots[inputs[k].slot as usize];
        self.acc.add_counts(&slot.rows, 1);
        for (orig, piece) in slot.utxo.pieces.iter().enumerate() {
          if fates[k][orig] != Fate::Intact(vout) {
            self.acc.add_piece(&self.catalog, &self.blocks, piece, -1);
          }
        }
      }
      for part in group {
        if !(reuse == Some(part.input) && part.intact) {
          self
            .acc
            .add_piece(&self.catalog, &self.blocks, &part.piece, 1);
        }
      }
      let rows = self.acc.take()?;

      outputs.push(NewOutput {
        vout,
        pieces,
        rows,
        reuse,
        kept,
      });
      i = end;
    }
    let mut fee = Vec::new();
    for part in &parts[i..] {
      push_merged(&mut fee, part.piece);
    }
    // Fee sats go to the coinbase after the subsidy and earlier fees.
    let fee_base = math::subsidy(height.into())
      .checked_add(ctx.fees)
      .filter(|base| base.checked_add(input_value - output_value).is_some())
      .context("coinbase stream offsets overflow")?;

    // Nothing can fail from here on. Detach the inputs.
    let mut move_inputs = Vec::with_capacity(inputs.len());
    for input in &inputs {
      self.detach(input.slot);
      let slot = &self.slots[input.slot as usize];
      self.index.remove(&OutKey(slot.outpoint));
      move_inputs.push(MoveInput {
        vin: input.vin,
        outpoint: slot.outpoint,
        utxo: slot.utxo.clone(),
      });
      if slot.created_seq != ctx.seq {
        ctx.spent.push((slot.outpoint, slot.utxo.clone()));
      }
    }

    // Locator: drop every input piece that does not stay.
    for (k, input) in inputs.iter().enumerate() {
      let pieces = std::mem::take(&mut self.slots[input.slot as usize].utxo.pieces);
      for (orig, piece) in pieces.iter().enumerate() {
        if !stays[k][orig] {
          self.locator_remove(piece);
        }
      }
      self.slots[input.slot as usize].utxo.pieces = pieces;
    }

    // Create the outputs, taking over reused slots.
    let mut move_outputs = Vec::with_capacity(outputs.len());
    let mut created = Vec::with_capacity(outputs.len());
    for output in outputs {
      let txout = &tx.outputs[output.vout as usize];
      let outpoint = OutPoint {
        txid: tx.txid,
        vout: output.vout,
      };
      let kind = if is_unspendable(&txout.script_pubkey) {
        UtxoKind::Unspendable
      } else {
        UtxoKind::Spendable
      };
      debug_assert!(check_pieces(txout.value.to_sat(), &output.pieces).is_ok());
      move_outputs.push(MoveOutput {
        vout: output.vout,
        value: txout.value.to_sat(),
        script_pubkey: txout.script_pubkey.clone(),
        kind,
        pieces: output.pieces.clone(),
      });
      let utxo = TrackedUtxo {
        value: txout.value.to_sat(),
        script_pubkey: txout.script_pubkey.clone(),
        height,
        kind,
        pieces: output.pieces,
      };
      let id = match output.reuse {
        Some(k) => {
          let id = inputs[k].slot;
          let slot = &mut self.slots[id as usize];
          slot.outpoint = outpoint;
          slot.utxo = utxo;
          slot.rows = output.rows;
          slot.created_seq = ctx.seq;
          self.index.insert(OutKey(outpoint), id);
          id
        }
        None => self.alloc_slot(outpoint, utxo, output.rows, ctx.seq),
      };
      let pieces = std::mem::take(&mut self.slots[id as usize].utxo.pieces);
      for (piece, &kept) in pieces.iter().zip(&output.kept) {
        if !kept && let Err(err) = self.locator_insert(piece, Loc::Slot(id)) {
          debug_assert!(false, "{err}");
        }
      }
      self.slots[id as usize].utxo.pieces = pieces;
      created.push(id);
      ctx.created.push(outpoint);
    }
    for (k, input) in inputs.iter().enumerate() {
      if !reused[k] {
        self.free_slot(input.slot);
      }
    }
    for id in created {
      self.attach(id);
    }

    for piece in &fee {
      push_merged(
        &mut ctx.fee_pieces,
        Piece {
          offset: fee_base + piece.offset,
          ..*piece
        },
      );
    }

    ctx.moves.push(Move {
      txid: tx.txid,
      tx_index,
      coinbase: false,
      input_value,
      output_value,
      inputs: move_inputs,
      outputs: move_outputs,
      fee,
      mined: Vec::new(),
      lost: Vec::new(),
    });
    Ok(())
  }

  fn apply_coinbase(&mut self, block: &ResolvedBlock, ctx: &mut BlockCtx) -> anyhow::Result<()> {
    let tx = &block.txs[0];
    let height = u64::from(block.height);
    let subsidy = math::subsidy(height);
    let first = math::first_sat(height);
    let mut output_value = 0u64;
    for output in &tx.outputs {
      output_value = output_value
        .checked_add(output.value.to_sat())
        .context("coinbase output values overflow")?;
    }
    let input_value = subsidy
      .checked_add(ctx.fees)
      .context("coinbase input overflow")?;
    ensure!(
      output_value <= input_value,
      "the coinbase claims {output_value} sats of {input_value} (subsidy {subsidy} + fees {})",
      ctx.fees
    );

    // Nothing can fail from here on.

    // BIP30: an output created where a tracked UTXO still sits replaces it.
    for vout in 0..tx.outputs.len() as u32 {
      let outpoint = OutPoint {
        txid: tx.txid,
        vout,
      };
      if let Some(&id) = self.index.get(&OutKey(outpoint)) {
        self.overwrite(id, block.height, ctx);
      }
    }

    let mined: Vec<Piece> = self
      .catalog
      .tracked_runs(first, first + subsidy)
      .into_iter()
      .map(|(start, len)| Piece {
        offset: start - first,
        start,
        len,
      })
      .collect();
    for piece in &mined {
      self.acc.add_piece(&self.catalog, &self.blocks, piece, 1);
    }
    for (row, n) in self.acc.take().unwrap_or_default() {
      self.touch(row).mined += n;
    }

    // The coinbase input stream: subsidy, then fees in transaction order.
    let mut groups: Vec<(u32, Vec<Piece>)> = Vec::new();
    let mut lost = Vec::new();
    let mut j = 0usize;
    let mut out_end = Vec::with_capacity(tx.outputs.len());
    let mut end = 0u64;
    for output in &tx.outputs {
      end += output.value.to_sat();
      out_end.push(end);
    }
    for piece in mined.iter().chain(ctx.fee_pieces.iter()) {
      let mut offset = piece.offset;
      let mut sat = piece.start;
      let mut left = piece.len;
      while left > 0 {
        while j < out_end.len() && out_end[j] <= offset {
          j += 1;
        }
        if j == out_end.len() {
          push_merged(
            &mut lost,
            Piece {
              offset: self.lost_sats + (offset - output_value),
              start: sat,
              len: left,
            },
          );
          break;
        }
        let out_start = out_end[j] - tx.outputs[j].value.to_sat();
        let take = left.min(out_end[j] - offset);
        let part = Piece {
          offset: offset - out_start,
          start: sat,
          len: take,
        };
        match groups.last_mut() {
          Some((vout, pieces)) if *vout == j as u32 => push_merged(pieces, part),
          _ => groups.push((j as u32, vec![part])),
        }
        offset += take;
        sat += take;
        left -= take;
      }
    }

    let mut move_outputs = Vec::with_capacity(groups.len());
    let mut created = Vec::with_capacity(groups.len());
    for (vout, pieces) in groups {
      let txout = &tx.outputs[vout as usize];
      let outpoint = OutPoint {
        txid: tx.txid,
        vout,
      };
      let kind = if is_unspendable(&txout.script_pubkey) {
        UtxoKind::Unspendable
      } else {
        UtxoKind::Spendable
      };
      debug_assert!(check_pieces(txout.value.to_sat(), &pieces).is_ok());
      for piece in &pieces {
        self.acc.add_piece(&self.catalog, &self.blocks, piece, 1);
      }
      let rows = self.acc.take().unwrap_or_default();
      move_outputs.push(MoveOutput {
        vout,
        value: txout.value.to_sat(),
        script_pubkey: txout.script_pubkey.clone(),
        kind,
        pieces: pieces.clone(),
      });
      let id = self.alloc_slot(
        outpoint,
        TrackedUtxo {
          value: txout.value.to_sat(),
          script_pubkey: txout.script_pubkey.clone(),
          height: block.height,
          kind,
          pieces,
        },
        rows,
        ctx.seq,
      );
      if let Err(err) = self.locate_slot(id) {
        debug_assert!(false, "{err}");
      }
      created.push(id);
      ctx.created.push(outpoint);
    }
    for id in created {
      self.attach(id);
    }

    for piece in &lost {
      if let Err(err) = self.push_lost(*piece) {
        debug_assert!(false, "{err}");
      }
    }
    self.lost_sats += input_value - output_value;

    if !mined.is_empty() || !move_outputs.is_empty() || !lost.is_empty() {
      ctx.moves.push(Move {
        txid: tx.txid,
        tx_index: 0,
        coinbase: true,
        input_value: 0,
        output_value,
        inputs: Vec::new(),
        outputs: move_outputs,
        fee: Vec::new(),
        mined,
        lost: lost.clone(),
      });
    }
    ctx.lost = lost;
    Ok(())
  }

  /// Moves the tracked UTXO in slot `id` to the overwritten list.
  fn overwrite(&mut self, id: u32, height: u32, ctx: &mut BlockCtx) {
    self.detach(id);
    self.unlocate_slot(id);
    let slot = &self.slots[id as usize];
    let overwritten = Overwritten {
      outpoint: slot.outpoint,
      height,
      utxo: slot.utxo.clone(),
      rows: slot.rows.clone(),
    };
    ctx
      .overwritten
      .push((overwritten.outpoint, overwritten.utxo.clone()));
    self.index.remove(&OutKey(overwritten.outpoint));
    self.free_slot(id);
    if let Err(err) = self.push_overwritten(overwritten) {
      debug_assert!(false, "{err}");
    }
  }

  fn finish(&mut self, block: &ResolvedBlock, ctx: BlockCtx) -> BlockDelta {
    let created = ctx
      .created
      .iter()
      .filter_map(|outpoint| {
        let &id = self.index.get(&OutKey(*outpoint))?;
        let slot = &self.slots[id as usize];
        (slot.created_seq == ctx.seq).then(|| (*outpoint, slot.utxo.clone()))
      })
      .collect();
    self.height = Some(block.height);
    self.hash = Some(block.hash);
    BlockDelta {
      height: block.height,
      hash: block.hash,
      header: block.header,
      moves: ctx.moves,
      spent: ctx.spent,
      created,
      overwritten: ctx.overwritten,
      lost: ctx.lost,
      lost_sats_before: ctx.lost_sats_before,
      lost_sats_after: self.lost_sats,
      rows: self.row_deltas(),
    }
  }

  /// The rows touched since `begin_rows` whose stats changed.
  pub(super) fn row_deltas(&mut self) -> Vec<crate::model::RowDelta> {
    self.touched.sort_unstable();
    self
      .touched
      .iter()
      .filter(|&&row| self.before[usize::from(row)] != self.stats[usize::from(row)])
      .map(|&row| crate::model::RowDelta {
        row,
        before: self.before[usize::from(row)],
        after: self.stats[usize::from(row)],
      })
      .collect()
  }

  /// Reverts a block that failed half way. Every step inverts one of
  /// `apply_block`'s, newest first.
  fn rollback(&mut self, ctx: BlockCtx) {
    for _ in &ctx.lost {
      let _ = self.pop_lost();
    }
    for outpoint in ctx.created.iter().rev() {
      if let Some(&id) = self.index.get(&OutKey(*outpoint))
        && self.slots[id as usize].created_seq == ctx.seq
      {
        self.unlocate_slot(id);
        self.detach(id);
        self.free_slot(id);
      }
    }
    for _ in ctx.overwritten.iter().rev() {
      if let Ok(overwritten) = self.pop_overwritten() {
        let _ = self.insert_utxo(
          overwritten.outpoint,
          overwritten.utxo,
          Some(overwritten.rows),
        );
      }
    }
    for (outpoint, utxo) in ctx.spent.into_iter().rev() {
      let _ = self.insert_utxo(outpoint, utxo, None);
    }
    // Only `mined` is not restored by the inverse operations above.
    for &row in &self.touched {
      debug_assert_eq!(
        RowStats {
          mined: self.before[usize::from(row)].mined,
          ..self.stats[usize::from(row)]
        },
        self.before[usize::from(row)]
      );
      self.stats[usize::from(row)] = self.before[usize::from(row)];
    }
    self.lost_sats = ctx.lost_sats_before;
    self.pop_time();
  }

  /// Inserts a UTXO with its locator entries and aggregates.
  pub(super) fn insert_utxo(
    &mut self,
    outpoint: OutPoint,
    utxo: TrackedUtxo,
    rows: Option<RowCounts>,
  ) -> anyhow::Result<u32> {
    ensure!(
      !self.index.contains_key(&OutKey(outpoint)),
      "{outpoint} already holds a tracked UTXO"
    );
    let rows = match rows {
      Some(rows) => rows,
      None => self.piece_rows(&utxo.pieces)?,
    };
    let id = self.alloc_slot(outpoint, utxo, rows, 0);
    if let Err(err) = self.locate_slot(id) {
      self.unlocate_slot(id);
      self.free_slot(id);
      return Err(err);
    }
    self.attach(id);
    Ok(id)
  }
}
