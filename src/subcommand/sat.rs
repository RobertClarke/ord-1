use {
  super::*,
  crate::{
    model::RowId,
    server::{catalog::Catalog, json::Render},
  },
};

fn parse_sat(sat: &str) -> Result<u64, String> {
  let sat = sat
    .replace('_', "")
    .parse::<u64>()
    .map_err(|err| format!("invalid sat `{sat}`: {err}"))?;
  if sat >= ordinals::Sat::SUPPLY {
    return Err(format!(
      "sat {sat} is beyond the last sat {}",
      ordinals::Sat::LAST.n()
    ));
  }
  Ok(sat)
}

#[derive(Debug, clap::Args)]
pub struct Find {
  /// Sat number
  #[arg(value_parser = parse_sat)]
  pub sat: u64,
}

impl Find {
  pub fn run(self, settings: Settings) -> anyhow::Result<()> {
    if !settings.index_path.exists() {
      anyhow::bail!(
        "there is no index at {}; run `ord index update` or `ord server`",
        settings.index_path.display()
      );
    }
    let store = open_store(&settings, false)?;
    let catalog = catalog_view(catalog(&settings)?);
    let reader = store.reader()?;
    let meta = reader
      .meta()?
      .context("the index is empty; run `ord index update`")?;
    meta.check_compatible(&crate::indexer::expected_meta(
      settings.chain,
      &settings.config,
      catalog.fingerprint(),
    ))?;
    let render = Render::new(
      &*catalog,
      &reader,
      settings.chain.network(),
      &settings.config,
    );
    print_json(&render.sat(self.sat)?)
  }
}

#[derive(Debug, clap::Args)]
pub struct Types {
  /// Sat number
  #[arg(value_parser = parse_sat)]
  pub sat: u64,
}

#[derive(Serialize)]
struct TypesOutput {
  sat: u64,
  height: u32,
  types: Vec<&'static str>,
  tracked: bool,
  rows: Vec<String>,
}

impl Types {
  pub fn run(self, settings: Settings) -> anyhow::Result<()> {
    let catalog = catalog_view(catalog(&settings)?);
    let tracked = catalog.is_tracked(self.sat);
    print_json(&TypesOutput {
      sat: self.sat,
      height: ordinals::Sat(self.sat).height().n(),
      types: catalog.types(self.sat),
      tracked,
      rows: if tracked {
        catalog
          .row_counts(self.sat, 1)
          .into_iter()
          .map(|(row, _)| catalog.rows()[usize::from(row)].id.clone())
          .collect()
      } else {
        Vec::new()
      },
    })
  }
}

#[derive(Serialize)]
struct RowOutput<'a> {
  #[serde(flatten)]
  row: &'a crate::server::catalog::RowInfo,
  total: u64,
}

pub(super) fn rows(settings: Settings) -> anyhow::Result<()> {
  let catalog = catalog_view(catalog(&settings)?);
  print_json(
    &catalog
      .rows()
      .iter()
      .enumerate()
      .map(|(row, info)| RowOutput {
        row: info,
        total: catalog.row_total(row as RowId),
      })
      .collect::<Vec<_>>(),
  )
}
