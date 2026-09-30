'use strict';

// Follows pieces of sats forward through the chain with bitcoind raw
// blocks, by ordinal theory (DESIGN.md §4): inputs concatenated in order,
// outputs take sats first-in-first-out, the rest is the fee, which goes to
// the coinbase after the subsidy and earlier transactions' fees; what the
// coinbase does not claim is lost.
//
// Used to locate block-9 sats for the baseline (ord's /sat has no satpoint
// for common sats), and as an independent re-implementation to compare
// with ord on early history.

const crypto = require('node:crypto');
const sm = require('./satmath');

function readVarint(buf, pos) {
  const first = buf[pos];
  if (first < 0xfd) return [first, pos + 1];
  if (first === 0xfd) return [buf.readUInt16LE(pos + 1), pos + 3];
  if (first === 0xfe) return [buf.readUInt32LE(pos + 1), pos + 5];
  return [Number(buf.readBigUInt64LE(pos + 1)), pos + 9];
}

// Parses a raw block into transactions: inputs (prevout txid in RPC order,
// vout), outputs (value, script hex) and the byte ranges needed for the txid.
function parseBlock(buf) {
  let pos = 80;
  let count;
  [count, pos] = readVarint(buf, pos);
  const txs = [];
  for (let t = 0; t < count; t++) {
    const start = pos;
    pos += 4; // version
    let witness = false;
    if (buf[pos] === 0 && buf[pos + 1] === 1) {
      witness = true;
      pos += 2;
    }
    const bodyStart = pos;
    let nIn;
    [nIn, pos] = readVarint(buf, pos);
    const inputs = [];
    for (let i = 0; i < nIn; i++) {
      const txid = Buffer.from(buf.subarray(pos, pos + 32)).reverse().toString('hex');
      const vout = buf.readUInt32LE(pos + 32);
      pos += 36;
      let len;
      [len, pos] = readVarint(buf, pos);
      pos += len + 4; // script + sequence
      inputs.push({ txid, vout });
    }
    let nOut;
    [nOut, pos] = readVarint(buf, pos);
    const outputs = [];
    for (let o = 0; o < nOut; o++) {
      const value = Number(buf.readBigUInt64LE(pos));
      pos += 8;
      let len;
      [len, pos] = readVarint(buf, pos);
      outputs.push({ value, script: buf.subarray(pos, pos + len) });
      pos += len;
    }
    const bodyEnd = pos;
    if (witness) {
      for (let i = 0; i < nIn; i++) {
        let items;
        [items, pos] = readVarint(buf, pos);
        for (let k = 0; k < items; k++) {
          let len;
          [len, pos] = readVarint(buf, pos);
          pos += len;
        }
      }
    }
    pos += 4; // locktime
    txs.push({ buf, start, bodyStart, bodyEnd, end: pos, witness, inputs, outputs });
  }
  return txs;
}

function txid(tx) {
  const parts = tx.witness
    ? [tx.buf.subarray(tx.start, tx.start + 4), tx.buf.subarray(tx.bodyStart, tx.bodyEnd), tx.buf.subarray(tx.end - 4, tx.end)]
    : [tx.buf.subarray(tx.start, tx.end)];
  const h1 = crypto.createHash('sha256');
  for (const p of parts) h1.update(p);
  return Buffer.from(crypto.createHash('sha256').update(h1.digest()).digest()).reverse().toString('hex');
}

// Distributes pieces positioned in a stream ({pos, start, len}) over
// consecutive slots [{from, to, key}]: returns [{key, offset, start, len}]
// plus the leftover pieces past the last slot ({pos, start, len}).
function distribute(pieces, slots) {
  const placed = [];
  const rest = [];
  for (const p of pieces) {
    let pos = p.pos;
    let start = p.start;
    let len = p.len;
    for (const s of slots) {
      if (len === 0) break;
      if (pos >= s.to) continue;
      if (pos < s.from) break;
      const take = Math.min(len, s.to - pos);
      placed.push({ key: s.key, offset: pos - s.from, start, len: take });
      pos += take;
      start += take;
      len -= take;
    }
    if (len > 0) rest.push({ pos, start, len });
  }
  return { placed, rest };
}

async function* rawBlocks(rpc, from, to, { batch = 100, maxBatchBytes = 64 << 20 } = {}) {
  let next = null;
  const fetch = async (a, b) => {
    const heights = [];
    for (let h = a; h <= b; h++) heights.push(h);
    const hashes = await rpc.batch(heights.map(h => ['getblockhash', [h]]));
    // Keep each JSON-RPC reply moderate: fewer blocks per request as they grow.
    const raws = await rpc.batch(
      hashes.map(hash => ['getblock', [hash, 0]]),
      Math.max(1, Math.floor(maxBatchBytes / Math.max(1, (a / 1000) ** 2 * 2000 + 2000))),
    );
    return heights.map((h, i) => ({ height: h, hash: hashes[i], hex: raws[i] }));
  };
  for (let a = from; a <= to; a += batch) {
    const b = Math.min(a + batch - 1, to);
    const cur = next ? await next : await fetch(a, b);
    const na = b + 1;
    next = na <= to ? fetch(na, Math.min(na + batch - 1, to)) : null;
    for (const blk of cur) {
      if (typeof blk.hex !== 'string') throw new Error(`getblock ${blk.height}: ${JSON.stringify(blk.hex)}`);
      yield { height: blk.height, hash: blk.hash, buf: Buffer.from(blk.hex, 'hex') };
    }
  }
}

// state: Map outpoint -> { value, pieces: [{offset, start, len}] }.
// Applies blocks from..to; returns { state, lost: [{height, start, len}],
// events: counts }. `blockFees(hash)` (getblock verbosity 3) is only
// called for blocks where tracked sats become fees.
async function trace({ rpc, state, from, to, log = { info() {} }, progressEvery = 50_000 }) {
  const lost = [];
  const stats = { blocks: 0, txs: 0, moves: 0, feeEvents: 0, bytes: 0 };
  // Tracked outpoints keyed by txid for a quick input test.
  const byTxid = new Map();
  const index = op => {
    const [t] = op.split(':');
    byTxid.set(t, (byTxid.get(t) || 0) + 1);
  };
  const unindex = op => {
    const [t] = op.split(':');
    const n = byTxid.get(t) - 1;
    if (n) byTxid.set(t, n);
    else byTxid.delete(t);
  };
  for (const op of state.keys()) index(op);

  const prevoutValue = async (txidHex, vout) => {
    const tx = await rpc.call('getrawtransaction', [txidHex, 1]);
    return Math.round(tx.vout[vout].value * 1e8);
  };

  for await (const blk of rawBlocks(rpc, from, to)) {
    stats.blocks++;
    stats.bytes += blk.buf.length;
    if (stats.blocks % progressEvery === 0) log.info(`  traced to ${blk.height}: ${state.size} outputs`);
    const txs = parseBlock(blk.buf);
    stats.txs += txs.length;
    const feePieces = []; // {pos (in this tx's fee), start, len, tx index}
    for (let t = 1; t < txs.length; t++) {
      const tx = txs[t];
      if (!tx.inputs.some(i => byTxid.has(i.txid) && state.has(`${i.txid}:${i.vout}`))) continue;
      stats.moves++;
      const id = txid(tx);
      // Input stream: every input's value; tracked ones carry pieces.
      let pos = 0;
      const pieces = [];
      for (const input of tx.inputs) {
        const op = `${input.txid}:${input.vout}`;
        const held = state.get(op);
        let value;
        if (held) {
          value = held.value;
          for (const p of held.pieces) pieces.push({ pos: pos + p.offset, start: p.start, len: p.len });
          state.delete(op);
          unindex(op);
        } else value = await prevoutValue(input.txid, input.vout);
        pos += value;
      }
      const inputValue = pos;
      let at = 0;
      const slots = tx.outputs.map((o, vout) => {
        const s = { from: at, to: at + o.value, key: `${id}:${vout}`, value: o.value };
        at += o.value;
        return s;
      });
      const { placed, rest } = distribute(pieces, slots);
      for (const p of placed) {
        if (!state.has(p.key)) {
          state.set(p.key, { value: slots[Number(p.key.split(':')[1])].value, pieces: [] });
          index(p.key);
        }
        state.get(p.key).pieces.push({ offset: p.offset, start: p.start, len: p.len });
      }
      for (const p of rest) feePieces.push({ tx: t, pos: p.pos - at, start: p.start, len: p.len, fee: inputValue - at });
    }
    if (feePieces.length) {
      // Fee sats follow the subsidy and every earlier transaction's fee.
      stats.feeEvents++;
      const full = await rpc.call('getblock', [blk.hash, 3]);
      const sats = btc => Math.round(btc * 1e8);
      const fees = full.tx.map((tx, i) => (i === 0 ? 0 : tx.vin.reduce((a, v) => a + sats(v.prevout.value), 0) - tx.vout.reduce((a, o) => a + sats(o.value), 0)));
      const before = [];
      let acc = sm.subsidy(blk.height);
      for (let i = 0; i < fees.length; i++) {
        before.push(acc);
        acc += fees[i];
      }
      const coinbase = txs[0];
      const cbid = txid(coinbase);
      let cat = 0;
      const slots = coinbase.outputs.map((o, vout) => {
        const s = { from: cat, to: cat + o.value, key: `${cbid}:${vout}`, value: o.value };
        cat += o.value;
        return s;
      });
      const { placed, rest } = distribute(
        feePieces.map(p => ({ pos: before[p.tx] + p.pos, start: p.start, len: p.len })),
        slots,
      );
      for (const p of placed) {
        if (!state.has(p.key)) {
          state.set(p.key, { value: slots[Number(p.key.split(':')[1])].value, pieces: [] });
          index(p.key);
        }
        state.get(p.key).pieces.push({ offset: p.offset, start: p.start, len: p.len });
      }
      for (const p of rest) lost.push({ height: blk.height, start: p.start, len: p.len });
    }
  }
  for (const v of state.values()) v.pieces.sort((a, b) => a.offset - b.offset);
  return { state, lost, stats };
}

module.exports = { parseBlock, txid, distribute, trace, rawBlocks, readVarint };
