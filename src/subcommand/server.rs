use {
  super::*,
  crate::{
    indexer::{Mode, RunMode, expected_meta, restore},
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
  /// Do not follow bitcoind's mempool (no pending activity in the API)
  #[arg(long, env = "ORD_NO_MEMPOOL")]
  pub no_mempool: bool,
  /// Seconds between mempool polls (also polled right after each block)
  #[arg(long, env = "ORD_MEMPOOL_INTERVAL", default_value_t = 5.0)]
  pub mempool_interval: f64,
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
    let catalog_arc = catalog.clone();
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
      stream_timeout: api::STREAM_TIMEOUT,
      streams: Default::default(),
      inscribed_rows: Default::default(),
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
        &TrackerFactory::new(catalog, settings.config.clone(), settings.chain),
        &published,
        &meta,
        IndexerOptions::new(&settings).supply_top,
      )?;
      published.update_status(|status| status.mode = Mode::Serving);
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

    let mempool = if self.no_sync || self.no_mempool {
      None
    } else {
      anyhow::ensure!(
        self.mempool_interval.is_finite() && self.mempool_interval >= 0.1,
        "--mempool-interval must be at least 0.1 seconds"
      );
      let options = crate::mempool::MempoolOptions {
        interval: Duration::from_secs_f64(self.mempool_interval),
        ..Default::default()
      };
      let rpc = crate::source::Rpc::with_retry_policy(
        settings.rpc.clone(),
        crate::source::RetryPolicy {
          attempts: 2,
          initial_delay: Duration::from_millis(200),
          max_delay: Duration::from_secs(1),
        },
      )?
      .with_cancel(shutdown.clone());
      let node = crate::mempool::RpcMempoolNode::new(rpc);
      let ctx = crate::market::MarketContext::new(
        catalog_arc.clone(),
        crate::market::Marketplaces::embedded(),
        settings.chain,
      );
      let store = store.clone();
      let published = published.clone();
      let shutdown = shutdown.clone();
      Some(
        thread::Builder::new()
          .name("mempool".into())
          .spawn(move || {
            crate::mempool::run(store, &node, &ctx, &published, options, &shutdown);
          })
          .context("failed to spawn the mempool thread")?,
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

    if let Some(handle) = mempool
      && handle.join().is_err()
    {
      log::error!("the mempool thread panicked");
    }

    served?;
    indexed
  }
}
