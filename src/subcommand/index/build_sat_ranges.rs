use super::*;

#[derive(Debug, Parser)]
pub(crate) struct BuildSatRanges {
  #[arg(long, help = "Commit to database every N UTXOs processed [default: 100000]")]
  commit_interval: Option<u64>,
  #[arg(long, help = "Force rebuild even if index already has data")]
  force: bool,
}

impl BuildSatRanges {
  pub(crate) fn run(self, settings: Settings) -> SubcommandResult {
    let index = Index::open(&settings)?;

    if !index.has_sat_index() {
      bail!("--index-sats is required to build sat range index");
    }

    if !index.has_sat_range_index() {
      bail!("--index-sat-ranges is required to build sat range index");
    }

    let commit_interval = self.commit_interval.unwrap_or(100_000);

    index.build_sat_range_index(commit_interval, self.force)?;

    Ok(None)
  }
}
