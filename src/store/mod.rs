//! Persistence (redb): tracked UTXOs, the sat locator, the script index,
//! per-block moves, per-row history, headers and undo records for reorgs.
//! See docs/DESIGN.md §6.
//!
//! OWNER: store+server+indexer agent. The public API below is used by the
//! indexer and server; extend freely, keep the listed signatures.
//!
//! # Tables
//!
//! Integers in keys are big-endian (redb's fixed-width integer keys order
//! numerically); values are the compact encodings of `codec.rs` (LEB128
//! varints, txids as 32 raw bytes in internal byte order).
//!
//! | table | key | value |
//! |---|---|---|
//! | `meta` | `&str` | see below |
//! | `headers` | height `u32` | the 80-byte consensus header (the block hash is its double SHA-256) |
//! | `utxos` | outpoint: txid ‖ vout BE (36 bytes) | `TrackedUtxo`: value, height, kind:u8, script blob, pieces |
//! | `locator` | first sat of a piece `u64` | kind:u8 (0 unspent, 1 burned, 2 lost, 3 overwritten), outpoint (txid ‖ vout; absent for lost), offset of the piece's first sat, len |
//! | `scripts` (multimap) | `0x00 ‖ script` (≤ 64 bytes) or `0x01 ‖ sha256(script)` | outpoint (36 bytes) |
//! | `overwritten` | outpoint ‖ replacing height BE (40 bytes) | `TrackedUtxo` |
//! | `lost` | ord lost-stream offset `u64` | start, len |
//! | `moves` | height `u32` | the block's moves (below); only blocks with moves, only retained heights |
//! | `row_history` | (row `u16`, height `u32`) | `RowStats` after the block, 8 varints; only rows the block changed |
//! | `undo` | height `u32` | the whole `BlockDelta`, self-contained; the last `UNDO_DEPTH` blocks |
//! | `unreferenced` | outpoint (36 bytes) | height of the block that created it: tracked UTXOs that no move output of their block matches (normally empty); their spends are stored in full |
//! | `sales` | (height `u32`, tx_index `u32`, vin `u32`) | a `Sale` (below), kept forever whatever the moves retention |
//! | `sale_txs` | txid (32 bytes) | (height, tx_index) of the transaction's sales |
//! | `sale_pieces` | (first sat of a sold piece `u64`, height, tx_index, vin) | the piece's length: finds the sales whose pieces cover a sat |
//! | `inscribed` | sat `u64` | count (envelopes that landed on it), first txid, first height, first transaction's index in its block: tracked sats only |
//! | `history` | (script key `u64`, height `u32`, tx_index `u32`) | a `HistoryEntry` (`codec.rs`): the address history, kept forever |
//!
//! **Address history** (`history.rs`, docs/API.md "Address history"): one
//! entry per transaction and script that sent or received tracked sats,
//! keyed by `history::history_key` (the first 8 bytes of the script's
//! SHA-256), for the blocks `TrackerConfig::address_history_from` covers.
//! Until the initial sync completes (`fast_sync_complete`), entries are
//! appended, height first, to the `history_staging` table of a separate
//! file next to the index (`index.history-staging.redb`), which keeps the
//! fast sync's writes sequential; the commit that completes the sync moves
//! them into `history` in key order, a bucket of script keys at a time
//! (`migrate_history`), and the file is deleted, so its pages do not stay
//! in the index as free space. Later blocks write to `history` directly. A
//! staging batch is committed just before the index commit that records it
//! (`history_staged` in `meta`); on open, staged blocks above the index's
//! tip are dropped, and a staging file the index does not record is
//! deleted. A rollback deletes the block's keys (`history::block_keys` of
//! its undo record) from `history` and the staging file. `history_from` in
//! `meta` is the first covered height.
//!
//! Bulk inserts in key order leave redb's leaves half full (a full leaf
//! splits in two halves, and the left one is never written again), so they
//! insert three entries in five first and the rest in a second pass
//! (`dense_order`): the second pass fills the leaves to about 80%.
//!
//! Pieces are encoded as a count and `(offset gap, start, len)` triples, the
//! gap being the distance from the previous piece's `offset_end()`.
//!
//! **Sales and inscriptions** (`market::analyze`, docs/API.md "Sales and
//! inscriptions") are written with their block and reverted with it: the
//! undo record carries the block's `MarketDelta`. A sale record is the
//! `Sale` (version byte, txid, tx_index, vin, kind, marketplace, seller
//! outpoint and UTXO, payment vout, price, proceeds, fees, buyer vout and
//! script) plus the buyer output's value, read from the block's moves at
//! commit. Pieces of different sales overlap (a sat sells many times), so
//! `sale_pieces` is scanned from `sat - longest sold piece + 1`
//! (`sale_piece_max_len` in `meta`) to `sat`.
//!
//! `meta` keys: `schema_version` (u32 LE), `meta` (`IndexMeta` as JSON),
//! `mined` (count ‖ varint per row: the per-row mined counters), `lost_sats`
//! (u64 LE, all sats lost so far), `moves_floor` (u32 LE: no block below it
//! has stored moves) and `moves_refs` (u8: some stored moves record refers to
//! an older block's moves).
//!
//! **Sat locator.** Every tracked piece anywhere (unspent, burned, lost or
//! overwritten) has one entry keyed by its first sat. Pieces never overlap,
//! so `locate(sat)` is the last entry at or below `sat`, checked against its
//! `len`.
//!
//! **Moves.** One record per block: a version byte, flags, the block's
//! distinct scripts (outputs and inputs refer to them by index), then the
//! moves with varint fields. A spent UTXO never changes between creation and
//! spend, so an input is stored as a reference to the output record of the
//! move that created it when that record is guaranteed to exist: always when
//! it was created in the same block, and when it was created in an older
//! block while moves are retained without limit. Otherwise (bounded
//! retention) inputs are stored in full, so pruning old blocks never breaks a
//! retained one. Retention (`MovesRetention`) is not index-defining: lowering
//! it later first rewrites retained records that refer to blocks about to be
//! pruned.
//!
//! **Commits.** `commit` writes any number of consecutive blocks in one write
//! transaction. The deltas are first netted in memory (a UTXO created and
//! spent within the batch never touches the `utxos`, `locator` or `scripts`
//! tables), then each table is written in key order.

#[cfg(test)]
mod bench;
mod codec;
#[cfg(test)]
pub(crate) mod tests;

use {
  self::codec::{
    Enc, Loc, OutPointKey, RefPolicy, decode_history, decode_inscribed, decode_loc, decode_lost,
    decode_moves, decode_row_stats, decode_sale, decode_u64s, decode_undo, decode_utxo,
    encode_history, encode_inscribed, encode_loc, encode_lost, encode_moves, encode_row_stats,
    encode_sale, encode_u64s, encode_undo, encode_utxo, find_output, header_bytes,
    header_from_bytes, moves_have_external_refs, outpoint_from_key, outpoint_key,
    overwritten_from_key, overwritten_key, script_key,
  },
  crate::history::HistoryEntry,
  crate::model::{
    BlockDelta, LocationKind, MarketDelta, Move, Piece, RowId, RowStats, Sale, SatLocation,
    TrackedUtxo, TrackerConfig, TrackerSnapshot, UtxoKind,
  },
  anyhow::{Context, bail, ensure},
  bitcoin::{BlockHash, OutPoint, Script, Txid, block::Header, hashes::Hash},
  rayon::prelude::*,
  redb::{
    Database, Durability, MultimapTableDefinition, MultimapTableHandle, ReadTransaction,
    ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition, TableHandle,
    WriteTransaction,
  },
  rustc_hash::FxHashMap,
  serde::{Deserialize, Serialize},
  std::{
    collections::{HashMap, HashSet},
    fmt::{self, Display, Formatter},
    path::{Path, PathBuf},
    str::FromStr,
    time::Instant,
  },
};

/// Bumped whenever a table layout or encoding changes (2: sales and
/// inscriptions; 3: an inscription's first transaction index, so a sale and
/// an inscription in one block are ordered; 4: the address history).
pub const SCHEMA_VERSION: u32 = 4;

/// Undo records are kept for this many blocks below and including the tip.
pub const UNDO_DEPTH: u32 = 144;

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const HEADERS: TableDefinition<u32, &[u8; 80]> = TableDefinition::new("headers");
const UTXOS: TableDefinition<&OutPointKey, &[u8]> = TableDefinition::new("utxos");
const LOCATOR: TableDefinition<u64, &[u8]> = TableDefinition::new("locator");
const SCRIPTS: MultimapTableDefinition<&[u8], &OutPointKey> =
  MultimapTableDefinition::new("scripts");
const OVERWRITTEN: TableDefinition<&[u8; 40], &[u8]> = TableDefinition::new("overwritten");
const LOST: TableDefinition<u64, &[u8]> = TableDefinition::new("lost");
const MOVES: TableDefinition<u32, &[u8]> = TableDefinition::new("moves");
const ROW_HISTORY: TableDefinition<(u16, u32), &[u8]> = TableDefinition::new("row_history");
const UNDO: TableDefinition<u32, &[u8]> = TableDefinition::new("undo");
const UNREFERENCED: TableDefinition<&OutPointKey, u32> = TableDefinition::new("unreferenced");
const SALES: TableDefinition<SaleKey, &[u8]> = TableDefinition::new("sales");
const SALE_TXS: TableDefinition<&[u8; 32], (u32, u32)> = TableDefinition::new("sale_txs");
const SALE_PIECES: TableDefinition<(u64, u32, u32, u32), u64> = TableDefinition::new("sale_pieces");
const INSCRIBED: TableDefinition<u64, &[u8]> = TableDefinition::new("inscribed");
const HISTORY: TableDefinition<HistoryKey, &[u8]> = TableDefinition::new("history");
const HISTORY_STAGING: TableDefinition<StagingKey, &[u8]> = TableDefinition::new("history_staging");

/// Key of a history entry: (script key, height, tx_index).
pub type HistoryKey = (u64, u32, u32);

/// Key of a staged history entry: (height, tx_index, script key).
type StagingKey = (u32, u32, u64);

/// Staged history moved into `history` per bucket of script keys holding
/// about this many stored bytes (bounds the memory of the move).
const HISTORY_MIGRATION_BUCKET_BYTES: u64 = 512 << 20;

const KEY_SCHEMA_VERSION: &str = "schema_version";
const KEY_META: &str = "meta";
const KEY_MINED: &str = "mined";
const KEY_LOST_SATS: &str = "lost_sats";
const KEY_MOVES_FLOOR: &str = "moves_floor";
const KEY_MOVES_REFS: &str = "moves_refs";
const KEY_SALE_PIECE_MAX_LEN: &str = "sale_piece_max_len";
const KEY_HISTORY_FROM: &str = "history_from";
const KEY_HISTORY_STAGED: &str = "history_staged";

/// The staging file of the index at `path`.
fn staging_path(path: &Path) -> PathBuf {
  path.with_extension("history-staging.redb")
}

/// The order of a dense bulk insert of `n` sorted entries: three in five,
/// then the rest (see the module docs).
fn dense_order(n: usize) -> impl Iterator<Item = usize> {
  (0..n)
    .filter(|i| i % 5 < 3)
    .chain((0..n).filter(|i| i % 5 >= 3))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexMeta {
  pub schema_version: u32,
  pub chain: String,
  pub config: TrackerConfig,
  pub catalog_fingerprint: String,
  pub height: Option<u32>,
  pub hash: Option<BlockHash>,
  /// The fast initial sync reached its stop height.
  pub fast_sync_complete: bool,
}

impl IndexMeta {
  /// Refuses an index built by a different schema, chain, config or catalog.
  pub fn check_compatible(&self, expected: &IndexMeta) -> anyhow::Result<()> {
    let reindex = "restart with --reindex to rebuild it";
    ensure!(
      self.schema_version == expected.schema_version,
      "the index was built with schema version {}, this ord uses {}; {reindex}",
      self.schema_version,
      expected.schema_version,
    );
    ensure!(
      self.chain == expected.chain,
      "the index was built for chain {}, not {}; use --chain {} or {reindex}",
      self.chain,
      expected.chain,
      self.chain,
    );
    ensure!(
      self.config == expected.config,
      "the index was built with tracker config {}, this ord would build {}; {reindex}",
      serde_json::to_string(&self.config).unwrap_or_default(),
      serde_json::to_string(&expected.config).unwrap_or_default(),
    );
    ensure!(
      self.catalog_fingerprint == expected.catalog_fingerprint,
      "the index was built with sat catalog {}, this ord's catalog is {} (rules, rows or \
       embedded data changed); {reindex}",
      self.catalog_fingerprint,
      expected.catalog_fingerprint,
    );
    Ok(())
  }
}

/// How many blocks' moves are kept (`--moves-retention`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovesRetention {
  All,
  /// The moves of the last N blocks (N = 0 keeps none).
  Blocks(u32),
}

impl MovesRetention {
  /// The dashboard backfills 1,008 blocks; four weeks leaves margin.
  pub const DEFAULT: Self = Self::Blocks(4032);
}

impl Default for MovesRetention {
  fn default() -> Self {
    Self::DEFAULT
  }
}

impl Display for MovesRetention {
  fn fmt(&self, f: &mut Formatter) -> fmt::Result {
    match self {
      Self::All => write!(f, "all"),
      Self::Blocks(n) => write!(f, "{n}"),
    }
  }
}

impl FromStr for MovesRetention {
  type Err = String;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    if s == "all" {
      Ok(Self::All)
    } else {
      s.parse()
        .map(Self::Blocks)
        .map_err(|_| format!("expected a number of blocks or `all`, got `{s}`"))
    }
  }
}

#[derive(Debug, Clone)]
pub struct StoreOptions {
  /// redb cache, in bytes (a tenth of it buffers writes).
  pub cache_size: usize,
  pub moves_retention: MovesRetention,
  /// Save redb's allocator state with every durable commit so reopening
  /// after a crash does not walk the whole file.
  pub quick_repair: bool,
}

impl Default for StoreOptions {
  fn default() -> Self {
    Self {
      cache_size: 1 << 30,
      moves_retention: MovesRetention::DEFAULT,
      quick_repair: true,
    }
  }
}

#[derive(Debug, Clone, Default)]
pub struct CommitOptions {
  /// `false`: redb `Durability::None`; the commit is visible to readers
  /// at once but only persisted by the next durable commit (after a crash
  /// the index reopens at the last durable one).
  pub durable: bool,
  /// The height the index is syncing to (the fast sync's stop height), so
  /// that moves which would be pruned before it is reached are not written.
  pub target_height: Option<u32>,
}

/// Timings and sizes of one commit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommitStats {
  pub blocks: usize,
  pub utxo_writes: usize,
  pub locator_writes: usize,
  pub script_writes: usize,
  pub moves: usize,
  pub moves_bytes: usize,
  pub history_entries: usize,
  /// Time spent moving the staged history into `history` (once).
  pub history_migration_seconds: f64,
  pub net_seconds: f64,
  pub write_seconds: f64,
  pub commit_seconds: f64,
}

/// Entries and on-disk bytes of one table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TableInfo {
  pub name: String,
  pub entries: u64,
  pub stored_bytes: u64,
  pub metadata_bytes: u64,
  pub fragmented_bytes: u64,
}

pub struct Store {
  db: Database,
  path: PathBuf,
  options: StoreOptions,
  /// The history staging file, while the initial sync stages entries.
  staging: std::sync::Mutex<Option<Database>>,
}

impl Store {
  pub fn open(path: &Path) -> anyhow::Result<Self> {
    Self::open_with(path, StoreOptions::default())
  }

  /// Opens (creating if missing) the index at `path`.
  pub fn open_with(path: &Path, options: StoreOptions) -> anyhow::Result<Self> {
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    // redb also "repairs" a file it initializes: only report real repairs.
    let existed = path.metadata().is_ok_and(|metadata| metadata.len() > 0);
    let db = Database::builder()
      .set_cache_size(options.cache_size)
      .set_repair_callback(move |session| {
        if existed {
          log::info!(
            "repairing the index after an unclean shutdown: {:.0}%",
            session.progress() * 100.0
          )
        }
      })
      .create(path)
      .map_err(|err| match err {
        redb::DatabaseError::DatabaseAlreadyOpen => anyhow::anyhow!(
          "the index at {} is in use by another ord process (e.g. `ord server`); stop it or \
           query its HTTP API instead",
          path.display()
        ),
        err => anyhow::Error::new(err).context(format!("failed to open {}", path.display())),
      })?;

    let store = Self {
      db,
      path: path.into(),
      options,
      staging: std::sync::Mutex::new(None),
    };

    store.create_tables()?;
    store.recover_staging()?;

    Ok(store)
  }

  pub fn path(&self) -> &Path {
    &self.path
  }

  pub fn options(&self) -> &StoreOptions {
    &self.options
  }

  fn create_tables(&self) -> anyhow::Result<()> {
    let tx = self.db.begin_write()?;
    let names = tx
      .list_tables()?
      .map(|table| table.name().to_string())
      .chain(
        tx.list_multimap_tables()?
          .map(|table| table.name().to_string()),
      )
      .collect::<Vec<String>>();
    if !names.is_empty() && !names.iter().any(|name| name == META.name()) {
      bail!(
        "{} is not an ord rare-sat tracker index (it has tables {}); use another --data-dir",
        self.path.display(),
        names.join(", ")
      );
    }
    tx.open_table(META)?;
    tx.open_table(HEADERS)?;
    tx.open_table(UTXOS)?;
    tx.open_table(LOCATOR)?;
    tx.open_multimap_table(SCRIPTS)?;
    tx.open_table(OVERWRITTEN)?;
    tx.open_table(LOST)?;
    tx.open_table(MOVES)?;
    tx.open_table(ROW_HISTORY)?;
    tx.open_table(UNDO)?;
    tx.open_table(UNREFERENCED)?;
    tx.open_table(SALES)?;
    tx.open_table(SALE_TXS)?;
    tx.open_table(SALE_PIECES)?;
    tx.open_table(INSCRIBED)?;
    tx.open_table(HISTORY)?;
    tx.commit()?;
    Ok(())
  }

  /// Opens the staging file if the index records staged history, after
  /// dropping blocks above the index's tip (staged, but the index commit
  /// did not happen); deletes a staging file the index does not record.
  fn recover_staging(&self) -> anyhow::Result<()> {
    let path = staging_path(&self.path);
    let reader = self.reader()?;
    let staged = reader.history_building()?;
    let tip = reader.meta().ok().flatten().and_then(|meta| meta.height);
    drop(reader);
    if !staged {
      if path.exists() {
        std::fs::remove_file(&path)
          .with_context(|| format!("failed to delete {}", path.display()))?;
      }
      return Ok(());
    }
    ensure!(
      path.exists(),
      "the index records staged address history, but {} is missing; restart with --reindex",
      path.display()
    );
    let db = self.open_staging()?;
    let tx = db.begin_write()?;
    {
      let mut table = tx.open_table(HISTORY_STAGING)?;
      let from = tip.map_or(0, |tip| tip.saturating_add(1));
      if tip != Some(u32::MAX) {
        for entry in table.extract_from_if((from, 0, 0).., |_, _| true)? {
          entry?;
        }
      }
    }
    tx.commit()?;
    *self.staging.lock().unwrap() = Some(db);
    Ok(())
  }

  fn open_staging(&self) -> anyhow::Result<Database> {
    let path = staging_path(&self.path);
    let db = Database::builder()
      .set_cache_size(self.options.cache_size.min(256 << 20))
      .create(&path)
      .with_context(|| format!("failed to open {}", path.display()))?;
    let tx = db.begin_write()?;
    tx.open_table(HISTORY_STAGING)?;
    tx.commit()?;
    Ok(db)
  }

  /// Closes and deletes the staging file.
  fn drop_staging(&self) -> anyhow::Result<()> {
    self.staging.lock().unwrap().take();
    let path = staging_path(&self.path);
    if path.exists() {
      std::fs::remove_file(&path)
        .with_context(|| format!("failed to delete {}", path.display()))?;
    }
    Ok(())
  }

  /// Appends staged entries, keyed (height, tx_index, script key) and
  /// sorted, in their own commit.
  fn stage(&self, entries: &[(StagingKey, Vec<u8>)], durable: bool) -> anyhow::Result<()> {
    let mut staging = self.staging.lock().unwrap();
    if staging.is_none() {
      *staging = Some(self.open_staging()?);
    }
    let db = staging.as_ref().unwrap();
    let mut tx = db.begin_write()?;
    if !durable {
      tx.set_durability(Durability::None)?;
    }
    {
      let mut table = tx.open_table(HISTORY_STAGING)?;
      for i in dense_order(entries.len()) {
        let (key, value) = &entries[i];
        table.insert(key, value.as_slice())?;
      }
    }
    tx.commit()?;
    Ok(())
  }

  /// Moves every staged entry into `history` (in `tx`), in key order, one
  /// bucket of script keys at a time: each bucket is a full scan of the
  /// staging table, sized to about `bucket_bytes` of it. Returns the
  /// number of entries moved.
  fn migrate_history(&self, tx: &WriteTransaction, bucket_bytes: u64) -> anyhow::Result<u64> {
    let staging = self.staging.lock().unwrap();
    let db = staging
      .as_ref()
      .context("the index records staged address history, but no staging file is open")?;
    let read = db.begin_read()?;
    let table = read.open_table(HISTORY_STAGING)?;
    let entries = table.len()?;
    let bytes = {
      let write = db.begin_write()?;
      let bytes = write.open_table(HISTORY_STAGING)?.stats()?.stored_bytes();
      write.abort()?;
      bytes
    };
    let buckets = u128::from(bytes.div_ceil(bucket_bytes).max(1));
    let bound = |bucket: u128| (bucket << 64) / buckets;
    let mut history = tx.open_table(HISTORY)?;
    for bucket in 0..buckets {
      // Script keys in [low, high) (the last bucket ends at 2^64).
      let (low, high) = (bound(bucket), bound(bucket + 1));
      let mut batch = Vec::new();
      for entry in table.iter()? {
        let (key, value) = entry?;
        let (height, tx_index, script) = key.value();
        if (low..high).contains(&u128::from(script)) {
          batch.push(((script, height, tx_index), value.value().to_vec()));
        }
      }
      batch.par_sort_unstable_by_key(|(key, _)| *key);
      for i in dense_order(batch.len()) {
        let (key, value) = &batch[i];
        history.insert(key, value.as_slice())?;
      }
    }
    Ok(entries)
  }

  /// Deletes everything; the index is then empty (no meta).
  pub fn wipe(&self) -> anyhow::Result<()> {
    let tx = self.db.begin_write()?;
    tx.delete_table(META)?;
    tx.delete_table(HEADERS)?;
    tx.delete_table(UTXOS)?;
    tx.delete_table(LOCATOR)?;
    tx.delete_multimap_table(SCRIPTS)?;
    tx.delete_table(OVERWRITTEN)?;
    tx.delete_table(LOST)?;
    tx.delete_table(MOVES)?;
    tx.delete_table(ROW_HISTORY)?;
    tx.delete_table(UNDO)?;
    tx.delete_table(UNREFERENCED)?;
    tx.delete_table(SALES)?;
    tx.delete_table(SALE_TXS)?;
    tx.delete_table(SALE_PIECES)?;
    tx.delete_table(INSCRIBED)?;
    tx.delete_table(HISTORY)?;
    tx.commit()?;
    self.drop_staging()?;
    self.create_tables()
  }

  /// A consistent read-only view.
  pub fn reader(&self) -> anyhow::Result<Reader> {
    Ok(Reader {
      tx: self.db.begin_read()?,
    })
  }

  pub fn meta(&self) -> anyhow::Result<Option<IndexMeta>> {
    self.reader()?.meta()
  }

  /// Initializes an empty index.
  pub fn init(&self, meta: &IndexMeta) -> anyhow::Result<()> {
    ensure!(
      meta.schema_version == SCHEMA_VERSION,
      "cannot initialize an index with schema version {}",
      meta.schema_version
    );
    let tx = self.db.begin_write()?;
    {
      let mut table = tx.open_table(META)?;
      ensure!(
        table.get(KEY_META)?.is_none(),
        "the index at {} is already initialized",
        self.path.display()
      );
      ensure!(
        tx.open_table(HEADERS)?.is_empty()?,
        "the index at {} has blocks but no meta; restart with --reindex",
        self.path.display()
      );
      table.insert(KEY_SCHEMA_VERSION, SCHEMA_VERSION.to_le_bytes().as_slice())?;
      table.insert(KEY_META, serde_json::to_vec(meta)?.as_slice())?;
      table.insert(KEY_LOST_SATS, 0u64.to_le_bytes().as_slice())?;
      table.insert(KEY_MOVES_FLOOR, 0u32.to_le_bytes().as_slice())?;
      table.insert(KEY_MOVES_REFS, [0u8].as_slice())?;
      table.insert(KEY_MINED, encode_u64s(&[]).as_slice())?;
    }
    tx.commit()?;
    Ok(())
  }

  /// Rebuilds the tracker's full state from the index.
  pub fn load_snapshot(&self) -> anyhow::Result<TrackerSnapshot> {
    let reader = self.reader()?;
    let meta = reader.meta()?.context("the index is not initialized")?;

    let mut times = Vec::new();
    if let Some(height) = meta.height {
      times.reserve(usize::try_from(height).unwrap() + 1);
      let headers = reader.tx.open_table(HEADERS)?;
      for (expected, entry) in (0..).zip(headers.range(0..=height)?) {
        let (key, value) = entry?;
        ensure!(
          key.value() == expected,
          "corrupt index: header {expected} is missing"
        );
        times.push(u32::from_le_bytes(
          value.value()[68..72].try_into().unwrap(),
        ));
      }
      ensure!(
        times.len() == usize::try_from(height).unwrap() + 1,
        "corrupt index: {} headers for height {height}",
        times.len()
      );
    }

    let mut utxos = Vec::new();
    {
      let table = reader.tx.open_table(UTXOS)?;
      utxos.reserve(usize::try_from(table.len()?).unwrap());
      for entry in table.iter()? {
        let (key, value) = entry?;
        utxos.push((outpoint_from_key(key.value()), decode_utxo(value.value())?));
      }
    }

    let mut lost = Vec::new();
    for entry in reader.tx.open_table(LOST)?.iter()? {
      let (key, value) = entry?;
      lost.push(decode_lost(key.value(), value.value())?);
    }

    let mut overwritten = Vec::new();
    for entry in reader.tx.open_table(OVERWRITTEN)?.iter()? {
      let (key, value) = entry?;
      let (outpoint, height) = overwritten_from_key(key.value());
      overwritten.push((outpoint, height, decode_utxo(value.value())?));
    }

    Ok(TrackerSnapshot {
      height: meta.height,
      hash: meta.hash,
      times,
      lost_sats: reader.lost_sats()?,
      utxos,
      lost,
      overwritten,
      mined: reader.mined()?,
    })
  }

  /// Persists consecutive blocks atomically. `mined` is the tracker's
  /// per-row mined counters after the last delta.
  pub fn commit(
    &self,
    deltas: &[BlockDelta],
    mined: &[u64],
    fast_sync_complete: bool,
  ) -> anyhow::Result<()> {
    self
      .commit_with(
        deltas,
        &[],
        &[],
        mined,
        fast_sync_complete,
        &CommitOptions {
          durable: true,
          target_height: None,
        },
      )
      .map(|_| ())
  }

  /// `markets` holds each block's `MarketDelta` (same length as `deltas`),
  /// or is empty for blocks without market data. `histories` likewise holds
  /// each block's address history entries (`history::block_entries`), `None`
  /// for a block the history does not cover, or is empty for none.
  pub fn commit_with(
    &self,
    deltas: &[BlockDelta],
    markets: &[MarketDelta],
    histories: &[Option<Vec<HistoryEntry>>],
    mined: &[u64],
    fast_sync_complete: bool,
    options: &CommitOptions,
  ) -> anyhow::Result<CommitStats> {
    ensure!(
      markets.is_empty() || markets.len() == deltas.len(),
      "{} market deltas for {} blocks",
      markets.len(),
      deltas.len()
    );
    ensure!(
      histories.is_empty() || histories.len() == deltas.len(),
      "{} history batches for {} blocks",
      histories.len(),
      deltas.len()
    );
    let no_market = MarketDelta::default();
    let market = |i: usize| markets.get(i).unwrap_or(&no_market);
    let start = Instant::now();
    let mut stats = CommitStats {
      blocks: deltas.len(),
      ..default()
    };

    let mut tx = self.db.begin_write()?;
    if options.durable {
      tx.set_quick_repair(self.options.quick_repair);
    } else {
      tx.set_durability(Durability::None)?;
    }

    let mut meta = read_meta(&tx.open_table(META)?)?.context("the index is not initialized")?;

    let mut expected_prev = meta.hash;
    for (expected_height, delta) in (meta.height.map_or(0, |height| height + 1)..).zip(deltas) {
      ensure!(
        delta.height == expected_height,
        "commit out of order: got block {} after {:?}",
        delta.height,
        meta.height
      );
      if let Some(prev) = expected_prev {
        ensure!(
          delta.header.prev_blockhash == prev,
          "block {} {} does not extend {prev}",
          delta.height,
          delta.hash
        );
      }
      ensure!(
        delta.header.block_hash() == delta.hash,
        "block {} hash {} does not match its header",
        delta.height,
        delta.hash
      );
      expected_prev = Some(delta.hash);
    }

    let net = Net::build(deltas);
    stats.net_seconds = start.elapsed().as_secs_f64();

    let write_start = Instant::now();
    let (utxo_writes, locator_writes, script_writes) = net.write(&tx)?;
    stats.utxo_writes = utxo_writes;
    stats.locator_writes = locator_writes;
    stats.script_writes = script_writes;

    let mut migrated = false;
    if fast_sync_complete && get_u32(&tx.open_table(META)?, KEY_HISTORY_STAGED)? == 1 {
      let start = Instant::now();
      let moved = self.migrate_history(&tx, HISTORY_MIGRATION_BUCKET_BYTES)?;
      tx.open_table(META)?.remove(KEY_HISTORY_STAGED)?;
      migrated = true;
      stats.history_migration_seconds = start.elapsed().as_secs_f64();
      log::info!(
        "moved {moved} staged address history entries into the history in {:.1}s",
        stats.history_migration_seconds
      );
    }

    if let Some(last) = deltas.last() {
      let tip = last.height;

      {
        let mut headers = tx.open_table(HEADERS)?;
        for delta in deltas {
          headers.insert(delta.height, &header_bytes(&delta.header))?;
        }
      }

      {
        let mut history = tx.open_table(ROW_HISTORY)?;
        let mut entries = deltas
          .iter()
          .flat_map(|delta| {
            delta
              .rows
              .iter()
              .map(move |row| ((row.row, delta.height), &row.after))
          })
          .collect::<Vec<_>>();
        entries.sort_unstable_by_key(|(key, _)| *key);
        for (key, after) in entries {
          history.insert(key, encode_row_stats(after).as_slice())?;
        }
      }

      {
        let mut table = tx.open_table(META)?;
        let mut floor = get_u32(&table, KEY_MOVES_FLOOR)?;
        let mut refs = table
          .get(KEY_MOVES_REFS)?
          .is_some_and(|value| value.value().first() == Some(&1));

        let mut moves = tx.open_table(MOVES)?;

        if let MovesRetention::Blocks(retention) = self.options.moves_retention {
          let target = options.target_height.unwrap_or(tip).max(tip);
          let new_floor = (target + 1).saturating_sub(retention).max(floor);
          if new_floor > floor {
            if refs {
              materialize_moves(&mut moves, new_floor)?;
              refs = false;
            }
            for entry in moves.extract_from_if(floor..new_floor, |_, _| true)? {
              entry?;
            }
            floor = new_floor;
          }
        }

        let older_from = match self.options.moves_retention {
          MovesRetention::All => Some(floor),
          MovesRetention::Blocks(_) => None,
        };

        // References to older blocks' moves rely on every tracked UTXO
        // being an output of a move of its block, as it is: check it, and
        // record the exceptions so that their spends are stored in full.
        let mut unreferenced = HashSet::new();
        if older_from.is_some() {
          let mut table = tx.open_table(UNREFERENCED)?;
          for entry in table.iter()? {
            unreferenced.insert(outpoint_from_key(entry?.0.value()));
          }
          let found = deltas
            .par_iter()
            .filter(|delta| delta.height >= floor)
            .flat_map_iter(|delta| unmatched_outputs(delta).map(move |outpoint| (outpoint, delta)))
            .map(|(outpoint, delta)| (outpoint, delta.height))
            .collect::<Vec<(OutPoint, u32)>>();
          for (i, (outpoint, height)) in found.iter().enumerate() {
            if i < 10 {
              log::warn!(
                "block {height}: tracked output {outpoint} is not an output of the block's \
                 moves; its spend will be stored in full"
              );
            }
            unreferenced.insert(*outpoint);
            table.insert(&outpoint_key(outpoint), height)?;
          }
          if !unreferenced.is_empty() {
            for (outpoint, _) in deltas.iter().flat_map(|delta| &delta.spent) {
              if unreferenced.contains(outpoint) {
                table.remove(&outpoint_key(outpoint))?;
              }
            }
          }
        }

        let policy = RefPolicy {
          older_from,
          unreferenced: Some(&unreferenced),
        };

        let encoded = deltas
          .par_iter()
          .filter(|delta| delta.height >= floor && !delta.moves.is_empty())
          .map(|delta| {
            let bytes = encode_moves(&delta.moves, delta.height, policy);
            (delta.height, delta.moves.len(), bytes)
          })
          .collect::<Vec<_>>();

        for (height, count, bytes) in encoded {
          refs |= moves_have_external_refs(&bytes);
          stats.moves += count;
          stats.moves_bytes += bytes.len();
          moves.insert(height, bytes.as_slice())?;
        }

        table.insert(KEY_MOVES_FLOOR, floor.to_le_bytes().as_slice())?;
        table.insert(KEY_MOVES_REFS, [u8::from(refs)].as_slice())?;
      }

      {
        let undo_from = (tip + 1).saturating_sub(UNDO_DEPTH);
        let mut undo = tx.open_table(UNDO)?;
        for entry in undo.extract_from_if(..undo_from, |_, _| true)? {
          entry?;
        }
        let encoded = deltas
          .par_iter()
          .enumerate()
          .filter(|(_, delta)| delta.height >= undo_from)
          .map(|(i, delta)| (delta.height, encode_undo(delta, market(i))))
          .collect::<Vec<_>>();
        for (height, bytes) in encoded {
          undo.insert(height, bytes.as_slice())?;
        }
      }

      if markets.iter().any(|market| *market != no_market) {
        let mut max_len = get_u64(&tx.open_table(META)?, KEY_SALE_PIECE_MAX_LEN)?;
        for (i, delta) in deltas.iter().enumerate() {
          write_market(&tx, delta, market(i), &mut max_len)?;
        }
        tx.open_table(META)?
          .insert(KEY_SALE_PIECE_MAX_LEN, max_len.to_le_bytes().as_slice())?;
      }

      stats.history_entries =
        self.write_history(&tx, deltas, histories, fast_sync_complete, options.durable)?;

      meta.height = Some(tip);
      meta.hash = Some(last.hash);

      let mut table = tx.open_table(META)?;
      table.insert(KEY_LOST_SATS, last.lost_sats_after.to_le_bytes().as_slice())?;
    }

    meta.fast_sync_complete = fast_sync_complete;

    {
      let mut table = tx.open_table(META)?;
      table.insert(KEY_META, serde_json::to_vec(&meta)?.as_slice())?;
      table.insert(KEY_MINED, encode_u64s(mined).as_slice())?;
    }

    stats.write_seconds = write_start.elapsed().as_secs_f64();

    let commit_start = Instant::now();
    tx.commit()?;
    stats.commit_seconds = commit_start.elapsed().as_secs_f64();

    if migrated {
      self.drop_staging()?;
    }

    Ok(stats)
  }

  /// Reverts the tip block in the database and returns its delta so the
  /// tracker can undo it too. `None` when no undo record is left.
  pub fn rollback_tip(&self) -> anyhow::Result<Option<BlockDelta>> {
    let mut tx = self.db.begin_write()?;
    tx.set_quick_repair(self.options.quick_repair);

    let mut meta = read_meta(&tx.open_table(META)?)?.context("the index is not initialized")?;

    let Some(height) = meta.height else {
      return Ok(None);
    };

    let (delta, market) = {
      let undo = tx.open_table(UNDO)?;
      let Some(bytes) = undo.get(height)? else {
        return Ok(None);
      };
      decode_undo(bytes.value(), height)?
    };

    revert_market(&tx, height, &market)?;

    {
      let mut utxos = tx.open_table(UTXOS)?;
      let mut locator = tx.open_table(LOCATOR)?;
      let mut scripts = tx.open_multimap_table(SCRIPTS)?;
      let mut overwritten = tx.open_table(OVERWRITTEN)?;
      let mut lost = tx.open_table(LOST)?;

      // Removals first: restored pieces may start at the same sats.
      for (outpoint, utxo) in &delta.created {
        let key = outpoint_key(outpoint);
        utxos.remove(&key)?;
        scripts.remove(script_key(utxo.script_pubkey.as_bytes()).as_slice(), &key)?;
        for piece in &utxo.pieces {
          locator.remove(piece.start)?;
        }
      }

      for piece in &delta.lost {
        locator.remove(piece.start)?;
        lost.remove(piece.offset)?;
      }

      for (outpoint, _) in &delta.overwritten {
        overwritten.remove(&overwritten_key(outpoint, height))?;
      }

      let mut enc = Enc::default();
      for (outpoint, utxo) in delta.overwritten.iter().chain(&delta.spent) {
        let key = outpoint_key(outpoint);
        utxos.insert(&key, encode_utxo(utxo).as_slice())?;
        scripts.insert(script_key(utxo.script_pubkey.as_bytes()).as_slice(), &key)?;
        for piece in &utxo.pieces {
          enc.clear();
          encode_loc(&utxo_loc(outpoint, utxo, piece), &mut enc);
          locator.insert(piece.start, enc.buf.as_slice())?;
        }
      }
    }

    {
      let mut history = tx.open_table(ROW_HISTORY)?;
      for row in &delta.rows {
        history.remove((row.row, height))?;
      }
    }

    let mut staging_emptied = false;
    {
      if get_u32(&tx.open_table(META)?, KEY_HISTORY_STAGED)? == 1
        && let Some(db) = &*self.staging.lock().unwrap()
      {
        let staging_tx = db.begin_write()?;
        {
          let mut staging = staging_tx.open_table(HISTORY_STAGING)?;
          for entry in
            staging.extract_from_if((height, 0, 0)..=(height, u32::MAX, u64::MAX), |_, _| true)?
          {
            entry?;
          }
          staging_emptied = staging.is_empty()?;
        }
        staging_tx.commit()?;
        if staging_emptied {
          tx.open_table(META)?.remove(KEY_HISTORY_STAGED)?;
        }
      }
      let mut history = tx.open_table(HISTORY)?;
      for (key, tx_index) in crate::history::block_keys(&delta) {
        history.remove((key, height, tx_index))?;
      }
      let mut table = tx.open_table(META)?;
      if table.get(KEY_HISTORY_FROM)?.is_some() && get_u32(&table, KEY_HISTORY_FROM)? == height {
        table.remove(KEY_HISTORY_FROM)?;
      }
    }

    tx.open_table(HEADERS)?.remove(height)?;
    tx.open_table(MOVES)?.remove(height)?;
    tx.open_table(UNDO)?.remove(height)?;

    {
      let mut table = tx.open_table(META)?;

      let mut mined = match table.get(KEY_MINED)? {
        Some(value) => decode_u64s(value.value())?,
        None => Vec::new(),
      };
      for row in &delta.rows {
        let row_index = usize::from(row.row);
        if mined.len() <= row_index {
          mined.resize(row_index + 1, 0);
        }
        mined[row_index] = row.before.mined;
      }
      table.insert(KEY_MINED, encode_u64s(&mined).as_slice())?;

      table.insert(
        KEY_LOST_SATS,
        delta.lost_sats_before.to_le_bytes().as_slice(),
      )?;

      if height == 0 {
        meta.height = None;
        meta.hash = None;
      } else {
        meta.height = Some(height - 1);
        meta.hash = Some(delta.header.prev_blockhash);
      }
      table.insert(KEY_META, serde_json::to_vec(&meta)?.as_slice())?;
    }

    tx.commit()?;

    if staging_emptied {
      self.drop_staging()?;
    }

    Ok(Some(delta))
  }

  pub fn block_hash(&self, height: u32) -> anyhow::Result<Option<BlockHash>> {
    self.reader()?.block_hash(height)
  }

  pub fn header(&self, height: u32) -> anyhow::Result<Option<Header>> {
    self.reader()?.header(height)
  }

  pub fn utxo(&self, outpoint: OutPoint) -> anyhow::Result<Option<TrackedUtxo>> {
    self.reader()?.utxo(outpoint)
  }

  pub fn locate(&self, sat: u64) -> anyhow::Result<Option<SatLocation>> {
    self.reader()?.locate(sat)
  }

  pub fn utxos_for_script(
    &self,
    script_pubkey: &Script,
  ) -> anyhow::Result<Vec<(OutPoint, TrackedUtxo)>> {
    self.reader()?.utxos_for_script(script_pubkey)
  }

  /// `None` when the block is not indexed or its moves were not retained.
  pub fn moves(&self, height: u32) -> anyhow::Result<Option<Vec<Move>>> {
    self.reader()?.moves(height)
  }

  pub fn row_history(
    &self,
    row: RowId,
    from: u32,
    to: u32,
  ) -> anyhow::Result<Vec<(u32, RowStats)>> {
    self.reader()?.row_history(row, from, to)
  }

  /// Entries and sizes of every table.
  pub fn table_info(&self) -> anyhow::Result<Vec<TableInfo>> {
    let tx = self.db.begin_write()?;
    let mut info = Vec::new();

    macro_rules! table {
      ($definition:expr) => {{
        let table = tx.open_table($definition)?;
        let stats = table.stats()?;
        info.push(TableInfo {
          name: $definition.name().into(),
          entries: table.len()?,
          stored_bytes: stats.stored_bytes(),
          metadata_bytes: stats.metadata_bytes(),
          fragmented_bytes: stats.fragmented_bytes(),
        });
      }};
    }

    table!(META);
    table!(HEADERS);
    table!(UTXOS);
    table!(LOCATOR);
    {
      let table = tx.open_multimap_table(SCRIPTS)?;
      let stats = table.stats()?;
      info.push(TableInfo {
        name: SCRIPTS.name().into(),
        entries: table.len()?,
        stored_bytes: stats.stored_bytes(),
        metadata_bytes: stats.metadata_bytes(),
        fragmented_bytes: stats.fragmented_bytes(),
      });
    }
    table!(OVERWRITTEN);
    table!(LOST);
    table!(MOVES);
    table!(ROW_HISTORY);
    table!(UNDO);
    table!(UNREFERENCED);
    table!(SALES);
    table!(SALE_TXS);
    table!(SALE_PIECES);
    table!(INSCRIBED);
    table!(HISTORY);

    tx.abort()?;

    // The staging file, while there is one.
    let staging = self.staging.lock().unwrap();
    match &*staging {
      Some(db) => {
        let tx = db.begin_write()?;
        let table = tx.open_table(HISTORY_STAGING)?;
        let stats = table.stats()?;
        info.push(TableInfo {
          name: HISTORY_STAGING.name().into(),
          entries: table.len()?,
          stored_bytes: stats.stored_bytes(),
          metadata_bytes: stats.metadata_bytes(),
          fragmented_bytes: stats.fragmented_bytes(),
        });
        drop(table);
        tx.abort()?;
      }
      None => info.push(TableInfo {
        name: HISTORY_STAGING.name().into(),
        entries: 0,
        stored_bytes: 0,
        metadata_bytes: 0,
        fragmented_bytes: 0,
      }),
    }
    Ok(info)
  }
}

fn default<T: Default>() -> T {
  Default::default()
}

fn read_meta(
  table: &impl ReadableTable<&'static str, &'static [u8]>,
) -> anyhow::Result<Option<IndexMeta>> {
  let Some(value) = table.get(KEY_META)? else {
    return Ok(None);
  };
  let schema_version = get_u32(table, KEY_SCHEMA_VERSION)?;
  ensure!(
    schema_version == SCHEMA_VERSION,
    "the index was built with schema version {schema_version}, this ord uses {SCHEMA_VERSION}; \
     restart with --reindex to rebuild it"
  );
  Ok(Some(
    serde_json::from_slice(value.value()).context("corrupt index meta")?,
  ))
}

fn get_u64(
  table: &impl ReadableTable<&'static str, &'static [u8]>,
  key: &str,
) -> anyhow::Result<u64> {
  let Some(value) = table.get(key)? else {
    return Ok(0);
  };
  Ok(u64::from_le_bytes(
    value
      .value()
      .try_into()
      .with_context(|| format!("corrupt index meta `{key}`"))?,
  ))
}

/// Key of a sale: (height, tx_index, vin), oldest first.
pub type SaleKey = (u32, u32, u32);

/// A sale as stored: the analyzer's `Sale`, its block, and the value of the
/// buyer's output (from the block's moves).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSale {
  pub height: u32,
  pub sale: Sale,
  pub buyer_value: Option<u64>,
}

impl StoredSale {
  pub fn key(&self) -> SaleKey {
    (self.height, self.sale.tx_index, self.sale.vin)
  }
}

/// The inscriptions on one tracked sat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inscribed {
  /// Envelopes that landed on the sat (reinscriptions count).
  pub count: u32,
  pub first_txid: Txid,
  pub first_height: u32,
  /// The first inscribing transaction's index in its block.
  pub first_tx_index: u32,
}

/// Writes a block's sales and inscriptions; `max_len` tracks the longest
/// sold piece.
fn write_market(
  tx: &WriteTransaction,
  delta: &BlockDelta,
  market: &MarketDelta,
  max_len: &mut u64,
) -> anyhow::Result<()> {
  let height = delta.height;

  if !market.sales.is_empty() {
    let mut sales = tx.open_table(SALES)?;
    let mut sale_txs = tx.open_table(SALE_TXS)?;
    let mut sale_pieces = tx.open_table(SALE_PIECES)?;
    for sale in &market.sales {
      let buyer_value = sale.buyer_vout.and_then(|vout| {
        delta
          .moves
          .iter()
          .filter(|m| m.txid == sale.txid)
          .flat_map(|m| &m.outputs)
          .find(|output| output.vout == vout)
          .map(|output| output.value)
      });
      sales.insert(
        (height, sale.tx_index, sale.vin),
        encode_sale(sale, buyer_value).as_slice(),
      )?;
      sale_txs.insert(sale.txid.as_byte_array(), (height, sale.tx_index))?;
      for piece in &sale.seller.pieces {
        sale_pieces.insert((piece.start, height, sale.tx_index, sale.vin), piece.len)?;
        *max_len = (*max_len).max(piece.len);
      }
    }
  }

  if !market.inscriptions.is_empty() {
    let mut inscribed = tx.open_table(INSCRIBED)?;
    for event in &market.inscriptions {
      let previous = match inscribed.get(event.sat)? {
        Some(value) => Some(decode_inscribed(value.value())?),
        None => None,
      };
      let value = match previous {
        Some((count, txid, first, first_tx_index)) => {
          encode_inscribed(count.saturating_add(1), &txid, first, first_tx_index)
        }
        None => encode_inscribed(1, &event.txid, height, event.tx_index),
      };
      inscribed.insert(event.sat, value.as_slice())?;
    }
  }

  Ok(())
}

/// Reverts `write_market` for the block at `height`.
fn revert_market(tx: &WriteTransaction, height: u32, market: &MarketDelta) -> anyhow::Result<()> {
  {
    let mut sales = tx.open_table(SALES)?;
    let mut sale_txs = tx.open_table(SALE_TXS)?;
    let mut sale_pieces = tx.open_table(SALE_PIECES)?;
    let mut removed = Vec::new();
    for entry in sales.range((height, 0, 0)..=(height, u32::MAX, u32::MAX))? {
      let (key, value) = entry?;
      removed.push((key.value(), decode_sale(value.value())?.0));
    }
    for ((height, tx_index, vin), sale) in removed {
      let points_here = sale_txs
        .get(sale.txid.as_byte_array())?
        .is_some_and(|value| value.value() == (height, tx_index));
      if points_here {
        sale_txs.remove(sale.txid.as_byte_array())?;
      }
      for piece in &sale.seller.pieces {
        sale_pieces.remove((piece.start, height, tx_index, vin))?;
      }
      sales.remove((height, tx_index, vin))?;
    }
  }

  if !market.inscriptions.is_empty() {
    let mut inscribed = tx.open_table(INSCRIBED)?;
    for event in market.inscriptions.iter().rev() {
      let Some(value) = inscribed.get(event.sat)? else {
        bail!(
          "corrupt index: undoing an inscription on sat {} that is not recorded",
          event.sat
        );
      };
      let (count, txid, first, first_tx_index) = decode_inscribed(value.value())?;
      drop(value);
      if count <= 1 {
        inscribed.remove(event.sat)?;
      } else {
        inscribed.insert(
          event.sat,
          encode_inscribed(count - 1, &txid, first, first_tx_index).as_slice(),
        )?;
      }
    }
  }

  Ok(())
}

/// Writes the batch's history entries: to the staging file while the
/// initial sync is not complete (recording `history_staged`), else to
/// `history`. Records the first covered height. Returns the number of
/// entries.
impl Store {
  fn write_history(
    &self,
    tx: &WriteTransaction,
    deltas: &[BlockDelta],
    histories: &[Option<Vec<HistoryEntry>>],
    fast_sync_complete: bool,
    durable: bool,
  ) -> anyhow::Result<usize> {
    let Some(first) = deltas
      .iter()
      .zip(histories)
      .find(|(_, history)| history.is_some())
      .map(|(delta, _)| delta.height)
    else {
      return Ok(0);
    };
    {
      let mut table = tx.open_table(META)?;
      if table.get(KEY_HISTORY_FROM)?.is_none() {
        table.insert(KEY_HISTORY_FROM, first.to_le_bytes().as_slice())?;
      }
    }

    let entries = deltas
      .par_iter()
      .zip(histories)
      .filter_map(|(delta, history)| history.as_ref().map(|entries| (delta.height, entries)))
      .flat_map_iter(|(height, entries)| entries.iter().map(move |entry| (height, entry)))
      .map(|(height, entry)| ((height, entry.tx_index, entry.key), encode_history(entry)))
      .collect::<Vec<(StagingKey, Vec<u8>)>>();

    if fast_sync_complete {
      let mut table = tx.open_table(HISTORY)?;
      let mut keyed = entries
        .into_iter()
        .map(|((height, tx_index, key), bytes)| ((key, height, tx_index), bytes))
        .collect::<Vec<(HistoryKey, Vec<u8>)>>();
      keyed.par_sort_unstable_by_key(|(key, _)| *key);
      for (key, bytes) in &keyed {
        table.insert(key, bytes.as_slice())?;
      }
      Ok(keyed.len())
    } else {
      let mut entries = entries;
      entries.par_sort_unstable_by_key(|(key, _)| *key);
      self.stage(&entries, durable)?;
      tx.open_table(META)?
        .insert(KEY_HISTORY_STAGED, 1u32.to_le_bytes().as_slice())?;
      Ok(entries.len())
    }
  }
}

fn get_u32(
  table: &impl ReadableTable<&'static str, &'static [u8]>,
  key: &str,
) -> anyhow::Result<u32> {
  let Some(value) = table.get(key)? else {
    return Ok(0);
  };
  Ok(u32::from_le_bytes(
    value
      .value()
      .try_into()
      .with_context(|| format!("corrupt index meta `{key}`"))?,
  ))
}

fn utxo_loc(outpoint: &OutPoint, utxo: &TrackedUtxo, piece: &Piece) -> Loc {
  Loc {
    kind: match utxo.kind {
      UtxoKind::Spendable => LocationKind::Unspent,
      UtxoKind::Unspendable => LocationKind::Burned,
    },
    outpoint: *outpoint,
    offset: piece.offset,
    len: piece.len,
  }
}

/// Rewrites the moves records at or above `floor` that refer to older blocks
/// so that they are self-contained, before the blocks below `floor` are
/// pruned.
fn materialize_moves(
  moves: &mut redb::Table<'_, u32, &'static [u8]>,
  floor: u32,
) -> anyhow::Result<()> {
  let mut rewrites = Vec::new();
  for entry in moves.range(floor..)? {
    let (key, value) = entry?;
    if moves_have_external_refs(value.value()) {
      let height = key.value();
      let resolved = resolve_moves(&*moves, height, value.value())?;
      rewrites.push((
        height,
        encode_moves(&resolved, height, RefPolicy::SELF_CONTAINED),
      ));
    }
  }
  for (height, bytes) in rewrites {
    moves.insert(height, bytes.as_slice())?;
  }
  Ok(())
}

/// Decodes a block's moves, resolving inputs stored as references to older
/// blocks' moves.
fn resolve_moves(
  table: &impl ReadableTable<u32, &'static [u8]>,
  height: u32,
  bytes: &[u8],
) -> anyhow::Result<Vec<Move>> {
  let (mut moves, external) = decode_moves(bytes, height)?;
  for reference in external {
    let value = table.get(reference.height)?.with_context(|| {
      format!(
        "corrupt index: moves of block {height} refer to block {}, whose moves are missing",
        reference.height
      )
    })?;
    let utxo =
      find_output(value.value(), reference.height, &reference.outpoint)?.with_context(|| {
        format!(
          "corrupt index: moves of block {height} refer to output {} of block {}, which is \
           not there",
          reference.outpoint, reference.height
        )
      })?;
    moves[reference.move_index].inputs[reference.input_index].utxo = utxo;
  }
  Ok(moves)
}

/// Tracked UTXOs the block created that none of its moves' outputs holds
/// exactly.
fn unmatched_outputs(delta: &BlockDelta) -> impl Iterator<Item = OutPoint> + '_ {
  let outputs = delta
    .moves
    .iter()
    .flat_map(|m| {
      m.outputs
        .iter()
        .map(move |output| ((m.txid, output.vout), output))
    })
    .collect::<HashMap<_, _>>();
  delta
    .created
    .iter()
    .filter(move |(outpoint, utxo)| {
      !(utxo.height == delta.height
        && outputs
          .get(&(outpoint.txid, outpoint.vout))
          .is_some_and(|output| {
            output.value == utxo.value
              && output.kind == utxo.kind
              && output.script_pubkey == utxo.script_pubkey
              && output.pieces == utxo.pieces
          }))
    })
    .map(|(outpoint, _)| *outpoint)
}

/// A batch's net effect on the `utxos`, `locator` and `scripts` tables.
struct Net<'a> {
  utxos: FxHashMap<OutPoint, Slot<&'a TrackedUtxo>>,
  locator: FxHashMap<u64, Slot<Loc>>,
  scripts: FxHashMap<(&'a [u8], OutPoint), Slot<()>>,
  overwritten: Vec<(OutPoint, u32, &'a TrackedUtxo)>,
  lost: Vec<&'a Piece>,
}

/// `existed`: the key was in the table before the batch; `value`: its final
/// state.
#[derive(Clone, Copy)]
struct Slot<T> {
  existed: bool,
  value: Option<T>,
}

/// Records `value` as the key's latest state; `existed` only matters on the
/// key's first touch in the batch.
fn set<K: std::hash::Hash + Eq, T>(
  map: &mut FxHashMap<K, Slot<T>>,
  key: K,
  value: Option<T>,
  existed: bool,
) {
  map
    .entry(key)
    .or_insert(Slot {
      existed,
      value: None,
    })
    .value = value;
}

impl<'a> Net<'a> {
  fn build(deltas: &'a [BlockDelta]) -> Self {
    let mut net = Net {
      utxos: FxHashMap::default(),
      locator: FxHashMap::default(),
      scripts: FxHashMap::default(),
      overwritten: Vec::new(),
      lost: Vec::new(),
    };

    for delta in deltas {
      // Removals before insertions: a piece that moves keeps its first sat.
      for (outpoint, utxo) in &delta.spent {
        set(&mut net.utxos, *outpoint, None, true);
        set(
          &mut net.scripts,
          (utxo.script_pubkey.as_bytes(), *outpoint),
          None,
          true,
        );
        for piece in &utxo.pieces {
          set(&mut net.locator, piece.start, None, true);
        }
      }

      for (outpoint, utxo) in &delta.overwritten {
        set(&mut net.utxos, *outpoint, None, true);
        set(
          &mut net.scripts,
          (utxo.script_pubkey.as_bytes(), *outpoint),
          None,
          true,
        );
        for piece in &utxo.pieces {
          set(
            &mut net.locator,
            piece.start,
            Some(Loc {
              kind: LocationKind::Overwritten,
              outpoint: *outpoint,
              offset: piece.offset,
              len: piece.len,
            }),
            true,
          );
        }
        net.overwritten.push((*outpoint, delta.height, utxo));
      }

      for (outpoint, utxo) in &delta.created {
        set(&mut net.utxos, *outpoint, Some(utxo), false);
        set(
          &mut net.scripts,
          (utxo.script_pubkey.as_bytes(), *outpoint),
          Some(()),
          false,
        );
        for piece in &utxo.pieces {
          set(
            &mut net.locator,
            piece.start,
            Some(utxo_loc(outpoint, utxo, piece)),
            false,
          );
        }
      }

      for piece in &delta.lost {
        set(
          &mut net.locator,
          piece.start,
          Some(Loc {
            kind: LocationKind::Lost,
            outpoint: OutPoint::null(),
            offset: piece.offset,
            len: piece.len,
          }),
          false,
        );
        net.lost.push(piece);
      }
    }

    net
  }

  /// Writes the net changes in key order; returns the number of
  /// (utxo, locator, script) writes.
  fn write(self, tx: &WriteTransaction) -> anyhow::Result<(usize, usize, usize)> {
    let Net {
      utxos,
      locator,
      scripts,
      overwritten,
      lost,
    } = self;

    let mut utxos = utxos
      .into_par_iter()
      .filter(|(_, slot)| slot.existed || slot.value.is_some())
      .map(|(outpoint, slot)| (outpoint_key(&outpoint), slot.value.map(encode_utxo)))
      .collect::<Vec<_>>();
    utxos.par_sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let mut locator = locator
      .into_par_iter()
      .filter(|(_, slot)| slot.existed || slot.value.is_some())
      .map(|(start, slot)| {
        (
          start,
          slot.value.map(|loc| {
            let mut enc = Enc::with_capacity(52);
            encode_loc(&loc, &mut enc);
            enc.buf
          }),
        )
      })
      .collect::<Vec<_>>();
    locator.par_sort_unstable_by_key(|(start, _)| *start);

    let mut scripts = scripts
      .into_par_iter()
      .filter(|(_, slot)| slot.existed != slot.value.is_some())
      .map(|((script, outpoint), slot)| {
        (
          script_key(script),
          outpoint_key(&outpoint),
          slot.value.is_some(),
        )
      })
      .collect::<Vec<_>>();
    scripts.par_sort_unstable_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));

    let counts = (utxos.len(), locator.len(), scripts.len());

    {
      let mut table = tx.open_table(UTXOS)?;
      for (key, value) in &utxos {
        match value {
          Some(value) => table.insert(key, value.as_slice())?,
          None => table.remove(key)?,
        };
      }
    }

    {
      let mut table = tx.open_table(LOCATOR)?;
      for (start, value) in &locator {
        match value {
          Some(value) => table.insert(start, value.as_slice())?,
          None => table.remove(start)?,
        };
      }
    }

    {
      let mut table = tx.open_multimap_table(SCRIPTS)?;
      for (script, outpoint, present) in &scripts {
        if *present {
          table.insert(script.as_slice(), outpoint)?;
        } else {
          table.remove(script.as_slice(), outpoint)?;
        }
      }
    }

    {
      let mut table = tx.open_table(OVERWRITTEN)?;
      for (outpoint, height, utxo) in overwritten {
        table.insert(
          &overwritten_key(&outpoint, height),
          encode_utxo(utxo).as_slice(),
        )?;
      }
    }

    {
      let mut table = tx.open_table(LOST)?;
      for piece in lost {
        table.insert(piece.offset, encode_lost(piece).as_slice())?;
      }
    }

    Ok(counts)
  }
}

/// A consistent read-only view of the index (one redb read transaction).
pub struct Reader {
  tx: ReadTransaction,
}

impl Reader {
  pub fn meta(&self) -> anyhow::Result<Option<IndexMeta>> {
    read_meta(&self.tx.open_table(META)?)
  }

  pub fn height(&self) -> anyhow::Result<Option<u32>> {
    Ok(self.meta()?.and_then(|meta| meta.height))
  }

  pub fn lost_sats(&self) -> anyhow::Result<u64> {
    let table = self.tx.open_table(META)?;
    let Some(value) = table.get(KEY_LOST_SATS)? else {
      return Ok(0);
    };
    Ok(u64::from_le_bytes(
      value.value().try_into().context("corrupt index meta")?,
    ))
  }

  pub fn mined(&self) -> anyhow::Result<Vec<u64>> {
    let table = self.tx.open_table(META)?;
    match table.get(KEY_MINED)? {
      Some(value) => decode_u64s(value.value()),
      None => Ok(Vec::new()),
    }
  }

  /// Lowest height whose moves may be stored: moves below it were pruned
  /// or never written (`--moves-retention`).
  pub fn moves_floor(&self) -> anyhow::Result<u32> {
    get_u32(&self.tx.open_table(META)?, KEY_MOVES_FLOOR)
  }

  pub fn header(&self, height: u32) -> anyhow::Result<Option<Header>> {
    let table = self.tx.open_table(HEADERS)?;
    let Some(value) = table.get(height)? else {
      return Ok(None);
    };
    Ok(Some(header_from_bytes(value.value())?))
  }

  pub fn block_hash(&self, height: u32) -> anyhow::Result<Option<BlockHash>> {
    Ok(self.header(height)?.map(|header| header.block_hash()))
  }

  /// Headers of `from..=to`, in height order.
  pub fn headers(&self, from: u32, to: u32) -> anyhow::Result<Vec<(u32, Header)>> {
    let table = self.tx.open_table(HEADERS)?;
    let mut headers = Vec::new();
    if from > to {
      return Ok(headers);
    }
    for entry in table.range(from..=to)? {
      let (key, value) = entry?;
      headers.push((key.value(), header_from_bytes(value.value())?));
    }
    Ok(headers)
  }

  pub fn utxo(&self, outpoint: OutPoint) -> anyhow::Result<Option<TrackedUtxo>> {
    let table = self.tx.open_table(UTXOS)?;
    let Some(value) = table.get(&outpoint_key(&outpoint))? else {
      return Ok(None);
    };
    Ok(Some(decode_utxo(value.value())?))
  }

  /// Tracked UTXOs that BIP30 duplicates replaced at this outpoint, with the
  /// replacing height.
  pub fn overwritten(&self, outpoint: OutPoint) -> anyhow::Result<Vec<(u32, TrackedUtxo)>> {
    let table = self.tx.open_table(OVERWRITTEN)?;
    let mut utxos = Vec::new();
    let first = overwritten_key(&outpoint, 0);
    let last = overwritten_key(&outpoint, u32::MAX);
    for entry in table.range::<&[u8; 40]>(&first..=&last)? {
      let (key, value) = entry?;
      utxos.push((
        overwritten_from_key(key.value()).1,
        decode_utxo(value.value())?,
      ));
    }
    Ok(utxos)
  }

  pub fn locate(&self, sat: u64) -> anyhow::Result<Option<SatLocation>> {
    let table = self.tx.open_table(LOCATOR)?;
    let Some(entry) = table.range(..=sat)?.next_back() else {
      return Ok(None);
    };
    let (key, value) = entry?;
    let start = key.value();
    let loc = decode_loc(value.value())?;
    if sat - start >= loc.len {
      return Ok(None);
    }
    Ok(Some(location(start, &loc, sat)))
  }

  /// Every tracked piece (unspent, burned, lost or overwritten) overlapping
  /// `[from, to)`, in sat order.
  pub fn pieces(
    &self,
    from: u64,
    to: u64,
  ) -> anyhow::Result<impl Iterator<Item = anyhow::Result<SatLocation>> + 'static> {
    let table = self.tx.open_table(LOCATOR)?;

    let mut first = from;
    if let Some(entry) = table.range(..from)?.next_back() {
      let (key, value) = entry?;
      let loc = decode_loc(value.value())?;
      if key.value() + loc.len > from {
        first = key.value();
      }
    }

    let range = table.range(first..to.max(first))?;

    Ok(range.map(|entry| {
      let (key, value) = entry?;
      let start = key.value();
      let loc = decode_loc(value.value())?;
      Ok(location(start, &loc, start))
    }))
  }

  pub fn utxos_for_script(
    &self,
    script_pubkey: &Script,
  ) -> anyhow::Result<Vec<(OutPoint, TrackedUtxo)>> {
    let scripts = self.tx.open_multimap_table(SCRIPTS)?;
    let utxos = self.tx.open_table(UTXOS)?;
    let mut result = Vec::new();
    for entry in scripts.get(script_key(script_pubkey.as_bytes()).as_slice())? {
      let key = *entry?.value();
      let Some(value) = utxos.get(&key)? else {
        bail!(
          "corrupt index: script index entry {} has no utxo",
          outpoint_from_key(&key)
        );
      };
      let utxo = decode_utxo(value.value())?;
      if utxo.script_pubkey.as_script() == script_pubkey {
        result.push((outpoint_from_key(&key), utxo));
      }
    }
    Ok(result)
  }

  /// `None` when the block is not indexed or its moves were not retained.
  pub fn moves(&self, height: u32) -> anyhow::Result<Option<Vec<Move>>> {
    let Some(tip) = self.height()? else {
      return Ok(None);
    };
    if height > tip || height < self.moves_floor()? {
      return Ok(None);
    }
    let table = self.tx.open_table(MOVES)?;
    let Some(value) = table.get(height)? else {
      return Ok(Some(Vec::new()));
    };
    Ok(Some(resolve_moves(&table, height, value.value())?))
  }

  /// Heights in `from..=to` whose stored moves record is not empty.
  pub fn heights_with_moves(&self, from: u32, to: u32) -> anyhow::Result<Vec<u32>> {
    let table = self.tx.open_table(MOVES)?;
    let mut heights = Vec::new();
    if from > to {
      return Ok(heights);
    }
    for entry in table.range(from..=to)? {
      heights.push(entry?.0.value());
    }
    Ok(heights)
  }

  /// The first height in `from..=to` whose stored moves record is not
  /// empty.
  pub fn next_height_with_moves(&self, from: u32, to: u32) -> anyhow::Result<Option<u32>> {
    if from > to {
      return Ok(None);
    }
    let table = self.tx.open_table(MOVES)?;
    let Some(entry) = table.range(from..=to)?.next() else {
      return Ok(None);
    };
    Ok(Some(entry?.0.value()))
  }

  /// The row's stored history entries in `from..=to`.
  pub fn row_history(
    &self,
    row: RowId,
    from: u32,
    to: u32,
  ) -> anyhow::Result<Vec<(u32, RowStats)>> {
    let table = self.tx.open_table(ROW_HISTORY)?;
    let mut history = Vec::new();
    if from > to {
      return Ok(history);
    }
    for entry in table.range((row, from)..=(row, to))? {
      let (key, value) = entry?;
      history.push((key.value().1, decode_row_stats(value.value())?));
    }
    Ok(history)
  }

  /// The row's counters after block `height`: its last history entry at or
  /// below `height`, with that entry's height (`None`: never changed).
  pub fn row_stats_at(&self, row: RowId, height: u32) -> anyhow::Result<Option<(u32, RowStats)>> {
    let table = self.tx.open_table(ROW_HISTORY)?;
    let Some(entry) = table.range((row, 0)..=(row, height))?.next_back() else {
      return Ok(None);
    };
    let (key, value) = entry?;
    Ok(Some((key.value().1, decode_row_stats(value.value())?)))
  }

  /// The row's counters before and after block `height`, if that block
  /// changed them.
  pub fn row_delta(&self, row: RowId, height: u32) -> anyhow::Result<Option<(RowStats, RowStats)>> {
    let table = self.tx.open_table(ROW_HISTORY)?;
    let Some(after) = table.get((row, height))? else {
      return Ok(None);
    };
    let after = decode_row_stats(after.value())?;
    let before = match height.checked_sub(1) {
      Some(previous) => match table.range((row, 0)..=(row, previous))?.next_back() {
        Some(entry) => decode_row_stats(entry?.1.value())?,
        None => RowStats::default(),
      },
      None => RowStats::default(),
    };
    Ok(Some((before, after)))
  }

  /// Sales from `from` (a sale key) to the end of block `to_height`, oldest
  /// first.
  pub fn sales(
    &self,
    from: SaleKey,
    to_height: u32,
  ) -> anyhow::Result<impl Iterator<Item = anyhow::Result<StoredSale>> + 'static> {
    let table = self.tx.open_table(SALES)?;
    let end = (to_height, u32::MAX, u32::MAX);
    let range = table.range(from..=end.max(from))?;
    Ok(range.map(|entry| {
      let (key, value) = entry?;
      let (height, _, _) = key.value();
      let (sale, buyer_value) = decode_sale(value.value())?;
      Ok(StoredSale {
        height,
        sale,
        buyer_value,
      })
    }))
  }

  pub fn sale(&self, key: SaleKey) -> anyhow::Result<Option<StoredSale>> {
    let table = self.tx.open_table(SALES)?;
    let Some(value) = table.get(key)? else {
      return Ok(None);
    };
    let (sale, buyer_value) = decode_sale(value.value())?;
    Ok(Some(StoredSale {
      height: key.0,
      sale,
      buyer_value,
    }))
  }

  /// The sales a transaction made, by vin.
  pub fn sales_for_tx(&self, txid: Txid) -> anyhow::Result<Vec<StoredSale>> {
    let Some((height, tx_index)) = self
      .tx
      .open_table(SALE_TXS)?
      .get(txid.as_byte_array())?
      .map(|value| value.value())
    else {
      return Ok(Vec::new());
    };
    self
      .sales((height, tx_index, 0), height)?
      .take_while(|sale| {
        sale
          .as_ref()
          .map_or(true, |sale| sale.sale.tx_index == tx_index)
      })
      .collect()
  }

  /// The sales whose sold pieces cover `sat`, oldest first.
  pub fn sales_for_sat(&self, sat: u64) -> anyhow::Result<Vec<StoredSale>> {
    let max_len = get_u64(&self.tx.open_table(META)?, KEY_SALE_PIECE_MAX_LEN)?;
    if max_len == 0 {
      return Ok(Vec::new());
    }
    let table = self.tx.open_table(SALE_PIECES)?;
    let low = sat.saturating_sub(max_len - 1);
    let mut keys = Vec::new();
    for entry in table.range((low, 0, 0, 0)..=(sat, u32::MAX, u32::MAX, u32::MAX))? {
      let (key, len) = entry?;
      let (start, height, tx_index, vin) = key.value();
      if sat - start < len.value() {
        keys.push((height, tx_index, vin));
      }
    }
    keys.sort_unstable();
    keys.dedup();
    let mut sales = Vec::with_capacity(keys.len());
    for key in keys {
      sales.push(
        self
          .sale(key)?
          .with_context(|| format!("corrupt index: sale piece for missing sale {key:?}"))?,
      );
    }
    Ok(sales)
  }

  pub fn inscribed(&self, sat: u64) -> anyhow::Result<Option<Inscribed>> {
    let table = self.tx.open_table(INSCRIBED)?;
    let Some(value) = table.get(sat)? else {
      return Ok(None);
    };
    let (count, first_txid, first_height, first_tx_index) = decode_inscribed(value.value())?;
    Ok(Some(Inscribed {
      count,
      first_txid,
      first_height,
      first_tx_index,
    }))
  }

  /// Inscribed sats in `[start, end)`.
  pub fn inscribed_in(&self, start: u64, end: u64) -> anyhow::Result<Vec<(u64, Inscribed)>> {
    if start >= end {
      return Ok(Vec::new());
    }
    self
      .inscribed_from(start)?
      .take_while(|entry| entry.as_ref().map_or(true, |(sat, _)| *sat < end))
      .collect()
  }

  /// The inscribed sats of `pieces`, in sat order.
  pub fn inscribed_in_pieces(&self, pieces: &[Piece]) -> anyhow::Result<Vec<(u64, Inscribed)>> {
    let table = self.tx.open_table(INSCRIBED)?;
    if table.is_empty()? {
      return Ok(Vec::new());
    }
    let mut inscribed = Vec::new();
    for piece in pieces {
      for entry in table.range(piece.start..piece.end())? {
        let (key, value) = entry?;
        let (count, first_txid, first_height, first_tx_index) = decode_inscribed(value.value())?;
        inscribed.push((
          key.value(),
          Inscribed {
            count,
            first_txid,
            first_height,
            first_tx_index,
          },
        ));
      }
    }
    inscribed.sort_unstable_by_key(|(sat, _)| *sat);
    Ok(inscribed)
  }

  /// Every inscribed sat from `from` on, in sat order.
  pub fn inscribed_from(
    &self,
    from: u64,
  ) -> anyhow::Result<impl Iterator<Item = anyhow::Result<(u64, Inscribed)>> + 'static> {
    let table = self.tx.open_table(INSCRIBED)?;
    Ok(table.range(from..)?.map(|entry| {
      let (key, value) = entry?;
      let (count, first_txid, first_height, first_tx_index) = decode_inscribed(value.value())?;
      Ok((
        key.value(),
        Inscribed {
          count,
          first_txid,
          first_height,
          first_tx_index,
        },
      ))
    }))
  }

  /// The first height the address history covers (`None`: no covered
  /// block indexed yet).
  pub fn history_from(&self) -> anyhow::Result<Option<u32>> {
    let table = self.tx.open_table(META)?;
    if table.get(KEY_HISTORY_FROM)?.is_none() {
      return Ok(None);
    }
    Ok(Some(get_u32(&table, KEY_HISTORY_FROM)?))
  }

  /// Whether history entries are still staged (the initial sync is not
  /// complete): `history` is then incomplete.
  pub fn history_building(&self) -> anyhow::Result<bool> {
    Ok(get_u32(&self.tx.open_table(META)?, KEY_HISTORY_STAGED)? == 1)
  }

  /// A script's history entries with heights in `from..=to` and keys below
  /// `before` (exclusive, as (height, tx_index)), newest first.
  pub fn history(
    &self,
    key: u64,
    from: u32,
    to: u32,
    before: Option<(u32, u32)>,
  ) -> anyhow::Result<impl Iterator<Item = anyhow::Result<HistoryEntry>> + 'static> {
    let table = self.tx.open_table(HISTORY)?;
    let low = (key, from, 0);
    let mut high = (key, to, u32::MAX);
    let mut empty = from > to;
    if let Some((height, tx_index)) = before {
      let cursor = (key, height, tx_index);
      if cursor <= low {
        empty = true;
      } else if cursor <= high {
        // Exclusive: the key just below the cursor.
        high = match tx_index.checked_sub(1) {
          Some(tx_index) => (key, height, tx_index),
          None => (key, height - 1, u32::MAX),
        };
      }
    }
    let range = if empty {
      table.range(low..low)?
    } else {
      table.range(low..=high)?
    };
    Ok(range.rev().map(|entry| {
      let (key, value) = entry?;
      let (script, height, tx_index) = key.value();
      decode_history(script, height, tx_index, value.value())
    }))
  }

  pub fn undo_heights(&self) -> anyhow::Result<Vec<u32>> {
    let table = self.tx.open_table(UNDO)?;
    let mut heights = Vec::new();
    for entry in table.iter()? {
      heights.push(entry?.0.value());
    }
    Ok(heights)
  }
}

fn location(start: u64, loc: &Loc, sat: u64) -> SatLocation {
  SatLocation {
    kind: loc.kind,
    outpoint: loc.outpoint,
    offset: loc.offset + (sat - start),
    piece: Piece {
      offset: loc.offset,
      start,
      len: loc.len,
    },
  }
}
