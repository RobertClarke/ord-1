//! A transaction that moved tracked sats, seen through its input and output
//! offsets: which sat sits at an input offset, where an input's pieces went.

use crate::model::{Move, MoveInput, ResolvedTx};

pub(super) struct TxView<'a> {
  pub tx: &'a ResolvedTx,
  pub mv: &'a Move,
  pub height: u32,
  /// Offset of each input's first sat in the transaction's input sats.
  input_base: Vec<u64>,
  /// One past each output's last offset.
  output_end: Vec<u64>,
  pub total_output: u64,
}

impl<'a> TxView<'a> {
  pub fn new(tx: &'a ResolvedTx, mv: &'a Move, height: u32) -> Self {
    let mut total = 0u64;
    let input_base = tx
      .inputs
      .iter()
      .map(|input| {
        let base = total;
        total = total.saturating_add(input.value);
        base
      })
      .collect();
    let mut total_output = 0u64;
    let output_end = tx
      .outputs
      .iter()
      .map(|output| {
        total_output = total_output.saturating_add(output.value.to_sat());
        total_output
      })
      .collect();
    Self {
      tx,
      mv,
      height,
      input_base,
      output_end,
      total_output,
    }
  }

  /// Offset of input `vin`'s first sat.
  pub fn input_base(&self, vin: usize) -> u64 {
    self.input_base[vin]
  }

  /// The move's record of input `vin`, if it carried tracked sats.
  pub fn tracked(&self, vin: u32) -> Option<&'a MoveInput> {
    self.mv.inputs.iter().find(|input| input.vin == vin)
  }

  /// The tracked sat at `offset` of the transaction's input sats, if any.
  pub fn sat_at(&self, offset: u64) -> Option<u64> {
    let vin = self
      .input_base
      .partition_point(|&base| base <= offset)
      .checked_sub(1)?;
    let local = offset - self.input_base[vin];
    if local >= self.tx.inputs[vin].value {
      return None;
    }
    let pieces = &self.tracked(vin as u32)?.utxo.pieces;
    let piece = pieces[pieces
      .partition_point(|piece| piece.offset <= local)
      .checked_sub(1)?];
    (local < piece.offset_end()).then(|| piece.start + (local - piece.offset))
  }

  /// How many of `input`'s tracked sats each output received, ascending by
  /// output; `None` is the fee.
  pub fn destinations(&self, input: &MoveInput) -> Vec<(Option<u32>, u64)> {
    let mut counts: Vec<(Option<u32>, u64)> = Vec::new();
    let mut add = |output: Option<u32>, n: u64| match counts.iter_mut().find(|(o, _)| *o == output)
    {
      Some((_, count)) => *count += n,
      None => counts.push((output, n)),
    };
    let base = self.input_base[input.vin as usize];
    for piece in &input.utxo.pieces {
      let mut offset = base + piece.offset;
      let end = offset + piece.len;
      while offset < end {
        let vout = self.output_end.partition_point(|&e| e <= offset);
        let Some(&output_end) = self.output_end.get(vout) else {
          add(None, end - offset);
          break;
        };
        let next = end.min(output_end);
        add(Some(vout as u32), next - offset);
        offset = next;
      }
    }
    counts.sort_unstable();
    counts
  }

  /// The output that received the most of `input`'s tracked sats (the lowest
  /// on ties); `None` when they all went to the fee.
  pub fn main_destination(&self, input: &MoveInput) -> Option<u32> {
    self
      .destinations(input)
      .into_iter()
      .filter_map(|(output, n)| Some((output?, n)))
      .max_by_key(|&(vout, n)| (n, std::cmp::Reverse(vout)))
      .map(|(vout, _)| vout)
  }
}
