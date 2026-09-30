//! Command line: global options (`settings::Options`) and subcommands.

use {
  crate::{
    indexer::{Indexer, IndexerOptions, Published, RpcNode, TrackerFactory},
    sats::SatCatalog,
    server::catalog::SatCatalogView,
    settings::{Options, Settings},
    store::Store,
  },
  anyhow::Context,
  clap::Parser,
  serde::Serialize,
  std::{
    sync::{
      Arc,
      atomic::{AtomicBool, Ordering},
    },
    thread,
  },
};

mod index;
mod sat;
mod server;

#[derive(Debug, Parser)]
#[command(
  name = "ord",
  version,
  about = "Rare-sat tracker: where every tracked rare sat is, block by block",
  long_about = "Rare-sat tracker: where every tracked rare sat is, block by block.\n\nEvery \
                option can also be set with the ORD_* environment variable named in its help."
)]
pub struct Arguments {
  #[command(flatten)]
  pub options: Options,
  #[command(subcommand)]
  pub subcommand: Subcommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum Subcommand {
  /// Index in the background and serve the JSON API
  Server(server::Server),
  /// Build or inspect the index
  #[command(subcommand)]
  Index(index::Index),
  /// Where a tracked sat is (as `GET /sat/<SAT>`)
  Find(sat::Find),
  /// A sat's types and rows, from the catalog
  Types(sat::Types),
  /// Row definitions and totals
  Rows,
}

impl Arguments {
  pub fn run(self) -> anyhow::Result<()> {
    let settings = Settings::from_options(self.options)?;
    match self.subcommand {
      Subcommand::Server(server) => server.run(settings),
      Subcommand::Index(index) => index.run(settings),
      Subcommand::Find(find) => find.run(settings),
      Subcommand::Types(types) => types.run(settings),
      Subcommand::Rows => sat::rows(settings),
    }
  }
}

pub(crate) fn print_json(value: &impl Serialize) -> anyhow::Result<()> {
  serde_json::to_writer_pretty(std::io::stdout().lock(), value)?;
  println!();
  Ok(())
}

pub(crate) fn catalog(settings: &Settings) -> anyhow::Result<Arc<SatCatalog>> {
  Ok(Arc::new(
    SatCatalog::new(&settings.config).context("failed to load the sat catalog")?,
  ))
}

/// Opens the index, deleting it first with `--reindex` when `indexing`.
pub(crate) fn open_store(settings: &Settings, indexing: bool) -> anyhow::Result<Store> {
  if indexing && settings.reindex && settings.index_path.exists() {
    log::info!("--reindex: deleting {}", settings.index_path.display());
    std::fs::remove_file(&settings.index_path)
      .with_context(|| format!("failed to delete {}", settings.index_path.display()))?;
  }
  Store::open_with(&settings.index_path, settings.store.clone())
}

pub(crate) fn indexer(
  settings: &Settings,
  store: Arc<Store>,
  catalog: Arc<SatCatalog>,
  published: Arc<Published>,
  shutdown: Arc<AtomicBool>,
  options: IndexerOptions,
) -> anyhow::Result<Indexer> {
  Ok(Indexer::new(
    store,
    Arc::new(RpcNode::new(settings.rpc.clone())?),
    Arc::new(TrackerFactory::new(catalog, settings.config.clone())),
    published,
    shutdown,
    options,
  ))
}

pub(crate) fn catalog_view(catalog: Arc<SatCatalog>) -> Arc<SatCatalogView> {
  Arc::new(SatCatalogView::new(catalog))
}

/// Sets `shutdown` on the first SIGINT/SIGTERM; exits on the second.
pub(crate) fn handle_signals(shutdown: Arc<AtomicBool>) {
  thread::Builder::new()
    .name("signals".into())
    .spawn(move || {
      let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
      {
        Ok(runtime) => runtime,
        Err(err) => {
          log::error!("failed to start the signal handler: {err}");
          return;
        }
      };
      runtime.block_on(async move {
        #[cfg(unix)]
        let mut terminate =
          match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(err) => {
              log::error!("failed to listen for SIGTERM: {err}");
              return;
            }
          };
        let mut signals = 0;
        loop {
          #[cfg(unix)]
          tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
          }
          #[cfg(not(unix))]
          let _ = tokio::signal::ctrl_c().await;
          signals += 1;
          if signals == 1 {
            log::info!("shutting down after the current commit (signal again to exit immediately)");
            shutdown.store(true, Ordering::Relaxed);
          } else {
            log::warn!("exiting without waiting for the indexer");
            std::process::exit(130);
          }
        }
      });
    })
    .expect("failed to spawn the signal thread");
}
