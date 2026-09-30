#!/usr/bin/env node
'use strict';

// Recomputes rows' supply counters from the /sats NDJSON listing and each
// located output's pieces (DESIGN.md §5), and compares them with /supply.
//
//   node tools/validate/supply-crosscheck.js --ours=URL [--rows=a,b] [--top=25]
//        [--out=FILE.jsonl] [--summary-json=FILE] [--retries=3]
//        [--fetch-concurrency=16] [--inline-pieces]
//
// Checked per row: mined = located + burned + lost + overwritten, located,
// burned, lost, overwritten, extracted, holders, extracted_holders,
// active_non_extracted (every window), total (vs /rows), each output's
// `extracted` flag and piece list, and the top holders' n / utxos /
// last_received_* (with --top).
//
// Extraction is per UTXO, the same for every row (lib/extracted.js): a
// spendable output created at or after extracted_since whose value is at
// most its budget, Σ over its pieces of (in-range sats + extracted_max_value)
// per contiguous run inside /status.ranges plus extracted_max_value × sats
// outside them. The pieces come from /output/{outpoint}, fetched once per
// outpoint for all rows; --inline-pieces takes them from
// /sats?…&pieces=true instead, which repeats an output's whole piece list
// on every line (quadratic for rows of big outputs).
//
// Exit code: 0 all equal, 1 any difference, 2 errors.

const fs = require('node:fs');
const { Ours } = require('./lib/ours');
const { rowSatBounds } = require('./lib/sampling');
const { rangesFromStatus } = require('./lib/satmath');
const { normalizeRanges, extractionBudget, isExtracted, pieceProblems } = require('./lib/extracted');
const { parseArgs, intOpt, listOpt, makeLogger, mapLimit, JsonlWriter } = require('./lib/util');

const YEAR = 365 * 86_400;

const USAGE = `usage: supply-crosscheck.js --ours=URL [--rows=a,b] [--top=25] [--out=FILE.jsonl] [--summary-json=FILE]
                            [--retries=3] [--fetch-concurrency=16] [--inline-pieces]`;

// Extraction budgets of located outputs, by outpoint, from their pieces.
// An output's pieces never change while it exists, so one entry serves
// every row (and every retry).
class Budgets {
  constructor({ ranges, maxValue, limit = 4_000_000 }) {
    this.ranges = normalizeRanges(ranges);
    this.maxValue = maxValue;
    this.limit = limit;
    this.map = new Map(); // outpoint -> { budget, problems, source }
    this.fetched = 0;
    this.inline = 0;
  }

  get(outpoint) {
    return this.map.get(outpoint);
  }

  set(outpoint, output, source) {
    if (this.map.size >= this.limit) this.map.clear();
    const pieces = output.pieces;
    const entry = {
      budget: extractionBudget(pieces, this.ranges, this.maxValue),
      problems: pieceProblems(pieces, output.value).slice(0, 5),
      source,
    };
    this.map.set(outpoint, entry);
    if (source === 'inline') this.inline++;
    else this.fetched++;
    return entry;
  }

  // Fetches /output for every outpoint without a budget yet. Returns the
  // problems met ([{ problem, outpoint, … }]).
  async fetchMissing(ours, outputs, { concurrency, log }) {
    const missing = [...outputs.keys()].filter(op => !this.map.has(op));
    const problems = [];
    if (!missing.length) return problems;
    if (missing.length > 1000) log.info(`    fetching ${missing.length} outputs' pieces`);
    await mapLimit(missing, concurrency, async op => {
      const out = await ours.output(op);
      const seen = outputs.get(op);
      if (!out) {
        problems.push({ problem: 'output_not_found', outpoint: op, note: '/sats places row sats there but /output has no such tracked output' });
        return;
      }
      if (!Array.isArray(out.pieces)) {
        problems.push({ problem: 'output_without_pieces', outpoint: op });
        return;
      }
      for (const [k, field] of [
        ['value', 'value'],
        ['script', 'script_pubkey'],
        ['height', 'created_height'],
        ['time', 'created_time'],
      ]) {
        if (seen[k] !== undefined && out[field] !== undefined && seen[k] !== out[field]) {
          problems.push({ problem: 'output_differs_from_sats_line', outpoint: op, field, sats_line: seen[k], output: out[field] });
        }
      }
      this.set(op, out, 'fetched');
    });
    return problems;
  }
}

// Folds one row's /sats lines into its counters. Extraction needs each
// output's budget from `budgets` (see Budgets), filled inline or by fetching.
class RowAccumulator {
  constructor({ extractedMaxValue, extractedSince, budgets }) {
    this.maxValue = extractedMaxValue;
    this.since = extractedSince;
    this.budgets = budgets;
    this.byKind = { unspent: 0, burned: 0, lost: 0, overwritten: 0 };
    this.outputs = new Map(); // outpoint -> { n, value, time, height, script, address, kind, flag }
    this.lines = 0;
    this.problems = [];
  }

  problem(p) {
    if (this.problems.length < 20) this.problems.push(p);
  }

  add(line) {
    this.lines++;
    const count = line.count;
    const loc = line.location;
    if (!Number.isSafeInteger(count) || count < 1 || !loc) {
      this.problem({ problem: 'bad_line', line });
      return;
    }
    if (!(loc.kind in this.byKind)) {
      this.problem({ problem: 'unknown_kind', line });
      return;
    }
    this.byKind[loc.kind] += count;
    if (loc.kind !== 'unspent') return;
    const out = loc.output;
    if (!out) {
      this.problem({ problem: 'unspent_without_output', line });
      return;
    }
    if (typeof out.created_time !== 'number') {
      // Without it neither `extracted` nor the activity windows can be
      // recomputed for this output.
      this.problem({ problem: 'output_without_created_time', outpoint: loc.outpoint });
    }
    let o = this.outputs.get(loc.outpoint);
    if (!o) {
      o = { n: 0, value: out.value, time: out.created_time, height: out.created_height, script: out.script_pubkey, address: out.address ?? null, kind: out.kind, flag: out.extracted };
      this.outputs.set(loc.outpoint, o);
      if (out.kind !== undefined && out.kind !== 'spendable') this.problem({ problem: 'located_in_unspendable_output', outpoint: loc.outpoint, kind: out.kind });
      if (Array.isArray(out.pieces) && !this.budgets.get(loc.outpoint)) this.budgets.set(loc.outpoint, out, 'inline');
    }
    o.n += count;
  }

  // Per-UTXO extraction (lib/extracted.js); null when the budget is unknown.
  extracted(outpoint, o) {
    const b = this.budgets.get(outpoint);
    if (!b) return null;
    return isExtracted({ spendable: o.kind === undefined || o.kind === 'spendable', createdTime: o.time, value: o.value, budget: b.budget }, { extractedSince: this.since });
  }

  // Output-level `extracted` flags against the rule, piece-list problems,
  // and outputs whose budget could not be computed.
  outputProblems() {
    const flags = [];
    const pieces = [];
    const unknown = [];
    for (const [outpoint, o] of this.outputs) {
      const b = this.budgets.get(outpoint);
      if (!b) {
        if (unknown.length < 20) unknown.push({ problem: 'budget_unknown', outpoint });
        continue;
      }
      for (const p of b.problems) if (pieces.length < 20) pieces.push({ outpoint, ...p });
      if (o.flag === undefined) continue;
      const expected = this.extracted(outpoint, o);
      if (o.flag !== expected && flags.length < 20) {
        flags.push({ outpoint, value: o.value, created_time: o.time, budget: b.budget, extracted: o.flag, expected });
      }
    }
    return { flags, pieces, unknown };
  }

  result({ tipTime, top }) {
    let extracted = 0;
    const active = { 1: 0, 2: 0, 3: 0, 4: 0, 5: 0, 7: 0 };
    const all = new Set();
    const xall = new Set();
    // Per holder, per list: counts plus the latest output (height, time).
    const holders = new Map();
    const latest = (cur, o) => (!cur || o.height > cur.height ? { height: o.height, time: o.time } : cur);
    for (const [outpoint, o] of this.outputs) {
      const isX = this.extracted(outpoint, o) === true;
      all.add(o.script);
      if (isX) {
        extracted += o.n;
        xall.add(o.script);
      } else {
        for (const years of Object.keys(active)) if (o.time >= tipTime - Number(years) * YEAR) active[years] += o.n;
      }
      if (!top) continue;
      let h = holders.get(o.script);
      if (!h) {
        h = { script_pubkey: o.script, address: o.address, x: { n: 0, utxos: 0, last: null, maxTime: -1 }, nx: { n: 0, utxos: 0, last: null, maxTime: -1 }, any: { n: 0, utxos: 0, last: null, maxTime: -1 } };
        holders.set(o.script, h);
      }
      for (const k of [isX ? 'x' : 'nx', 'any']) {
        h[k].n += o.n;
        h[k].utxos++;
        h[k].last = latest(h[k].last, o);
        h[k].maxTime = Math.max(h[k].maxTime, o.time);
      }
    }
    // The holder lists under every reading of the contract (see
    // compareHolders): n/utxos of the list's own outputs or of all located
    // outputs, last_received from the list's outputs or from all the
    // holder's outputs (DESIGN.md §5 now says: the list's own).
    const lists = { extracted: [], non_extracted: [] };
    if (top) {
      for (const h of holders.values()) {
        if (h.x.n) lists.extracted.push({ script_pubkey: h.script_pubkey, own: h.x, any: h.any });
        if (h.nx.n) lists.non_extracted.push({ script_pubkey: h.script_pubkey, own: h.nx, any: h.any });
      }
    }
    return {
      stats: {
        located: this.byKind.unspent,
        burned: this.byKind.burned,
        lost: this.byKind.lost,
        overwritten: this.byKind.overwritten,
        extracted,
        holders: all.size,
        extracted_holders: xall.size,
      },
      active_non_extracted: Object.fromEntries(Object.entries(active).map(([y, n]) => [`${y}y`, n])),
      lists,
    };
  }
}

// Readings of a holder entry. `count`: the list's own sats/outputs, or all
// the holder's located ones. `last`: the latest of the list's outputs or of
// all the holder's outputs; its time is that output's block time (or, per
// DESIGN.md's wording, the latest creation time).
const READINGS = [];
for (const count of ['own', 'any']) for (const last of ['own', 'any']) for (const time of ['block', 'max']) READINGS.push({ count, last, time });

function expectedEntry(h, reading) {
  const c = h[reading.count];
  const l = h[reading.last];
  return {
    n: c.n,
    utxos: c.utxos,
    last_received_height: l.last.height,
    last_received_time: reading.time === 'block' ? l.last.time : l.maxTime,
  };
}

// Compares a reported top-N list with the recomputed holders under every
// reading and keeps the one that fits best. Holders tied with the N-th are
// interchangeable, so membership is only required strictly above the cut.
function compareHolders(reported, holders, top) {
  if (!Array.isArray(reported)) return { reading: null, diffs: [{ problem: 'missing_list' }] };
  let best = null;
  for (const reading of READINGS) {
    const diffs = [];
    const ranked = holders
      .map(h => ({ script_pubkey: h.script_pubkey, ...expectedEntry(h, reading) }))
      .sort((a, b) => b.n - a.n || a.script_pubkey.localeCompare(b.script_pubkey));
    const byScript = new Map(ranked.map(h => [h.script_pubkey, h]));
    const expectedLen = Math.min(top, ranked.length);
    if (reported.length !== expectedLen) diffs.push({ problem: 'length', reported: reported.length, computed: expectedLen });
    reported.forEach((r, i) => {
      const c = byScript.get(r.script_pubkey);
      if (!c) {
        diffs.push({ problem: 'unknown_holder', index: i, reported: r });
        return;
      }
      for (const k of ['n', 'utxos', 'last_received_height', 'last_received_time']) {
        if (r[k] !== undefined && r[k] !== c[k]) diffs.push({ problem: `holder_${k}`, index: i, script_pubkey: r.script_pubkey, reported: r[k], computed: c[k] });
      }
      if (i > 0 && reported[i - 1].n < r.n) diffs.push({ problem: 'not_sorted', index: i });
    });
    if (ranked.length > top && top > 0) {
      const cut = ranked[top - 1].n;
      const have = new Set(reported.map(h => h.script_pubkey));
      for (const h of ranked) if (h.n > cut && !have.has(h.script_pubkey)) diffs.push({ problem: 'missing_holder', script_pubkey: h.script_pubkey, n: h.n });
    }
    if (!best || diffs.length < best.diffs.length) best = { reading, diffs };
    if (!diffs.length) break;
  }
  return best;
}

async function crosscheckRow(ours, row, { status, top, log, budgets, inlinePieces = false, fetchConcurrency = 16 }) {
  const acc = new RowAccumulator({ extractedMaxValue: status.extracted_max_value, extractedSince: status.extracted_since, budgets });
  // The supply snapshot is published after each commit, so it can lag
  // /sats: read it before and after the stream (and the output fetches)
  // and keep the result only if both snapshots and /status agree on the
  // height (the caller retries).
  const supply = await ours.supply();
  const reported = top ? await ours.supplyRow(row.id, top) : null;
  const rowSupply = reported || supply.rows.find(r => r.id === row.id);
  if (!rowSupply) throw new Error(`/supply has no row ${row.id}`);
  // Every sat of a height-scoped or block-confined row lies in these bounds.
  const bounds = rowSatBounds(row);
  await ours.sats(row.id, line => acc.add(line), { ...(bounds ? { from: bounds[0], to: bounds[1] } : {}), pieces: inlinePieces });
  const fetchProblems = await budgets.fetchMissing(ours, acc.outputs, { concurrency: fetchConcurrency, log });
  const supplyAfter = await ours.supply();
  const heights = { status: status.height, supply: supply.height, supply_after: supplyAfter.height };
  const tipTime = supply.time;
  const computed = acc.result({ tipTime, top });
  const diffs = [];
  const s = rowSupply.stats || {};
  for (const [k, v] of Object.entries(computed.stats)) {
    if (s[k] !== v) diffs.push({ field: k, reported: s[k], computed: v });
  }
  const sum = s.located + s.burned + s.lost + s.overwritten;
  if (s.mined !== sum) diffs.push({ field: 'mined', reported: s.mined, computed: sum, note: 'mined must equal located + burned + lost + overwritten' });
  if (row.total !== undefined && rowSupply.total !== undefined && row.total !== rowSupply.total) {
    diffs.push({ field: 'total', reported: rowSupply.total, computed: row.total, note: '/rows total' });
  }
  if (rowSupply.total !== undefined && s.mined > rowSupply.total) diffs.push({ field: 'mined', reported: s.mined, computed: rowSupply.total, note: 'mined exceeds total' });
  for (const [k, v] of Object.entries(computed.active_non_extracted)) {
    const r = rowSupply.active_non_extracted?.[k];
    if (r !== v) diffs.push({ field: `active_non_extracted.${k}`, reported: r, computed: v });
  }
  const readings = {};
  if (top) {
    for (const [list, key] of [
      ['top_extracted_holders', 'extracted'],
      ['top_non_extracted_holders', 'non_extracted'],
    ]) {
      const { reading, diffs: d } = compareHolders(reported[list], computed.lists[key], top);
      readings[list] = reading;
      for (const x of d) diffs.push({ field: list, reading, ...x });
    }
  }
  const outputs = acc.outputProblems();
  for (const p of outputs.flags) diffs.push({ field: 'output.extracted', ...p });
  for (const p of outputs.pieces) diffs.push({ field: 'output.pieces', ...p });
  for (const p of outputs.unknown) diffs.push({ field: 'extracted', ...p, note: 'no pieces for this output: extracted counts are incomplete' });
  for (const p of fetchProblems.slice(0, 20)) diffs.push({ field: 'output', ...p });
  for (const p of acc.problems) diffs.push({ field: 'sats_line', ...p });
  log.info(`  ${row.id}: ${acc.lines} pieces, ${acc.outputs.size} outputs, ${diffs.length ? `${diffs.length} differences` : 'ok'}`);
  return { row: row.id, heights, lines: acc.lines, outputs: acc.outputs.size, reported: rowSupply.stats, computed: computed.stats, holder_readings: readings, diffs };
}

async function main(argv, io = {}) {
  const stdout = io.stdout || process.stdout;
  const opts = parseArgs(argv, { booleans: ['help', 'quiet', 'inlinePieces'] });
  if (opts.help || !opts.ours) {
    (io.stderr || process.stderr).write(`${USAGE}\n`);
    return opts.help ? 0 : 2;
  }
  const log = io.log || makeLogger({ quiet: opts.quiet });
  const fetchConcurrency = Math.max(1, intOpt(opts, 'fetchConcurrency', 16));
  const ours = new Ours(opts.ours, { concurrency: fetchConcurrency });
  const top = intOpt(opts, 'top', 0);
  const retries = intOpt(opts, 'retries', 3);
  const inlinePieces = !!opts.inlinePieces;
  // Extraction parameters and tracked ranges are index-defining (a change
  // needs a reindex), so one read serves the whole run.
  const status0 = await ours.status();
  const ranges = rangesFromStatus(status0);
  const budgets = new Budgets({ ranges, maxValue: status0.extracted_max_value });
  log.info(`extraction: since ${status0.extracted_since}, max value ${status0.extracted_max_value}, ranges ${ranges.map(r => `${r.id ?? ''}[${r.start},${r.end})`).join(' ')}`);
  const allRows = await ours.rows();
  const wanted = listOpt(opts, 'rows');
  const rows = wanted ? allRows.filter(r => wanted.includes(r.id)) : allRows;
  if (wanted) for (const id of wanted) if (!allRows.some(r => r.id === id)) log.warn(`unknown row ${id}`);

  const results = [];
  let errors = 0;
  for (const row of rows) {
    let done = false;
    for (let attempt = 0; attempt <= retries && !done; attempt++) {
      const status = await ours.status();
      try {
        for (const k of ['extracted_max_value', 'extracted_since']) {
          if (status[k] !== status0[k]) throw new Error(`/status ${k} changed during the run (${status0[k]} -> ${status[k]})`);
        }
        if (JSON.stringify(rangesFromStatus(status)) !== JSON.stringify(ranges)) throw new Error('/status ranges changed during the run');
        const res = await crosscheckRow(ours, row, { status, top, log, budgets, inlinePieces, fetchConcurrency });
        const after = await ours.status();
        const seen = [res.heights.supply, res.heights.supply_after, after.height];
        if (seen.some(h => h !== status.height)) {
          log.info(`  ${row.id}: heights moved during the check (status ${status.height}, supply ${seen.join(' / ')}), retrying`);
          continue;
        }
        results.push({ height: status.height, ...res });
        done = true;
      } catch (err) {
        log.warn(`${row.id}: ${err.message}`);
        if (attempt === retries) {
          errors++;
          results.push({ row: row.id, error: err.message, diffs: [] });
        }
      }
    }
    if (!done && !results.some(r => r.row === row.id)) {
      errors++;
      results.push({ row: row.id, error: 'height kept moving', diffs: [] });
    }
  }

  const bad = results.filter(r => r.diffs.length);
  const w = Math.max(20, ...results.map(r => r.row.length + 2));
  stdout.write(`${'row'.padEnd(w)}${'pieces'.padStart(10)}${'outputs'.padStart(10)}${'located'.padStart(12)}${'extracted'.padStart(12)}${'holders'.padStart(10)}  result\n`);
  for (const r of results) {
    const c = r.computed || {};
    const verdict = r.error ? `ERROR ${r.error}` : r.diffs.length ? `${r.diffs.length} differences: ${[...new Set(r.diffs.map(d => d.field))].join(', ')}` : 'ok';
    stdout.write(`${r.row.padEnd(w)}${String(r.lines ?? '-').padStart(10)}${String(r.outputs ?? '-').padStart(10)}${String(c.located ?? '-').padStart(12)}${String(c.extracted ?? '-').padStart(12)}${String(c.holders ?? '-').padStart(10)}  ${verdict}\n`);
  }
  if (top) {
    // Which reading of the holder-list contract the fork follows (see
    // compareHolders); DESIGN.md §5 specifies count=own last=own.
    const seen = new Map();
    for (const r of results) {
      for (const [list, reading] of Object.entries(r.holder_readings || {})) {
        if (!reading) continue;
        const key = `${list} count=${reading.count} last=${reading.last} time=${reading.time}`;
        seen.set(key, (seen.get(key) || 0) + 1);
      }
    }
    for (const [key, n] of seen) stdout.write(`holder lists: ${key} (${n} rows)\n`);
  }
  const out = new JsonlWriter(opts.out || null);
  for (const r of bad) for (const d of r.diffs) out.write({ row: r.row, height: r.height, ...d });
  out.close();
  if (opts.out) stdout.write(`differences: ${out.count} -> ${opts.out}\n`);
  stdout.write(`output budgets: ${budgets.fetched} from /output, ${budgets.inline} inline\n`);
  if (opts.summaryJson) {
    const summary = { results, errors, extraction: { since: status0.extracted_since, max_value: status0.extracted_max_value, ranges, budgets_fetched: budgets.fetched, budgets_inline: budgets.inline } };
    fs.writeFileSync(opts.summaryJson, `${JSON.stringify(summary, null, 2)}\n`);
  }
  return bad.length ? 1 : errors ? 2 : 0;
}

module.exports = { main, RowAccumulator, Budgets, compareHolders };

if (require.main === module) {
  main(process.argv.slice(2)).then(
    code => process.exit(code),
    err => {
      process.stderr.write(`${err.stack || err}\n`);
      process.exit(2);
    },
  );
}
