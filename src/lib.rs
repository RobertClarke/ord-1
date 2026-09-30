//! ord, rare-sat tracker fork. See docs/DESIGN.md.

pub mod indexer;
pub mod model;
pub mod sats;
pub mod server;
pub mod settings;
pub mod source;
pub mod store;
pub mod tracker;

pub fn main() {
  env_logger::init();
  eprintln!("ord rare-sat tracker: not wired up yet");
  std::process::exit(1);
}
