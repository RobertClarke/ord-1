use {
  super::*,
  crate::{
    indexer::RunMode,
    store::{IndexMeta, TableInfo},
  },
  bitcoin::BlockHash,
};

#[derive(Debug, clap::Subcommand)]
pub enum Index {
  /// Index to bitcoind's tip, then exit
  Update,
  /// Index meta and table sizes
  Info,
}

impl Index {
  pub fn run(self, settings: Settings) -> anyhow::Result<()> {
    match self {
      Self::Update => update(settings),
      Self::Info => info(settings),
    }
  }
}

#[derive(Serialize)]
struct UpdateOutput {
  height: Option<u32>,
  hash: Option<BlockHash>,
  fast_sync_complete: bool,
  tracked_utxos: u64,
  tracked_pieces: u64,
  lost_sats: u64,
  commits: u64,
}

fn update(settings: Settings) -> anyhow::Result<()> {
  let store = Arc::new(open_store(&settings, true)?);
  let catalog = catalog(&settings)?;
  let published = Arc::new(Published::default());
  let shutdown = Arc::new(AtomicBool::new(false));
  handle_signals(shutdown.clone());

  let indexer = indexer(
    &settings,
    store,
    catalog,
    published.clone(),
    shutdown,
    IndexerOptions {
      max_retries: Some(5),
      ..IndexerOptions::new(&settings)
    },
  )?;
  indexer.run(RunMode::UntilCaughtUp)?;

  let status = published.status();
  print_json(&UpdateOutput {
    height: status.height,
    hash: status.hash,
    fast_sync_complete: status.fast_sync_complete,
    tracked_utxos: status.tracked_utxos,
    tracked_pieces: status.tracked_pieces,
    lost_sats: status.lost_sats,
    commits: status.timings.commits,
  })
}

#[derive(Serialize)]
struct InfoOutput {
  path: String,
  file_bytes: u64,
  meta: Option<IndexMeta>,
  lost_sats: u64,
  mined: Vec<u64>,
  moves_retention: String,
  moves_from: u32,
  undo_heights: Option<(u32, u32)>,
  tables: Vec<TableInfo>,
}

fn info(settings: Settings) -> anyhow::Result<()> {
  if !settings.index_path.exists() {
    anyhow::bail!(
      "there is no index at {}; run `ord index update` or `ord server`",
      settings.index_path.display()
    );
  }
  let store = open_store(&settings, false)?;
  let reader = store.reader()?;
  let undo = reader.undo_heights()?;
  let output = InfoOutput {
    path: settings.index_path.display().to_string(),
    file_bytes: std::fs::metadata(&settings.index_path)?.len(),
    meta: reader.meta()?,
    lost_sats: reader.lost_sats()?,
    mined: reader.mined()?,
    moves_retention: settings.store.moves_retention.to_string(),
    moves_from: reader.moves_floor()?,
    undo_heights: undo.first().zip(undo.last()).map(|(a, b)| (*a, *b)),
    tables: {
      drop(reader);
      store.table_info()?
    },
  };
  print_json(&output)
}
