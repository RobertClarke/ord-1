use super::*;

mod build_sat_ranges;
mod export;
pub mod info;
mod update;

#[derive(Debug, Parser)]
pub(crate) enum IndexSubcommand {
  #[command(about = "Build the SAT_RANGE_TO_OUTPOINT index from existing UTXO data")]
  BuildSatRanges(build_sat_ranges::BuildSatRanges),
  #[command(about = "Write inscription numbers and ids to a tab-separated file")]
  Export(export::Export),
  #[command(about = "Print index statistics")]
  Info(info::Info),
  #[command(about = "Update the index", alias = "run")]
  Update,
}

impl IndexSubcommand {
  pub(crate) fn run(self, settings: Settings) -> SubcommandResult {
    match self {
      Self::BuildSatRanges(build_sat_ranges) => build_sat_ranges.run(settings),
      Self::Export(export) => export.run(settings),
      Self::Info(info) => info.run(settings),
      Self::Update => update::run(settings),
    }
  }
}
