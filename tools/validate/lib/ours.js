'use strict';

const { JsonClient } = require('./http');

// Client for the fork's API (docs/API.md).
class Ours {
  constructor(base, { concurrency = 32, timeoutMs = 120_000 } = {}) {
    this.http = new JsonClient(base, { concurrency, timeoutMs, name: 'ours' });
  }

  get base() {
    return this.http.base;
  }

  status() {
    return this.http.getJson('/status');
  }

  rows() {
    return this.http.getJson('/rows');
  }

  supply() {
    return this.http.getJson('/supply');
  }

  supplyRow(row, top = 100) {
    return this.http.getJson(`/supply/${encodeURIComponent(row)}?top=${top}`);
  }

  // { status, body } (404 means untracked or unknown).
  async sat(n) {
    const res = await this.http.get(`/sat/${n}`, { retry500: 2 });
    return { status: res.status, body: res.json ?? res.text };
  }

  // Output JSON (with every piece), or null when the output holds no
  // tracked sat (404).
  async output(outpoint) {
    const res = await this.http.get(`/output/${outpoint}`);
    if (res.status === 404) return null;
    if (res.status !== 200 || !res.json) throw new Error(`ours /output/${outpoint} -> ${res.status}: ${String(res.text).slice(0, 200)}`);
    return res.json;
  }

  async blockMoves(height) {
    const res = await this.http.get(`/block/${height}/moves`);
    return res.status === 200 ? res.json : null;
  }

  // NDJSON: one line per piece of the row, in sat order. `pieces: true`
  // asks for each location's output with its pieces (every line repeats
  // them, so only for rows of small outputs).
  sats(row, onLine, { from, to, pieces = false } = {}) {
    const q = new URLSearchParams({ row });
    if (from !== undefined) q.set('from', String(from));
    if (to !== undefined) q.set('to', String(to));
    if (pieces) q.set('pieces', 'true');
    return this.http.stream(`/sats?${q}`, onLine);
  }

  // NDJSON: one line per move, oldest first; to - from ≤ 10,000.
  moves(from, to, onLine, { row } = {}) {
    const q = new URLSearchParams({ from: String(from), to: String(to) });
    if (row) q.set('row', row);
    return this.http.stream(`/moves?${q}`, onLine);
  }
}

module.exports = { Ours };
