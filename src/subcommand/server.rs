use {
  super::*,
  crate::{
    indexer::{RunMode, expected_meta, restore},
    server::{self as api, ServerState},
  },
  std::{
    net::ToSocketAddrs,
    time::{Duration, SystemTime, UNIX_EPOCH},
  },
};

#[derive(Debug, clap::Args)]
pub struct Server {
  /// Listen on this address
  #[arg(long, env = "ORD_HTTP_ADDRESS", default_value = "127.0.0.1")]
  pub http_address: String,
  /// Listen on this port
  #[arg(long, env = "ORD_HTTP_PORT", default_value_t = 8090)]
  pub http_port: u16,
  /// Serve the index as it is, without indexing
  #[arg(long, env = "ORD_NO_SYNC")]
  pub no_sync: bool,
}

impl Server {
  pub fn run(self, settings: Settings) -> anyhow::Result<()> {
    let address = (self.http_address.as_str(), self.http_port)
      .to_socket_addrs()
      .with_context(|| format!("invalid --http-address {}", self.http_address))?
      .next()
      .with_context(|| format!("{} resolves to no address", self.http_address))?;

    let store = Arc::new(open_store(&settings, !self.no_sync)?);
    let catalog = catalog(&settings)?;
    let published = Arc::new(Published::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    handle_signals(shutdown.clone());

    let started = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map(|duration| duration.as_secs())
      .unwrap_or_default();

    let state = Arc::new(ServerState {
      store: store.clone(),
      catalog: catalog_view(catalog.clone()),
      published: published.clone(),
      chain: settings.chain,
      config: settings.config.clone(),
      moves_retention: settings.store.moves_retention,
      started,
    });

    let indexer = if self.no_sync {
      let meta = store
        .meta()?
        .context("the index is empty; run without --no-sync to build it")?;
      meta.check_compatible(&expected_meta(
        settings.chain,
        &settings.config,
        catalog.fingerprint(),
      ))?;
      restore(
        &store,
        &TrackerFactory::new(catalog, settings.config.clone()),
        &published,
        &meta,
        IndexerOptions::new(&settings).supply_top,
      )?;
      None
    } else {
      let indexer = indexer(
        &settings,
        store.clone(),
        catalog,
        published.clone(),
        shutdown.clone(),
        IndexerOptions::new(&settings),
      )?;
      let shutdown = shutdown.clone();
      Some(
        thread::Builder::new()
          .name("indexer".into())
          .spawn(move || {
            let result = indexer.run(RunMode::Forever);
            if let Err(err) = &result {
              log::error!("indexer failed: {err:#}");
            }
            // The server stops with the indexer.
            shutdown.store(true, Ordering::Relaxed);
            result
          })
          .context("failed to spawn the indexer thread")?,
      )
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
      .enable_all()
      .thread_name("http")
      .build()?;

    let served = runtime.block_on(async {
      let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to listen on {address}"))?;
      log::info!("serving the JSON API on http://{address}");

      let stopped = {
        let shutdown = shutdown.clone();
        async move {
          while !shutdown.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(100)).await;
          }
        }
      };

      let grace = {
        let shutdown = shutdown.clone();
        async move {
          while !shutdown.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(100)).await;
          }
          tokio::time::sleep(Duration::from_secs(10)).await;
        }
      };

      tokio::select! {
        result = api::serve(state, listener, stopped) => result,
        () = grace => {
          log::warn!("closing HTTP connections still open");
          Ok(())
        }
      }
    });

    // Stop the indexer if the server failed on its own.
    shutdown.store(true, Ordering::Relaxed);
    runtime.shutdown_timeout(Duration::from_secs(1));

    let indexed = match indexer {
      Some(handle) => handle
        .join()
        .map_err(|_| anyhow::anyhow!("the indexer thread panicked"))?,
      None => Ok(()),
    };

    served?;
    indexed
  }
}
