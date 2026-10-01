//! Inscriptions that land on tracked sats.
//!
//! ord's rule (`src/index/updater/inscription_updater.rs` at 03c9b87b): an
//! envelope revealed by input `i` inscribes the sat at offset
//! `pointer` of the transaction's input sats when it has a pointer below
//! the transaction's total output value, otherwise the first sat of input
//! `i` (the sum of the earlier inputs' values). An envelope revealed by an
//! input worth nothing is unbound: it inscribes no sat. Reinscriptions are
//! envelopes like any other, so a sat gets one event per envelope.
//!
//! Not modelled: ord also unbinds envelopes with unrecognized even fields;
//! `Envelope` does not carry that flag (rare, and only pre-jubilee-era
//! oddities).

use {super::view::TxView, crate::model::InscriptionEvent};

pub(super) fn detect(view: &TxView, out: &mut Vec<InscriptionEvent>) {
  for (vin, input) in view.tx.inputs.iter().enumerate() {
    if input.envelopes.is_empty() || input.value == 0 {
      continue;
    }
    for envelope in &input.envelopes {
      let offset = envelope
        .pointer
        .filter(|&pointer| pointer < view.total_output)
        .unwrap_or(view.input_base(vin));
      if let Some(sat) = view.sat_at(offset) {
        out.push(InscriptionEvent {
          sat,
          txid: view.tx.txid,
          vin: vin as u32,
        });
      }
    }
  }
}
