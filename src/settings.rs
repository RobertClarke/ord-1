//! Global command-line options (every one also read from an `ORD_*`
//! environment variable) and the settings resolved from them.

use {
  crate::{
    model::{TrackedRange, TrackerConfig},
    source::{RpcAuth, RpcConfig},
    store::{MovesRetention, StoreOptions},
  },
  anyhow::{Context, bail, ensure},
  bitcoin::Network,
  clap::ValueEnum,
  std::{
    fmt::{self, Display, Formatter},
    path::{Path, PathBuf},
    str::FromStr,
  },
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Chain {
  #[default]
  Mainnet,
  Testnet4,
  Signet,
  Regtest,
}

impl Chain {
  pub fn network(self) -> Network {
    match self {
      Self::Mainnet => Network::Bitcoin,
      Self::Testnet4 => Network::Testnet4,
      Self::Signet => Network::Signet,
      Self::Regtest => Network::Regtest,
    }
  }

  /// The `--chain` spelling, also used in the API and the index meta.
  pub fn name(self) -> &'static str {
    match self {
      Self::Mainnet => "mainnet",
      Self::Testnet4 => "testnet4",
      Self::Signet => "signet",
      Self::Regtest => "regtest",
    }
  }

  /// `getblockchaininfo.chain` of a bitcoind on this chain.
  pub fn bitcoind_name(self) -> &'static str {
    match self {
      Self::Mainnet => "main",
      Self::Testnet4 => "testnet4",
      Self::Signet => "signet",
      Self::Regtest => "regtest",
    }
  }

  pub fn default_rpc_port(self) -> u16 {
    match self {
      Self::Mainnet => 8332,
      Self::Testnet4 => 48332,
      Self::Signet => 38332,
      Self::Regtest => 18443,
    }
  }

  /// `dir` for mainnet, `dir/<chain>` otherwise (bitcoind and ord layout).
  pub fn join_with_data_dir(self, dir: &Path) -> PathBuf {
    match self {
      Self::Mainnet => dir.into(),
      _ => dir.join(self.name()),
    }
  }
}

impl Display for Chain {
  fn fmt(&self, f: &mut Formatter) -> fmt::Result {
    f.write_str(self.name())
  }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum FastSync {
  /// Use the fast initial sync for an empty index when bitcoind is more
  /// than 12 blocks ahead.
  #[default]
  Auto,
  /// Always index block by block with `getblock <hash> 3`.
  Never,
}

/// `--tracked-range ID:START:END`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeArg(pub TrackedRange);

impl FromStr for RangeArg {
  type Err = String;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    let parts = s.split(':').collect::<Vec<&str>>();
    let [id, start, end] = parts.as_slice() else {
      return Err(format!("expected ID:START:END, got `{s}`"));
    };
    let parse = |value: &str| {
      value
        .replace('_', "")
        .parse::<u64>()
        .map_err(|err| format!("invalid sat `{value}`: {err}"))
    };
    let (start, end) = (parse(start)?, parse(end)?);
    if id.is_empty() || start >= end {
      return Err(format!(
        "expected a non-empty id and START < END, got `{s}`"
      ));
    }
    Ok(Self(TrackedRange {
      id: (*id).into(),
      start,
      end,
    }))
  }
}

#[derive(Debug, Clone, clap::Args)]
pub struct Options {
  /// bitcoind JSON-RPC URL [default: http://127.0.0.1:<chain RPC port>]
  #[arg(long, env = "ORD_BITCOIN_RPC_URL", value_name = "URL")]
  pub bitcoin_rpc_url: Option<String>,

  /// bitcoind RPC user (with --bitcoin-rpc-password, instead of the cookie)
  #[arg(long, env = "ORD_BITCOIN_RPC_USERNAME", value_name = "USER")]
  pub bitcoin_rpc_username: Option<String>,

  /// bitcoind RPC password
  #[arg(
    long,
    env = "ORD_BITCOIN_RPC_PASSWORD",
    value_name = "PASSWORD",
    hide_env_values = true
  )]
  pub bitcoin_rpc_password: Option<String>,

  /// bitcoind cookie file [default: <bitcoin data dir>/[<chain>/].cookie]
  #[arg(long, env = "ORD_COOKIE_FILE", value_name = "PATH")]
  pub cookie_file: Option<PathBuf>,

  /// bitcoind data directory, to find the default cookie file [default:
  /// ~/.bitcoin]
  #[arg(long, env = "ORD_BITCOIN_DATA_DIR", value_name = "DIR")]
  pub bitcoin_data_dir: Option<PathBuf>,

  /// bitcoind RPC timeout in seconds
  #[arg(
    long,
    env = "ORD_BITCOIN_RPC_TIMEOUT",
    default_value_t = 120,
    value_name = "SECONDS"
  )]
  pub bitcoin_rpc_timeout: u64,

  /// Where the index lives (in a <chain> subdirectory except on mainnet)
  /// [default: <data dir>/ord-sats]
  #[arg(long, env = "ORD_DATA_DIR", value_name = "DIR")]
  pub data_dir: Option<PathBuf>,

  /// Chain; must match bitcoind's
  #[arg(long, env = "ORD_CHAIN", value_enum, default_value_t)]
  pub chain: Chain,

  /// Commit the fast sync and catch-up every this many blocks (or 30 s)
  #[arg(
    long,
    env = "ORD_COMMIT_INTERVAL",
    default_value_t = 5000,
    value_name = "BLOCKS",
    value_parser = clap::value_parser!(u32).range(1..)
  )]
  pub commit_interval: u32,

  /// Parallel getblock requests
  #[arg(
    long,
    env = "ORD_FETCH_THREADS",
    default_value_t = 16,
    value_name = "N",
    value_parser = clap::value_parser!(u16).range(1..)
  )]
  pub fetch_threads: u16,

  /// Fast initial sync of an empty index
  #[arg(long, env = "ORD_FAST_SYNC", value_enum, default_value_t)]
  pub fast_sync: FastSync,

  /// Delete the index and build it again
  #[arg(long, env = "ORD_REINDEX")]
  pub reindex: bool,

  /// Keep the moves of this many recent blocks, or `all`
  #[arg(
    long,
    env = "ORD_MOVES_RETENTION",
    default_value_t = MovesRetention::DEFAULT,
    value_name = "BLOCKS|all"
  )]
  pub moves_retention: MovesRetention,

  /// Index database cache, in MiB
  #[arg(
    long,
    env = "ORD_DB_CACHE_MIB",
    default_value_t = 4096,
    value_name = "MIB"
  )]
  pub db_cache_mib: usize,

  /// Track every sat of this range (repeatable; replaces the default
  /// block9:45000000000:50000000000); needs a reindex when changed
  #[arg(
    long = "tracked-range",
    env = "ORD_TRACKED_RANGES",
    value_name = "ID:START:END",
    value_delimiter = ','
  )]
  pub tracked_ranges: Vec<RangeArg>,

  /// Extraction allowance: an output counts as extracted when worth at most
  /// this many sats per individually tracked sat it holds, plus each run of
  /// tracked-range sats' length and this much again [default: 10000]; needs
  /// a reindex when changed
  #[arg(long, env = "ORD_EXTRACTED_MAX_VALUE", value_name = "SATS")]
  pub extracted_max_value: Option<u64>,

  /// Outputs created by blocks at or after this unix time count as extracted
  /// [default: 1670976000]; needs a reindex when changed
  #[arg(long, env = "ORD_EXTRACTED_SINCE", value_name = "UNIX_TIME")]
  pub extracted_since: Option<u32>,

  /// First block of the address history: `ordinals` (the first block at or
  /// after --extracted-since), `all` or a height [default: ordinals]; needs
  /// a reindex when changed
  #[arg(
    long,
    env = "ORD_ADDRESS_HISTORY_FROM",
    value_name = "HEIGHT|ordinals|all"
  )]
  pub address_history_from: Option<crate::model::HistoryFrom>,

  /// Index only blocks below this height (tests and benchmarks)
  #[arg(long, env = "ORD_HEIGHT_LIMIT", value_name = "HEIGHT")]
  pub height_limit: Option<u32>,
}

/// Resolved settings.
#[derive(Debug, Clone)]
pub struct Settings {
  pub chain: Chain,
  pub rpc: RpcConfig,
  /// Chain-specific data directory.
  pub data_dir: PathBuf,
  pub index_path: PathBuf,
  pub commit_interval: u32,
  pub fetch_threads: usize,
  pub fast_sync: FastSync,
  pub reindex: bool,
  pub store: StoreOptions,
  pub config: TrackerConfig,
  pub height_limit: Option<u32>,
}

impl Settings {
  pub fn from_options(options: Options) -> anyhow::Result<Self> {
    let chain = options.chain;

    let url = match options.bitcoin_rpc_url {
      Some(url) if url.contains("://") => url,
      Some(url) => format!("http://{url}"),
      None => format!("http://127.0.0.1:{}", chain.default_rpc_port()),
    };

    let auth = match (options.bitcoin_rpc_username, options.bitcoin_rpc_password) {
      (Some(username), Some(password)) => RpcAuth::UserPass { username, password },
      (Some(_), None) => bail!("--bitcoin-rpc-username needs --bitcoin-rpc-password"),
      (None, Some(_)) => bail!("--bitcoin-rpc-password needs --bitcoin-rpc-username"),
      (None, None) => match options.cookie_file {
        Some(path) => RpcAuth::Cookie(path),
        None => {
          let bitcoin_data_dir = match options.bitcoin_data_dir {
            Some(dir) => dir,
            None => default_bitcoin_data_dir()?,
          };
          RpcAuth::Cookie(chain.join_with_data_dir(&bitcoin_data_dir).join(".cookie"))
        }
      },
    };

    let data_dir = match options.data_dir {
      Some(dir) => dir,
      None => dirs::data_dir()
        .context("could not find the user data directory; pass --data-dir")?
        .join("ord-sats"),
    };
    let data_dir = chain.join_with_data_dir(&data_dir);

    let mut config = TrackerConfig::default();
    if !options.tracked_ranges.is_empty() {
      config.ranges = options
        .tracked_ranges
        .into_iter()
        .map(|range| range.0)
        .collect();
      config.ranges.sort_by_key(|range| range.start);
      for pair in config.ranges.windows(2) {
        ensure!(
          pair[0].end <= pair[1].start,
          "tracked ranges {} and {} overlap",
          pair[0].id,
          pair[1].id
        );
      }
    }
    if let Some(value) = options.extracted_max_value {
      config.extracted_max_value = value;
    }
    if let Some(time) = options.extracted_since {
      config.extracted_since_time = time;
    }
    if let Some(from) = options.address_history_from {
      config.address_history_from = from;
    }

    Ok(Self {
      chain,
      rpc: RpcConfig {
        url,
        auth,
        timeout_secs: options.bitcoin_rpc_timeout,
      },
      index_path: data_dir.join("index.redb"),
      data_dir,
      commit_interval: options.commit_interval,
      fetch_threads: options.fetch_threads.into(),
      fast_sync: options.fast_sync,
      reindex: options.reindex,
      store: StoreOptions {
        cache_size: options.db_cache_mib.saturating_mul(1 << 20),
        moves_retention: options.moves_retention,
        ..StoreOptions::default()
      },
      config,
      height_limit: options.height_limit,
    })
  }
}

fn default_bitcoin_data_dir() -> anyhow::Result<PathBuf> {
  if cfg!(target_os = "linux") {
    Ok(
      dirs::home_dir()
        .context("could not find the home directory; pass --cookie-file")?
        .join(".bitcoin"),
    )
  } else {
    Ok(
      dirs::data_dir()
        .context("could not find the data directory; pass --cookie-file")?
        .join("Bitcoin"),
    )
  }
}

#[cfg(test)]
mod tests {
  use {super::*, clap::Parser};

  #[derive(Parser)]
  struct Test {
    #[command(flatten)]
    options: Options,
  }

  fn parse(args: &[&str]) -> anyhow::Result<Settings> {
    Settings::from_options(
      Test::try_parse_from(std::iter::once("ord").chain(args.iter().copied()))?.options,
    )
  }

  #[test]
  fn defaults() {
    let settings = parse(&["--data-dir", "/data", "--bitcoin-data-dir", "/btc"]).unwrap();
    assert_eq!(settings.chain, Chain::Mainnet);
    assert_eq!(settings.rpc.url, "http://127.0.0.1:8332");
    assert_eq!(settings.rpc.auth, RpcAuth::Cookie("/btc/.cookie".into()));
    assert_eq!(settings.data_dir, Path::new("/data"));
    assert_eq!(settings.index_path, Path::new("/data/index.redb"));
    assert_eq!(settings.commit_interval, 5000);
    assert_eq!(settings.fetch_threads, 16);
    assert_eq!(settings.fast_sync, FastSync::Auto);
    assert!(!settings.reindex);
    assert_eq!(settings.store.moves_retention, MovesRetention::DEFAULT);
    assert_eq!(settings.store.cache_size, 4096 << 20);
    assert_eq!(settings.config, TrackerConfig::default());
  }

  #[test]
  fn chain_specific_paths_and_ports() {
    let settings = parse(&[
      "--chain",
      "regtest",
      "--data-dir",
      "/data",
      "--bitcoin-data-dir",
      "/btc",
    ])
    .unwrap();
    assert_eq!(settings.rpc.url, "http://127.0.0.1:18443");
    assert_eq!(
      settings.rpc.auth,
      RpcAuth::Cookie("/btc/regtest/.cookie".into())
    );
    assert_eq!(settings.index_path, Path::new("/data/regtest/index.redb"));
  }

  #[test]
  fn rpc_auth_and_url() {
    let settings = parse(&[
      "--bitcoin-rpc-url",
      "10.0.0.1:8332",
      "--bitcoin-rpc-username",
      "alice",
      "--bitcoin-rpc-password",
      "secret",
      "--data-dir",
      "/data",
    ])
    .unwrap();
    assert_eq!(settings.rpc.url, "http://10.0.0.1:8332");
    assert_eq!(
      settings.rpc.auth,
      RpcAuth::UserPass {
        username: "alice".into(),
        password: "secret".into()
      }
    );
    assert!(
      parse(&["--bitcoin-rpc-username", "alice", "--data-dir", "/data"])
        .unwrap_err()
        .to_string()
        .contains("--bitcoin-rpc-password")
    );
    let settings = parse(&["--cookie-file", "/c", "--data-dir", "/data"]).unwrap();
    assert_eq!(settings.rpc.auth, RpcAuth::Cookie("/c".into()));
  }

  #[test]
  fn tracker_config_options() {
    let settings = parse(&[
      "--data-dir",
      "/data",
      "--tracked-range",
      "block78:390_000_000_000:395_000_000_000",
      "--tracked-range",
      "block9:45000000000:50000000000",
      "--extracted-max-value",
      "546",
      "--extracted-since",
      "1700000000",
      "--moves-retention",
      "all",
    ])
    .unwrap();
    assert_eq!(
      settings
        .config
        .ranges
        .iter()
        .map(|range| range.id.as_str())
        .collect::<Vec<&str>>(),
      ["block9", "block78"]
    );
    assert_eq!(settings.config.extracted_max_value, 546);
    assert_eq!(settings.config.extracted_since_time, 1_700_000_000);
    assert_eq!(settings.store.moves_retention, MovesRetention::All);

    assert!(settings_err(&["--tracked-range", "x:5:5"]).contains("START < END"));
    assert!(
      parse(&[
        "--data-dir",
        "/data",
        "--tracked-range",
        "a:0:10",
        "--tracked-range",
        "b:5:20"
      ])
      .unwrap_err()
      .to_string()
      .contains("overlap")
    );
    assert!(settings_err(&["--moves-retention", "forever"]).contains("`all`"));
    assert!(settings_err(&["--commit-interval", "0"]).contains("commit-interval"));
  }

  fn settings_err(args: &[&str]) -> String {
    match parse(args) {
      Ok(_) => panic!("{args:?} should fail"),
      Err(err) => err.to_string(),
    }
  }
}
