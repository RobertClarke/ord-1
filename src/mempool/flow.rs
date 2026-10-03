//! The sat flow of one unconfirmed transaction, without a tracker: the same
//! first-in-first-out rules as `Tracker::apply_block` (docs/DESIGN.md §4),
//! read-only. Given which inputs spend tracked UTXOs (and what those hold),
//! it returns the `Move` the tracker would record for the transaction in a
//! block (`tx_index` aside, which the caller sets).

use {
  crate::{
    model::{Move, MoveInput, MoveOutput, Piece, ResolvedTx, UtxoKind},
    source::is_unspendable,
  },
  anyhow::{Context, ensure},
};

/// Appends `piece`, merging it into the last piece when both its offsets and
/// sats continue it (as the tracker does).
fn push_merged(pieces: &mut Vec<Piece>, piece: Piece) {
  if let Some(last) = pieces.last_mut()
    && last.offset_end() == piece.offset
    && last.end() == piece.start
  {
    last.len += piece.len;
    return;
  }
  pieces.push(piece);
}

/// The move of `tx`, whose tracked inputs are `inputs` (in vin order; each
/// `utxo` as it is before the spend). Every input of `tx` must carry its
/// value. Fails on inconsistent data (an input value that differs from the
/// tracked UTXO's, an unspendable or duplicate tracked input, outputs worth
/// more than the inputs).
pub fn tx_move(tx: &ResolvedTx, mut inputs: Vec<MoveInput>, tx_index: u32) -> anyhow::Result<Move> {
  inputs.sort_by_key(|input| input.vin);
  ensure!(
    !tx.inputs.is_empty(),
    "transaction {} has no inputs",
    tx.txid
  );
  let mut bases = Vec::with_capacity(tx.inputs.len());
  let mut input_value = 0u64;
  for input in &tx.inputs {
    bases.push(input_value);
    input_value = input_value
      .checked_add(input.value)
      .context("input values overflow")?;
  }
  let mut out_end = Vec::with_capacity(tx.outputs.len());
  let mut output_value = 0u64;
  for output in &tx.outputs {
    output_value = output_value
      .checked_add(output.value.to_sat())
      .context("output values overflow")?;
    out_end.push(output_value);
  }
  ensure!(
    output_value <= input_value,
    "transaction {} spends {input_value} sats but creates {output_value}",
    tx.txid
  );

  let mut outputs: Vec<(u32, Vec<Piece>)> = Vec::new();
  let mut fee = Vec::new();
  let mut j = 0usize;
  for (k, input) in inputs.iter().enumerate() {
    let vin = input.vin as usize;
    let resolved = tx
      .inputs
      .get(vin)
      .with_context(|| format!("{}: no input {vin}", tx.txid))?;
    ensure!(
      resolved.previous_output == input.outpoint,
      "{}:{vin} spends {}, not {}",
      tx.txid,
      resolved.previous_output,
      input.outpoint
    );
    ensure!(
      resolved.value == input.utxo.value,
      "{}:{vin} spends {} as {} sats, but it holds {}",
      tx.txid,
      input.outpoint,
      resolved.value,
      input.utxo.value
    );
    ensure!(
      input.utxo.kind == UtxoKind::Spendable,
      "{}:{vin} spends unspendable output {}",
      tx.txid,
      input.outpoint
    );
    ensure!(
      inputs[..k].iter().all(|other| other.vin != input.vin),
      "{} lists input {vin} twice",
      tx.txid
    );
    for piece in &input.utxo.pieces {
      let mut offset = bases[vin] + piece.offset;
      let mut sat = piece.start;
      let mut left = piece.len;
      while left > 0 {
        while j < out_end.len() && out_end[j] <= offset {
          j += 1;
        }
        if j == out_end.len() {
          push_merged(
            &mut fee,
            Piece {
              offset: offset - output_value,
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
        match outputs.last_mut() {
          Some((vout, pieces)) if *vout == j as u32 => push_merged(pieces, part),
          _ => outputs.push((j as u32, vec![part])),
        }
        offset += take;
        sat += take;
        left -= take;
      }
    }
  }

  let outputs = outputs
    .into_iter()
    .map(|(vout, pieces)| {
      let txout = &tx.outputs[vout as usize];
      MoveOutput {
        vout,
        value: txout.value.to_sat(),
        script_pubkey: txout.script_pubkey.clone(),
        kind: if is_unspendable(&txout.script_pubkey) {
          UtxoKind::Unspendable
        } else {
          UtxoKind::Spendable
        },
        pieces,
      }
    })
    .collect();

  Ok(Move {
    txid: tx.txid,
    tx_index,
    coinbase: false,
    input_value,
    output_value,
    inputs,
    outputs,
    fee,
    mined: Vec::new(),
    lost: Vec::new(),
  })
}
