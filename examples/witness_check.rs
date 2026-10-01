//! Mainnet validation of the sources' sighash flags and inscription
//! envelopes.
//!
//! ```text
//! # Sighash of the seller's input in Magisat sales (API pages saved as JSON).
//! cargo run --release --example witness_check -- sales magisat-*.json
//! # Envelopes per block against an ord --index-sats oracle.
//! cargo run --release --example witness_check -- envelopes --heights 780000,800000
//! ```
//!
//! Both read bitcoind from `ORD_BITCOIN_RPC_URL` / `_USERNAME` / `_PASSWORD`
//! (or `--cookie-file`); `envelopes` reads the oracle from `ORACLE_ORD_URL`.
//! Every block is decoded twice, like the fast source (`decode_block`) and
//! like the live source (`getblock 3`), and the two must agree.

use {
  anyhow::{Context, bail, ensure},
  bitcoin::{BlockHash, OutPoint, Transaction, Txid, consensus},
  clap::{Parser, Subcommand},
  ord::{
    model::{Envelope, ResolvedBlock, ResolvedTx},
    source::{LiveSource, Rpc, RpcAuth, RpcConfig, decode_block, input_sighash},
  },
  serde::Deserialize,
  serde_json::{Value, json},
  std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    str::FromStr,
    thread,
    time::Duration,
  },
};

#[derive(Parser)]
struct Args {
  #[command(subcommand)]
  command: Command,
  #[arg(long, env = "ORD_BITCOIN_RPC_URL")]
  rpc_url: String,
  #[arg(long, env = "ORD_BITCOIN_RPC_USERNAME")]
  rpc_username: Option<String>,
  #[arg(long, env = "ORD_BITCOIN_RPC_PASSWORD", hide_env_values = true)]
  rpc_password: Option<String>,
  #[arg(long)]
  cookie_file: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
  /// Sighash flag of the input spending each sale's `utxo` in its
  /// `buyerTxId`, by sale type.
  Sales {
    /// Magisat activity API responses (`{count, results}`).
    files: Vec<PathBuf>,
  },
  /// Envelope statistics over a height range (fast path only), listing
  /// envelopes ord would unbind for an unrecognized even field.
  Scan {
    #[arg(long)]
    from: u32,
    #[arg(long)]
    to: u32,
    #[arg(long, default_value_t = 1)]
    step: u32,
    #[arg(long, default_value_t = 8)]
    threads: usize,
  },
  /// Our envelopes per transaction vs the oracle's inscriptions per block.
  Envelopes {
    #[arg(long, value_delimiter = ',', required = true)]
    heights: Vec<u32>,
    #[arg(long, env = "ORACLE_ORD_URL")]
    oracle: String,
    /// Pause between oracle requests.
    #[arg(long, default_value_t = 150)]
    delay_ms: u64,
    /// Per block, check where up to this many inscriptions landed (those
    /// with pointers first) against the oracle's satpoints.
    #[arg(long, default_value_t = 0)]
    locations: usize,
  },
}

fn main() -> anyhow::Result<()> {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
  let args = Args::parse();

  let auth = match (&args.cookie_file, &args.rpc_username, &args.rpc_password) {
    (Some(path), _, _) => RpcAuth::Cookie(path.clone()),
    (None, Some(username), Some(password)) => RpcAuth::UserPass {
      username: username.clone(),
      password: password.clone(),
    },
    (None, None, None) => RpcAuth::None,
    _ => bail!("give both ORD_BITCOIN_RPC_USERNAME and ORD_BITCOIN_RPC_PASSWORD, or --cookie-file"),
  };
  let rpc = Rpc::new(RpcConfig {
    url: args.rpc_url.clone(),
    auth,
    timeout_secs: 120,
  })?;

  match args.command {
    Command::Sales { files } => sales(&rpc, &files),
    Command::Scan {
      from,
      to,
      step,
      threads,
    } => scan(&rpc, from, to, step, threads),
    Command::Envelopes {
      heights,
      oracle,
      delay_ms,
      locations,
    } => envelopes(
      &rpc,
      &heights,
      &oracle,
      Duration::from_millis(delay_ms),
      locations,
    ),
  }
}

#[derive(Deserialize)]
struct Page {
  results: Vec<Activity>,
}

#[derive(Deserialize)]
struct Activity {
  #[serde(rename = "type")]
  kind: String,
  data: ActivityData,
}

#[derive(Deserialize)]
struct ActivityData {
  snapshot: Snapshot,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
  buyer_tx_id: Option<String>,
  utxo: String,
  signature_index: Option<u32>,
}

struct Sale {
  kind: String,
  txid: Txid,
  utxo: OutPoint,
  signature_index: Option<u32>,
}

fn flag(sighash: Option<u8>) -> String {
  match sighash {
    Some(0x00) => "0x00 DEFAULT".into(),
    Some(0x01) => "0x01 ALL".into(),
    Some(0x02) => "0x02 NONE".into(),
    Some(0x03) => "0x03 SINGLE".into(),
    Some(0x81) => "0x81 ALL|ANYONECANPAY".into(),
    Some(0x82) => "0x82 NONE|ANYONECANPAY".into(),
    Some(0x83) => "0x83 SINGLE|ANYONECANPAY".into(),
    Some(other) => format!("{other:#04x}"),
    None => "none".into(),
  }
}

fn sales(rpc: &Rpc, files: &[PathBuf]) -> anyhow::Result<()> {
  let mut sales = Vec::new();
  let mut seen = BTreeSet::new();
  for file in files {
    let page = serde_json::from_slice::<Page>(&fs::read(file)?)
      .with_context(|| format!("parsing {}", file.display()))?;
    for activity in page.results {
      let snapshot = activity.data.snapshot;
      let Some(txid) = snapshot.buyer_tx_id else {
        continue;
      };
      let sale = Sale {
        kind: activity.kind,
        txid: Txid::from_str(&txid)?,
        utxo: OutPoint::from_str(&snapshot.utxo)?,
        signature_index: snapshot.signature_index,
      };
      if seen.insert((sale.txid, sale.utxo)) {
        sales.push(sale);
      }
    }
  }
  println!("{} distinct sales", sales.len());

  let live = LiveSource::new(rpc.clone());
  let mut blocks = BTreeMap::<BlockHash, ResolvedBlock>::new();

  // (type, flag) → count; (type, spent output type, flag) → count.
  let mut by_flag = BTreeMap::<(String, String), u64>::new();
  let mut by_output = BTreeMap::<(String, String, String), u64>::new();
  let mut signature_index = BTreeMap::<(String, bool), u64>::new();
  let mut missing = 0;

  for sale in &sales {
    let tx = rpc.call::<Value>(
      "getrawtransaction",
      &[sale.txid.to_string().into(), 2.into()],
    )?;
    let Some(block_hash) = tx["blockhash"].as_str() else {
      missing += 1;
      println!("{}: not confirmed", sale.txid);
      continue;
    };
    let block_hash = BlockHash::from_str(block_hash)?;
    let raw = ord_hex(tx["hex"].as_str().context("no hex")?)?;
    let transaction = consensus::deserialize::<Transaction>(&raw)?;
    ensure!(transaction.compute_txid() == sale.txid);

    let Some(vin) = transaction
      .input
      .iter()
      .position(|input| input.previous_output == sale.utxo)
    else {
      missing += 1;
      println!("{}: does not spend {}", sale.txid, sale.utxo);
      continue;
    };
    let input = &transaction.input[vin];
    let sighash = input_sighash(&input.script_sig, &input.witness);

    // The same flag through the live source.
    if let std::collections::btree_map::Entry::Vacant(entry) = blocks.entry(block_hash) {
      let height = rpc.call::<Value>("getblockheader", &[block_hash.to_string().into()])?["height"]
        .as_u64()
        .context("no height")? as u32;
      entry.insert(live.block(block_hash, height)?);
    }
    let live_tx = blocks[&block_hash]
      .txs
      .iter()
      .find(|tx| tx.txid == sale.txid)
      .context("transaction not in its block")?;
    ensure!(
      live_tx.inputs[vin].sighash == sighash,
      "{}:{vin}: live source says {:?}, raw transaction {:?}",
      sale.txid,
      live_tx.inputs[vin].sighash,
      sighash
    );

    let output_type = tx["vin"][vin]["prevout"]["scriptPubKey"]["type"]
      .as_str()
      .unwrap_or("?")
      .to_string();

    *by_flag
      .entry((sale.kind.clone(), flag(sighash)))
      .or_default() += 1;
    *by_output
      .entry((sale.kind.clone(), output_type, flag(sighash)))
      .or_default() += 1;
    *signature_index
      .entry((sale.kind.clone(), sale.signature_index == Some(vin as u32)))
      .or_default() += 1;
  }

  println!("\nsighash of the input spending `utxo`, by sale type:");
  for ((kind, flag), count) in &by_flag {
    println!("  {kind:<16} {flag:<26} {count:>5}");
  }
  println!("\nby spent output type:");
  for ((kind, output, flag), count) in &by_output {
    println!("  {kind:<16} {output:<22} {flag:<26} {count:>5}");
  }
  println!("\nsignatureIndex equals the input index:");
  for ((kind, equal), count) in &signature_index {
    println!("  {kind:<16} {equal:<6} {count:>5}");
  }
  println!(
    "\n{missing} sales skipped; {} blocks cross-checked against the live source",
    blocks.len()
  );
  Ok(())
}

fn scan(rpc: &Rpc, from: u32, to: u32, step: u32, threads: usize) -> anyhow::Result<()> {
  let heights = (from..=to)
    .step_by(step.max(1) as usize)
    .collect::<Vec<u32>>();
  let next = std::sync::atomic::AtomicUsize::new(0);
  let results = std::sync::Mutex::new(Vec::new());

  thread::scope(|scope| -> anyhow::Result<()> {
    let workers = (0..threads.max(1))
      .map(|_| {
        scope.spawn(|| -> anyhow::Result<()> {
          loop {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(&height) = heights.get(i) else {
              return Ok(());
            };
            let hash = rpc.block_hash(height)?;
            let block = decode_block(height, hash, &rpc.block_raw(hash)?, true)?;
            let mut stats = [0u64; 4];
            let mut flagged = Vec::new();
            for tx in &block.txs {
              let mut index = 0;
              for (vin, input) in tx.inputs.iter().enumerate() {
                for envelope in &input.envelopes {
                  stats[0] += 1;
                  stats[1] += u64::from(envelope.pointer.is_some());
                  stats[2] += u64::from(envelope.unrecognized_even_field);
                  stats[3] += u64::from(vin > 0);
                  if envelope.unrecognized_even_field {
                    flagged.push(format!("{height} {}i{index} vin {vin}", tx.txid));
                  }
                  index += 1;
                }
              }
            }
            results.lock().unwrap().push((height, stats, flagged));
          }
        })
      })
      .collect::<Vec<_>>();
    for worker in workers {
      worker.join().unwrap()?;
    }
    Ok(())
  })?;

  let mut results = results.into_inner().unwrap();
  results.sort_by_key(|(height, ..)| *height);
  let mut totals = [0u64; 4];
  for (_, stats, flagged) in &results {
    for (total, stat) in totals.iter_mut().zip(stats) {
      *total += stat;
    }
    for line in flagged {
      println!("unrecognized even field: {line}");
    }
  }
  println!(
    "{} blocks {from}..={to} step {step}: {} envelopes, {} with pointers, {} with unrecognized even fields, {} past the first input",
    results.len(),
    totals[0],
    totals[1],
    totals[2],
    totals[3]
  );
  Ok(())
}

fn ord_hex(text: &str) -> anyhow::Result<Vec<u8>> {
  Ok(hex::decode(text)?)
}

/// `txid i index` ids of the inscriptions created in a block, per the oracle.
fn oracle_inscriptions(
  client: &reqwest::blocking::Client,
  oracle: &str,
  height: u32,
  delay: Duration,
) -> anyhow::Result<Vec<(Txid, u32)>> {
  #[derive(Deserialize)]
  struct Inscriptions {
    ids: Vec<String>,
    more: bool,
  }

  let mut ids = Vec::new();
  for page in 0.. {
    let url = format!("{oracle}/inscriptions/block/{height}/{page}");
    let response = client
      .get(&url)
      .header("Accept", "application/json")
      .send()?
      .error_for_status()
      .with_context(|| url.clone())?
      .json::<Inscriptions>()?;
    for id in response.ids {
      let (txid, index) = id.split_once('i').context("malformed inscription id")?;
      ids.push((Txid::from_str(txid)?, index.parse()?));
    }
    thread::sleep(delay);
    if !response.more {
      break;
    }
  }
  Ok(ids)
}

/// Where ord puts an envelope revealed by input `vin` of `tx`: the input-sat
/// offset is the pointer when it is inside the outputs, else the input's
/// first sat; `(vout, offset)` of that sat, `None` when it is a fee sat.
fn expected_location(tx: &ResolvedTx, vin: usize, pointer: Option<u64>) -> Option<(u32, u64)> {
  let total_output = tx
    .outputs
    .iter()
    .map(|output| output.value.to_sat())
    .sum::<u64>();
  let input_start = tx.inputs[..vin]
    .iter()
    .map(|input| input.value)
    .sum::<u64>();
  let offset = pointer
    .filter(|&pointer| pointer < total_output)
    .unwrap_or(input_start);
  let mut start = 0;
  for (vout, output) in tx.outputs.iter().enumerate() {
    let end = start + output.value.to_sat();
    if offset < end {
      return Some((vout as u32, offset - start));
    }
    start = end;
  }
  None
}

#[derive(Default)]
struct Locations {
  checked: u64,
  checked_with_pointer: u64,
  moved: u64,
  /// Inscriptions whose unbound charm was compared, and how many were
  /// unbound.
  unbound_checked: u64,
  unbound: u64,
  /// Unbound for an unrecognized even field, for a 0-value input.
  unbound_even_field: u64,
  unbound_zero_value: u64,
  wrong: Vec<String>,
}

fn check_locations(
  client: &reqwest::blocking::Client,
  oracle: &str,
  block: &ResolvedBlock,
  limit: usize,
  delay: Duration,
  locations: &mut Locations,
) -> anyhow::Result<()> {
  // (txid, inscription index, tx, vin, envelope): ones ord unbinds first,
  // then pointers, then envelopes past the first input.
  let mut candidates = Vec::new();
  for tx in &block.txs {
    let mut index = 0;
    for (vin, input) in tx.inputs.iter().enumerate() {
      for envelope in &input.envelopes {
        candidates.push((tx.txid, index, tx, vin, *envelope));
        index += 1;
      }
    }
  }
  candidates.sort_by_key(|(_, _, tx, vin, envelope)| {
    (
      !(envelope.unrecognized_even_field || tx.inputs[*vin].value == 0),
      envelope.pointer.is_none(),
      *vin == 0,
    )
  });

  for (txid, index, tx, vin, envelope) in candidates.into_iter().take(limit) {
    let pointer = envelope.pointer;
    let url = format!("{oracle}/inscription/{txid}i{index}");
    let inscription = client
      .get(&url)
      .header("Accept", "application/json")
      .send()?
      .error_for_status()
      .with_context(|| url.clone())?
      .json::<Value>()?;
    thread::sleep(delay);

    // ord unbinds exactly when the envelope has an unrecognized even field
    // or the input spends a 0-value output.
    let unbound = inscription["charms"]
      .as_array()
      .context("no charms")?
      .iter()
      .any(|charm| charm == "unbound");
    let expected_unbound = envelope.unrecognized_even_field || tx.inputs[vin].value == 0;
    locations.unbound_checked += 1;
    locations.unbound += u64::from(unbound);
    if unbound {
      locations.unbound_even_field += u64::from(envelope.unrecognized_even_field);
      locations.unbound_zero_value += u64::from(tx.inputs[vin].value == 0);
    }
    if unbound != expected_unbound {
      locations.wrong.push(format!(
        "{txid}i{index} (input {vin}, value {}, {envelope:?}): oracle unbound {unbound}",
        tx.inputs[vin].value
      ));
    }
    if unbound {
      continue;
    }

    let satpoint = inscription["satpoint"].as_str().context("no satpoint")?;
    let mut parts = satpoint.rsplitn(3, ':');
    let offset = parts.next().context("satpoint")?.parse::<u64>()?;
    let vout = parts.next().context("satpoint")?.parse::<u32>()?;
    let satpoint_txid = Txid::from_str(parts.next().context("satpoint")?)?;

    // Only inscriptions still where they were revealed say where they
    // landed (others moved on, went to fees or are unbound).
    if satpoint_txid != txid {
      locations.moved += 1;
      continue;
    }
    locations.checked += 1;
    locations.checked_with_pointer += u64::from(pointer.is_some());
    let expected = expected_location(tx, vin, pointer);
    if expected != Some((vout, offset)) {
      locations.wrong.push(format!(
        "{txid}i{index} (input {vin}, pointer {pointer:?}): expected {expected:?}, oracle {vout}:{offset}"
      ));
    }
  }
  Ok(())
}

fn envelopes(
  rpc: &Rpc,
  heights: &[u32],
  oracle: &str,
  delay: Duration,
  location_limit: usize,
) -> anyhow::Result<()> {
  let mut locations = Locations::default();
  let client = reqwest::blocking::Client::builder()
    .no_proxy()
    .timeout(Duration::from_secs(120))
    .build()?;
  let live = LiveSource::new(rpc.clone());

  let mut total_ours = 0;
  let mut total_oracle = 0;
  let mut mismatched_blocks = 0;
  let mut pointers = 0;
  let mut non_first_input = 0;
  let mut multi = 0;

  println!(
    "{:>7} {:>6} {:>7} {:>7} {:>9} {:>6}  status",
    "height", "txs", "ours", "oracle", "pointers", "multi"
  );

  for &height in heights {
    let hash = rpc.block_hash(height)?;
    let raw = rpc.block_raw(hash)?;
    let fast = decode_block(height, hash, &raw, true)?;
    let live_block = live.block(hash, height)?;

    // The fast and live sources agree on every input.
    ensure!(fast.txs.len() == live_block.txs.len());
    for (fast_tx, live_tx) in fast.txs.iter().zip(&live_block.txs) {
      ensure!(fast_tx.txid == live_tx.txid);
      for (vin, (a, b)) in fast_tx.inputs.iter().zip(&live_tx.inputs).enumerate() {
        ensure!(
          (a.previous_output, a.sighash, &a.envelopes)
            == (b.previous_output, b.sighash, &b.envelopes),
          "block {height} {}:{vin}: fast {a:?} live {b:?}",
          fast_tx.txid
        );
      }
    }

    // Ours: envelopes per transaction, numbered like ord's inscription ids
    // (input order, then envelope order).
    let mut ours = BTreeMap::<Txid, u32>::new();
    let mut block_pointers = 0;
    let mut block_multi = 0;
    for tx in &fast.txs {
      let count = tx
        .inputs
        .iter()
        .map(|input| input.envelopes.len() as u32)
        .sum::<u32>();
      if count > 0 {
        ours.insert(tx.txid, count);
      }
      for (vin, input) in tx.inputs.iter().enumerate() {
        block_pointers += input
          .envelopes
          .iter()
          .filter(|envelope: &&Envelope| envelope.pointer.is_some())
          .count();
        if input.envelopes.len() > 1 {
          block_multi += 1;
        }
        if vin > 0 && !input.envelopes.is_empty() {
          non_first_input += 1;
        }
      }
    }

    let ids = oracle_inscriptions(&client, oracle, height, delay)?;
    let mut theirs = BTreeMap::<Txid, Vec<u32>>::new();
    for (txid, index) in &ids {
      theirs.entry(*txid).or_default().push(*index);
    }

    let mut problems = Vec::new();
    for (txid, indices) in &mut theirs {
      indices.sort_unstable();
      let count = indices.len() as u32;
      if *indices != (0..count).collect::<Vec<u32>>() {
        problems.push(format!("{txid}: oracle indices {indices:?}"));
      }
      match ours.get(txid) {
        Some(ours) if *ours == count => {}
        Some(ours) => problems.push(format!("{txid}: ours {ours}, oracle {count}")),
        None => problems.push(format!("{txid}: ours 0, oracle {count}")),
      }
    }
    for (txid, count) in &ours {
      if !theirs.contains_key(txid) {
        problems.push(format!("{txid}: ours {count}, oracle 0"));
      }
    }

    if location_limit > 0 {
      check_locations(
        &client,
        oracle,
        &live_block,
        location_limit,
        delay,
        &mut locations,
      )?;
    }

    let ours_total = ours.values().sum::<u32>();
    total_ours += u64::from(ours_total);
    total_oracle += ids.len() as u64;
    pointers += block_pointers;
    multi += block_multi;
    println!(
      "{height:>7} {:>6} {ours_total:>7} {:>7} {block_pointers:>9} {block_multi:>6}  {}",
      fast.txs.len(),
      ids.len(),
      if problems.is_empty() {
        "ok"
      } else {
        "MISMATCH"
      }
    );
    if !problems.is_empty() {
      mismatched_blocks += 1;
      for problem in problems.iter().take(20) {
        println!("          {problem}");
      }
    }
  }

  println!(
    "\n{} blocks: {total_ours} envelopes (ours), {total_oracle} inscriptions (oracle), {mismatched_blocks} blocks mismatched; {pointers} pointers, {multi} inputs with several envelopes, {non_first_input} envelope-carrying inputs past the first",
    heights.len()
  );
  if location_limit > 0 {
    println!(
      "inscriptions: {} checked for the unbound charm ({} unbound: {} with an unrecognized even field, {} revealed by a 0-value input); {} still at their reveal checked for location ({} with pointers), {} had moved; {} wrong",
      locations.unbound_checked,
      locations.unbound,
      locations.unbound_even_field,
      locations.unbound_zero_value,
      locations.checked,
      locations.checked_with_pointer,
      locations.moved,
      locations.wrong.len(),
    );
    for wrong in &locations.wrong {
      println!("  {wrong}");
    }
  }
  println!(
    "{}",
    json!({
      "blocks": heights.len(),
      "ours": total_ours,
      "oracle": total_oracle,
      "mismatched_blocks": mismatched_blocks,
      "locations_checked": locations.checked,
      "locations_wrong": locations.wrong.len(),
    })
  );
  if mismatched_blocks > 0 || !locations.wrong.is_empty() {
    bail!(
      "{mismatched_blocks} blocks mismatched, {} locations wrong",
      locations.wrong.len()
    );
  }
  Ok(())
}
