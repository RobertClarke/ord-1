//! ord, rare-sat tracker fork. See docs/DESIGN.md.

pub mod indexer;
pub mod model;
pub mod sats;
pub mod server;
pub mod settings;
pub mod source;
pub mod store;
pub mod subcommand;
pub mod tracker;

pub fn main() {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("ord=info")).init();

  let arguments = <subcommand::Arguments as clap::Parser>::parse();

  if let Err(err) = arguments.run() {
    eprintln!("error: {err}");
    for cause in err.chain().skip(1) {
      eprintln!("because: {cause}");
    }
    if std::env::var_os("RUST_BACKTRACE").is_some() {
      eprintln!("{}", err.backtrace());
    }
    std::process::exit(1);
  }
}
