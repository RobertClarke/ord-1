//! Core-shaped JSON for `getblock` verbosity 1–3 and verbose
//! `getblockheader`.
//!
//! Amounts are JSON numbers in BTC like Core's, but printed the way
//! `serde_json` prints an `f64` (`50.0`, `5.46e-6`) rather than Core's fixed
//! eight decimals; both denote the same exact decimal.

use {super::*, serde_json::json};

impl State {
  pub(crate) fn height_of(&self, hash: &BlockHash) -> Option<usize> {
    self.hashes.iter().position(|candidate| candidate == hash)
  }

  /// `getblock <hash> <verbosity>` for verbosity 1, 2 or 3.
  pub(crate) fn block_json(&self, hash: &BlockHash, verbosity: u64) -> Option<Value> {
    let block = self.blocks.get(hash)?;
    let height = self.height_of(hash)?;

    let mut json = self.header_json(hash)?;
    let object = json.as_object_mut().unwrap();

    object.insert(
      "strippedsize".into(),
      (block
        .txdata
        .iter()
        .map(Transaction::base_size)
        .sum::<usize>()
        + 80
        + 9)
        .into(),
    );
    object.insert("size".into(), block.total_size().into());
    object.insert("weight".into(), block.weight().to_wu().into());

    let txs = block
      .txdata
      .iter()
      .map(|tx| {
        if verbosity == 1 {
          tx.compute_txid().to_string().into()
        } else {
          self.tx_json(tx, verbosity >= 3)
        }
      })
      .collect::<Vec<Value>>();
    object.insert("tx".into(), txs.into());

    debug_assert_eq!(object["height"], json!(height));

    Some(json)
  }

  /// Verbose `getblockheader <hash>`.
  pub(crate) fn header_json(&self, hash: &BlockHash) -> Option<Value> {
    let block = self.blocks.get(hash)?;
    let height = self.height_of(hash)?;
    let tip = self.hashes.len() - 1;
    let header = &block.header;

    let mut times = self.hashes[height.saturating_sub(10)..=height]
      .iter()
      .map(|hash| self.blocks[hash].header.time)
      .collect::<Vec<u32>>();
    times.sort_unstable();

    let mut json = json!({
      "hash": hash.to_string(),
      "confirmations": tip - height + 1,
      "height": height,
      "version": header.version.to_consensus(),
      "versionHex": format!("{:08x}", header.version.to_consensus() as u32),
      "merkleroot": header.merkle_root.to_string(),
      "time": header.time,
      "mediantime": times[times.len() / 2],
      "nonce": header.nonce,
      "bits": format!("{:08x}", header.bits.to_consensus()),
      "difficulty": Some(header.difficulty_float()).filter(|d| d.is_finite()).unwrap_or(0.0),
      "chainwork": format!("{:064x}", height + 1),
      "nTx": block.txdata.len(),
    });

    let object = json.as_object_mut().unwrap();
    if height > 0 {
      object.insert(
        "previousblockhash".into(),
        header.prev_blockhash.to_string().into(),
      );
    }
    if let Some(next) = self.hashes.get(height + 1) {
      object.insert("nextblockhash".into(), next.to_string().into());
    }

    Some(json)
  }

  fn tx_json(&self, tx: &Transaction, prevouts: bool) -> Value {
    let coinbase = tx.is_coinbase();
    let mut input_value = Some(Amount::ZERO);

    let vin = tx
      .input
      .iter()
      .map(|input| {
        let mut json = if coinbase {
          json!({ "coinbase": hex::encode(input.script_sig.as_bytes()) })
        } else {
          json!({
            "txid": input.previous_output.txid.to_string(),
            "vout": input.previous_output.vout,
            "scriptSig": {
              "asm": input.script_sig.to_asm_string(),
              "hex": hex::encode(input.script_sig.as_bytes()),
            },
          })
        };

        let object = json.as_object_mut().unwrap();

        if !input.witness.is_empty() {
          object.insert(
            "txinwitness".into(),
            input
              .witness
              .iter()
              .map(hex::encode)
              .collect::<Vec<String>>()
              .into(),
          );
        }

        if prevouts && !coinbase {
          match self.spent_output(&input.previous_output) {
            Some((output, height, generated)) => {
              input_value = input_value.and_then(|sum| sum.checked_add(output.value));
              object.insert(
                "prevout".into(),
                json!({
                  "generated": generated,
                  "height": height,
                  "value": amount(output.value),
                  "scriptPubKey": self.script_json(&output.script_pubkey),
                }),
              );
            }
            None => input_value = None,
          }
        }

        object.insert("sequence".into(), input.sequence.0.into());

        json
      })
      .collect::<Vec<Value>>();

    let vout = tx
      .output
      .iter()
      .enumerate()
      .map(|(n, output)| {
        json!({
          "value": amount(output.value),
          "n": n,
          "scriptPubKey": self.script_json(&output.script_pubkey),
        })
      })
      .collect::<Vec<Value>>();

    let mut json = json!({
      "txid": tx.compute_txid().to_string(),
      "hash": tx.compute_wtxid().to_string(),
      "version": tx.version.0,
      "size": tx.total_size(),
      "vsize": tx.vsize(),
      "weight": tx.weight().to_wu(),
      "locktime": tx.lock_time.to_consensus_u32(),
      "vin": vin,
      "vout": vout,
    });

    let object = json.as_object_mut().unwrap();

    if prevouts
      && !coinbase
      && let Some(fee) = input_value.and_then(|input| {
        input.checked_sub(tx.output.iter().map(|output| output.value).sum::<Amount>())
      })
    {
      object.insert("fee".into(), amount(fee));
    }

    object.insert("hex".into(), hex::encode(serialize(tx)).into());

    json
  }

  /// The output `outpoint` refers to, the height of the block that created
  /// it and whether that was a coinbase. Spent outputs are served too.
  fn spent_output(&self, outpoint: &OutPoint) -> Option<(TxOut, u32, bool)> {
    let tx = self.transactions.get(&outpoint.txid)?;
    let output = tx.output.get(usize::try_from(outpoint.vout).ok()?)?.clone();
    let height = *self.txid_to_block_height.get(&outpoint.txid)?;
    Some((output, height, tx.is_coinbase()))
  }

  fn script_json(&self, script: &Script) -> Value {
    let mut json = json!({
      "asm": script.to_asm_string(),
      "hex": hex::encode(script.as_bytes()),
      "type": script_type(script),
    });
    if let Ok(address) = Address::from_script(script, self.network) {
      json
        .as_object_mut()
        .unwrap()
        .insert("address".into(), address.to_string().into());
    }
    json
  }
}

fn amount(amount: Amount) -> Value {
  amount.to_btc().into()
}

fn script_type(script: &Script) -> &'static str {
  if script.is_p2pk() {
    "pubkey"
  } else if script.is_p2pkh() {
    "pubkeyhash"
  } else if script.is_p2sh() {
    "scripthash"
  } else if script.is_p2wpkh() {
    "witness_v0_keyhash"
  } else if script.is_p2wsh() {
    "witness_v0_scripthash"
  } else if script.is_p2tr() {
    "witness_v1_taproot"
  } else if script.is_op_return() {
    "nulldata"
  } else if script.is_multisig() {
    "multisig"
  } else if script.is_witness_program() {
    "witness_unknown"
  } else {
    "nonstandard"
  }
}
