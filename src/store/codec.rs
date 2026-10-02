//! Compact binary encodings of the store's values (the layouts are documented
//! in `store/mod.rs`). Integers are LEB128 varints unless noted; txids are 32
//! raw bytes in internal (consensus) byte order.

use {
  crate::model::{
    BlockDelta, InscriptionEvent, LocationKind, MarketDelta, Move, MoveInput, MoveOutput, Piece,
    RowDelta, RowStats, Sale, SaleKind, TrackedUtxo, UtxoKind,
  },
  anyhow::{Context, bail, ensure},
  bitcoin::{
    BlockHash, OutPoint, ScriptBuf, Txid,
    block::Header,
    consensus::{Decodable, Encodable},
    hashes::Hash,
  },
  std::collections::{HashMap, HashSet},
};

/// Key of the `utxos` table: txid then big-endian vout, so the outputs of one
/// transaction are adjacent.
pub(crate) type OutPointKey = [u8; 36];

pub(crate) fn outpoint_key(outpoint: &OutPoint) -> OutPointKey {
  let mut key = [0; 36];
  key[..32].copy_from_slice(outpoint.txid.as_byte_array());
  key[32..].copy_from_slice(&outpoint.vout.to_be_bytes());
  key
}

pub(crate) fn outpoint_from_key(key: &OutPointKey) -> OutPoint {
  OutPoint {
    txid: Txid::from_byte_array(key[..32].try_into().unwrap()),
    vout: u32::from_be_bytes(key[32..].try_into().unwrap()),
  }
}

/// Key of the `overwritten` table: outpoint key then the big-endian height of
/// the block that replaced it.
pub(crate) type OverwrittenKey = [u8; 40];

pub(crate) fn overwritten_key(outpoint: &OutPoint, height: u32) -> OverwrittenKey {
  let mut key = [0; 40];
  key[..36].copy_from_slice(&outpoint_key(outpoint));
  key[36..].copy_from_slice(&height.to_be_bytes());
  key
}

pub(crate) fn overwritten_from_key(key: &OverwrittenKey) -> (OutPoint, u32) {
  (
    outpoint_from_key(key[..36].try_into().unwrap()),
    u32::from_be_bytes(key[36..].try_into().unwrap()),
  )
}

/// Scripts up to this length are script-index keys as they are; longer ones
/// (bare multisig, nonstandard, big OP_RETURNs) are keyed by their SHA-256.
pub(crate) const MAX_RAW_SCRIPT_KEY: usize = 64;

/// Key of the `scripts` multimap: `0x00 ‖ script` for scripts of at most
/// `MAX_RAW_SCRIPT_KEY` bytes, else `0x01 ‖ sha256(script)`.
pub(crate) fn script_key(script: &[u8]) -> Vec<u8> {
  let mut key = Vec::with_capacity(1 + script.len().min(32));
  if script.len() <= MAX_RAW_SCRIPT_KEY {
    key.push(0);
    key.extend_from_slice(script);
  } else {
    key.push(1);
    key.extend_from_slice(bitcoin::hashes::sha256::Hash::hash(script).as_byte_array());
  }
  key
}

pub(crate) fn header_bytes(header: &Header) -> [u8; 80] {
  let mut bytes = [0; 80];
  let mut cursor = &mut bytes[..];
  header
    .consensus_encode(&mut cursor)
    .expect("an 80-byte buffer fits a header");
  bytes
}

pub(crate) fn header_from_bytes(bytes: &[u8]) -> anyhow::Result<Header> {
  let mut reader = bytes;
  Header::consensus_decode(&mut reader).context("corrupt header record")
}

fn kind_code(kind: UtxoKind) -> u8 {
  match kind {
    UtxoKind::Spendable => 0,
    UtxoKind::Unspendable => 1,
  }
}

fn kind_from_code(code: u8) -> anyhow::Result<UtxoKind> {
  Ok(match code {
    0 => UtxoKind::Spendable,
    1 => UtxoKind::Unspendable,
    _ => bail!("corrupt store record: utxo kind {code}"),
  })
}

pub(crate) fn location_code(kind: LocationKind) -> u8 {
  match kind {
    LocationKind::Unspent => 0,
    LocationKind::Burned => 1,
    LocationKind::Lost => 2,
    LocationKind::Overwritten => 3,
  }
}

fn location_from_code(code: u8) -> anyhow::Result<LocationKind> {
  Ok(match code {
    0 => LocationKind::Unspent,
    1 => LocationKind::Burned,
    2 => LocationKind::Lost,
    3 => LocationKind::Overwritten,
    _ => bail!("corrupt store record: location kind {code}"),
  })
}

/// Append-only encoder.
#[derive(Default)]
pub(crate) struct Enc {
  pub(crate) buf: Vec<u8>,
}

impl Enc {
  pub(crate) fn with_capacity(capacity: usize) -> Self {
    Self {
      buf: Vec::with_capacity(capacity),
    }
  }

  pub(crate) fn clear(&mut self) {
    self.buf.clear();
  }

  pub(crate) fn u8(&mut self, value: u8) {
    self.buf.push(value);
  }

  pub(crate) fn varint(&mut self, mut value: u64) {
    while value >= 0x80 {
      self.buf.push((value as u8) | 0x80);
      value >>= 7;
    }
    self.buf.push(value as u8);
  }

  pub(crate) fn raw(&mut self, bytes: &[u8]) {
    self.buf.extend_from_slice(bytes);
  }

  pub(crate) fn blob(&mut self, bytes: &[u8]) {
    self.varint(bytes.len() as u64);
    self.raw(bytes);
  }

  pub(crate) fn txid(&mut self, txid: &Txid) {
    self.raw(txid.as_byte_array());
  }

  pub(crate) fn outpoint(&mut self, outpoint: &OutPoint) {
    self.txid(&outpoint.txid);
    self.varint(outpoint.vout.into());
  }

  /// Offsets are stored as the gap after the previous piece.
  pub(crate) fn pieces(&mut self, pieces: &[Piece]) {
    self.varint(pieces.len() as u64);
    let mut next_offset = 0;
    for piece in pieces {
      self.varint(piece.offset.wrapping_sub(next_offset));
      self.varint(piece.start);
      self.varint(piece.len);
      next_offset = piece.offset_end();
    }
  }

  pub(crate) fn utxo(&mut self, utxo: &TrackedUtxo) {
    self.varint(utxo.value);
    self.varint(utxo.height.into());
    self.u8(kind_code(utxo.kind));
    self.blob(utxo.script_pubkey.as_bytes());
    self.pieces(&utxo.pieces);
  }

  pub(crate) fn row_stats(&mut self, stats: &RowStats) {
    for value in [
      stats.mined,
      stats.located,
      stats.burned,
      stats.lost,
      stats.overwritten,
      stats.extracted,
      stats.holders,
      stats.extracted_holders,
    ] {
      self.varint(value);
    }
  }

  pub(crate) fn utxo_list(&mut self, utxos: &[(OutPoint, TrackedUtxo)]) {
    self.varint(utxos.len() as u64);
    for (outpoint, utxo) in utxos {
      self.outpoint(outpoint);
      self.utxo(utxo);
    }
  }
}

/// Bounds-checked decoder over one record.
pub(crate) struct Dec<'a> {
  buf: &'a [u8],
  pos: usize,
}

impl<'a> Dec<'a> {
  pub(crate) fn new(buf: &'a [u8]) -> Self {
    Self { buf, pos: 0 }
  }

  pub(crate) fn is_empty(&self) -> bool {
    self.pos == self.buf.len()
  }

  pub(crate) fn finish(&self) -> anyhow::Result<()> {
    ensure!(
      self.is_empty(),
      "corrupt store record: {} trailing bytes",
      self.buf.len() - self.pos
    );
    Ok(())
  }

  pub(crate) fn u8(&mut self) -> anyhow::Result<u8> {
    let Some(&byte) = self.buf.get(self.pos) else {
      bail!("corrupt store record: truncated");
    };
    self.pos += 1;
    Ok(byte)
  }

  pub(crate) fn varint(&mut self) -> anyhow::Result<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
      let byte = self.u8()?;
      ensure!(
        shift < 64 && (shift != 63 || byte <= 1),
        "corrupt store record: varint overflow"
      );
      value |= u64::from(byte & 0x7f) << shift;
      if byte & 0x80 == 0 {
        return Ok(value);
      }
      shift += 7;
    }
  }

  pub(crate) fn u32(&mut self) -> anyhow::Result<u32> {
    u32::try_from(self.varint()?).context("corrupt store record: u32 out of range")
  }

  pub(crate) fn len(&mut self) -> anyhow::Result<usize> {
    let len = usize::try_from(self.varint()?).context("corrupt store record: length")?;
    ensure!(
      len <= self.buf.len() - self.pos,
      "corrupt store record: length {len} past the end"
    );
    Ok(len)
  }

  pub(crate) fn raw(&mut self, len: usize) -> anyhow::Result<&'a [u8]> {
    ensure!(
      len <= self.buf.len() - self.pos,
      "corrupt store record: truncated"
    );
    let bytes = &self.buf[self.pos..self.pos + len];
    self.pos += len;
    Ok(bytes)
  }

  pub(crate) fn blob(&mut self) -> anyhow::Result<&'a [u8]> {
    let len = self.len()?;
    self.raw(len)
  }

  pub(crate) fn txid(&mut self) -> anyhow::Result<Txid> {
    Ok(Txid::from_byte_array(self.raw(32)?.try_into().unwrap()))
  }

  pub(crate) fn outpoint(&mut self) -> anyhow::Result<OutPoint> {
    Ok(OutPoint {
      txid: self.txid()?,
      vout: self.u32()?,
    })
  }

  pub(crate) fn pieces(&mut self) -> anyhow::Result<Vec<Piece>> {
    let n = self.len()?;
    let mut pieces = Vec::with_capacity(n);
    let mut next_offset = 0u64;
    for _ in 0..n {
      let offset = next_offset.wrapping_add(self.varint()?);
      let start = self.varint()?;
      let len = self.varint()?;
      ensure!(len > 0, "corrupt store record: empty piece");
      let piece = Piece { offset, start, len };
      next_offset = piece.offset_end();
      pieces.push(piece);
    }
    Ok(pieces)
  }

  pub(crate) fn utxo(&mut self) -> anyhow::Result<TrackedUtxo> {
    Ok(TrackedUtxo {
      value: self.varint()?,
      height: self.u32()?,
      kind: kind_from_code(self.u8()?)?,
      script_pubkey: ScriptBuf::from_bytes(self.blob()?.to_vec()),
      pieces: self.pieces()?,
    })
  }

  pub(crate) fn row_stats(&mut self) -> anyhow::Result<RowStats> {
    Ok(RowStats {
      mined: self.varint()?,
      located: self.varint()?,
      burned: self.varint()?,
      lost: self.varint()?,
      overwritten: self.varint()?,
      extracted: self.varint()?,
      holders: self.varint()?,
      extracted_holders: self.varint()?,
    })
  }

  pub(crate) fn utxo_list(&mut self) -> anyhow::Result<Vec<(OutPoint, TrackedUtxo)>> {
    let n = self.len()?;
    let mut utxos = Vec::with_capacity(n);
    for _ in 0..n {
      utxos.push((self.outpoint()?, self.utxo()?));
    }
    Ok(utxos)
  }
}

pub(crate) fn encode_utxo(utxo: &TrackedUtxo) -> Vec<u8> {
  let mut enc = Enc::with_capacity(32 + utxo.script_pubkey.len() + 12 * utxo.pieces.len());
  enc.utxo(utxo);
  enc.buf
}

pub(crate) fn decode_utxo(bytes: &[u8]) -> anyhow::Result<TrackedUtxo> {
  let mut dec = Dec::new(bytes);
  let utxo = dec.utxo()?;
  dec.finish()?;
  Ok(utxo)
}

pub(crate) fn encode_row_stats(stats: &RowStats) -> Vec<u8> {
  let mut enc = Enc::with_capacity(16);
  enc.row_stats(stats);
  enc.buf
}

pub(crate) fn decode_row_stats(bytes: &[u8]) -> anyhow::Result<RowStats> {
  let mut dec = Dec::new(bytes);
  let stats = dec.row_stats()?;
  dec.finish()?;
  Ok(stats)
}

/// Value of the sat locator: where the piece starting at the key sat is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Loc {
  pub(crate) kind: LocationKind,
  /// ord's null outpoint for `Lost`.
  pub(crate) outpoint: OutPoint,
  /// Offset of the piece's first sat in the outpoint (or the lost stream).
  pub(crate) offset: u64,
  pub(crate) len: u64,
}

pub(crate) fn encode_loc(loc: &Loc, enc: &mut Enc) {
  enc.u8(location_code(loc.kind));
  if loc.kind != LocationKind::Lost {
    enc.outpoint(&loc.outpoint);
  }
  enc.varint(loc.offset);
  enc.varint(loc.len);
}

pub(crate) fn decode_loc(bytes: &[u8]) -> anyhow::Result<Loc> {
  let mut dec = Dec::new(bytes);
  let kind = location_from_code(dec.u8()?)?;
  let outpoint = if kind == LocationKind::Lost {
    OutPoint::null()
  } else {
    dec.outpoint()?
  };
  let loc = Loc {
    kind,
    outpoint,
    offset: dec.varint()?,
    len: dec.varint()?,
  };
  dec.finish()?;
  Ok(loc)
}

pub(crate) fn encode_lost(piece: &Piece) -> Vec<u8> {
  let mut enc = Enc::with_capacity(12);
  enc.varint(piece.start);
  enc.varint(piece.len);
  enc.buf
}

pub(crate) fn decode_lost(offset: u64, bytes: &[u8]) -> anyhow::Result<Piece> {
  let mut dec = Dec::new(bytes);
  let piece = Piece {
    offset,
    start: dec.varint()?,
    len: dec.varint()?,
  };
  dec.finish()?;
  Ok(piece)
}

/// Mined counters: count, then one varint per row.
pub(crate) fn encode_u64s(values: &[u64]) -> Vec<u8> {
  let mut enc = Enc::with_capacity(values.len() * 3 + 2);
  enc.varint(values.len() as u64);
  for &value in values {
    enc.varint(value);
  }
  enc.buf
}

pub(crate) fn decode_u64s(bytes: &[u8]) -> anyhow::Result<Vec<u64>> {
  let mut dec = Dec::new(bytes);
  let n = dec.len()?;
  let mut values = Vec::with_capacity(n);
  for _ in 0..n {
    values.push(dec.varint()?);
  }
  dec.finish()?;
  Ok(values)
}

// ---------------------------------------------------------------------------
// Moves
//
// One record per block:
//
//   version u8 (= MOVES_V1)
//   flags u8                      bit 0: some input refers to another block
//   scripts: n, len, n × offset u32 LE, the scripts concatenated (`len`
//            bytes): every distinct script_pubkey of the block, script i
//            spanning offset i to offset i + 1 (or `len`)
//   moves: n
//   index: n × (txid[..8] as u64 LE, offset u32 LE), sorted: where each
//          move starts in the body, to find a move by txid without
//          decoding the others
//   body: n × move
//
//   move    = txid[32] tx_index coinbase:u8 input_value output_value
//             inputs outputs fee:pieces mined:pieces lost:pieces
//   inputs  = n, n × (vin tag body)
//             tag 0 (full):        outpoint value height kind:u8 script_index pieces
//             tag 1 (this block):  move_index vout
//                  the output `vout` of the move `move_index` of this block
//             tag 2 (older block): height_back outpoint
//                  the output of the move with that txid in block
//                  `height - height_back`, whose moves are retained
//   outputs = n, n × (vout value script_index kind:u8 pieces)
//
// A spent UTXO never changes between its creation and its spend, so the
// output record of the move that created it is the input's `TrackedUtxo`
// (with `height` = the creating block).
// ---------------------------------------------------------------------------

const MOVES_V1: u8 = 1;
const FLAG_EXTERNAL_REFS: u8 = 1;

const INPUT_FULL: u8 = 0;
const INPUT_SAME_BLOCK: u8 = 1;
const INPUT_OLDER_BLOCK: u8 = 2;

const INDEX_ENTRY: usize = 12;

/// Which inputs may be stored as references instead of in full.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RefPolicy<'a> {
  /// Inputs created at or above this height (and below the block's own
  /// height) may refer to the creating block's moves. `None`: never.
  pub(crate) older_from: Option<u32>,
  /// Outputs that the moves of their block do not hold as they are: never
  /// referred to.
  pub(crate) unreferenced: Option<&'a HashSet<OutPoint>>,
}

impl RefPolicy<'static> {
  /// Self-contained records (undo records, bounded retention).
  pub(crate) const SELF_CONTAINED: Self = Self {
    older_from: None,
    unreferenced: None,
  };
}

struct ScriptTable<'a> {
  index: HashMap<&'a [u8], u64>,
  scripts: Vec<&'a [u8]>,
}

impl<'a> ScriptTable<'a> {
  fn id(&mut self, script: &'a [u8]) -> u64 {
    let next = self.scripts.len() as u64;
    *self.index.entry(script).or_insert_with(|| {
      self.scripts.push(script);
      next
    })
  }
}

enum InputEncoding {
  Full,
  SameBlock { move_index: usize },
  OlderBlock,
}

fn txid_prefix(txid: &Txid) -> u64 {
  u64::from_le_bytes(txid.as_byte_array()[..8].try_into().unwrap())
}

/// Encodes one block's moves (`height` is the block's height).
pub(crate) fn encode_moves(moves: &[Move], height: u32, policy: RefPolicy) -> Vec<u8> {
  // Outputs created in this block, for same-block input references.
  let mut created: HashMap<(Txid, u32), (usize, &MoveOutput)> = HashMap::new();
  for (i, m) in moves.iter().enumerate() {
    for output in &m.outputs {
      created.insert((m.txid, output.vout), (i, output));
    }
  }

  let mut scripts = ScriptTable {
    index: HashMap::new(),
    scripts: Vec::new(),
  };

  let mut flags = 0;

  let mut encodings = Vec::with_capacity(moves.len());
  for m in moves {
    let mut inputs = Vec::with_capacity(m.inputs.len());
    for input in &m.inputs {
      let utxo = &input.utxo;
      let encoding = match created.get(&(input.outpoint.txid, input.outpoint.vout)) {
        Some(&(move_index, output))
          if utxo.height == height
            && output.value == utxo.value
            && output.kind == utxo.kind
            && output.script_pubkey == utxo.script_pubkey
            && output.pieces == utxo.pieces =>
        {
          InputEncoding::SameBlock { move_index }
        }
        _ => match policy.older_from {
          Some(from)
            if utxo.height >= from
              && utxo.height < height
              && !policy
                .unreferenced
                .is_some_and(|unreferenced| unreferenced.contains(&input.outpoint)) =>
          {
            flags |= FLAG_EXTERNAL_REFS;
            InputEncoding::OlderBlock
          }
          _ => {
            scripts.id(utxo.script_pubkey.as_bytes());
            InputEncoding::Full
          }
        },
      };
      inputs.push(encoding);
    }
    for output in &m.outputs {
      scripts.id(output.script_pubkey.as_bytes());
    }
    encodings.push(inputs);
  }

  let mut body = Enc::with_capacity(moves.len() * 160);
  let mut index = Vec::with_capacity(moves.len());
  for (m, inputs) in moves.iter().zip(encodings) {
    index.push((txid_prefix(&m.txid), body.buf.len() as u32));
    body.txid(&m.txid);
    body.varint(m.tx_index.into());
    body.u8(m.coinbase.into());
    body.varint(m.input_value);
    body.varint(m.output_value);
    body.varint(m.inputs.len() as u64);
    for (input, encoding) in m.inputs.iter().zip(inputs) {
      body.varint(input.vin.into());
      match encoding {
        InputEncoding::Full => {
          body.u8(INPUT_FULL);
          body.outpoint(&input.outpoint);
          body.varint(input.utxo.value);
          body.varint(input.utxo.height.into());
          body.u8(kind_code(input.utxo.kind));
          body.varint(scripts.index[input.utxo.script_pubkey.as_bytes()]);
          body.pieces(&input.utxo.pieces);
        }
        InputEncoding::SameBlock { move_index } => {
          body.u8(INPUT_SAME_BLOCK);
          body.varint(move_index as u64);
          body.varint(input.outpoint.vout.into());
        }
        InputEncoding::OlderBlock => {
          body.u8(INPUT_OLDER_BLOCK);
          body.varint((height - input.utxo.height).into());
          body.outpoint(&input.outpoint);
        }
      }
    }
    body.varint(m.outputs.len() as u64);
    for output in &m.outputs {
      body.varint(output.vout.into());
      body.varint(output.value);
      body.varint(scripts.index[output.script_pubkey.as_bytes()]);
      body.u8(kind_code(output.kind));
      body.pieces(&output.pieces);
    }
    body.pieces(&m.fee);
    body.pieces(&m.mined);
    body.pieces(&m.lost);
  }
  index.sort_unstable();

  let scripts_len = scripts.scripts.iter().map(|s| s.len()).sum::<usize>();
  let mut enc = Enc::with_capacity(
    32 + scripts.scripts.len() * 4 + scripts_len + index.len() * INDEX_ENTRY + body.buf.len(),
  );
  enc.u8(MOVES_V1);
  enc.u8(flags);
  enc.varint(scripts.scripts.len() as u64);
  enc.varint(scripts_len as u64);
  let mut offset = 0u32;
  for script in &scripts.scripts {
    enc.raw(&offset.to_le_bytes());
    offset += script.len() as u32;
  }
  for script in &scripts.scripts {
    enc.raw(script);
  }
  enc.varint(moves.len() as u64);
  for (prefix, offset) in index {
    enc.raw(&prefix.to_le_bytes());
    enc.raw(&offset.to_le_bytes());
  }
  enc.raw(&body.buf);
  enc.buf
}

/// Whether a moves record refers to other blocks' moves.
pub(crate) fn moves_have_external_refs(bytes: &[u8]) -> bool {
  bytes.len() >= 2 && bytes[1] & FLAG_EXTERNAL_REFS != 0
}

/// An input stored as a reference to an older block's moves.
pub(crate) struct ExternalRef {
  pub(crate) move_index: usize,
  pub(crate) input_index: usize,
  pub(crate) height: u32,
  pub(crate) outpoint: OutPoint,
}

/// A moves record's header: its scripts, its index and its body.
struct MovesRecord<'a> {
  script_offsets: &'a [u8],
  scripts: &'a [u8],
  n: usize,
  index: &'a [u8],
  body: &'a [u8],
}

impl<'a> MovesRecord<'a> {
  fn parse(bytes: &'a [u8]) -> anyhow::Result<Self> {
    let mut dec = Dec::new(bytes);
    let version = dec.u8()?;
    ensure!(
      version == MOVES_V1,
      "corrupt store record: moves version {version}"
    );
    let _flags = dec.u8()?;
    let n_scripts = dec.len()?;
    let scripts_len = dec.len()?;
    let script_offsets = dec.raw(
      n_scripts
        .checked_mul(4)
        .context("corrupt store record: script offsets")?,
    )?;
    let scripts = dec.raw(scripts_len)?;
    let n = dec.len()?;
    let index = dec.raw(
      n.checked_mul(INDEX_ENTRY)
        .context("corrupt store record: moves index")?,
    )?;
    let body = dec.raw(bytes.len() - dec.pos)?;
    Ok(Self {
      script_offsets,
      scripts,
      n,
      index,
      body,
    })
  }

  fn script_offset(&self, index: usize) -> Option<usize> {
    self
      .script_offsets
      .get(index * 4..index * 4 + 4)
      .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()) as usize)
  }

  fn script(&self, index: u64) -> anyhow::Result<ScriptBuf> {
    let index = usize::try_from(index).context("corrupt store record: script index")?;
    let start = self
      .script_offset(index)
      .context("corrupt store record: script index")?;
    let end = self.script_offset(index + 1).unwrap_or(self.scripts.len());
    let script = self
      .scripts
      .get(start..end)
      .context("corrupt store record: script offsets")?;
    Ok(ScriptBuf::from_bytes(script.to_vec()))
  }

  fn index_entry(&self, i: usize) -> (u64, usize) {
    let entry = &self.index[i * INDEX_ENTRY..(i + 1) * INDEX_ENTRY];
    (
      u64::from_le_bytes(entry[..8].try_into().unwrap()),
      u32::from_le_bytes(entry[8..].try_into().unwrap()) as usize,
    )
  }

  /// Decodes one move; same-block and older-block references are returned
  /// as `(input index, reference)` with placeholder UTXOs.
  fn decode_move(&self, dec: &mut Dec, height: u32) -> anyhow::Result<(Move, Vec<(usize, Ref)>)> {
    let txid = dec.txid()?;
    let tx_index = dec.u32()?;
    let coinbase = dec.u8()? != 0;
    let input_value = dec.varint()?;
    let output_value = dec.varint()?;
    let n_inputs = dec.len()?;
    let mut inputs = Vec::with_capacity(n_inputs);
    let mut refs = Vec::new();
    for input_index in 0..n_inputs {
      let vin = dec.u32()?;
      let placeholder = || TrackedUtxo {
        value: 0,
        script_pubkey: ScriptBuf::new(),
        height: 0,
        kind: UtxoKind::Spendable,
        pieces: Vec::new(),
      };
      let input = match dec.u8()? {
        INPUT_FULL => {
          let outpoint = dec.outpoint()?;
          let value = dec.varint()?;
          let height = dec.u32()?;
          let kind = kind_from_code(dec.u8()?)?;
          let script_pubkey = self.script(dec.varint()?)?;
          let pieces = dec.pieces()?;
          MoveInput {
            vin,
            outpoint,
            utxo: TrackedUtxo {
              value,
              script_pubkey,
              height,
              kind,
              pieces,
            },
          }
        }
        INPUT_SAME_BLOCK => {
          let source = usize::try_from(dec.varint()?).context("corrupt store record")?;
          let vout = dec.u32()?;
          refs.push((input_index, Ref::SameBlock { source, vout }));
          MoveInput {
            vin,
            outpoint: OutPoint {
              txid: Txid::all_zeros(),
              vout,
            },
            utxo: placeholder(),
          }
        }
        INPUT_OLDER_BLOCK => {
          let back = dec.u32()?;
          let outpoint = dec.outpoint()?;
          let created = height
            .checked_sub(back)
            .context("corrupt store record: reference height")?;
          refs.push((
            input_index,
            Ref::OlderBlock {
              height: created,
              outpoint,
            },
          ));
          MoveInput {
            vin,
            outpoint,
            utxo: placeholder(),
          }
        }
        tag => bail!("corrupt store record: input tag {tag}"),
      };
      inputs.push(input);
    }
    let n_outputs = dec.len()?;
    let mut outputs = Vec::with_capacity(n_outputs);
    for _ in 0..n_outputs {
      let vout = dec.u32()?;
      let value = dec.varint()?;
      let script_pubkey = self.script(dec.varint()?)?;
      let kind = kind_from_code(dec.u8()?)?;
      let pieces = dec.pieces()?;
      outputs.push(MoveOutput {
        vout,
        value,
        script_pubkey,
        kind,
        pieces,
      });
    }
    let fee = dec.pieces()?;
    let mined = dec.pieces()?;
    let lost = dec.pieces()?;
    Ok((
      Move {
        txid,
        tx_index,
        coinbase,
        input_value,
        output_value,
        inputs,
        outputs,
        fee,
        mined,
        lost,
      },
      refs,
    ))
  }
}

enum Ref {
  SameBlock { source: usize, vout: u32 },
  OlderBlock { height: u32, outpoint: OutPoint },
}

/// Decodes a block's moves. Inputs referring to older blocks are returned in
/// `ExternalRef`s with a placeholder `utxo` for the caller to resolve.
pub(crate) fn decode_moves(
  bytes: &[u8],
  height: u32,
) -> anyhow::Result<(Vec<Move>, Vec<ExternalRef>)> {
  let record = MovesRecord::parse(bytes)?;
  let mut dec = Dec::new(record.body);
  let mut moves = Vec::with_capacity(record.n);
  let mut same_block = Vec::new();
  let mut external = Vec::new();
  for move_index in 0..record.n {
    let (m, refs) = record.decode_move(&mut dec, height)?;
    for (input_index, reference) in refs {
      match reference {
        Ref::SameBlock { source, vout } => same_block.push((move_index, input_index, source, vout)),
        Ref::OlderBlock { height, outpoint } => external.push(ExternalRef {
          move_index,
          input_index,
          height,
          outpoint,
        }),
      }
    }
    moves.push(m);
  }
  dec.finish()?;

  for (move_index, input_index, source, vout) in same_block {
    let creator = moves
      .get(source)
      .context("corrupt store record: same-block reference")?;
    let txid = creator.txid;
    let output = creator
      .outputs
      .iter()
      .find(|output| output.vout == vout)
      .context("corrupt store record: same-block reference output")?;
    let utxo = utxo_from_output(output, height);
    let input = &mut moves[move_index].inputs[input_index];
    input.outpoint.txid = txid;
    input.utxo = utxo;
  }

  Ok((moves, external))
}

/// The output `outpoint` of the block's moves (`bytes` is the block's
/// record), decoding only the moves whose txid shares its 8-byte prefix.
pub(crate) fn find_output(
  bytes: &[u8],
  height: u32,
  outpoint: &OutPoint,
) -> anyhow::Result<Option<TrackedUtxo>> {
  let record = MovesRecord::parse(bytes)?;
  let prefix = txid_prefix(&outpoint.txid);
  let first = {
    let (mut lo, mut hi) = (0, record.n);
    while lo < hi {
      let mid = (lo + hi) / 2;
      if record.index_entry(mid).0 < prefix {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    lo
  };
  for i in first..record.n {
    let (entry_prefix, offset) = record.index_entry(i);
    if entry_prefix != prefix {
      break;
    }
    ensure!(
      offset < record.body.len(),
      "corrupt store record: moves index offset"
    );
    let mut dec = Dec::new(&record.body[offset..]);
    let (m, _) = record.decode_move(&mut dec, height)?;
    if m.txid != outpoint.txid {
      continue;
    }
    if let Some(output) = m.outputs.iter().find(|output| output.vout == outpoint.vout) {
      return Ok(Some(utxo_from_output(output, height)));
    }
  }
  Ok(None)
}

pub(crate) fn utxo_from_output(output: &MoveOutput, height: u32) -> TrackedUtxo {
  TrackedUtxo {
    value: output.value,
    script_pubkey: output.script_pubkey.clone(),
    height,
    kind: output.kind,
    pieces: output.pieces.clone(),
  }
}

// ---------------------------------------------------------------------------
// Undo records: a whole `BlockDelta`, self-contained.
//
//   version u8 (= UNDO_V1)
//   hash[32] header[80]
//   moves: blob (moves record, SELF_CONTAINED)
//   spent created: utxo lists
//   overwritten: utxo list
//   lost: pieces
//   lost_sats_before lost_sats_after
//   rows: n, n × (row before:row_stats after:row_stats)
// ---------------------------------------------------------------------------

/// Version 2 appended the block's `MarketDelta`; version 3 its inscriptions'
/// transaction indices.
const UNDO_V3: u8 = 3;

pub(crate) fn encode_undo(delta: &BlockDelta, market: &MarketDelta) -> Vec<u8> {
  let mut enc = Enc::with_capacity(256);
  enc.u8(UNDO_V3);
  enc.raw(delta.hash.as_byte_array());
  enc.raw(&header_bytes(&delta.header));
  enc.blob(&encode_moves(
    &delta.moves,
    delta.height,
    RefPolicy::SELF_CONTAINED,
  ));
  enc.utxo_list(&delta.spent);
  enc.utxo_list(&delta.created);
  enc.utxo_list(&delta.overwritten);
  enc.pieces(&delta.lost);
  enc.varint(delta.lost_sats_before);
  enc.varint(delta.lost_sats_after);
  enc.varint(delta.rows.len() as u64);
  for row in &delta.rows {
    enc.varint(row.row.into());
    enc.row_stats(&row.before);
    enc.row_stats(&row.after);
  }
  enc.varint(market.sales.len() as u64);
  for sale in &market.sales {
    enc.blob(&encode_sale(sale, None));
  }
  enc.varint(market.inscriptions.len() as u64);
  for event in &market.inscriptions {
    enc.varint(event.sat);
    enc.txid(&event.txid);
    enc.varint(event.vin.into());
    enc.varint(event.tx_index.into());
  }
  enc.buf
}

pub(crate) fn decode_undo(bytes: &[u8], height: u32) -> anyhow::Result<(BlockDelta, MarketDelta)> {
  let mut dec = Dec::new(bytes);
  let version = dec.u8()?;
  ensure!(
    version == UNDO_V3,
    "corrupt store record: undo version {version}"
  );
  let hash = BlockHash::from_byte_array(dec.raw(32)?.try_into().unwrap());
  let header = header_from_bytes(dec.raw(80)?)?;
  let (moves, external) = decode_moves(dec.blob()?, height)?;
  ensure!(
    external.is_empty(),
    "corrupt store record: undo moves refer to other blocks"
  );
  let spent = dec.utxo_list()?;
  let created = dec.utxo_list()?;
  let overwritten = dec.utxo_list()?;
  let lost = dec.pieces()?;
  let lost_sats_before = dec.varint()?;
  let lost_sats_after = dec.varint()?;
  let n_rows = dec.len()?;
  let mut rows = Vec::with_capacity(n_rows);
  for _ in 0..n_rows {
    rows.push(RowDelta {
      row: u16::try_from(dec.varint()?).context("corrupt store record: row id")?,
      before: dec.row_stats()?,
      after: dec.row_stats()?,
    });
  }
  let n_sales = dec.len()?;
  let mut sales = Vec::with_capacity(n_sales);
  for _ in 0..n_sales {
    sales.push(decode_sale(dec.blob()?)?.0);
  }
  let n_inscriptions = dec.len()?;
  let mut inscriptions = Vec::with_capacity(n_inscriptions);
  for _ in 0..n_inscriptions {
    inscriptions.push(InscriptionEvent {
      sat: dec.varint()?,
      txid: dec.txid()?,
      vin: dec.u32()?,
      tx_index: dec.u32()?,
    });
  }
  dec.finish()?;
  Ok((
    BlockDelta {
      height,
      hash,
      header,
      moves,
      spent,
      created,
      overwritten,
      lost,
      lost_sats_before,
      lost_sats_after,
      rows,
    },
    MarketDelta {
      sales,
      inscriptions,
    },
  ))
}

// ---------------------------------------------------------------------------
// Sales: one record per sale (key: height, tx_index, vin).
//
//   version u8 (= SALE_V1)
//   txid[32] tx_index vin kind:u8 marketplace:blob
//   seller_outpoint seller:utxo
//   payment_vout:option price seller_proceeds fees
//   buyer_vout:option buyer_script:option-blob
//   buyer_value:option        computed by the store from the block's moves
//
// An option is a varint: 0 for none, else value + 1 (blobs: a flag byte).
// ---------------------------------------------------------------------------

const SALE_V1: u8 = 1;

fn sale_kind_code(kind: SaleKind) -> u8 {
  match kind {
    SaleKind::Listing => 0,
    SaleKind::Offer => 1,
    SaleKind::Unknown => 2,
  }
}

fn sale_kind_from_code(code: u8) -> anyhow::Result<SaleKind> {
  Ok(match code {
    0 => SaleKind::Listing,
    1 => SaleKind::Offer,
    2 => SaleKind::Unknown,
    _ => bail!("corrupt store record: sale kind {code}"),
  })
}

impl Enc {
  fn option(&mut self, value: Option<u64>) {
    self.varint(value.map_or(0, |value| value + 1));
  }
}

impl Dec<'_> {
  fn option(&mut self) -> anyhow::Result<Option<u64>> {
    Ok(match self.varint()? {
      0 => None,
      value => Some(value - 1),
    })
  }

  fn option_u32(&mut self) -> anyhow::Result<Option<u32>> {
    self
      .option()?
      .map(|value| u32::try_from(value).context("corrupt store record: u32 out of range"))
      .transpose()
  }
}

pub(crate) fn encode_sale(sale: &Sale, buyer_value: Option<u64>) -> Vec<u8> {
  let mut enc = Enc::with_capacity(128 + sale.seller.script_pubkey.len());
  enc.u8(SALE_V1);
  enc.txid(&sale.txid);
  enc.varint(sale.tx_index.into());
  enc.varint(sale.vin.into());
  enc.u8(sale_kind_code(sale.kind));
  enc.blob(sale.marketplace.as_bytes());
  enc.outpoint(&sale.seller_outpoint);
  enc.utxo(&sale.seller);
  enc.option(sale.payment_vout.map(u64::from));
  enc.varint(sale.price);
  enc.varint(sale.seller_proceeds);
  enc.varint(sale.fees);
  enc.option(sale.buyer_vout.map(u64::from));
  match &sale.buyer_script {
    Some(script) => {
      enc.u8(1);
      enc.blob(script.as_bytes());
    }
    None => enc.u8(0),
  }
  enc.option(buyer_value);
  enc.buf
}

/// The sale and the buyer output's value.
pub(crate) fn decode_sale(bytes: &[u8]) -> anyhow::Result<(Sale, Option<u64>)> {
  let mut dec = Dec::new(bytes);
  let version = dec.u8()?;
  ensure!(
    version == SALE_V1,
    "corrupt store record: sale version {version}"
  );
  let txid = dec.txid()?;
  let tx_index = dec.u32()?;
  let vin = dec.u32()?;
  let kind = sale_kind_from_code(dec.u8()?)?;
  let marketplace =
    String::from_utf8(dec.blob()?.to_vec()).context("corrupt store record: marketplace id")?;
  let seller_outpoint = dec.outpoint()?;
  let seller = dec.utxo()?;
  let payment_vout = dec.option_u32()?;
  let price = dec.varint()?;
  let seller_proceeds = dec.varint()?;
  let fees = dec.varint()?;
  let buyer_vout = dec.option_u32()?;
  let buyer_script = match dec.u8()? {
    0 => None,
    1 => Some(ScriptBuf::from_bytes(dec.blob()?.to_vec())),
    flag => bail!("corrupt store record: buyer script flag {flag}"),
  };
  let buyer_value = dec.option()?;
  dec.finish()?;
  Ok((
    Sale {
      txid,
      tx_index,
      vin,
      kind,
      marketplace,
      seller_outpoint,
      seller,
      payment_vout,
      price,
      seller_proceeds,
      fees,
      buyer_vout,
      buyer_script,
    },
    buyer_value,
  ))
}

/// Value of the `inscribed` table: count, first txid, first height, first
/// transaction's index in its block.
pub(crate) fn encode_inscribed(
  count: u32,
  first_txid: &Txid,
  first_height: u32,
  first_tx_index: u32,
) -> Vec<u8> {
  let mut enc = Enc::with_capacity(44);
  enc.varint(count.into());
  enc.txid(first_txid);
  enc.varint(first_height.into());
  enc.varint(first_tx_index.into());
  enc.buf
}

pub(crate) fn decode_inscribed(bytes: &[u8]) -> anyhow::Result<(u32, Txid, u32, u32)> {
  let mut dec = Dec::new(bytes);
  let count = dec.u32()?;
  let txid = dec.txid()?;
  let height = dec.u32()?;
  let tx_index = dec.u32()?;
  dec.finish()?;
  Ok((count, txid, height, tx_index))
}

// History entries (`history` and `history_staging` values): flags (varint:
// direction in bits 0-1, kind in bits 2-4, then extracted_in, extracted_out,
// seller, buyer, has extracted_sats, has isolated, has burned, has lead sat,
// has counterparty), the txid, the received side if the direction has one
// and the sent side likewise (each: count, sats, value, row count and
// (row gap, n) pairs), the optional fields flagged, and the counterparty
// count. Key fields (script key, height, tx_index) are in the table key.

const HISTORY_EXTRACTED_IN: u64 = 1 << 5;
const HISTORY_EXTRACTED_OUT: u64 = 1 << 6;
const HISTORY_SELLER: u64 = 1 << 7;
const HISTORY_BUYER: u64 = 1 << 8;
const HISTORY_EXTRACTED_SATS: u64 = 1 << 9;
const HISTORY_ISOLATED: u64 = 1 << 10;
const HISTORY_BURNED: u64 = 1 << 11;
const HISTORY_LEAD: u64 = 1 << 12;
const HISTORY_COUNTERPARTY: u64 = 1 << 13;

fn encode_side(enc: &mut Enc, side: &crate::history::Side) {
  enc.varint(side.count.into());
  enc.varint(side.sats);
  enc.varint(side.value);
  enc.varint(side.rows.len() as u64);
  let mut previous = 0;
  for &(row, n) in &side.rows {
    enc.varint(u64::from(row - previous));
    enc.varint(n);
    previous = row;
  }
}

fn decode_side(dec: &mut Dec) -> anyhow::Result<crate::history::Side> {
  let count = dec.u32()?;
  let sats = dec.varint()?;
  let value = dec.varint()?;
  let n = dec.len()?;
  let mut rows = Vec::with_capacity(n);
  let mut row = 0u64;
  for _ in 0..n {
    row += dec.varint()?;
    rows.push((
      crate::model::RowId::try_from(row).context("corrupt history record: row")?,
      dec.varint()?,
    ));
  }
  Ok(crate::history::Side {
    sats,
    value,
    count,
    rows,
  })
}

pub(crate) fn encode_history(entry: &crate::history::HistoryEntry) -> Vec<u8> {
  use crate::history::Direction;
  let mut flags = u64::from(entry.direction.code()) | u64::from(entry.kind.code()) << 2;
  for (set, flag) in [
    (entry.extracted_in, HISTORY_EXTRACTED_IN),
    (entry.extracted_out, HISTORY_EXTRACTED_OUT),
    (entry.seller, HISTORY_SELLER),
    (entry.buyer, HISTORY_BUYER),
    (entry.extracted_sats > 0, HISTORY_EXTRACTED_SATS),
    (entry.isolated > 0, HISTORY_ISOLATED),
    (entry.burned > 0, HISTORY_BURNED),
    (entry.lead_sat.is_some(), HISTORY_LEAD),
    (entry.counterparty.is_some(), HISTORY_COUNTERPARTY),
  ] {
    if set {
      flags |= flag;
    }
  }
  let mut enc = Enc::with_capacity(64);
  enc.varint(flags);
  enc.txid(&entry.txid);
  if entry.direction != Direction::Out {
    encode_side(&mut enc, &entry.received);
  }
  if entry.direction != Direction::In {
    encode_side(&mut enc, &entry.sent);
  }
  for value in [entry.extracted_sats, entry.isolated, entry.burned] {
    if value > 0 {
      enc.varint(value);
    }
  }
  if let Some(sat) = entry.lead_sat {
    enc.varint(sat);
  }
  if let Some(script) = &entry.counterparty {
    enc.blob(script.as_bytes());
  }
  enc.varint(entry.counterparties.into());
  enc.buf
}

pub(crate) fn decode_history(
  key: u64,
  height: u32,
  tx_index: u32,
  bytes: &[u8],
) -> anyhow::Result<crate::history::HistoryEntry> {
  use crate::history::{Direction, HistoryEntry, HistoryKind, Side};
  let mut dec = Dec::new(bytes);
  let flags = dec.varint()?;
  let direction = Direction::from_code((flags & 3) as u8)
    .with_context(|| format!("corrupt history record: direction {}", flags & 3))?;
  let kind = HistoryKind::from_code(((flags >> 2) & 7) as u8)
    .with_context(|| format!("corrupt history record: kind {}", (flags >> 2) & 7))?;
  let txid = dec.txid()?;
  let received = if direction != Direction::Out {
    decode_side(&mut dec)?
  } else {
    Side::default()
  };
  let sent = if direction != Direction::In {
    decode_side(&mut dec)?
  } else {
    Side::default()
  };
  let mut optional = |flag: u64| -> anyhow::Result<u64> {
    if flags & flag != 0 {
      dec.varint()
    } else {
      Ok(0)
    }
  };
  let extracted_sats = optional(HISTORY_EXTRACTED_SATS)?;
  let isolated = optional(HISTORY_ISOLATED)?;
  let burned = optional(HISTORY_BURNED)?;
  let lead_sat = if flags & HISTORY_LEAD != 0 {
    Some(dec.varint()?)
  } else {
    None
  };
  let counterparty = if flags & HISTORY_COUNTERPARTY != 0 {
    Some(ScriptBuf::from_bytes(dec.blob()?.to_vec()))
  } else {
    None
  };
  let counterparties = dec.u32()?;
  dec.finish()?;
  Ok(HistoryEntry {
    key,
    height,
    tx_index,
    txid,
    kind,
    direction,
    received,
    sent,
    extracted_in: flags & HISTORY_EXTRACTED_IN != 0,
    extracted_out: flags & HISTORY_EXTRACTED_OUT != 0,
    extracted_sats,
    isolated,
    burned,
    seller: flags & HISTORY_SELLER != 0,
    buyer: flags & HISTORY_BUYER != 0,
    counterparties,
    counterparty,
    lead_sat,
  })
}
