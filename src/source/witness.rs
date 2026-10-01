//! What the sources read from an input's scriptSig and witness: the sighash
//! flag of its (first) signature, and ord's inscription envelopes.
//!
//! Both sources call these on the same bytes (the fast source on decoded
//! raw blocks, the live source on `getblock 3`'s `scriptSig.hex` and
//! `txinwitness`), so they agree by construction. Neither allocates unless
//! the input reveals an envelope.

use {
  crate::model::Envelope,
  bitcoin::{Script, Witness},
  memchr::memmem,
  std::sync::LazyLock,
};

/// The sighash flag of the input's (first) signature, recognized from the
/// scriptSig and witness alone (the spent script is not known):
///
/// * taproot key path (one witness element, or two with an annex): a 64-byte
///   signature is `0x00` (SIGHASH_DEFAULT), a 65-byte one its last byte;
/// * taproot script path (the last element, annex aside, is a control block
///   with leaf version 0xc0): the first 64- or 65-byte element below the
///   tapscript;
/// * segwit v0 (P2WPKH, P2WSH, also P2SH-wrapped): the first element, the
///   witness script or public key aside, that is a strict DER signature plus
///   flag byte;
/// * no witness (P2PKH, P2PK, P2SH multisig, …): the first scriptSig push
///   that is a strict DER signature plus flag byte;
/// * otherwise `None`.
pub fn input_sighash(script_sig: &Script, witness: &Witness) -> Option<u8> {
  let len = witness.len();

  if len == 0 {
    return script_sig_pushes(script_sig.as_bytes())
      .find(|push| is_der_signature(push))
      .and_then(|signature| signature.last().copied());
  }

  // BIP341: with at least two elements, a last one starting with 0x50 is
  // the annex. Only taproot has one.
  let annex = len >= 2
    && witness
      .last()
      .is_some_and(|last| last.first() == Some(&0x50));
  let len = len - usize::from(annex);

  if len == 1 {
    return witness.nth(0).and_then(schnorr_flag);
  }

  if witness.nth(len - 1).is_some_and(is_tapscript_control_block) {
    return witness
      .iter()
      .take(len - 2)
      .find(|element| matches!(element.len(), 64 | 65))
      .and_then(schnorr_flag);
  }

  if annex {
    return None;
  }

  witness
    .iter()
    .take(len - 1)
    .find(|element| is_der_signature(element))
    .and_then(|signature| signature.last().copied())
}

fn schnorr_flag(signature: &[u8]) -> Option<u8> {
  match signature.len() {
    64 => Some(0x00),
    65 => Some(signature[64]),
    _ => None,
  }
}

/// A BIP341 control block for leaf version 0xc0 (tapscript), either parity.
fn is_tapscript_control_block(control_block: &[u8]) -> bool {
  let len = control_block.len();
  (33..=33 + 32 * 128).contains(&len)
    && (len - 33).is_multiple_of(32)
    && control_block[0] & 0xfe == 0xc0
}

/// Core's `IsValidSignatureEncoding` (BIP66 strict DER), for a signature
/// followed by its sighash flag byte.
pub fn is_der_signature(sig: &[u8]) -> bool {
  // 0x30 <len> 0x02 <len R> <R> 0x02 <len S> <S> <flag>
  let size = sig.len();
  if !(9..=73).contains(&size) || sig[0] != 0x30 || usize::from(sig[1]) != size - 3 {
    return false;
  }

  let len_r = usize::from(sig[3]);
  if 5 + len_r >= size {
    return false;
  }

  let len_s = usize::from(sig[5 + len_r]);
  if len_r + len_s + 7 != size {
    return false;
  }

  sig[2] == 0x02
    && len_r != 0
    && sig[4] & 0x80 == 0
    && !(len_r > 1 && sig[4] == 0x00 && sig[5] & 0x80 == 0)
    && sig[len_r + 4] == 0x02
    && len_s != 0
    && sig[len_r + 6] & 0x80 == 0
    && !(len_s > 1 && sig[len_r + 6] == 0x00 && sig[len_r + 7] & 0x80 == 0)
}

/// The data pushes of a scriptSig, up to the first non-push opcode or
/// parse error.
fn script_sig_pushes(script: &[u8]) -> impl Iterator<Item = &[u8]> {
  Instructions::new(script).map_while(|instruction| match instruction {
    Ok(Instruction::Push(push)) => Some(push),
    _ => None,
  })
}

/// ord's inscription envelopes in the input, in order, with their pointers.
///
/// An exact port of ord 0.29's `RawEnvelope::from_transaction` and
/// `Inscription::pointer`:
///
/// * the script is the witness's tapscript as `Witness::tapscript` finds it
///   (the second-to-last element, or third-to-last with an annex; one
///   element, or two with an annex, is a key path spend) without checking
///   that the spent output is taproot, like ord;
/// * an envelope is `OP_FALSE OP_IF <"ord"> … OP_ENDIF` (any push encoding)
///   with only pushes and `OP_PUSHNUM_*` in between; every one is listed,
///   cursed ones (duplicate or unknown even fields, pushnum, stutter, …)
///   included, since ord indexes them all;
/// * a script that fails to parse anywhere yields no envelopes at all;
/// * the pointer is the first value of tag 2 before the body, read
///   little-endian, `None` if absent or if a byte past the eighth is not
///   zero;
/// * `unrecognized_even_field` is ord's flag of that name (which unbinds the
///   inscription): a complete field before the body whose tag's first byte
///   is even, other than the first pointer.
///
/// ord only indexes envelopes from its first inscription height (767,430
/// on mainnet); this reports them at every height.
pub fn input_envelopes(witness: &Witness) -> Vec<Envelope> {
  match tapscript(witness) {
    Some(script) => script_envelopes(script),
    None => Vec::new(),
  }
}

/// `Witness::tapscript` (deprecated upstream, but what ord uses).
fn tapscript(witness: &Witness) -> Option<&[u8]> {
  let len = witness.len();
  let annex = witness
    .last()
    .is_some_and(|last| last.first() == Some(&0x50));
  match len {
    0 | 1 => None,
    2 if annex => None,
    _ if annex => witness.nth(len - 3),
    _ => witness.nth(len - 2),
  }
}

/// Every envelope needs the protocol id "ord" in some push encoding, so
/// scripts without those bytes (nearly all) are skipped without parsing.
static ORD: LazyLock<memmem::Finder<'static>> = LazyLock::new(|| memmem::Finder::new(b"ord"));

/// The envelopes in a tapscript.
pub(crate) fn script_envelopes(script: &[u8]) -> Vec<Envelope> {
  if ORD.find(script).is_none() {
    return Vec::new();
  }
  parse_envelopes(script).unwrap_or_default()
}

const OPCODE_IF: u8 = 0x63;
const OPCODE_ENDIF: u8 = 0x68;
const OPCODE_PUSHNUM_NEG1: u8 = 0x4f;
const OPCODE_PUSHNUM_1: u8 = 0x51;
const OPCODE_PUSHNUM_16: u8 = 0x60;

/// `RawEnvelope::from_tapscript`, reduced to what `Envelope` needs. `None`
/// when the script does not parse.
fn parse_envelopes(script: &[u8]) -> Option<Vec<Envelope>> {
  let mut instructions = Instructions::new(script).peekable();
  let mut envelopes = Vec::new();

  while let Some(instruction) = instructions.next() {
    if instruction.ok()? != Instruction::Push(&[]) {
      continue;
    }

    // `accept`: consume the next instruction only if it is the expected one
    // (an error is left for the outer loop to fail on).
    if instructions.peek() != Some(&Ok(Instruction::Op(OPCODE_IF))) {
      continue;
    }
    instructions.next();

    if instructions.peek() != Some(&Ok(Instruction::Push(b"ord"))) {
      continue;
    }
    instructions.next();

    let mut payload = Payload::default();
    loop {
      match instructions.next() {
        // The script ends inside the envelope: no envelope, and the outer
        // loop ends too.
        None => break,
        Some(Err(())) => return None,
        Some(Ok(Instruction::Op(OPCODE_ENDIF))) => {
          envelopes.push(Envelope {
            pointer: payload.pointer,
            unrecognized_even_field: payload.unrecognized_even_field,
          });
          break;
        }
        Some(Ok(Instruction::Op(OPCODE_PUSHNUM_NEG1))) => payload.push(&[0x81]),
        Some(Ok(Instruction::Op(op @ OPCODE_PUSHNUM_1..=OPCODE_PUSHNUM_16))) => {
          payload.push(&[op - OPCODE_PUSHNUM_1 + 1])
        }
        Some(Ok(Instruction::Push(push))) => payload.push(push),
        // Any other opcode ends the envelope without one.
        Some(Ok(Instruction::Op(_))) => break,
      }
    }
  }

  Some(envelopes)
}

/// Streams an envelope's payload into what `Envelope` needs. Fields are the
/// (tag, value) pairs before the body, which starts at the first empty push
/// in a tag position; a tag without a value is ignored, like ord's
/// incomplete field.
#[derive(Default)]
struct Payload {
  index: usize,
  body: bool,
  /// The tag at the last even position, awaiting its value.
  tag: Tag,
  pointer_seen: bool,
  pointer: Option<u64>,
  unrecognized_even_field: bool,
}

#[derive(Default, Clone, Copy)]
enum Tag {
  #[default]
  Odd,
  Pointer,
  /// Any other tag whose first byte is even.
  Even,
}

impl Payload {
  fn push(&mut self, item: &[u8]) {
    if self.body {
      return;
    }
    if self.index.is_multiple_of(2) {
      match item.first() {
        None => self.body = true,
        Some(_) if item == [2] => self.tag = Tag::Pointer,
        Some(first) if first % 2 == 0 => self.tag = Tag::Even,
        Some(_) => self.tag = Tag::Odd,
      }
    } else {
      match self.tag {
        // ord takes the first value of a repeated tag...
        Tag::Pointer if !self.pointer_seen => {
          self.pointer_seen = true;
          self.pointer = pointer(item);
        }
        // ...and flags what is left with an even tag.
        Tag::Pointer | Tag::Even => self.unrecognized_even_field = true,
        Tag::Odd => {}
      }
    }
    self.index += 1;
  }
}

/// `Inscription::pointer`: little-endian, at most eight significant bytes.
fn pointer(value: &[u8]) -> Option<u64> {
  if value.iter().skip(8).any(|&byte| byte != 0) {
    return None;
  }
  let mut bytes = [0u8; 8];
  let len = value.len().min(8);
  bytes[..len].copy_from_slice(&value[..len]);
  Some(u64::from_le_bytes(bytes))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Instruction<'a> {
  Push(&'a [u8]),
  Op(u8),
}

/// rust-bitcoin's `Script::instructions` (non-minimal pushes accepted; the
/// first error ends the iteration), without its allocations.
struct Instructions<'a> {
  script: &'a [u8],
  done: bool,
}

impl<'a> Instructions<'a> {
  fn new(script: &'a [u8]) -> Self {
    Self {
      script,
      done: false,
    }
  }

  fn take(&mut self, n: usize) -> Result<&'a [u8], ()> {
    if self.script.len() < n {
      self.done = true;
      return Err(());
    }
    let (taken, rest) = self.script.split_at(n);
    self.script = rest;
    Ok(taken)
  }

  fn push(&mut self, length_bytes: usize) -> Result<Instruction<'a>, ()> {
    let length = self.take(length_bytes)?;
    let n = length
      .iter()
      .rev()
      .fold(0usize, |n, &byte| n << 8 | usize::from(byte));
    self.take(n).map(Instruction::Push)
  }
}

impl<'a> Iterator for Instructions<'a> {
  type Item = Result<Instruction<'a>, ()>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.done {
      return None;
    }
    let (&opcode, rest) = self.script.split_first()?;
    self.script = rest;
    Some(match opcode {
      0x00..=0x4b => self.take(usize::from(opcode)).map(Instruction::Push),
      0x4c => self.push(1),
      0x4d => self.push(2),
      0x4e => self.push(4),
      _ => Ok(Instruction::Op(opcode)),
    })
  }
}

#[cfg(test)]
mod tests {
  use {
    super::*,
    bitcoin::{
      ScriptBuf,
      hashes::Hash as _,
      opcodes::{self, all::*},
      script::{self, PushBytesBuf},
      secp256k1::{self, Message, Secp256k1, SecretKey},
    },
  };

  fn push(bytes: &[u8]) -> PushBytesBuf {
    PushBytesBuf::try_from(bytes.to_vec()).unwrap()
  }

  /// A real ECDSA signature with `flag` appended.
  fn der(flag: u8) -> Vec<u8> {
    let secp = Secp256k1::new();
    let key = SecretKey::from_slice(&[0x11; 32]).unwrap();
    let mut sig = secp
      .sign_ecdsa(&Message::from_digest([0x22; 32]), &key)
      .serialize_der()
      .to_vec();
    sig.push(flag);
    sig
  }

  fn pubkey() -> Vec<u8> {
    let secp = Secp256k1::new();
    let key = SecretKey::from_slice(&[0x11; 32]).unwrap();
    secp256k1::PublicKey::from_secret_key(&secp, &key)
      .serialize()
      .to_vec()
  }

  fn schnorr(flag: Option<u8>) -> Vec<u8> {
    let mut sig = vec![0x5a; 64];
    sig.extend(flag);
    sig
  }

  fn control_block(parity: u8, depth: usize) -> Vec<u8> {
    let mut control = vec![0xc0 | parity];
    control.extend(vec![0x33; 32 + 32 * depth]);
    control
  }

  fn witness(elements: &[&[u8]]) -> Witness {
    Witness::from_slice(elements)
  }

  const FLAGS: [u8; 6] = [0x01, 0x02, 0x03, 0x81, 0x82, 0x83];

  #[test]
  fn taproot_key_path() {
    let none = ScriptBuf::new();
    assert_eq!(
      input_sighash(&none, &witness(&[&schnorr(None)])),
      Some(0x00)
    );
    for flag in FLAGS {
      assert_eq!(
        input_sighash(&none, &witness(&[&schnorr(Some(flag))])),
        Some(flag)
      );
      // With an annex.
      assert_eq!(
        input_sighash(&none, &witness(&[&schnorr(Some(flag)), &[0x50, 1, 2]])),
        Some(flag)
      );
    }
    assert_eq!(
      input_sighash(&none, &witness(&[&schnorr(None), &[0x50]])),
      Some(0x00)
    );
    // Not a signature.
    assert_eq!(input_sighash(&none, &witness(&[&[0x51; 63]])), None);
    assert_eq!(input_sighash(&none, &witness(&[&[0x51; 66]])), None);
    assert_eq!(
      input_sighash(&none, &witness(&[&[0x50; 66], &[0x50]])),
      None
    );
  }

  #[test]
  fn taproot_script_path() {
    let none = ScriptBuf::new();
    let tapscript = vec![0x20; 34];
    for parity in [0, 1] {
      for flag in [None, Some(0x83), Some(0x01)] {
        let sig = schnorr(flag);
        let control = control_block(parity, 2);
        assert_eq!(
          input_sighash(&none, &witness(&[&sig, &tapscript, &control])),
          Some(flag.unwrap_or(0)),
        );
        // The first signature: multisig stacks have empty slots and later
        // signatures; preimages and other data are skipped.
        assert_eq!(
          input_sighash(
            &none,
            &witness(&[
              &[],
              &[7; 32],
              &sig,
              &schnorr(Some(0x02)),
              &tapscript,
              &control
            ])
          ),
          Some(flag.unwrap_or(0)),
        );
        // Annex.
        assert_eq!(
          input_sighash(&none, &witness(&[&sig, &tapscript, &control, &[0x50, 9]])),
          Some(flag.unwrap_or(0)),
        );
      }
    }
    // No signature below the tapscript.
    assert_eq!(
      input_sighash(
        &none,
        &witness(&[&[1; 32], &tapscript, &control_block(0, 0)])
      ),
      None
    );
    // A control block for another leaf version is not tapscript.
    let mut other = control_block(0, 0);
    other[0] = 0xc2;
    assert_eq!(
      input_sighash(&none, &witness(&[&schnorr(None), &tapscript, &other])),
      None
    );
  }

  #[test]
  fn segwit_v0() {
    let none = ScriptBuf::new();
    for flag in FLAGS {
      // P2WPKH.
      assert_eq!(
        input_sighash(&none, &witness(&[&der(flag), &pubkey()])),
        Some(flag)
      );
      // P2WSH 2-of-3: dummy, two signatures, witness script.
      assert_eq!(
        input_sighash(
          &none,
          &witness(&[&[], &der(flag), &der(0x01), &[0x52; 105]])
        ),
        Some(flag)
      );
      // P2SH-P2WPKH: the scriptSig pushes the redeem script.
      let redeem = ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([9; 20]));
      let script_sig = script::Builder::new()
        .push_slice(push(redeem.as_bytes()))
        .into_script();
      assert_eq!(
        input_sighash(&script_sig, &witness(&[&der(flag), &pubkey()])),
        Some(flag)
      );
    }
    // The witness script itself is never taken for a signature.
    assert_eq!(
      input_sighash(&none, &witness(&[&[1; 20], &der(0x01)])),
      None
    );
    // Nor are malformed signatures.
    let mut bad = der(0x01);
    bad[0] = 0x31;
    assert_eq!(input_sighash(&none, &witness(&[&bad, &pubkey()])), None);
  }

  #[test]
  fn legacy() {
    let empty = Witness::new();
    for flag in FLAGS {
      // P2PKH.
      let p2pkh = script::Builder::new()
        .push_slice(push(&der(flag)))
        .push_slice(push(&pubkey()))
        .into_script();
      assert_eq!(input_sighash(&p2pkh, &empty), Some(flag));
      // P2PK.
      let p2pk = script::Builder::new()
        .push_slice(push(&der(flag)))
        .into_script();
      assert_eq!(input_sighash(&p2pk, &empty), Some(flag));
      // P2SH 2-of-3 multisig.
      let multisig = script::Builder::new()
        .push_opcode(opcodes::OP_0)
        .push_slice(push(&der(flag)))
        .push_slice(push(&der(0x02)))
        .push_slice(push(&[0x52; 105]))
        .into_script();
      assert_eq!(input_sighash(&multisig, &empty), Some(flag));
    }
    // Non-push opcodes stop the scan.
    let script = script::Builder::new()
      .push_opcode(OP_NOP)
      .push_slice(push(&der(0x01)))
      .into_script();
    assert_eq!(input_sighash(&script, &empty), None);
    assert_eq!(input_sighash(&ScriptBuf::new(), &empty), None);
    // Truncated push.
    assert_eq!(
      input_sighash(&ScriptBuf::from_bytes(vec![0x48, 0x30]), &empty),
      None
    );
  }

  #[test]
  fn strict_der() {
    for flag in FLAGS {
      assert!(is_der_signature(&der(flag)));
    }
    // Low-S, minimal encodings from the DER grammar's edge cases.
    let mut minimal = vec![0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01, 0x01];
    assert!(is_der_signature(&minimal));
    minimal[4] = 0x80; // negative R
    assert!(!is_der_signature(&minimal));
    let padded = [0x30, 0x07, 0x02, 0x02, 0x00, 0x01, 0x02, 0x01, 0x01, 0x01];
    assert!(!is_der_signature(&padded)); // unnecessary leading zero
    let needed = [0x30, 0x07, 0x02, 0x02, 0x00, 0x80, 0x02, 0x01, 0x01, 0x01];
    assert!(is_der_signature(&needed));
    assert!(!is_der_signature(&[0x30; 8]));
    assert!(!is_der_signature(&[0x30; 74]));
    let mut wrong_length = der(0x01);
    wrong_length[1] += 1;
    assert!(!is_der_signature(&wrong_length));
  }

  /// `OP_FALSE OP_IF "ord" <items> OP_ENDIF`.
  fn envelope(builder: script::Builder, items: &[&[u8]]) -> script::Builder {
    let mut builder = builder
      .push_opcode(opcodes::OP_FALSE)
      .push_opcode(OP_IF)
      .push_slice(b"ord");
    for item in items {
      builder = builder.push_slice(push(item));
    }
    builder.push_opcode(OP_ENDIF)
  }

  fn reveal(script: ScriptBuf) -> Witness {
    witness(&[&schnorr(None), script.as_bytes(), &control_block(0, 0)])
  }

  fn envelopes(script: ScriptBuf) -> Vec<Envelope> {
    input_envelopes(&reveal(script))
  }

  const NO_POINTER: Envelope = Envelope {
    pointer: None,
    unrecognized_even_field: false,
  };

  fn at(pointer: u64) -> Envelope {
    Envelope {
      pointer: Some(pointer),
      unrecognized_even_field: false,
    }
  }

  /// ord unbinds these.
  fn unbound(pointer: Option<u64>) -> Envelope {
    Envelope {
      pointer,
      unrecognized_even_field: true,
    }
  }

  /// A typical reveal script: key check, then the envelope.
  fn inscription(items: &[&[u8]]) -> ScriptBuf {
    envelope(
      script::Builder::new()
        .push_slice([0x20; 32])
        .push_opcode(OP_CHECKSIG),
      items,
    )
    .into_script()
  }

  #[test]
  fn no_envelopes() {
    assert!(input_envelopes(&Witness::new()).is_empty());
    assert!(envelopes(ScriptBuf::new()).is_empty());
    assert!(
      envelopes(
        script::Builder::new()
          .push_slice([0x20; 32])
          .push_opcode(OP_CHECKSIG)
          .into_script()
      )
      .is_empty()
    );
    // Key path spends, with or without annex, have no tapscript.
    let script = inscription(&[&[1], b"text/plain", &[], b"hi"]);
    assert!(input_envelopes(&witness(&[script.as_bytes()])).is_empty());
    assert!(input_envelopes(&witness(&[script.as_bytes(), &[0x50]])).is_empty());
    // Wrong protocol, no OP_FALSE, no OP_ENDIF.
    let wrong = script::Builder::new()
      .push_opcode(opcodes::OP_FALSE)
      .push_opcode(OP_IF)
      .push_slice(b"orc")
      .push_opcode(OP_ENDIF)
      .into_script();
    assert!(envelopes(wrong).is_empty());
    let no_false = script::Builder::new()
      .push_opcode(OP_IF)
      .push_slice(b"ord")
      .push_opcode(OP_ENDIF)
      .into_script();
    assert!(envelopes(no_false).is_empty());
    let no_endif = script::Builder::new()
      .push_opcode(opcodes::OP_FALSE)
      .push_opcode(OP_IF)
      .push_slice(b"ord")
      .push_slice(b"x")
      .into_script();
    assert!(envelopes(no_endif).is_empty());
    // A non-push opcode inside ends the envelope.
    let opcode = script::Builder::new()
      .push_opcode(opcodes::OP_FALSE)
      .push_opcode(OP_IF)
      .push_slice(b"ord")
      .push_opcode(OP_NOP)
      .push_opcode(OP_ENDIF)
      .into_script();
    assert!(envelopes(opcode).is_empty());
  }

  #[test]
  fn one_envelope() {
    // No fields, no body.
    assert_eq!(
      envelopes(envelope(script::Builder::new(), &[]).into_script()),
      [NO_POINTER]
    );
    assert_eq!(
      envelopes(inscription(&[&[1], b"text/plain", &[], b"hello"])),
      [NO_POINTER]
    );
    // The envelope in a P2WSH-style witness counts too, as in ord.
    let script = inscription(&[&[1], b"text/plain", &[], b"hello"]);
    assert_eq!(
      input_envelopes(&witness(&[script.as_bytes(), &[0x51]])),
      [NO_POINTER]
    );
    // With an annex, the tapscript is third from last.
    assert_eq!(
      input_envelopes(&witness(&[
        &schnorr(None),
        script.as_bytes(),
        &control_block(1, 1),
        &[0x50, 0]
      ])),
      [NO_POINTER]
    );
  }

  #[test]
  fn pointers() {
    // Tag 2, little-endian, trailing zeros allowed past eight bytes.
    let cases: [(&[u8], Option<u64>); 8] = [
      (&[], Some(0)),
      (&[0], Some(0)),
      (&[1], Some(1)),
      (&[0x10, 0x27], Some(10_000)),
      (&[1, 2, 3, 4, 5, 6, 7, 8], Some(0x0807060504030201)),
      (&[1, 2, 3, 4, 5, 6, 7, 8, 0, 0], Some(0x0807060504030201)),
      (&[1, 2, 3, 4, 5, 6, 7, 8, 9], None),
      (&[0, 0, 0, 0, 0, 0, 0, 0, 0, 1], None),
    ];
    for (value, expected) in cases {
      assert_eq!(
        envelopes(inscription(&[&[2], value, &[1], b"text/plain", &[], b"hi"])),
        [Envelope {
          pointer: expected,
          unrecognized_even_field: false
        }],
        "{value:?}"
      );
    }
    // The first of duplicate pointer tags wins (ord's `Tag::take`); the
    // second is left over, an unrecognized even field.
    assert_eq!(
      envelopes(inscription(&[&[2], &[5], &[2], &[6]])),
      [unbound(Some(5))]
    );
    assert_eq!(
      envelopes(inscription(&[
        &[2],
        &[1, 2, 3, 4, 5, 6, 7, 8, 9],
        &[2],
        &[6]
      ])),
      [unbound(None)]
    );
    // Only before the body, and only as a complete field.
    assert_eq!(envelopes(inscription(&[&[], &[2], &[7]])), [NO_POINTER]);
    assert_eq!(envelopes(inscription(&[&[1], b"a", &[2]])), [NO_POINTER]);
    // An empty push in a value position is a value, not the body.
    assert_eq!(envelopes(inscription(&[&[1], &[], &[2], &[9]])), [at(9)]);
    // Tag [2, 0] is not tag 2 but an unknown even tag.
    assert_eq!(envelopes(inscription(&[&[2, 0], &[9]])), [unbound(None)]);
    // OP_2 is the tag too (a "pushnum" curse, still indexed).
    let pushnum = script::Builder::new()
      .push_opcode(opcodes::OP_FALSE)
      .push_opcode(OP_IF)
      .push_slice(b"ord")
      .push_opcode(OP_PUSHNUM_2)
      .push_opcode(OP_PUSHNUM_16)
      .push_opcode(OP_ENDIF)
      .into_script();
    assert_eq!(envelopes(pushnum), [at(16)]);
    let negative = script::Builder::new()
      .push_opcode(opcodes::OP_FALSE)
      .push_opcode(OP_IF)
      .push_slice(b"ord")
      .push_opcode(OP_PUSHNUM_2)
      .push_opcode(OP_PUSHNUM_NEG1)
      .push_opcode(OP_ENDIF)
      .into_script();
    assert_eq!(envelopes(negative), [at(0x81)]);
  }

  #[test]
  fn several_envelopes() {
    let mut builder = script::Builder::new()
      .push_slice([0x20; 32])
      .push_opcode(OP_CHECKSIG);
    builder = envelope(builder, &[&[1], b"text/plain", &[], b"a"]);
    builder = envelope(builder, &[&[2], &[0x22, 0x02], &[], b"b"]);
    // Duplicate and unknown even fields: cursed, still inscriptions.
    builder = envelope(builder, &[&[1], b"x", &[1], b"y", &[4], &[1]]);
    builder = envelope(builder, &[&[2], &[3]]);
    assert_eq!(
      envelopes(builder.into_script()),
      [NO_POINTER, at(0x0222), unbound(None), at(3)]
    );
  }

  #[test]
  fn unrecognized_even_fields() {
    let flagged: [&[&[u8]]; 7] = [
      // Unknown even tags, including ord's reserved "unbound" tag 66.
      &[&[4], &[1]],
      &[&[66], &[1]],
      &[&[1], b"text/plain", &[22], &[0], &[], b"body"],
      // A tag of one zero byte (not the empty body separator).
      &[&[0], &[1]],
      // Multi-byte tags count by their first byte.
      &[&[0x0e, 0x01], &[1]],
      // A repeated pointer.
      &[&[2], &[1], &[1], b"x", &[2], &[1]],
      // An empty value is still a value.
      &[&[4], &[]],
    ];
    for items in flagged {
      assert!(
        envelopes(inscription(items))[0].unrecognized_even_field,
        "{items:?}"
      );
    }

    let not_flagged: [&[&[u8]]; 8] = [
      &[],
      &[&[1], b"text/plain", &[], b"body"],
      // Unknown odd tags, multi-byte tags with an odd first byte.
      &[&[255], &[1], &[0x0f, 0x00], &[1]],
      // Known tags, repeated: duplicate fields, but odd.
      &[&[1], b"a", &[1], b"b", &[7], b"m", &[7], b"n"],
      // Parents, metadata and properties may repeat.
      &[
        &[3],
        &[1; 36],
        &[3],
        &[2; 36],
        &[5],
        &[0xa0],
        &[5],
        &[0xa0],
        &[17],
        &[1],
        &[17],
        &[2],
      ],
      // An even tag without a value (incomplete field).
      &[&[1], b"a", &[4]],
      // Even tags in the body.
      &[&[], &[4], &[1], &[2], &[2]],
      // A single pointer.
      &[&[2], &[0x10]],
    ];
    for items in not_flagged {
      assert!(
        !envelopes(inscription(items))[0].unrecognized_even_field,
        "{items:?}"
      );
    }

    // OP_PUSHNUM tags: OP_4 is even, OP_5 and OP_1NEGATE (0x81) odd.
    for (op, flagged) in [
      (OP_PUSHNUM_4, true),
      (OP_PUSHNUM_5, false),
      (OP_PUSHNUM_NEG1, false),
    ] {
      let script = script::Builder::new()
        .push_opcode(opcodes::OP_FALSE)
        .push_opcode(OP_IF)
        .push_slice(b"ord")
        .push_opcode(op)
        .push_opcode(OP_PUSHNUM_1)
        .push_opcode(OP_ENDIF)
        .into_script();
      assert_eq!(
        envelopes(script)[0].unrecognized_even_field,
        flagged,
        "{op:?}"
      );
    }
  }

  #[test]
  fn malformed_scripts_have_no_envelopes() {
    // A truncated push anywhere, even after a complete envelope, discards
    // the input's envelopes.
    let mut script = inscription(&[&[1], b"text/plain", &[], b"hi"]).into_bytes();
    script.push(0x01);
    assert!(envelopes(ScriptBuf::from_bytes(script)).is_empty());
    let mut script = inscription(&[]).into_bytes();
    script.extend([0x4d, 0xff]);
    assert!(envelopes(ScriptBuf::from_bytes(script)).is_empty());
  }

  #[test]
  fn stutter_and_non_minimal_encodings() {
    // OP_FALSE OP_FALSE OP_IF "ord" OP_ENDIF: the second OP_FALSE starts it.
    let stutter = script::Builder::new()
      .push_opcode(opcodes::OP_FALSE)
      .push_opcode(opcodes::OP_FALSE)
      .push_opcode(OP_IF)
      .push_slice(b"ord")
      .push_opcode(OP_ENDIF)
      .into_script();
    assert_eq!(envelopes(stutter), [NO_POINTER]);
    // OP_PUSHDATA1 0 for OP_FALSE and OP_PUSHDATA2 for "ord".
    let non_minimal = ScriptBuf::from_bytes(vec![
      0x4c,
      0x00,
      OPCODE_IF,
      0x4d,
      0x03,
      0x00,
      b'o',
      b'r',
      b'd',
      0x4c,
      0x01,
      0x02,
      0x01,
      0x2a,
      OPCODE_ENDIF,
    ]);
    assert_eq!(envelopes(non_minimal), [at(42)]);
    // "ord" in data does not open an envelope by itself.
    let data = script::Builder::new()
      .push_slice(b"ord")
      .push_opcode(OP_DROP)
      .into_script();
    assert!(envelopes(data).is_empty());
  }

  #[test]
  fn instructions_match_rust_bitcoin() {
    let scripts = [
      inscription(&[&[1], b"text/plain", &[], &[7; 600], &[8; 80]]).into_bytes(),
      vec![
        0x4c, 0x00, 0x4d, 0x01, 0x00, 0xaa, 0x4e, 0x02, 0, 0, 0, 1, 2, 0x63, 0x4f, 0x51,
      ],
      vec![0x4c],
      vec![0x4d, 0x01],
      vec![0x4e, 0x05, 0, 0, 0, 1, 2],
      vec![0x02, 0x01],
      vec![0x00],
    ];
    for bytes in scripts {
      let ours = Instructions::new(&bytes).collect::<Vec<_>>();
      let theirs = Script::from_bytes(&bytes)
        .instructions()
        .map(|instruction| match instruction {
          Ok(bitcoin::script::Instruction::PushBytes(push)) => {
            Ok(Instruction::Push(push.as_bytes()))
          }
          Ok(bitcoin::script::Instruction::Op(op)) => Ok(Instruction::Op(op.to_u8())),
          Err(_) => Err(()),
        })
        .collect::<Vec<_>>();
      assert_eq!(ours, theirs, "{bytes:02x?}");
    }
  }
}
