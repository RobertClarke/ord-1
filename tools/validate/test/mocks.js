'use strict';

// Mock servers for the self-tests, built from baseline entries:
// * an oracle (ord --index-sats JSON API) that answers /sat with the
//   recorded answers and synthesizes /output ranges that put every recorded
//   sat at its recorded offset;
// * "ours" (docs/API.md) that reports the recorded locations, with hooks
//   to mutate answers;
// * a bitcoind JSON-RPC (gettxout / getblockheader / getblockcount).

const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const sm = require('../lib/satmath');
const { readJsonl } = require('../lib/util');

function startServer(handler, { port = 0, host = '127.0.0.1' } = {}) {
  return new Promise(resolve => {
    const server = http.createServer((req, res) => {
      Promise.resolve(handler(req, res)).catch(err => {
        res.writeHead(500, { 'content-type': 'text/plain' });
        res.end(String(err.stack || err));
      });
    });
    server.keepAliveTimeout = 1000;
    server.listen(port, host, () => {
      const address = server.address();
      resolve({
        url: `http://${host}:${address.port}`,
        close: () =>
          new Promise(done => {
            server.closeAllConnections?.();
            server.close(() => done());
          }),
      });
    });
  });
}

function sendJson(res, status, body) {
  res.writeHead(status, { 'content-type': 'application/json' });
  res.end(JSON.stringify(body));
}

function sendText(res, status, text) {
  res.writeHead(status, { 'content-type': 'text/plain' });
  res.end(text);
}

const BASELINE_DIR = path.join(__dirname, '..', 'baseline');

function loadBaseline(dir = BASELINE_DIR) {
  const meta = JSON.parse(fs.readFileSync(path.join(dir, 'meta.json'), 'utf8'));
  const entries = fs
    .readdirSync(dir)
    .filter(f => f.endsWith('.jsonl'))
    .sort()
    .flatMap(f => readJsonl(path.join(dir, f)));
  return { meta, entries };
}

// Unmined sats, far above anything a baseline can hold: filler material for
// synthesized ranges.
class Filler {
  constructor(start = 2_050_000_000_000_000) {
    this.next = start;
  }

  take(len) {
    const r = [this.next, this.next + len];
    this.next += len + 7;
    return r;
  }
}

// [[start, end), …] placing each {sat, offset} at its offset in an output
// of `value` sats.
function synthesizeRanges(placed, value, filler) {
  const sorted = [...placed].sort((a, b) => a.offset - b.offset);
  const ranges = [];
  let at = 0;
  for (const { sat, offset } of sorted) {
    if (offset < at) continue; // duplicate offset
    if (offset > at) ranges.push(filler.take(offset - at));
    ranges.push([sat, sat + 1]);
    at = offset + 1;
  }
  if (value > at) ranges.push(filler.take(value - at));
  return ranges;
}

function mockOracle({ entries, height, lostSats }) {
  const filler = new Filler();
  const bySat = new Map();
  const outputs = new Map(); // outpoint -> { placed, value, script_pubkey, address }
  const lostPlaced = [];
  for (const e of entries) {
    bySat.set(e.sat, e);
    const loc = e.location;
    if (!loc) continue;
    if (loc.kind === 'lost') {
      lostPlaced.push({ sat: e.sat, offset: loc.offset });
      continue;
    }
    if (!outputs.has(loc.outpoint)) {
      outputs.set(loc.outpoint, { placed: [], value: 0, script_pubkey: loc.script_pubkey ?? '51', address: loc.address ?? null });
    }
    const o = outputs.get(loc.outpoint);
    o.placed.push({ sat: e.sat, offset: loc.offset });
    o.value = Math.max(o.value, loc.value ?? 0, loc.offset + 1);
  }
  const lostTotal = Math.max(lostSats || 0, ...lostPlaced.map(p => p.offset + 1));
  const lostRanges = synthesizeRanges(lostPlaced, lostTotal, filler);
  const outputJson = new Map();
  for (const [outpoint, o] of outputs) {
    const unspendable = o.script_pubkey.startsWith('6a');
    outputJson.set(outpoint, {
      address: o.address,
      confirmations: 10,
      indexed: true,
      inscriptions: [],
      outpoint,
      runes: {},
      sat_ranges: synthesizeRanges(o.placed, o.value, filler),
      script_pubkey: o.script_pubkey,
      // ord flags OP_RETURN outputs spent (they are not in the UTXO set).
      spent: unspendable,
      transaction: outpoint.split(':')[0],
      value: o.value,
    });
  }
  const state = { height, requests: 0, statusCalls: 0, onStatus: null };
  const handler = (req, res) => {
    state.requests++;
    const url = new URL(req.url, 'http://x');
    const p = url.pathname;
    if (p === '/status') {
      state.statusCalls++;
      state.onStatus?.(state);
      return sendJson(res, 200, { height: state.height, lost_sats: lostTotal, sat_index: true, chain: 'mainnet' });
    }
    let m = /^\/sat\/(\d+)$/.exec(p);
    if (m) {
      const sat = Number(m[1]);
      const e = bySat.get(sat);
      if (e && e.oracle && e.oracle.status === 500) return sendText(res, 500, 'Internal Server Error');
      const rarity = sm.rarity(sat);
      const satpoint = e?.oracle?.satpoint ?? null;
      return sendJson(res, 200, {
        address: e?.oracle?.address ?? null,
        block: sm.heightOfSat(sat),
        charms: [],
        inscriptions: [],
        number: sat,
        rarity,
        satpoint,
      });
    }
    m = /^\/output\/([0-9a-f]{64}:\d+)$/.exec(p);
    if (m) {
      if (m[1] === sm.NULL_OUTPOINT) {
        return sendJson(res, 200, {
          address: null,
          indexed: true,
          outpoint: sm.NULL_OUTPOINT,
          sat_ranges: lostRanges,
          script_pubkey: '',
          spent: false,
          value: lostTotal,
        });
      }
      const out = outputJson.get(m[1]);
      return out ? sendJson(res, 200, out) : sendText(res, 404, `output ${m[1]} not found`);
    }
    return sendText(res, 404, 'not found');
  };
  return { handler, state, outputJson, lostRanges };
}

// Ours: every located entry is a tracked 1-sat piece in a row named after
// its kind ('block9' and '450x' are range rows). `mutate(sat, body)` may
// rewrite a /sat answer.
// `unlocated`: what to report for tracked entries the baseline could not
// locate: 'untracked' (skip them) or 'fake' (a location in an output
// bitcoind reports spent).
function mockOurs({ entries, height, lostSats, mutate = null, unlocated = 'untracked' }) {
  const tracked = new Map();
  const untracked = new Set();
  const rows = new Map();
  // A sat can be in several sets: answer with its best-located entry.
  const best = new Map();
  for (const e of entries) {
    if (e.expect === 'untracked') {
      untracked.add(e.sat);
      continue;
    }
    const cur = best.get(e.sat);
    if (!cur || (!cur.location && e.location)) best.set(e.sat, e);
  }
  const locationOf = e => {
    const loc = e.location;
    if (e.expect === 'known_diff') {
      // The fork reports sats of the overwritten coinbases as overwritten.
      const bip = sm.BIP30.find(b => sm.heightOfSat(e.sat) === b.height);
      const offset = sm.blockOffset(e.sat);
      return { kind: 'overwritten', outpoint: `${bip.txid}:0`, offset, satpoint: `${bip.txid}:0:${offset}`, output: null };
    }
    if (loc) {
      return {
        kind: loc.kind,
        satpoint: loc.satpoint,
        outpoint: loc.outpoint,
        offset: loc.offset,
        output:
          loc.kind === 'lost'
            ? null
            : {
                outpoint: loc.outpoint,
                value: loc.value,
                address: loc.address ?? null,
                script_pubkey: loc.script_pubkey,
                kind: loc.kind === 'burned' ? 'unspendable' : 'spendable',
                created_height: loc.created_height ?? 0,
                created_time: 1_231_006_505,
                extracted: false,
                pieces: [{ offset: loc.offset, start: e.sat, len: 1, types: [e.kind] }],
                rows: { [e.kind]: 1 },
              },
      };
    }
    if (unlocated === 'fake') {
      const outpoint = `${'ff'.repeat(32)}:${e.sat % 1000}`;
      return { kind: 'unspent', satpoint: `${outpoint}:0`, outpoint, offset: 0, output: { outpoint, value: 546, address: null, script_pubkey: '51', kind: 'spendable', created_height: 1, pieces: [] } };
    }
    return null;
  };
  const addToRow = (row, sat) => {
    if (!rows.has(row)) rows.set(row, new Set());
    rows.get(row).add(sat);
  };
  for (const e of entries) {
    if (e.expect === 'untracked') continue;
    if (!tracked.has(e.sat)) {
      const b = best.get(e.sat);
      const location = locationOf(b);
      if (!location) continue;
      tracked.set(e.sat, { sat: e.sat, height: sm.heightOfSat(e.sat), types: [b.kind], tracked: true, rows: [b.kind], location });
    }
    addToRow(e.kind === '450x' ? 'block9' : e.kind, e.sat);
    if (e.kind === '450x') addToRow('450x', e.sat);
  }
  for (const [row, set] of rows) rows.set(row, [...set]);
  for (const list of rows.values()) list.sort((a, b) => a - b);
  const state = { height, lostSats, requests: 0, satRequests: 0, statusCalls: 0, onStatus: null, onSat: null, mutate };
  const handler = (req, res) => {
    state.requests++;
    const url = new URL(req.url, 'http://x');
    const p = url.pathname;
    if (p === '/status') {
      state.statusCalls++;
      state.onStatus?.(state);
      return sendJson(res, 200, {
        chain: 'mainnet',
        height: state.height,
        mode: 'live',
        lost_sats: state.lostSats,
        extracted_max_value: 10_000,
        extracted_since: 1_670_976_000,
      });
    }
    if (p === '/rows') {
      return sendJson(
        res,
        200,
        [...rows.keys()].sort().map(id => ({ id, label: id, all_of: [id], any_of: [], heights: null, range: id === 'block9' || id === '450x', total: rows.get(id).length })),
      );
    }
    if (p === '/sats') {
      const list = rows.get(url.searchParams.get('row'));
      if (!list) return sendJson(res, 404, { error: 'unknown row' });
      res.writeHead(200, { 'content-type': 'application/x-ndjson' });
      for (const sat of list) {
        const t = tracked.get(sat);
        res.write(`${JSON.stringify({ piece: { offset: t.location.offset, start: sat, len: 1, types: t.types }, count: 1, location: t.location })}\n`);
      }
      return res.end();
    }
    if (p === '/moves') {
      res.writeHead(200, { 'content-type': 'application/x-ndjson' });
      return res.end();
    }
    const m = /^\/sat\/(\d+)$/.exec(p);
    if (m) {
      state.satRequests++;
      const sat = Number(m[1]);
      state.onSat?.(sat, state);
      let body = tracked.get(sat) || { sat, height: sm.heightOfSat(sat), types: [], tracked: false, rows: [], location: null };
      if (untracked.has(sat)) body = { sat, height: sm.heightOfSat(sat), types: [], tracked: false, rows: [], location: null };
      if (state.mutate) body = state.mutate(sat, structuredClone(body), state) ?? body;
      return sendJson(res, 200, body);
    }
    return sendJson(res, 404, { error: 'not found' });
  };
  return { handler, state, tracked, rows };
}

// bitcoind: gettxout answers from `utxos` (outpoint -> created height).
function mockBitcoind({ utxos, tip }) {
  const state = { tip, calls: 0 };
  const bestblock = 'bb'.repeat(32);
  const answer = ({ method, params, id }) => {
    state.calls++;
    let result = null;
    if (method === 'getblockcount') result = state.tip;
    else if (method === 'getblockheader') result = { hash: params[0], height: state.tip };
    else if (method === 'gettxout') {
      const created = utxos.get(`${params[0]}:${params[1]}`);
      result = created === undefined ? null : { bestblock, confirmations: state.tip - created + 1, value: 0.0001 };
    } else return { id, result: null, error: { code: -32601, message: 'Method not found' } };
    return { id, result, error: null };
  };
  const handler = async (req, res) => {
    let body = '';
    for await (const chunk of req) body += chunk;
    const parsed = JSON.parse(body);
    sendJson(res, 200, Array.isArray(parsed) ? parsed.map(answer) : answer(parsed));
  };
  return { handler, state };
}

module.exports = { startServer, loadBaseline, mockOracle, mockOurs, mockBitcoind, synthesizeRanges, Filler, BASELINE_DIR };
