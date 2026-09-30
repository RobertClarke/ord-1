'use strict';

const { JsonClient } = require('./http');
const { JsonTokenizer, ObjectBuilder } = require('./jsonstream');
const { NULL_OUTPOINT } = require('./satmath');

// Oracle = upstream ord (0.29-ish) with --index-sats. What it can answer:
// * GET /sat/<n>: `satpoint` only for sats with rarity ≥ uncommon (ord's
//   SAT_TO_SATPOINT) or with an inscription; `null` for every common sat
//   (black, alphas, palindromes, block 9 …). A *lost* uncommon sat makes it
//   answer 500 (it tries to load the null txid to compute the address).
// * GET /output/<outpoint>: `sat_ranges` ([[start, end), …], in offset order,
//   not merged) for unspent outputs; `spent: true, sat_ranges: null` once
//   spent; 404 for unknown outpoints. Consolidation outputs can carry
//   millions of ranges (hundreds of MB), so /output is always streamed.
// * GET /output/<null outpoint>: every lost range in lost order; the sum of
//   lengths equals /status.lost_sats.
class Oracle {
  constructor(base, { concurrency = 16, timeoutMs = 120_000 } = {}) {
    this.http = new JsonClient(base, { concurrency: Math.min(concurrency, 32), timeoutMs, name: 'oracle' });
    this.outputs = new Map();
    this.lost = null;
  }

  get base() {
    return this.http.base;
  }

  async status() {
    return this.http.getJson('/status');
  }

  async height() {
    return (await this.status()).height;
  }

  // { status, body } with body the JSON (200) or the error text.
  // `retry500: 0` for sats known to be lost (a deterministic 500).
  async sat(n, { retry500 = 1 } = {}) {
    const res = await this.http.get(`/sat/${n}`, { retry500 });
    return { status: res.status, body: res.status === 200 ? res.json : res.text };
  }

  // Scans /output/<outpoint> for the offsets of `targets` (see scanOutput).
  // Cached until `resetCache()`; asking for new targets re-scans.
  output(outpoint, targets = []) {
    const cached = this.outputs.get(outpoint);
    if (cached && targets.every(t => cached.targets.has(t))) return cached.promise;
    const all = new Set([...(cached ? cached.targets : []), ...targets]);
    const promise = this.http.request(`/output/${outpoint}`, res => scanOutputResponse(res, [...all]), {
      accept: 'application/json',
    });
    this.outputs.set(outpoint, { targets: all, promise });
    promise.catch(() => {
      if (this.outputs.get(outpoint)?.promise === promise) this.outputs.delete(outpoint);
    });
    return promise;
  }

  resetCache() {
    this.outputs.clear();
    this.lost = null;
  }

  // Lost-sat index built from the null outpoint's ranges.
  lostIndex() {
    if (!this.lost) {
      this.lost = this.http
        .getJson(`/output/${NULL_OUTPOINT}`)
        .then(out => LostIndex.fromRanges(out.sat_ranges || []));
      this.lost.catch(() => (this.lost = null));
    }
    return this.lost;
  }
}

const WINDOW = 5;
const KEEP_RANGES = 200;

function lowerBound(sorted, x) {
  let lo = 0;
  let hi = sorted.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (sorted[mid] < x) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

// Streams an ord /output response. Returns the output's fields plus, for
// each target sat, its offset (null if absent) and the ranges around it:
// { status, outpoint, value, script_pubkey, address, spent, indexed,
//   confirmations, transaction, inscriptions (count), runes (count),
//   sat_ranges: null | { count, total, ranges (≤ 200, else null) },
//   offsets: { sat: offset|null }, windows: { sat: {first_index, offset_before, ranges} } }
async function scanOutputResponse(res, targets) {
  if (res.status === 404) {
    await res.body?.cancel().catch(() => {});
    return { status: 404 };
  }
  if (res.status !== 200) {
    const text = await res.text().catch(() => '');
    throw new Error(`oracle /output -> ${res.status}: ${text.slice(0, 200)}`);
  }
  const sorted = [...new Set(targets)].sort((a, b) => a - b);
  const offsets = {};
  const windows = {};
  for (const t of sorted) offsets[t] = null;
  let count = 0;
  let offset = 0;
  let kept = [];
  const ring = [];
  const pending = [];
  const onItem = pair => {
    const [start, end] = pair;
    for (let p = pending.length - 1; p >= 0; p--) {
      pending[p].window.ranges.push([start, end]);
      if (--pending[p].remaining === 0) pending.splice(p, 1);
    }
    for (let k = lowerBound(sorted, start); k < sorted.length && sorted[k] < end; k++) {
      const sat = sorted[k];
      if (offsets[sat] !== null) continue;
      offsets[sat] = offset + (sat - start);
      const window = {
        first_index: count - ring.length,
        offset_before: ring.length ? ring[0][2] : offset,
        ranges: [...ring.map(r => [r[0], r[1]]), [start, end]],
      };
      windows[sat] = window;
      pending.push({ window, remaining: WINDOW });
    }
    ring.push([start, end, offset]);
    if (ring.length > WINDOW) ring.shift();
    if (kept) {
      kept.push([start, end]);
      if (kept.length > KEEP_RANGES) kept = null;
    }
    offset += end - start;
    count++;
  };
  const builder = new ObjectBuilder({ special: { sat_ranges: { onItem } } });
  const tokenizer = new JsonTokenizer(builder);
  const decoder = new TextDecoder();
  for await (const chunk of res.body) tokenizer.write(decoder.decode(chunk, { stream: true }));
  tokenizer.write(decoder.decode());
  tokenizer.end();
  const doc = builder.root || {};
  const hasRanges = doc.sat_ranges && doc.sat_ranges.$special === 'sat_ranges';
  return {
    status: 200,
    outpoint: doc.outpoint,
    value: doc.value,
    script_pubkey: doc.script_pubkey,
    address: doc.address ?? null,
    spent: doc.spent,
    indexed: doc.indexed,
    confirmations: doc.confirmations,
    transaction: doc.transaction,
    inscriptions: Array.isArray(doc.inscriptions) ? doc.inscriptions.length : null,
    runes: doc.runes && typeof doc.runes === 'object' ? Object.keys(doc.runes).length : null,
    sat_ranges: hasRanges ? { count, total: offset, ranges: kept } : null,
    offsets,
    windows,
  };
}

// Offset of `sat` in an output given its sat ranges, or null.
function offsetInRanges(satRanges, sat) {
  if (!Array.isArray(satRanges)) return null;
  let offset = 0;
  for (const [start, end] of satRanges) {
    if (sat >= start && sat < end) return offset + (sat - start);
    offset += end - start;
  }
  return null;
}

class LostIndex {
  constructor(sorted, total, count) {
    this.sorted = sorted; // [start, end, lostOffset] sorted by start
    this.total = total;
    this.count = count;
  }

  static fromRanges(ranges) {
    let offset = 0;
    const sorted = [];
    for (const [start, end] of ranges) {
      sorted.push([start, end, offset]);
      offset += end - start;
    }
    sorted.sort((a, b) => a[0] - b[0]);
    return new LostIndex(sorted, offset, ranges.length);
  }

  // Lost offset of `sat`, or null when it is not lost.
  offset(sat) {
    let lo = 0;
    let hi = this.sorted.length - 1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      const [start, end, off] = this.sorted[mid];
      if (sat < start) hi = mid - 1;
      else if (sat >= end) lo = mid + 1;
      else return off + (sat - start);
    }
    return null;
  }

  // Ranges in sat order.
  *ranges() {
    for (const [start, end, offset] of this.sorted) yield { start, end, offset };
  }
}

function parseSatpoint(satpoint) {
  if (typeof satpoint !== 'string') return null;
  const m = /^([0-9a-f]{64}):(\d+):(\d+)$/.exec(satpoint);
  if (!m) return null;
  return { txid: m[1], vout: Number(m[2]), outpoint: `${m[1]}:${m[2]}`, offset: Number(m[3]) };
}

// Core's IsUnspendable: OP_RETURN first, or longer than 10,000 bytes.
function isUnspendableScript(hex) {
  if (typeof hex !== 'string') return false;
  return hex.startsWith('6a') || hex.length / 2 > 10_000;
}

module.exports = { Oracle, LostIndex, offsetInRanges, parseSatpoint, isUnspendableScript, scanOutputResponse };
