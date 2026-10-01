//! Store unit tests with hand-made and generated deltas.

use {
  super::{codec::*, *},
  crate::model::{InscriptionEvent, MoveInput, MoveOutput, RowDelta, SaleKind},
  bitcoin::{
    CompactTarget, ScriptBuf, TxMerkleNode, Txid,
    block::Version,
    hashes::{Hash, sha256d},
  },
  pretty_assertions::assert_eq,
  redb::ReadableMultimapTable,
  std::collections::{BTreeMap, BTreeSet},
  tempfile::TempDir,
};

pub(crate) struct Rng(u64);

impl Rng {
  pub(crate) fn new(seed: u64) -> Self {
    Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
  }

  pub(crate) fn next(&mut self) -> u64 {
    self.0 ^= self.0 >> 12;
    self.0 ^= self.0 << 25;
    self.0 ^= self.0 >> 27;
    self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
  }

  pub(crate) fn below(&mut self, n: u64) -> u64 {
    self.next() % n
  }

  pub(crate) fn chance(&mut self, percent: u64) -> bool {
    self.below(100) < percent
  }
}

pub(crate) fn test_header(prev: Option<BlockHash>, height: u32) -> Header {
  Header {
    version: Version::ONE,
    prev_blockhash: prev.unwrap_or_else(BlockHash::all_zeros),
    merkle_root: TxMerkleNode::all_zeros(),
    time: 1_600_000_000 + height * 600,
    bits: CompactTarget::from_consensus(0x207f_ffff),
    nonce: height,
  }
}

pub(crate) fn test_txid(n: u64) -> Txid {
  Txid::from_raw_hash(sha256d::Hash::hash(&n.to_le_bytes()))
}

fn meta() -> IndexMeta {
  IndexMeta {
    schema_version: SCHEMA_VERSION,
    chain: "regtest".into(),
    config: TrackerConfig::default(),
    catalog_fingerprint: "test".into(),
    height: None,
    hash: None,
    fast_sync_complete: false,
  }
}

fn store(dir: &TempDir, retention: MovesRetention) -> Store {
  let store = Store::open_with(
    &dir.path().join("index.redb"),
    StoreOptions {
      cache_size: 16 << 20,
      moves_retention: retention,
      quick_repair: false,
    },
  )
  .unwrap();
  if store.meta().unwrap().is_none() {
    store.init(&meta()).unwrap();
  }
  store
}

const ROWS: usize = 2;

/// Generates consistent random blocks and keeps the model state they lead
/// to.
pub(crate) struct TestChain {
  rng: Rng,
  pub(crate) height: Option<u32>,
  pub(crate) hash: Option<BlockHash>,
  pub(crate) times: Vec<u32>,
  pub(crate) utxos: BTreeMap<OutPoint, TrackedUtxo>,
  pub(crate) lost: Vec<Piece>,
  pub(crate) overwritten: Vec<(OutPoint, u32, TrackedUtxo)>,
  pub(crate) lost_sats: u64,
  pub(crate) stats: Vec<RowStats>,
  next_txid: u64,
  scripts: Vec<ScriptBuf>,
  coinbases: Vec<OutPoint>,
  /// Non-coinbase transactions per block.
  pub(crate) txs_per_block: u64,
}

pub(crate) const SUBSIDY: u64 = 1_000;

impl TestChain {
  pub(crate) fn new(seed: u64) -> Self {
    let mut scripts = Vec::new();
    for i in 0..6u8 {
      let mut script = vec![0x00, 0x14];
      script.extend([i; 20]);
      scripts.push(ScriptBuf::from_bytes(script));
    }
    // Long enough to be keyed by its hash.
    scripts.push(ScriptBuf::from_bytes(vec![0x51; 100]));
    Self {
      rng: Rng::new(seed),
      height: None,
      hash: None,
      times: Vec::new(),
      utxos: BTreeMap::new(),
      lost: Vec::new(),
      overwritten: Vec::new(),
      lost_sats: 0,
      stats: vec![RowStats::default(); ROWS],
      next_txid: 0,
      scripts,
      coinbases: Vec::new(),
      txs_per_block: 4,
    }
  }

  pub(crate) fn mined(&self) -> Vec<u64> {
    self.stats.iter().map(|stats| stats.mined).collect()
  }

  pub(crate) fn snapshot(&self) -> TrackerSnapshot {
    TrackerSnapshot {
      height: self.height,
      hash: self.hash,
      times: self.times.clone(),
      lost_sats: self.lost_sats,
      utxos: self
        .utxos
        .iter()
        .map(|(outpoint, utxo)| (*outpoint, utxo.clone()))
        .collect(),
      lost: self.lost.clone(),
      overwritten: self.overwritten.clone(),
      mined: self.mined(),
    }
  }

  fn txid(&mut self) -> Txid {
    self.next_txid += 1;
    test_txid(self.next_txid)
  }

  fn script(&mut self) -> ScriptBuf {
    let i = self.rng.below(self.scripts.len() as u64) as usize;
    self.scripts[i].clone()
  }

  /// Distributes input pieces (offsets in the input stream) over outputs of
  /// the given values; returns per-output pieces and the leftover (fee).
  fn distribute(pieces: &[Piece], values: &[u64]) -> (Vec<Vec<Piece>>, Vec<Piece>) {
    let mut outputs = vec![Vec::new(); values.len()];
    let mut fee = Vec::new();
    let mut bounds = Vec::new();
    let mut total = 0;
    for value in values {
      bounds.push((total, total + value));
      total += value;
    }
    for piece in pieces {
      let mut offset = piece.offset;
      let mut start = piece.start;
      let mut len = piece.len;
      while len > 0 {
        match bounds
          .iter()
          .position(|(lo, hi)| offset >= *lo && offset < *hi)
        {
          Some(i) => {
            let (lo, hi) = bounds[i];
            let take = len.min(hi - offset);
            push_piece(
              &mut outputs[i],
              Piece {
                offset: offset - lo,
                start,
                len: take,
              },
            );
            offset += take;
            start += take;
            len -= take;
          }
          None => {
            push_piece(
              &mut fee,
              Piece {
                offset: offset - total,
                start,
                len,
              },
            );
            len = 0;
          }
        }
      }
    }
    (outputs, fee)
  }

  pub(crate) fn next_block(&mut self) -> BlockDelta {
    let height = self.height.map_or(0, |height| height + 1);
    let header = test_header(self.hash, height);
    let hash = header.block_hash();
    let stats_before = self.stats.clone();

    let mut spent = Vec::new();
    let mut created: BTreeMap<OutPoint, TrackedUtxo> = BTreeMap::new();
    let mut moves = Vec::new();

    for tx_index in 1..=self.txs_per_block {
      let spendable = self
        .utxos
        .iter()
        .filter(|(_, utxo)| utxo.kind == UtxoKind::Spendable)
        .map(|(outpoint, _)| *outpoint)
        .collect::<Vec<OutPoint>>();
      if spendable.is_empty() {
        break;
      }
      let n_inputs = 1 + self.rng.below(2) as usize;
      let mut inputs = Vec::new();
      for _ in 0..n_inputs {
        let outpoint = spendable[self.rng.below(spendable.len() as u64) as usize];
        if inputs.iter().any(|(o, _)| *o == outpoint) {
          continue;
        }
        let utxo = self.utxos.remove(&outpoint).unwrap();
        self.coinbases.retain(|coinbase| *coinbase != outpoint);
        if created.remove(&outpoint).is_none() {
          spent.push((outpoint, utxo.clone()));
        }
        inputs.push((outpoint, utxo));
      }

      let input_value = inputs.iter().map(|(_, utxo)| utxo.value).sum::<u64>();
      let mut stream = Vec::new();
      let mut base = 0;
      for (_, utxo) in &inputs {
        for piece in &utxo.pieces {
          stream.push(Piece {
            offset: base + piece.offset,
            ..*piece
          });
        }
        base += utxo.value;
      }

      // No fee, so every piece lands in an output.
      let n_outputs = 1 + self.rng.below(3) as usize;
      let mut values = Vec::new();
      let mut left = input_value;
      for i in 0..n_outputs {
        let value = if i + 1 == n_outputs {
          left
        } else {
          self.rng.below(left + 1)
        };
        values.push(value);
        left -= value;
      }

      let (distributed, fee) = Self::distribute(&stream, &values);
      assert!(fee.is_empty());

      let txid = self.txid();
      let mut move_outputs = Vec::new();
      for (vout, (value, pieces)) in values.iter().zip(distributed).enumerate() {
        if pieces.is_empty() {
          continue;
        }
        let unspendable = self.rng.chance(10);
        let script_pubkey = if unspendable {
          ScriptBuf::from_bytes(vec![0x6a, 0x01, 0x2a])
        } else {
          self.script()
        };
        let kind = if unspendable {
          UtxoKind::Unspendable
        } else {
          UtxoKind::Spendable
        };
        let outpoint = OutPoint {
          txid,
          vout: vout as u32,
        };
        let utxo = TrackedUtxo {
          value: *value,
          script_pubkey: script_pubkey.clone(),
          height,
          kind,
          pieces: pieces.clone(),
        };
        self.utxos.insert(outpoint, utxo.clone());
        created.insert(outpoint, utxo);
        move_outputs.push(MoveOutput {
          vout: vout as u32,
          value: *value,
          script_pubkey,
          kind,
          pieces,
        });
      }

      moves.push(Move {
        txid,
        tx_index: tx_index as u32,
        coinbase: false,
        input_value,
        output_value: input_value,
        inputs: inputs
          .into_iter()
          .enumerate()
          .map(|(vin, (outpoint, utxo))| MoveInput {
            vin: vin as u32,
            outpoint,
            utxo,
          })
          .collect(),
        outputs: move_outputs,
        fee: Vec::new(),
        mined: Vec::new(),
        lost: Vec::new(),
      });

      self.stats[1].located += 1;
      self.stats[1].holders = self.rng.below(10);
    }

    // Coinbase: mints [height * SUBSIDY, +10) at offsets 0..10 of its input
    // stream, then more untracked sats.
    let mined_piece = Piece {
      offset: 0,
      start: u64::from(height) * SUBSIDY,
      len: 10,
    };
    let claimed = if self.rng.chance(20) { 5 } else { SUBSIDY };

    let mut overwritten = Vec::new();
    let coinbase_txid = if !self.coinbases.is_empty() && self.rng.chance(5) {
      let i = self.rng.below(self.coinbases.len() as u64) as usize;
      let outpoint = self.coinbases[i];
      let old = self.utxos.remove(&outpoint).unwrap();
      assert!(!created.contains_key(&outpoint));
      overwritten.push((outpoint, old.clone()));
      self.overwritten.push((outpoint, height, old));
      outpoint.txid
    } else {
      self.txid()
    };

    let (distributed, lost_stream) = Self::distribute(&[mined_piece], &[claimed]);
    let coinbase_outpoint = OutPoint {
      txid: coinbase_txid,
      vout: 0,
    };
    let script_pubkey = self.script();
    let utxo = TrackedUtxo {
      value: claimed,
      script_pubkey: script_pubkey.clone(),
      height,
      kind: UtxoKind::Spendable,
      pieces: distributed[0].clone(),
    };
    self.utxos.insert(coinbase_outpoint, utxo.clone());
    created.insert(coinbase_outpoint, utxo);
    if !self.coinbases.contains(&coinbase_outpoint) {
      self.coinbases.push(coinbase_outpoint);
    }

    let lost_sats_before = self.lost_sats;
    let lost = lost_stream
      .iter()
      .map(|piece| Piece {
        offset: piece.offset + lost_sats_before,
        ..*piece
      })
      .collect::<Vec<Piece>>();
    self.lost.extend(&lost);
    self.lost_sats += SUBSIDY - claimed;

    moves.push(Move {
      txid: coinbase_txid,
      tx_index: 0,
      coinbase: true,
      input_value: 0,
      output_value: claimed,
      inputs: Vec::new(),
      outputs: vec![MoveOutput {
        vout: 0,
        value: claimed,
        script_pubkey,
        kind: UtxoKind::Spendable,
        pieces: distributed[0].clone(),
      }],
      fee: Vec::new(),
      mined: vec![mined_piece],
      lost: lost.clone(),
    });

    self.stats[0].mined += 10;
    self.stats[0].lost += 10 - distributed[0].iter().map(|p| p.len).sum::<u64>();

    let rows = (0..ROWS)
      .filter(|&row| stats_before[row] != self.stats[row])
      .map(|row| RowDelta {
        row: row as RowId,
        before: stats_before[row],
        after: self.stats[row],
      })
      .collect();

    self.height = Some(height);
    self.hash = Some(hash);
    self.times.push(header.time);

    BlockDelta {
      height,
      hash,
      header,
      moves,
      spent,
      created: created.into_iter().collect(),
      overwritten,
      lost,
      lost_sats_before,
      lost_sats_after: self.lost_sats,
      rows,
    }
  }
}

/// A deterministic market for a generated block: every tracked input of a
/// transaction is sold to its first output; every third block inscribes its
/// first minted sat and reinscribes the one three blocks below.
pub(crate) fn market_for(delta: &BlockDelta) -> MarketDelta {
  let mut market = MarketDelta::default();
  for m in delta.moves.iter().filter(|m| !m.coinbase) {
    for input in &m.inputs {
      market.sales.push(Sale {
        txid: m.txid,
        tx_index: m.tx_index,
        vin: input.vin,
        kind: if input.vin % 2 == 0 {
          SaleKind::Listing
        } else {
          SaleKind::Offer
        },
        marketplace: "magisat".into(),
        seller_outpoint: input.outpoint,
        seller: input.utxo.clone(),
        payment_vout: (m.outputs.len() > 1).then_some(1),
        price: input.utxo.value + 45_000,
        seller_proceeds: input.utxo.value + 44_000,
        fees: 1_000,
        buyer_vout: m.outputs.first().map(|output| output.vout),
        buyer_script: m.outputs.first().map(|output| output.script_pubkey.clone()),
      });
    }
  }
  if delta.height.is_multiple_of(3) {
    let coinbase = delta.moves.iter().find(|m| m.coinbase).unwrap();
    market.inscriptions.push(InscriptionEvent {
      sat: u64::from(delta.height) * SUBSIDY,
      txid: coinbase.txid,
      vin: 0,
    });
    if delta.height >= 3 {
      market.inscriptions.push(InscriptionEvent {
        sat: u64::from(delta.height - 3) * SUBSIDY,
        txid: coinbase.txid,
        vin: 1,
      });
    }
  }
  market
}

fn markets(deltas: &[BlockDelta]) -> Vec<MarketDelta> {
  deltas.iter().map(market_for).collect()
}

fn push_piece(pieces: &mut Vec<Piece>, piece: Piece) {
  if let Some(last) = pieces.last_mut()
    && last.offset_end() == piece.offset
    && last.end() == piece.start
  {
    last.len += piece.len;
    return;
  }
  pieces.push(piece);
}

type Dump = BTreeMap<&'static str, Vec<(Vec<u8>, Vec<u8>)>>;

fn raw<K: redb::Key + 'static, V: redb::Value + 'static>(
  table: &redb::ReadOnlyTable<K, V>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
  table
    .iter()
    .unwrap()
    .map(|entry| {
      let (key, value) = entry.unwrap();
      (
        K::as_bytes(&key.value()).as_ref().to_vec(),
        V::as_bytes(&value.value()).as_ref().to_vec(),
      )
    })
    .collect()
}

/// Every table's raw contents (optionally without the undo table).
fn dump(store: &Store, with_undo: bool) -> Dump {
  let tx = store.db.begin_read().unwrap();
  let mut dump = Dump::new();

  macro_rules! table {
    ($definition:expr, $name:literal) => {{
      dump.insert($name, raw(&tx.open_table($definition).unwrap()));
    }};
  }

  table!(META, "meta");
  table!(HEADERS, "headers");
  table!(UTXOS, "utxos");
  table!(LOCATOR, "locator");
  table!(OVERWRITTEN, "overwritten");
  table!(LOST, "lost");
  table!(MOVES, "moves");
  table!(ROW_HISTORY, "row_history");
  table!(SALES, "sales");
  table!(SALE_TXS, "sale_txs");
  table!(SALE_PIECES, "sale_pieces");
  table!(INSCRIBED, "inscribed");
  if with_undo {
    table!(UNDO, "undo");
  }

  let scripts = tx.open_multimap_table(SCRIPTS).unwrap();
  let mut entries = Vec::new();
  for entry in scripts.iter().unwrap() {
    let (key, values) = entry.unwrap();
    for value in values {
      entries.push((key.value().to_vec(), value.unwrap().value().to_vec()));
    }
  }
  dump.insert("scripts", entries);

  dump
}

fn sorted(mut snapshot: TrackerSnapshot) -> TrackerSnapshot {
  snapshot
    .utxos
    .sort_by_key(|(outpoint, _)| outpoint_key(outpoint));
  snapshot.lost.sort();
  snapshot
    .overwritten
    .sort_by_key(|(outpoint, height, _)| (outpoint_key(outpoint), *height));
  snapshot
}

fn outpoint(n: u64, vout: u32) -> OutPoint {
  OutPoint {
    txid: test_txid(n),
    vout,
  }
}

fn p2wpkh(n: u8) -> ScriptBuf {
  let mut script = vec![0x00, 0x14];
  script.extend([n; 20]);
  ScriptBuf::from_bytes(script)
}

fn block(prev: Option<BlockHash>, height: u32) -> BlockDelta {
  let header = test_header(prev, height);
  BlockDelta {
    height,
    hash: header.block_hash(),
    header,
    moves: Vec::new(),
    spent: Vec::new(),
    created: Vec::new(),
    overwritten: Vec::new(),
    lost: Vec::new(),
    lost_sats_before: 0,
    lost_sats_after: 0,
    rows: Vec::new(),
  }
}

#[test]
fn varints_round_trip() {
  for value in [0, 1, 127, 128, 255, 300, u32::MAX.into(), 1 << 56, u64::MAX] {
    let mut enc = Enc::default();
    enc.varint(value);
    let mut dec = Dec::new(&enc.buf);
    assert_eq!(dec.varint().unwrap(), value);
    dec.finish().unwrap();
  }
  assert!(Dec::new(&[0xff; 11]).varint().is_err());
  assert!(Dec::new(&[0x80]).varint().is_err());
}

#[test]
fn utxo_and_loc_round_trip() {
  let utxo = TrackedUtxo {
    value: 5_000_000_000,
    script_pubkey: p2wpkh(7),
    height: 9,
    kind: UtxoKind::Unspendable,
    pieces: vec![
      Piece {
        offset: 0,
        start: 45_000_000_000,
        len: 5_000_000_000,
      },
      Piece {
        offset: 7_000_000_000,
        start: 3,
        len: 1,
      },
    ],
  };
  assert_eq!(decode_utxo(&encode_utxo(&utxo)).unwrap(), utxo);

  for kind in [
    LocationKind::Unspent,
    LocationKind::Burned,
    LocationKind::Lost,
    LocationKind::Overwritten,
  ] {
    let loc = Loc {
      kind,
      outpoint: if kind == LocationKind::Lost {
        OutPoint::null()
      } else {
        outpoint(1, 3)
      },
      offset: 99,
      len: 7,
    };
    let mut enc = Enc::default();
    encode_loc(&loc, &mut enc);
    assert_eq!(decode_loc(&enc.buf).unwrap(), loc);
  }

  let stats = RowStats {
    mined: 1,
    located: 2,
    burned: 3,
    lost: 4,
    overwritten: 5,
    extracted: 6,
    holders: 7,
    extracted_holders: u64::MAX,
  };
  assert_eq!(decode_row_stats(&encode_row_stats(&stats)).unwrap(), stats);

  assert!(decode_utxo(&encode_utxo(&utxo)[..10]).is_err());
}

#[test]
fn script_keys() {
  let short = p2wpkh(1);
  assert_eq!(script_key(short.as_bytes())[0], 0);
  assert_eq!(&script_key(short.as_bytes())[1..], short.as_bytes());
  let long = vec![0x51; 65];
  assert_eq!(script_key(&long)[0], 1);
  assert_eq!(script_key(&long).len(), 33);
  assert_ne!(script_key(&long), script_key(&[0x51; 66]));
}

#[test]
fn moves_round_trip_with_references() {
  let mut chain = TestChain::new(7);
  chain.txs_per_block = 12;
  let deltas = (0..40).map(|_| chain.next_block()).collect::<Vec<_>>();

  let mut same_block_refs = 0;
  for delta in &deltas {
    let bytes = encode_moves(&delta.moves, delta.height, RefPolicy::SELF_CONTAINED);
    assert!(!moves_have_external_refs(&bytes));
    let (moves, external) = decode_moves(&bytes, delta.height).unwrap();
    assert!(external.is_empty());
    assert_eq!(moves, delta.moves);
    same_block_refs += delta
      .moves
      .iter()
      .flat_map(|m| &m.inputs)
      .filter(|input| input.utxo.height == delta.height)
      .count();
  }
  assert!(
    same_block_refs > 0,
    "the generator should chain within blocks"
  );

  // With references to older blocks, resolution goes through their records.
  let records = deltas
    .iter()
    .map(|delta| {
      (
        delta.height,
        encode_moves(
          &delta.moves,
          delta.height,
          RefPolicy {
            older_from: Some(0),
            unreferenced: None,
          },
        ),
      )
    })
    .collect::<BTreeMap<u32, Vec<u8>>>();

  let mut external_refs = 0;
  for delta in &deltas {
    let (mut moves, external) = decode_moves(&records[&delta.height], delta.height).unwrap();
    external_refs += external.len();
    for reference in external {
      let (older, _) = decode_moves(&records[&reference.height], reference.height).unwrap();
      let output = older
        .iter()
        .find(|m| m.txid == reference.outpoint.txid)
        .unwrap()
        .outputs
        .iter()
        .find(|o| o.vout == reference.outpoint.vout)
        .unwrap();
      moves[reference.move_index].inputs[reference.input_index].utxo =
        utxo_from_output(output, reference.height);
    }
    assert_eq!(moves, delta.moves);
  }
  assert!(external_refs > 0);
}

#[test]
fn undo_round_trip() {
  let mut chain = TestChain::new(3);
  for _ in 0..30 {
    let delta = chain.next_block();
    let market = market_for(&delta);
    assert_eq!(
      decode_undo(&encode_undo(&delta, &market), delta.height).unwrap(),
      (delta, market)
    );
  }
}

#[test]
fn commit_and_query() {
  let dir = TempDir::new().unwrap();
  let store = store(&dir, MovesRetention::All);

  let a = outpoint(1, 0);
  let b = outpoint(2, 0);
  let c = outpoint(2, 1);
  let alice = p2wpkh(1);
  let bob = p2wpkh(2);

  let utxo_a = TrackedUtxo {
    value: 5_000_000_000,
    script_pubkey: alice.clone(),
    height: 0,
    kind: UtxoKind::Spendable,
    pieces: vec![Piece {
      offset: 0,
      start: 0,
      len: 10,
    }],
  };

  let mut genesis = block(None, 0);
  genesis.created = vec![(a, utxo_a.clone())];
  genesis.lost = vec![Piece {
    offset: 0,
    start: 4_999_999_999,
    len: 1,
  }];
  genesis.lost_sats_after = 1;
  genesis.moves = vec![Move {
    txid: a.txid,
    tx_index: 0,
    coinbase: true,
    input_value: 0,
    output_value: 5_000_000_000,
    inputs: Vec::new(),
    outputs: vec![MoveOutput {
      vout: 0,
      value: 5_000_000_000,
      script_pubkey: alice.clone(),
      kind: UtxoKind::Spendable,
      pieces: utxo_a.pieces.clone(),
    }],
    fee: Vec::new(),
    mined: utxo_a.pieces.clone(),
    lost: genesis.lost.clone(),
  }];
  genesis.rows = vec![RowDelta {
    row: 0,
    before: RowStats::default(),
    after: RowStats {
      mined: 11,
      located: 10,
      lost: 1,
      holders: 1,
      ..default()
    },
  }];

  store.commit(&[genesis.clone()], &[11, 0], false).unwrap();

  let meta = store.meta().unwrap().unwrap();
  assert_eq!(meta.height, Some(0));
  assert_eq!(meta.hash, Some(genesis.hash));
  assert!(!meta.fast_sync_complete);
  assert_eq!(store.block_hash(0).unwrap(), Some(genesis.hash));
  assert_eq!(store.header(0).unwrap(), Some(genesis.header));
  assert_eq!(store.block_hash(1).unwrap(), None);
  assert_eq!(store.utxo(a).unwrap(), Some(utxo_a.clone()));
  assert_eq!(
    store.utxos_for_script(&alice).unwrap(),
    vec![(a, utxo_a.clone())]
  );
  assert_eq!(
    store.locate(3).unwrap(),
    Some(SatLocation {
      kind: LocationKind::Unspent,
      outpoint: a,
      offset: 3,
      piece: utxo_a.pieces[0],
    })
  );
  assert_eq!(store.locate(10).unwrap(), None);
  assert_eq!(
    store.locate(4_999_999_999).unwrap(),
    Some(SatLocation {
      kind: LocationKind::Lost,
      outpoint: OutPoint::null(),
      offset: 0,
      piece: genesis.lost[0],
    })
  );
  assert_eq!(store.moves(0).unwrap(), Some(genesis.moves.clone()));
  assert_eq!(store.moves(1).unwrap(), None);
  assert_eq!(
    store.row_history(0, 0, 10).unwrap(),
    vec![(0, genesis.rows[0].after)]
  );
  assert_eq!(store.row_history(1, 0, 10).unwrap(), Vec::new());

  // Block 1 splits A into B (sats 0..4, burned) and C (4..10), and Bob's
  // coinbase-less block moves nothing else.
  let mut one = block(Some(genesis.hash), 1);
  let utxo_b = TrackedUtxo {
    value: 4,
    script_pubkey: ScriptBuf::from_bytes(vec![0x6a]),
    height: 1,
    kind: UtxoKind::Unspendable,
    pieces: vec![Piece {
      offset: 0,
      start: 0,
      len: 4,
    }],
  };
  let utxo_c = TrackedUtxo {
    value: 4_999_999_996,
    script_pubkey: bob.clone(),
    height: 1,
    kind: UtxoKind::Spendable,
    pieces: vec![Piece {
      offset: 0,
      start: 4,
      len: 6,
    }],
  };
  one.spent = vec![(a, utxo_a.clone())];
  one.created = vec![(b, utxo_b.clone()), (c, utxo_c.clone())];
  one.lost_sats_before = 1;
  one.lost_sats_after = 1;
  one.moves = vec![Move {
    txid: b.txid,
    tx_index: 1,
    coinbase: false,
    input_value: 5_000_000_000,
    output_value: 5_000_000_000,
    inputs: vec![MoveInput {
      vin: 0,
      outpoint: a,
      utxo: utxo_a.clone(),
    }],
    outputs: vec![
      MoveOutput {
        vout: 0,
        value: 4,
        script_pubkey: utxo_b.script_pubkey.clone(),
        kind: UtxoKind::Unspendable,
        pieces: utxo_b.pieces.clone(),
      },
      MoveOutput {
        vout: 1,
        value: 4_999_999_996,
        script_pubkey: bob.clone(),
        kind: UtxoKind::Spendable,
        pieces: utxo_c.pieces.clone(),
      },
    ],
    fee: Vec::new(),
    mined: Vec::new(),
    lost: Vec::new(),
  }];

  store.commit(&[one.clone()], &[11, 0], true).unwrap();

  assert!(store.meta().unwrap().unwrap().fast_sync_complete);
  assert_eq!(store.utxo(a).unwrap(), None);
  assert_eq!(store.utxos_for_script(&alice).unwrap(), Vec::new());
  assert_eq!(
    store.utxos_for_script(&bob).unwrap(),
    vec![(c, utxo_c.clone())]
  );
  assert_eq!(store.locate(2).unwrap().unwrap().kind, LocationKind::Burned);
  assert_eq!(
    store.locate(9).unwrap(),
    Some(SatLocation {
      kind: LocationKind::Unspent,
      outpoint: c,
      offset: 5,
      piece: utxo_c.pieces[0],
    })
  );
  assert_eq!(store.moves(1).unwrap(), Some(one.moves.clone()));

  // Blocks without moves have an empty list.
  let mut two = block(Some(one.hash), 2);
  two.lost_sats_before = 1;
  two.lost_sats_after = 1;
  store.commit(&[two], &[11, 0], true).unwrap();
  assert_eq!(store.moves(2).unwrap(), Some(Vec::new()));

  let reader = store.reader().unwrap();
  assert_eq!(
    reader
      .pieces(0, u64::MAX)
      .unwrap()
      .map(|location| location.unwrap().piece.start)
      .collect::<Vec<u64>>(),
    vec![0, 4, 4_999_999_999]
  );
  // A piece that starts before `from` but overlaps it is included.
  assert_eq!(
    reader
      .pieces(5, 6)
      .unwrap()
      .map(|location| location.unwrap().piece.start)
      .collect::<Vec<u64>>(),
    vec![4]
  );
  assert_eq!(reader.pieces(10, 11).unwrap().count(), 0);
  assert_eq!(
    reader.row_delta(0, 0).unwrap(),
    Some((RowStats::default(), genesis.rows[0].after))
  );
  assert_eq!(reader.row_delta(0, 1).unwrap(), None);
  assert_eq!(
    reader.row_stats_at(0, 2).unwrap(),
    Some((0, genesis.rows[0].after))
  );
  assert_eq!(reader.lost_sats().unwrap(), 1);
  assert_eq!(reader.mined().unwrap(), vec![11, 0]);
}

#[test]
fn commit_rejects_gaps_and_forks() {
  let dir = TempDir::new().unwrap();
  let store = store(&dir, MovesRetention::All);

  let genesis = block(None, 0);
  assert!(store.commit(&[block(None, 1)], &[], false).is_err());
  store
    .commit(std::slice::from_ref(&genesis), &[], false)
    .unwrap();
  assert!(
    store
      .commit(std::slice::from_ref(&genesis), &[], false)
      .is_err()
  );

  let fork = block(Some(BlockHash::all_zeros()), 1);
  let err = store.commit(&[fork], &[], false).unwrap_err().to_string();
  assert!(err.contains("does not extend"), "{err}");

  let mut wrong_hash = block(Some(genesis.hash), 1);
  wrong_hash.hash = genesis.hash;
  assert!(store.commit(&[wrong_hash], &[], false).is_err());

  let one = block(Some(genesis.hash), 1);
  let three = block(Some(one.hash), 3);
  assert!(store.commit(&[one.clone(), three], &[], false).is_err());
  assert_eq!(store.meta().unwrap().unwrap().height, Some(0));
  store.commit(&[one], &[], false).unwrap();
}

#[test]
fn batched_commits_equal_block_by_block_commits() {
  let mut chain = TestChain::new(42);
  let deltas = (0..120).map(|_| chain.next_block()).collect::<Vec<_>>();

  let one_dir = TempDir::new().unwrap();
  let one_by_one = store(&one_dir, MovesRetention::All);
  let mut mined = vec![0; ROWS];
  for delta in &deltas {
    for row in &delta.rows {
      mined[usize::from(row.row)] = row.after.mined;
    }
    one_by_one
      .commit_with(
        std::slice::from_ref(delta),
        &[market_for(delta)],
        &mined,
        false,
        &CommitOptions {
          durable: true,
          target_height: None,
        },
      )
      .unwrap();
  }

  let batch_dir = TempDir::new().unwrap();
  let batched = store(&batch_dir, MovesRetention::All);
  for batch in deltas.chunks(37) {
    let mut mined = vec![0; ROWS];
    for delta in &deltas[..=usize::try_from(batch.last().unwrap().height).unwrap()] {
      for row in &delta.rows {
        mined[usize::from(row.row)] = row.after.mined;
      }
    }
    batched
      .commit_with(
        batch,
        &markets(batch),
        &mined,
        false,
        &CommitOptions {
          durable: true,
          target_height: None,
        },
      )
      .unwrap();
  }

  assert_eq!(dump(&batched, true), dump(&one_by_one, true));

  assert_eq!(
    sorted(batched.load_snapshot().unwrap()),
    sorted(chain.snapshot())
  );

  // Every block's moves read back, resolving references to older blocks.
  for delta in &deltas {
    assert_eq!(batched.moves(delta.height).unwrap().unwrap(), delta.moves);
  }

  // Every tracked piece is where the model says.
  let reader = batched.reader().unwrap();
  for (outpoint, utxo) in &chain.utxos {
    assert_eq!(reader.utxo(*outpoint).unwrap().as_ref(), Some(utxo));
    for piece in &utxo.pieces {
      let location = reader.locate(piece.start + piece.len - 1).unwrap().unwrap();
      assert_eq!(location.outpoint, *outpoint);
      assert_eq!(location.piece, *piece);
      assert_eq!(location.offset, piece.offset_end() - 1);
    }
    assert!(
      reader
        .utxos_for_script(&utxo.script_pubkey)
        .unwrap()
        .contains(&(*outpoint, utxo.clone()))
    );
  }
  for piece in &chain.lost {
    assert_eq!(
      reader.locate(piece.start).unwrap().unwrap().kind,
      LocationKind::Lost
    );
  }
  assert!(
    !chain.overwritten.is_empty(),
    "no BIP30 overwrite generated"
  );
  for (outpoint, height, utxo) in &chain.overwritten {
    assert!(
      reader
        .overwritten(*outpoint)
        .unwrap()
        .contains(&(*height, utxo.clone()))
    );
    for piece in &utxo.pieces {
      assert_eq!(
        reader.locate(piece.start).unwrap().unwrap().kind,
        LocationKind::Overwritten
      );
    }
  }
}

#[test]
fn rollback_restores_every_table() {
  let dir = TempDir::new().unwrap();
  let store = store(&dir, MovesRetention::All);

  let mut chain = TestChain::new(9);
  let mut deltas = Vec::new();
  let mut dumps = vec![dump(&store, false)];
  let mut mined = vec![0; ROWS];
  let mut minted = vec![mined.clone()];
  for _ in 0..60 {
    let delta = chain.next_block();
    for row in &delta.rows {
      mined[usize::from(row.row)] = row.after.mined;
    }
    store
      .commit_with(
        std::slice::from_ref(&delta),
        &[market_for(&delta)],
        &mined,
        false,
        &CommitOptions {
          durable: true,
          target_height: None,
        },
      )
      .unwrap();
    dumps.push(dump(&store, false));
    minted.push(mined.clone());
    deltas.push(delta);
  }

  for (i, delta) in deltas.iter().enumerate().rev() {
    assert_eq!(store.rollback_tip().unwrap().as_ref(), Some(delta));
    let mut expected = dumps[i].clone();
    let mut actual = dump(&store, false);
    // The mined counters of rows the block did not touch may differ in
    // length only; compare them decoded.
    let decoded = |dump: &mut Dump| {
      let meta = dump.get_mut("meta").unwrap();
      let index = meta
        .iter()
        .position(|(key, _)| key == KEY_MINED.as_bytes())
        .unwrap();
      let (_, value) = meta.remove(index);
      let mut values = decode_u64s(&value).unwrap();
      values.resize(ROWS, 0);
      values
    };
    assert_eq!(decoded(&mut actual), minted[i]);
    let _ = decoded(&mut expected);
    // `moves_refs` and `sale_piece_max_len` are conservative bounds that
    // rollbacks leave as they are.
    for dump in [&mut actual, &mut expected] {
      dump.get_mut("meta").unwrap().retain(|(key, _)| {
        key != KEY_MOVES_REFS.as_bytes() && key != KEY_SALE_PIECE_MAX_LEN.as_bytes()
      });
    }
    assert_eq!(
      actual, expected,
      "after rolling back block {}",
      delta.height
    );
  }

  assert_eq!(store.rollback_tip().unwrap(), None);
  assert_eq!(store.meta().unwrap().unwrap().height, None);
}

#[test]
fn undo_records_cover_the_last_144_blocks() {
  let dir = TempDir::new().unwrap();
  let store = store(&dir, MovesRetention::Blocks(0));

  let mut chain = TestChain::new(5);
  chain.txs_per_block = 1;
  let deltas = (0..200).map(|_| chain.next_block()).collect::<Vec<_>>();
  store.commit(&deltas[..150], &chain.mined(), false).unwrap();
  store.commit(&deltas[150..], &chain.mined(), false).unwrap();

  assert_eq!(
    store.reader().unwrap().undo_heights().unwrap(),
    (56..200).collect::<Vec<u32>>()
  );

  for delta in deltas[56..].iter().rev() {
    assert_eq!(store.rollback_tip().unwrap().unwrap().height, delta.height);
  }
  assert_eq!(store.rollback_tip().unwrap(), None);
  assert_eq!(store.meta().unwrap().unwrap().height, Some(55));
  assert_eq!(store.meta().unwrap().unwrap().hash, Some(deltas[55].hash));

  // The index goes on from where the rollback left it.
  store
    .commit(&deltas[56..60], &chain.mined(), false)
    .unwrap();
  assert_eq!(store.meta().unwrap().unwrap().height, Some(59));
}

#[test]
fn snapshot_round_trip_and_restart() {
  let dir = TempDir::new().unwrap();
  let mut chain = TestChain::new(11);
  let deltas = (0..80).map(|_| chain.next_block()).collect::<Vec<_>>();

  {
    let store = store(&dir, MovesRetention::All);
    assert_eq!(store.load_snapshot().unwrap(), TrackerSnapshot::default());
    store.commit(&deltas[..50], &chain.mined(), false).unwrap();
    store.commit(&deltas[50..], &chain.mined(), true).unwrap();
    assert_eq!(
      sorted(store.load_snapshot().unwrap()),
      sorted(chain.snapshot())
    );
  }

  let store = store(&dir, MovesRetention::All);
  let meta = store.meta().unwrap().unwrap();
  assert_eq!(meta.height, Some(79));
  assert!(meta.fast_sync_complete);
  assert_eq!(
    sorted(store.load_snapshot().unwrap()),
    sorted(chain.snapshot())
  );
  assert_eq!(store.moves(79).unwrap().unwrap(), deltas[79].moves);
}

#[test]
fn non_durable_commits_are_visible_and_persist_at_the_next_durable_one() {
  let dir = TempDir::new().unwrap();
  let mut chain = TestChain::new(13);
  let deltas = (0..30).map(|_| chain.next_block()).collect::<Vec<_>>();
  {
    let store = store(&dir, MovesRetention::All);
    for batch in deltas.chunks(10) {
      store
        .commit_with(
          batch,
          &[],
          &chain.mined(),
          false,
          &CommitOptions {
            durable: false,
            target_height: None,
          },
        )
        .unwrap();
      assert_eq!(
        store.meta().unwrap().unwrap().height,
        Some(batch.last().unwrap().height)
      );
    }
    store.commit(&[], &chain.mined(), true).unwrap();
  }
  let store = store(&dir, MovesRetention::All);
  let meta = store.meta().unwrap().unwrap();
  assert_eq!(meta.height, Some(29));
  assert!(meta.fast_sync_complete);
}

#[test]
fn bounded_moves_retention() {
  let dir = TempDir::new().unwrap();
  let store = store(&dir, MovesRetention::Blocks(5));
  let mut chain = TestChain::new(17);
  let deltas = (0..30).map(|_| chain.next_block()).collect::<Vec<_>>();

  // A fast sync to height 29 does not write moves it would prune.
  store
    .commit_with(
      &deltas[..20],
      &[],
      &chain.mined(),
      false,
      &CommitOptions {
        durable: true,
        target_height: Some(29),
      },
    )
    .unwrap();
  assert_eq!(
    store.reader().unwrap().heights_with_moves(0, 100).unwrap(),
    Vec::<u32>::new()
  );
  assert_eq!(store.reader().unwrap().moves_floor().unwrap(), 25);
  assert_eq!(store.moves(19).unwrap(), None);

  store.commit(&deltas[20..], &chain.mined(), true).unwrap();
  assert_eq!(
    store.reader().unwrap().heights_with_moves(0, 100).unwrap(),
    (25..30).collect::<Vec<u32>>()
  );
  for delta in &deltas[25..] {
    assert_eq!(store.moves(delta.height).unwrap().unwrap(), delta.moves);
  }
  assert_eq!(store.moves(24).unwrap(), None);

  // Rolling back keeps the floor; the rolled-back heights have no moves.
  store.rollback_tip().unwrap().unwrap();
  assert_eq!(store.moves(29).unwrap(), None);
  assert_eq!(store.moves(28).unwrap().unwrap(), deltas[28].moves);
}

#[test]
fn lowering_retention_keeps_retained_moves_readable() {
  let dir = TempDir::new().unwrap();
  let mut chain = TestChain::new(19);
  chain.txs_per_block = 6;
  let deltas = (0..60).map(|_| chain.next_block()).collect::<Vec<_>>();

  {
    let store = store(&dir, MovesRetention::All);
    store.commit(&deltas[..40], &chain.mined(), false).unwrap();
    let reader = store.reader().unwrap();
    assert_eq!(reader.moves_floor().unwrap(), 0);
    let refs = store
      .db
      .begin_read()
      .unwrap()
      .open_table(MOVES)
      .unwrap()
      .iter()
      .unwrap()
      .filter(|entry| moves_have_external_refs(entry.as_ref().unwrap().1.value()))
      .count();
    assert!(refs > 0, "unbounded retention should store references");
  }

  // Blocks 30..40 stay and refer to blocks below 30, which go.
  let store = store(&dir, MovesRetention::Blocks(30));
  store.commit(&deltas[40..], &chain.mined(), false).unwrap();
  assert_eq!(store.reader().unwrap().moves_floor().unwrap(), 30);
  for delta in &deltas[30..] {
    assert_eq!(store.moves(delta.height).unwrap().unwrap(), delta.moves);
  }
  for delta in &deltas[..30] {
    assert_eq!(store.moves(delta.height).unwrap(), None);
  }
  let tx = store.db.begin_read().unwrap();
  for entry in tx.open_table(MOVES).unwrap().iter().unwrap() {
    assert!(!moves_have_external_refs(entry.unwrap().1.value()));
  }
}

#[test]
fn refuses_incompatible_indexes() {
  let expected = meta();

  let mut other = expected.clone();
  other.schema_version += 1;
  let err = other.check_compatible(&expected).unwrap_err().to_string();
  assert!(err.contains("--reindex"), "{err}");

  let mut other = expected.clone();
  other.chain = "mainnet".into();
  let err = other.check_compatible(&expected).unwrap_err().to_string();
  assert!(err.contains("chain mainnet"), "{err}");

  let mut other = expected.clone();
  other.config.extracted_max_value += 1;
  let err = other.check_compatible(&expected).unwrap_err().to_string();
  assert!(err.contains("--reindex"), "{err}");

  let mut other = expected.clone();
  other.catalog_fingerprint = "other".into();
  let err = other.check_compatible(&expected).unwrap_err().to_string();
  assert!(err.contains("--reindex"), "{err}");

  let mut other = expected.clone();
  other.height = Some(5);
  other.fast_sync_complete = true;
  other.check_compatible(&expected).unwrap();

  // An index written by another schema version is refused on read.
  let dir = TempDir::new().unwrap();
  {
    let store = store(&dir, MovesRetention::All);
    let tx = store.db.begin_write().unwrap();
    tx.open_table(META)
      .unwrap()
      .insert(KEY_SCHEMA_VERSION, 999u32.to_le_bytes().as_slice())
      .unwrap();
    tx.commit().unwrap();
  }
  let store = Store::open(&dir.path().join("index.redb")).unwrap();
  let err = store.meta().unwrap_err().to_string();
  assert!(err.contains("schema version 999"), "{err}");
  assert!(err.contains("--reindex"), "{err}");
}

#[test]
fn refuses_foreign_databases_and_double_init() {
  let dir = TempDir::new().unwrap();
  let path = dir.path().join("index.redb");
  {
    let db = Database::create(&path).unwrap();
    let tx = db.begin_write().unwrap();
    tx.open_table(TableDefinition::<u64, u64>::new("HEIGHT_TO_BLOCK_HEADER"))
      .unwrap();
    tx.commit().unwrap();
  }
  let err = Store::open(&path).err().unwrap().to_string();
  assert!(err.contains("not an ord rare-sat tracker index"), "{err}");

  let dir = TempDir::new().unwrap();
  let store = store(&dir, MovesRetention::All);
  assert!(store.init(&meta()).is_err());
}

#[test]
fn wipe_empties_the_index() {
  let dir = TempDir::new().unwrap();
  let store = store(&dir, MovesRetention::All);
  let mut chain = TestChain::new(23);
  let deltas = (0..10).map(|_| chain.next_block()).collect::<Vec<_>>();
  store.commit(&deltas, &chain.mined(), false).unwrap();
  store.wipe().unwrap();
  assert_eq!(store.meta().unwrap(), None);
  assert!(
    store
      .table_info()
      .unwrap()
      .iter()
      .all(|table| table.entries == 0)
  );
  store.init(&meta()).unwrap();
  store.commit(&deltas, &chain.mined(), false).unwrap();
  assert_eq!(store.meta().unwrap().unwrap().height, Some(9));
}

#[test]
fn table_info_lists_every_table() {
  let dir = TempDir::new().unwrap();
  let store = store(&dir, MovesRetention::All);
  let mut chain = TestChain::new(29);
  let deltas = (0..10).map(|_| chain.next_block()).collect::<Vec<_>>();
  store.commit(&deltas, &chain.mined(), false).unwrap();
  let info = store.table_info().unwrap();
  let names = info
    .iter()
    .map(|table| table.name.as_str())
    .collect::<BTreeSet<&str>>();
  assert_eq!(
    names,
    [
      "headers",
      "inscribed",
      "locator",
      "lost",
      "meta",
      "moves",
      "overwritten",
      "row_history",
      "sale_pieces",
      "sale_txs",
      "sales",
      "scripts",
      "undo",
      "unreferenced",
      "utxos"
    ]
    .into_iter()
    .collect()
  );
  let headers = info.iter().find(|table| table.name == "headers").unwrap();
  assert_eq!(headers.entries, 10);
}

#[test]
fn outputs_missing_from_their_blocks_moves_are_never_referenced() {
  let dir = TempDir::new().unwrap();
  let store = store(&dir, MovesRetention::All);
  let mut chain = TestChain::new(31);
  chain.txs_per_block = 3;
  let mut deltas = (0..20).map(|_| chain.next_block()).collect::<Vec<_>>();

  // Block 3's moves lose their outputs, as a buggy tracker might.
  let stripped = deltas[3]
    .moves
    .iter_mut()
    .flat_map(|m| m.outputs.drain(..))
    .count();
  assert!(stripped > 0);

  store.commit(&deltas[..10], &chain.mined(), false).unwrap();
  store.commit(&deltas[10..], &chain.mined(), false).unwrap();

  // Every block's moves still read back: spends of block 3's outputs were
  // stored in full.
  for delta in &deltas {
    assert_eq!(store.moves(delta.height).unwrap().unwrap(), delta.moves);
  }

  // Only block 3's still-unspent outputs remain listed.
  let tx = store.db.begin_read().unwrap();
  let listed = raw(&tx.open_table(UNREFERENCED).unwrap());
  let unspent_from_3 = chain
    .utxos
    .iter()
    .filter(|(_, utxo)| utxo.height == 3)
    .count();
  assert_eq!(listed.len(), unspent_from_3);
}

#[test]
fn find_output_uses_the_txid_index() {
  let txid = |prefix: u8, rest: u8| {
    let mut bytes = [rest; 32];
    bytes[..8].copy_from_slice(&[prefix; 8]);
    Txid::from_byte_array(bytes)
  };
  let output = |vout: u32, value: u64| MoveOutput {
    vout,
    value,
    script_pubkey: p2wpkh(vout as u8),
    kind: UtxoKind::Spendable,
    pieces: vec![Piece {
      offset: 0,
      start: value,
      len: 1,
    }],
  };
  let m = |txid: Txid, outputs: Vec<MoveOutput>| Move {
    txid,
    tx_index: 1,
    coinbase: false,
    input_value: 0,
    output_value: 0,
    inputs: Vec::new(),
    outputs,
    fee: Vec::new(),
    mined: Vec::new(),
    lost: Vec::new(),
  };
  // Two txids share their first 8 bytes; one txid is in two moves.
  let moves = vec![
    m(txid(9, 1), vec![output(0, 10)]),
    m(txid(3, 1), vec![output(1, 11), output(2, 12)]),
    m(txid(3, 2), vec![output(0, 13)]),
    m(txid(9, 1), vec![output(5, 14)]),
  ];
  let bytes = encode_moves(&moves, 7, RefPolicy::SELF_CONTAINED);
  assert_eq!(decode_moves(&bytes, 7).unwrap().0, moves);

  let find = |txid: Txid, vout: u32| {
    find_output(&bytes, 7, &OutPoint { txid, vout })
      .unwrap()
      .map(|utxo| utxo.value)
  };
  assert_eq!(find(txid(3, 1), 1), Some(11));
  assert_eq!(find(txid(3, 1), 2), Some(12));
  assert_eq!(find(txid(3, 2), 0), Some(13));
  assert_eq!(find(txid(9, 1), 0), Some(10));
  assert_eq!(find(txid(9, 1), 5), Some(14));
  assert_eq!(find(txid(3, 2), 1), None);
  assert_eq!(find(txid(4, 1), 0), None);
  let utxo = find_output(
    &bytes,
    7,
    &OutPoint {
      txid: txid(3, 1),
      vout: 2,
    },
  )
  .unwrap()
  .unwrap();
  assert_eq!(utxo.height, 7);
  assert_eq!(utxo.script_pubkey, p2wpkh(2));

  // An empty block's record.
  let empty = encode_moves(&[], 3, RefPolicy::SELF_CONTAINED);
  assert_eq!(decode_moves(&empty, 3).unwrap().0, Vec::new());
  assert_eq!(find_output(&empty, 3, &OutPoint::null()).unwrap(), None);
}

/// Brute-force expectations for the market tables of `deltas`.
struct ExpectedMarket {
  sales: Vec<StoredSale>,
  inscribed: BTreeMap<u64, Inscribed>,
}

impl ExpectedMarket {
  fn of(deltas: &[BlockDelta]) -> Self {
    let mut sales = Vec::new();
    let mut inscribed = BTreeMap::<u64, Inscribed>::new();
    for delta in deltas {
      let market = market_for(delta);
      for sale in market.sales {
        let buyer_value = sale.buyer_vout.map(|vout| {
          delta
            .moves
            .iter()
            .find(|m| m.txid == sale.txid)
            .unwrap()
            .outputs
            .iter()
            .find(|output| output.vout == vout)
            .unwrap()
            .value
        });
        sales.push(StoredSale {
          height: delta.height,
          sale,
          buyer_value,
        });
      }
      for event in market.inscriptions {
        inscribed
          .entry(event.sat)
          .and_modify(|inscribed| inscribed.count += 1)
          .or_insert(Inscribed {
            count: 1,
            first_txid: event.txid,
            first_height: delta.height,
          });
      }
    }
    sales.sort_by_key(StoredSale::key);
    Self { sales, inscribed }
  }

  fn check(&self, store: &Store) {
    let reader = store.reader().unwrap();
    let sales = reader
      .sales((0, 0, 0), u32::MAX)
      .unwrap()
      .collect::<anyhow::Result<Vec<_>>>()
      .unwrap();
    assert_eq!(sales, self.sales);

    for sale in &self.sales {
      let of_tx = self
        .sales
        .iter()
        .filter(|other| other.sale.txid == sale.sale.txid)
        .cloned()
        .collect::<Vec<_>>();
      assert_eq!(reader.sales_for_tx(sale.sale.txid).unwrap(), of_tx);

      // The first and last sat of every sold piece.
      for piece in &sale.sale.seller.pieces {
        for sat in [piece.start, piece.end() - 1] {
          let covering = self
            .sales
            .iter()
            .filter(|other| {
              other
                .sale
                .seller
                .pieces
                .iter()
                .any(|piece| piece.start <= sat && sat < piece.end())
            })
            .cloned()
            .collect::<Vec<_>>();
          assert_eq!(reader.sales_for_sat(sat).unwrap(), covering, "sat {sat}");
        }
      }
    }
    assert_eq!(
      reader.sales_for_tx(test_txid(u64::MAX)).unwrap(),
      Vec::new()
    );
    assert_eq!(reader.sales_for_sat(u64::MAX - 1).unwrap(), Vec::new());

    let inscribed = reader
      .inscribed_from(0)
      .unwrap()
      .collect::<anyhow::Result<BTreeMap<_, _>>>()
      .unwrap();
    assert_eq!(inscribed, self.inscribed);
    for (sat, expected) in &self.inscribed {
      assert_eq!(reader.inscribed(*sat).unwrap(), Some(*expected));
      assert_eq!(
        reader.inscribed_in(*sat, sat + 1).unwrap(),
        vec![(*sat, *expected)]
      );
    }
    assert_eq!(reader.inscribed(1).unwrap(), None);
  }
}

fn commit_market(store: &Store, deltas: &[BlockDelta], mined: &[u64]) {
  store
    .commit_with(
      deltas,
      &markets(deltas),
      mined,
      false,
      &CommitOptions {
        durable: true,
        target_height: None,
      },
    )
    .unwrap();
}

#[test]
fn market_rows_round_trip_roll_back_and_persist() {
  let dir = TempDir::new().unwrap();
  let mut chain = TestChain::new(37);
  chain.txs_per_block = 6;
  let deltas = (0..90).map(|_| chain.next_block()).collect::<Vec<_>>();
  let expected = ExpectedMarket::of(&deltas);
  assert!(expected.sales.len() > 200);
  assert!(
    expected
      .inscribed
      .values()
      .any(|inscribed| inscribed.count > 1)
  );
  assert!(
    expected
      .sales
      .iter()
      .any(|sale| sale.sale.seller.pieces.iter().any(|piece| piece.len > 1)),
    "the sales should include pieces of several sats"
  );

  {
    let store = store(&dir, MovesRetention::Blocks(10));
    commit_market(&store, &deltas[..40], &chain.mined());
    for delta in &deltas[40..] {
      commit_market(&store, std::slice::from_ref(delta), &chain.mined());
    }
    // Sales are kept whatever the moves retention.
    assert_eq!(store.reader().unwrap().moves_floor().unwrap(), 80);
    expected.check(&store);

    // Rolling back blocks removes their sales and inscriptions.
    for _ in 0..20 {
      store.rollback_tip().unwrap().unwrap();
    }
    ExpectedMarket::of(&deltas[..70]).check(&store);
    for delta in &deltas[70..] {
      commit_market(&store, std::slice::from_ref(delta), &chain.mined());
    }
  }

  // And all of it survives a restart.
  let store = store(&dir, MovesRetention::Blocks(10));
  expected.check(&store);
}

#[test]
fn sale_records_round_trip() {
  let mut chain = TestChain::new(41);
  chain.txs_per_block = 3;
  for _ in 0..20 {
    let delta = chain.next_block();
    for sale in market_for(&delta).sales {
      for buyer_value in [None, Some(546)] {
        assert_eq!(
          decode_sale(&encode_sale(&sale, buyer_value)).unwrap(),
          (sale.clone(), buyer_value)
        );
      }
      let unknown = Sale {
        kind: SaleKind::Unknown,
        marketplace: "unknown".into(),
        payment_vout: None,
        buyer_vout: None,
        buyer_script: None,
        ..sale
      };
      assert_eq!(
        decode_sale(&encode_sale(&unknown, None)).unwrap(),
        (unknown, None)
      );
    }
  }
  assert_eq!(
    decode_inscribed(&encode_inscribed(3, &test_txid(1), 777)).unwrap(),
    (3, test_txid(1), 777)
  );
}
