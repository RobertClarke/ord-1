//! Response shapes of docs/API.md and their rendering from store records.

use {
  super::catalog::Catalog,
  crate::{
    model::{
      Holder, LocationKind, Move, Piece, RowCounts, RowId, RowStats, SatLocation, TrackedUtxo,
      TrackerConfig, UtxoKind,
    },
    store::Reader,
  },
  bitcoin::{Address, BlockHash, Network, OutPoint, Script, Txid},
  serde::{Serialize, Serializer, ser::SerializeMap},
  std::{cell::RefCell, collections::HashMap},
};

/// `{ "<row id>": count }` in `RowId` order, without zeros.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowMap(pub Vec<(String, u64)>);

impl Serialize for RowMap {
  fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(Some(self.0.len()))?;
    for (id, count) in &self.0 {
      map.serialize_entry(id, count)?;
    }
    map.end()
  }
}

/// `{ "1y": n, … }`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct YearsMap(pub Vec<(u32, u64)>);

impl Serialize for YearsMap {
  fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(Some(self.0.len()))?;
    for (years, count) in &self.0 {
      map.serialize_entry(&format!("{years}y"), count)?;
    }
    map.end()
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PieceJson {
  pub offset: u64,
  pub start: u64,
  pub len: u64,
  pub types: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutputJson {
  pub outpoint: OutPoint,
  pub value: u64,
  pub address: Option<String>,
  pub script_pubkey: String,
  pub kind: UtxoKind,
  pub created_height: u32,
  pub created_time: Option<u32>,
  pub extracted: bool,
  /// Omitted in `/sats` lines unless asked for (outputs can hold tens of
  /// thousands of pieces).
  #[serde(skip_serializing_if = "Option::is_none")]
  pub pieces: Option<Vec<PieceJson>>,
  pub rows: RowMap,
  /// Tracked sats of the output that carry inscriptions.
  pub inscribed_sats: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocationJson {
  pub kind: LocationKind,
  pub satpoint: String,
  pub outpoint: OutPoint,
  pub offset: u64,
  pub output: Option<OutputJson>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SatJson {
  pub sat: u64,
  pub height: u32,
  pub types: Vec<&'static str>,
  pub tracked: bool,
  pub rows: Vec<String>,
  pub location: Option<LocationJson>,
  /// Carries an inscription (tracked sats only).
  pub inscribed: bool,
  /// Envelopes that landed on it, counting reinscriptions.
  pub inscriptions: u32,
  /// Sales that sold it, oldest first.
  pub sales: Vec<super::sales::SaleJson>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MoveInputJson {
  pub vin: u32,
  pub outpoint: OutPoint,
  pub value: u64,
  pub address: Option<String>,
  pub script_pubkey: String,
  pub created_height: u32,
  pub created_time: Option<u32>,
  pub extracted: bool,
  pub pieces: Vec<PieceJson>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MoveOutputJson {
  pub vout: u32,
  pub value: u64,
  pub address: Option<String>,
  pub script_pubkey: String,
  pub kind: UtxoKind,
  pub extracted: bool,
  pub pieces: Vec<PieceJson>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MoveJson {
  pub txid: Txid,
  pub tx_index: u32,
  pub coinbase: bool,
  pub input_value: u64,
  pub output_value: u64,
  pub fee: u64,
  pub inputs: Vec<MoveInputJson>,
  pub outputs: Vec<MoveOutputJson>,
  pub fee_pieces: Vec<PieceJson>,
  pub mined_pieces: Vec<PieceJson>,
  pub lost_pieces: Vec<PieceJson>,
  pub rows: RowMap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HolderJson {
  pub address: Option<String>,
  pub script_pubkey: String,
  pub n: u64,
  pub utxos: u64,
  pub last_received_height: u32,
  pub last_received_time: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeltaJson {
  pub height: u32,
  pub before: RowStats,
  pub after: RowStats,
}

/// `RowStats` with the current `inscribed` count (`/supply` only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SupplyStatsJson {
  #[serde(flatten)]
  pub stats: RowStats,
  /// Located sats of the row that carry inscriptions.
  pub inscribed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RowSupplyJson {
  pub id: String,
  pub total: u64,
  pub stats: SupplyStatsJson,
  pub active_non_extracted: YearsMap,
  pub delta: Option<DeltaJson>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub top_extracted_holders: Option<Vec<HolderJson>>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub top_non_extracted_holders: Option<Vec<HolderJson>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockJson {
  pub height: u32,
  pub hash: BlockHash,
  pub prev_hash: BlockHash,
  pub time: u32,
  /// `null` when the block's moves are not retained.
  pub moves: Option<u64>,
  pub rows_moved: Option<RowMap>,
}

/// Renders store records with the catalog's types and rows and the
/// headers' times. One per request (it caches header times).
pub struct Render<'a> {
  pub catalog: &'a dyn Catalog,
  pub reader: &'a Reader,
  pub network: Network,
  pub config: &'a TrackerConfig,
  times: RefCell<HashMap<u32, Option<u32>>>,
}

impl<'a> Render<'a> {
  pub fn new(
    catalog: &'a dyn Catalog,
    reader: &'a Reader,
    network: Network,
    config: &'a TrackerConfig,
  ) -> Self {
    Self {
      catalog,
      reader,
      network,
      config,
      times: RefCell::default(),
    }
  }

  /// Header time of a block, if indexed.
  pub fn time(&self, height: u32) -> anyhow::Result<Option<u32>> {
    if let Some(time) = self.times.borrow().get(&height) {
      return Ok(*time);
    }
    let time = self.reader.header(height)?.map(|header| header.time);
    self.times.borrow_mut().insert(height, time);
    Ok(time)
  }

  pub fn address(&self, script: &Script) -> Option<String> {
    Address::from_script(script, self.network)
      .ok()
      .map(|address| address.to_string())
  }

  /// `model::output_extracted` (DESIGN.md §5); an output whose creating
  /// block's time is unknown is not extracted.
  pub fn extracted(
    &self,
    kind: UtxoKind,
    value: u64,
    pieces: &[Piece],
    created_time: Option<u32>,
  ) -> bool {
    created_time
      .is_some_and(|time| crate::model::output_extracted(kind, value, pieces, time, self.config))
  }

  pub fn piece(&self, piece: &Piece) -> PieceJson {
    PieceJson {
      offset: piece.offset,
      start: piece.start,
      len: piece.len,
      types: self.catalog.types(piece.start),
    }
  }

  pub fn pieces(&self, pieces: &[Piece]) -> Vec<PieceJson> {
    pieces.iter().map(|piece| self.piece(piece)).collect()
  }

  /// Adds the row counts of `pieces` to `counts` (dense, by `RowId`).
  pub fn count(&self, counts: &mut Vec<u64>, pieces: &[Piece]) {
    for piece in pieces {
      self.add(counts, &self.catalog.row_counts(piece.start, piece.len));
    }
  }

  pub fn add(&self, counts: &mut Vec<u64>, row_counts: &RowCounts) {
    for &(row, count) in row_counts {
      let row = usize::from(row);
      if counts.len() <= row {
        counts.resize(row + 1, 0);
      }
      counts[row] += count;
    }
  }

  pub fn row_map(&self, counts: &[u64]) -> RowMap {
    RowMap(
      counts
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .map(|(row, count)| (self.row_name(row as RowId), *count))
        .collect(),
    )
  }

  pub fn row_name(&self, row: RowId) -> String {
    self
      .catalog
      .rows()
      .get(usize::from(row))
      .map_or_else(|| row.to_string(), |row| row.id.clone())
  }

  pub fn output(&self, outpoint: OutPoint, utxo: &TrackedUtxo) -> anyhow::Result<OutputJson> {
    self.output_with(outpoint, utxo, true)
  }

  /// `with_pieces: false` leaves out the `pieces` array.
  pub fn output_with(
    &self,
    outpoint: OutPoint,
    utxo: &TrackedUtxo,
    with_pieces: bool,
  ) -> anyhow::Result<OutputJson> {
    let created_time = self.time(utxo.height)?;
    let mut counts = Vec::new();
    self.count(&mut counts, &utxo.pieces);
    Ok(OutputJson {
      outpoint,
      value: utxo.value,
      address: self.address(&utxo.script_pubkey),
      script_pubkey: utxo.script_pubkey.to_hex_string(),
      kind: utxo.kind,
      created_height: utxo.height,
      created_time,
      extracted: self.extracted(utxo.kind, utxo.value, &utxo.pieces, created_time),
      pieces: with_pieces.then(|| self.pieces(&utxo.pieces)),
      rows: self.row_map(&counts),
      inscribed_sats: self
        .reader
        .inscribed_in_pieces(&utxo.pieces)?
        .into_iter()
        .map(|(sat, _)| sat)
        .collect(),
    })
  }

  /// The output a location refers to: the tracked UTXO, or for an
  /// overwritten piece the replaced UTXO holding it.
  pub fn location_output(
    &self,
    location: &SatLocation,
    with_pieces: bool,
  ) -> anyhow::Result<Option<OutputJson>> {
    let sat = location.piece.start;
    let utxo = match location.kind {
      LocationKind::Lost => return Ok(None),
      LocationKind::Unspent | LocationKind::Burned => self.reader.utxo(location.outpoint)?,
      LocationKind::Overwritten => self
        .reader
        .overwritten(location.outpoint)?
        .into_iter()
        .map(|(_, utxo)| utxo)
        .find(|utxo| {
          utxo
            .pieces
            .iter()
            .any(|piece| piece.start <= sat && sat < piece.end())
        }),
    };
    utxo
      .map(|utxo| self.output_with(location.outpoint, &utxo, with_pieces))
      .transpose()
  }

  pub fn location(&self, location: &SatLocation) -> anyhow::Result<LocationJson> {
    Ok(LocationJson {
      kind: location.kind,
      satpoint: format!("{}:{}", location.outpoint, location.offset),
      outpoint: location.outpoint,
      offset: location.offset,
      output: self.location_output(location, true)?,
    })
  }

  pub fn sat(&self, sat: u64) -> anyhow::Result<SatJson> {
    let tracked = self.catalog.is_tracked(sat);
    let rows = self
      .catalog
      .sat_rows(sat)
      .into_iter()
      .map(|row| self.row_name(row))
      .collect();
    let location = match self.reader.locate(sat)? {
      Some(location) => Some(self.location(&location)?),
      None => None,
    };
    let inscribed = self.reader.inscribed(sat)?;
    let sales = self
      .reader
      .sales_for_sat(sat)?
      .iter()
      .map(|sale| self.sale(sale))
      .collect::<anyhow::Result<_>>()?;
    Ok(SatJson {
      sat,
      height: ordinals::Sat(sat).height().n(),
      types: self.catalog.types(sat),
      tracked,
      rows,
      location,
      inscribed: inscribed.is_some(),
      inscriptions: inscribed.map_or(0, |inscribed| inscribed.count),
      sales,
    })
  }

  /// Row counts of the sats a move moved: for the coinbase, the sats it
  /// minted or received (its outputs and lost pieces); otherwise its
  /// tracked inputs.
  pub fn move_counts(&self, m: &Move) -> Vec<u64> {
    let mut counts = Vec::new();
    if m.coinbase {
      for output in &m.outputs {
        self.count(&mut counts, &output.pieces);
      }
      self.count(&mut counts, &m.lost);
    } else {
      for input in &m.inputs {
        self.count(&mut counts, &input.utxo.pieces);
      }
    }
    counts
  }

  /// `time` is the block's.
  pub fn move_json(&self, m: &Move, time: Option<u32>) -> anyhow::Result<MoveJson> {
    let mut inputs = Vec::with_capacity(m.inputs.len());
    for input in &m.inputs {
      let created_time = self.time(input.utxo.height)?;
      inputs.push(MoveInputJson {
        vin: input.vin,
        outpoint: input.outpoint,
        value: input.utxo.value,
        address: self.address(&input.utxo.script_pubkey),
        script_pubkey: input.utxo.script_pubkey.to_hex_string(),
        created_height: input.utxo.height,
        created_time,
        extracted: self.extracted(
          input.utxo.kind,
          input.utxo.value,
          &input.utxo.pieces,
          created_time,
        ),
        pieces: self.pieces(&input.utxo.pieces),
      });
    }

    let outputs = m
      .outputs
      .iter()
      .map(|output| MoveOutputJson {
        vout: output.vout,
        value: output.value,
        address: self.address(&output.script_pubkey),
        script_pubkey: output.script_pubkey.to_hex_string(),
        kind: output.kind,
        extracted: self.extracted(output.kind, output.value, &output.pieces, time),
        pieces: self.pieces(&output.pieces),
      })
      .collect();

    Ok(MoveJson {
      txid: m.txid,
      tx_index: m.tx_index,
      coinbase: m.coinbase,
      input_value: m.input_value,
      output_value: m.output_value,
      fee: m.input_value.saturating_sub(m.output_value),
      inputs,
      outputs,
      fee_pieces: self.pieces(&m.fee),
      mined_pieces: self.pieces(&m.mined),
      lost_pieces: self.pieces(&m.lost),
      rows: self.row_map(&self.move_counts(m)),
    })
  }

  pub fn holder(&self, holder: &Holder) -> anyhow::Result<HolderJson> {
    Ok(HolderJson {
      address: self.address(&holder.script_pubkey),
      script_pubkey: holder.script_pubkey.to_hex_string(),
      n: holder.count,
      utxos: holder.utxos,
      last_received_height: holder.last_received_height,
      last_received_time: self.time(holder.last_received_height)?,
    })
  }
}
