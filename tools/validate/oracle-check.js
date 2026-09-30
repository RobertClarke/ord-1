#!/usr/bin/env node
'use strict';

// Compares the fork's sat locations with upstream ord (--index-sats).
//
// Live mode (both at the same height):
//   node tools/validate/oracle-check.js --ours=URL --oracle=URL
//        [--samples-per-row=20] [--seed=S] [--concurrency=16]
//        [--rows=a,b | --skip-rows] [--edges=epochs,block9,…|none]
//        [--out=FILE.jsonl] [--summary-json=FILE] [--sync-timeout=3600] [--retries=3]
//
// Baseline mode (any height; needs bitcoind for gettxout):
//   node tools/validate/oracle-check.js --ours=URL --baseline[=DIR] [--oracle=URL]
//
// Exit code: 0 all matched, 1 any mismatch, 2 incomplete (errors) or bad usage.
//
// How a sat is compared (see lib/check.js):
// * uncommon-or-rarer sats: the oracle's /sat satpoint, verbatim;
// * every other sat (ord's /sat has no satpoint for common sats): our
//   claim must be backed by the oracle's /output ranges for our outpoint
//   (exact offset; output still indexed, i.e. unspent or OP_RETURN);
// * lost sats: ord's null outpoint 000…000:4294967295 with the offset from
//   the oracle's lost ranges (/output/<null outpoint>);
// * BIP30 (blocks 91812 / 91722, overwritten by 91842 / 91880): reported
//   as known differences, not failures.

const fs = require('node:fs');
const path = require('node:path');
const sm = require('./lib/satmath');
const { Oracle } = require('./lib/oracle');
const { Ours } = require('./lib/ours');
const { Rpc } = require('./lib/rpc');
const { SampleSet, LiveChecker, BaselineChecker } = require('./lib/check');
const { ALL_EDGES, FEE_HEAVY_BLOCKS, sampleRows, buildEdges } = require('./lib/sampling');
const { parseArgs, intOpt, listOpt, makeRng, makeLogger, sleep, JsonlWriter, readJsonl } = require('./lib/util');

const USAGE = `usage:
  oracle-check.js --ours=URL --oracle=URL [--samples-per-row=20] [--seed=S] [--concurrency=16] [--scan-concurrency=8]
                  [--rows=a,b | --skip-rows] [--edges=${ALL_EDGES.join(',')}|none]
                  [--fee-heavy-blocks=h,…] [--sats=n,…] [--untracked-sats=n,…]
                  [--out=FILE.jsonl] [--summary-json=FILE]
                  [--sync-timeout=3600] [--retries=3] [--quiet]
  oracle-check.js --ours=URL --baseline[=DIR] [--oracle=URL] [--rpc-url=URL] [--out=FILE.jsonl]
bitcoind (baseline mode): ORD_BITCOIN_RPC_URL / ORD_BITCOIN_RPC_USERNAME / ORD_BITCOIN_RPC_PASSWORD`;

async function heights(ours, oracle) {
  const [o, r] = await Promise.all([ours.status(), oracle.status()]);
  return { ours: o.height, oracle: r.height, oursStatus: o, oracleStatus: r };
}

async function waitForSync(ours, oracle, { timeoutS, pollS, log }) {
  const deadline = Date.now() + timeoutS * 1000;
  let last = '';
  for (;;) {
    let h = null;
    try {
      h = await heights(ours, oracle);
    } catch (err) {
      log.warn(`status: ${err.message}`);
    }
    if (h && typeof h.ours === 'number' && h.ours === h.oracle) return h;
    if (Date.now() > deadline) throw new Error(`heights never matched (ours ${h?.ours}, oracle ${h?.oracle})`);
    const msg = `waiting for equal heights: ours ${h?.ours ?? '?'} (${h?.oursStatus?.mode ?? '?'}), oracle ${h?.oracle ?? '?'}`;
    if (msg !== last) log.info(msg);
    last = msg;
    await sleep(pollS * 1000);
  }
}

function summarize(results, order) {
  const sets = new Map();
  const bump = (set, key) => {
    if (!sets.has(set)) sets.set(set, { checked: 0, matched: 0, mismatched: 0, known_diff: 0, skipped: 0, errors: 0 });
    sets.get(set)[key]++;
  };
  const total = { checked: 0, matched: 0, mismatched: 0, known_diff: 0, skipped: 0, errors: 0 };
  const key = { match: 'matched', mismatch: 'mismatched', known_diff: 'known_diff', skip: 'skipped', error: 'errors' };
  for (const r of results) {
    const k = key[r.status];
    for (const set of r.sets) {
      bump(set, k);
      if (r.status === 'match' || r.status === 'mismatch') bump(set, 'checked');
    }
    total[k]++;
    if (r.status === 'match' || r.status === 'mismatch') total.checked++;
  }
  const rank = s => {
    const i = order.indexOf(s);
    return i === -1 ? order.length : i;
  };
  const names = [...sets.keys()].sort((a, b) => rank(a) - rank(b) || a.localeCompare(b));
  return { sets: names.map(name => ({ set: name, ...sets.get(name) })), total };
}

function printSummary(summary, out = process.stdout) {
  const cols = ['checked', 'matched', 'mismatched', 'known_diff', 'skipped', 'errors'];
  const w = Math.max(24, ...summary.sets.map(s => s.set.length + 2));
  out.write(`${'set'.padEnd(w)}${cols.map(c => c.padStart(11)).join('')}\n`);
  for (const s of summary.sets) out.write(`${s.set.padEnd(w)}${cols.map(c => String(s[c]).padStart(11)).join('')}\n`);
  out.write(`${'TOTAL (distinct sats)'.padEnd(w)}${cols.map(c => String(summary.total[c]).padStart(11)).join('')}\n`);
}

function methodCounts(results) {
  const counts = {};
  for (const r of results) {
    const k = `${r.method ?? 'none'}:${r.status}`;
    counts[k] = (counts[k] || 0) + 1;
  }
  return counts;
}

function reasonCounts(results) {
  const counts = {};
  for (const r of results) for (const reason of r.reasons || []) counts[reason] = (counts[reason] || 0) + 1;
  return counts;
}

function writeResults(opts, results, defaultStem) {
  const outPath = opts.out || `${defaultStem}.mismatches.jsonl`;
  const knownPath = outPath.replace(/(\.jsonl)?$/, '.known.jsonl');
  const bad = new JsonlWriter(outPath);
  const known = new JsonlWriter(knownPath);
  for (const r of results) {
    if (r.status === 'mismatch' || r.status === 'error') bad.write(r);
    else if (r.status === 'known_diff') known.write(r);
  }
  bad.close();
  known.close();
  return { outPath, knownPath, mismatches: bad.count, known: known.count };
}

async function live(opts, log, stdout) {
  const concurrency = Math.min(intOpt(opts, 'concurrency', 16), 32);
  const ours = new Ours(opts.ours, { concurrency });
  const oracle = new Oracle(opts.oracle, { concurrency });
  const rng = makeRng(opts.seed || `oracle-check-${Date.now()}`);
  const syncOpts = { timeoutS: intOpt(opts, 'syncTimeout', 3600), pollS: intOpt(opts, 'poll', 5), log };
  const edges = opts.edges === 'none' ? [] : listOpt(opts, 'edges') || ALL_EDGES;
  const feeHeavy = listOpt(opts, 'feeHeavyBlocks')?.map(Number) || FEE_HEAVY_BLOCKS;
  const started = new Date().toISOString();

  let h = await waitForSync(ours, oracle, syncOpts);
  const height0 = h.ours;
  log.info(`heights match at ${height0}`);

  const rows = await ours.rows();
  const rowFilter = listOpt(opts, 'rows');
  const sampled = opts.skipRows ? [] : rows.filter(r => !rowFilter || rowFilter.includes(r.id));
  const samples = new SampleSet();
  const perRow = intOpt(opts, 'samplesPerRow', 20);
  log.info(`sampling ${perRow} sats from each of ${sampled.length} rows`);
  const rowData = await sampleRows(ours, sampled, { perRow, rng, log });
  for (const [rowId, sats] of rowData.samples) for (const s of sats) samples.add(s, `row:${rowId}`, 'tracked');
  log.info(`edge sets: ${edges.join(', ') || 'none'}`);
  await buildEdges({ ours, oracle, rng, log, height: height0, edges, rowData, feeHeavy }, (sat, set, expect) => samples.add(sat, set, expect));
  for (const s of listOpt(opts, 'sats') || []) samples.add(Number(s), 'cli', 'tracked');
  for (const s of listOpt(opts, 'untrackedSats') || []) samples.add(Number(s), 'cli-untracked', 'untracked');

  const mined = sm.firstSat(height0 + 1);
  const list = samples.list().filter(s => s.sat < mined);
  log.info(`${list.length} distinct sats to check`);

  const checker = new LiveChecker({ ours, oracle, log, concurrency, scanConcurrency: Math.min(intOpt(opts, 'scanConcurrency', 8), 32) });
  const results = await checker.check(list);
  let after = await heights(ours, oracle);
  let moved = after.ours !== height0 || after.oracle !== height0;
  const retries = intOpt(opts, 'retries', 3);
  const history = [{ pass: 0, height: height0, moved_during: moved }];
  for (let attempt = 1; attempt <= retries; attempt++) {
    const bad = [...results.values()].filter(r => r.status === 'mismatch' || r.status === 'error');
    const hasErrors = bad.some(r => r.status === 'error');
    if (!bad.length || !(moved || (hasErrors && attempt === 1))) break;
    log.info(`re-checking ${bad.length} sats (${moved ? 'a block landed mid-run' : 'errors'})`);
    h = await waitForSync(ours, oracle, syncOpts);
    oracle.resetCache();
    const again = await checker.check(bad.map(r => samples.map.get(r.sat)));
    for (const [sat, r] of again) {
      r.rechecked = attempt;
      results.set(sat, r);
    }
    after = await heights(ours, oracle);
    moved = after.ours !== h.ours || after.oracle !== h.ours;
    history.push({ pass: attempt, height: h.ours, rechecked: bad.length, moved_during: moved });
  }

  const all = [...results.values()];
  const order = [...rows.map(r => `row:${r.id}`), ...ALL_EDGES.flatMap(e => [`edge:${e}`]), 'edge:vintage-plain', 'edge:450x', 'edge:oracle-lost', 'bip30'];
  const summary = summarize(all, order);
  printSummary(summary, stdout);
  const files = writeResults(opts, all, `oracle-check-${height0}`);
  const reasons = reasonCounts(all.filter(r => r.status === 'mismatch' || r.status === 'error'));
  const report = {
    mode: 'live',
    started,
    finished: new Date().toISOString(),
    height: height0,
    passes: history,
    rows_streamed: Object.fromEntries(rowData.lines),
    row_stream_errors: rowData.errors,
    special_pieces: { lost: rowData.special.lost.length, burned: rowData.special.burned.length, overwritten: rowData.special.overwritten.length },
    summary,
    methods: methodCounts(all),
    reasons,
    files,
    http: { ours: ours.http.stats, oracle: oracle.http.stats },
  };
  if (opts.summaryJson) fs.writeFileSync(opts.summaryJson, `${JSON.stringify(report, null, 2)}\n`);
  if (Object.keys(reasons).length) stdout.write(`mismatch reasons: ${JSON.stringify(reasons)}\n`);
  stdout.write(`known differences (BIP30): ${files.known} -> ${files.knownPath}\n`);
  stdout.write(`mismatches/errors: ${files.mismatches} -> ${files.outPath}\n`);
  // A row that could not be streamed was not sampled: the run is incomplete.
  for (const e of rowData.errors) stdout.write(`row ${e.row} not sampled: ${e.error}\n`);
  return summary.total.mismatched ? 1 : summary.total.errors || rowData.errors.length ? 2 : 0;
}

function loadBaseline(dir) {
  const files = fs.readdirSync(dir).filter(f => f.endsWith('.jsonl')).sort();
  if (!files.length) throw new Error(`no *.jsonl in ${dir}`);
  return files.flatMap(f => readJsonl(path.join(dir, f)));
}

async function baseline(opts, log, stdout) {
  const dir = opts.baseline === true ? path.join(__dirname, 'baseline') : opts.baseline;
  const concurrency = Math.min(intOpt(opts, 'concurrency', 16), 32);
  const ours = new Ours(opts.ours, { concurrency });
  const oracle = opts.oracle ? new Oracle(opts.oracle, { concurrency }) : null;
  const rpc = Rpc.fromEnv({ rpcUrl: opts.rpcUrl, concurrency: 4 });
  const entries = loadBaseline(dir);
  const status = await ours.status();
  if (typeof status.height !== 'number') throw new Error('ours has no height yet');
  const setsFilter = listOpt(opts, 'sets');
  const list = setsFilter ? entries.filter(e => setsFilter.includes(e.set)) : entries;
  const checker = new BaselineChecker({ ours, oracle, rpc, log, concurrency, scanConcurrency: Math.min(intOpt(opts, 'scanConcurrency', 8), 32) });
  const { results, tip } = await checker.check(list, { height: status.height, lostSats: status.lost_sats });
  const all = [...results.values()].map(r => ({ ...r, sets: r.sets.map(s => `baseline:${s}`) }));
  const summary = summarize(all, []);
  printSummary(summary, stdout);
  const skips = {};
  for (const r of all) if (r.status === 'skip') for (const why of r.reasons) skips[why] = (skips[why] || 0) + 1;
  stdout.write(`skipped because: ${JSON.stringify(skips)}\n`);
  const files = writeResults(opts, all, `oracle-check-baseline-${status.height}`);
  const reasons = reasonCounts(all.filter(r => r.status === 'mismatch' || r.status === 'error'));
  if (Object.keys(reasons).length) stdout.write(`mismatch reasons: ${JSON.stringify(reasons)}\n`);
  stdout.write(`mismatches/errors: ${files.mismatches} -> ${files.outPath}\n`);
  if (opts.summaryJson) {
    const report = { mode: 'baseline', dir, height: status.height, bitcoind_tip: tip, summary, methods: methodCounts(all), skips, reasons, files };
    fs.writeFileSync(opts.summaryJson, `${JSON.stringify(report, null, 2)}\n`);
  }
  return summary.total.mismatched ? 1 : summary.total.errors ? 2 : 0;
}

// Returns the exit code; `io` lets tests capture output.
async function main(argv, io = {}) {
  const stdout = io.stdout || process.stdout;
  const opts = parseArgs(argv, { booleans: ['help', 'quiet', 'skipRows'] });
  if (opts.help || !opts.ours || (!opts.baseline && !opts.oracle)) {
    (io.stderr || process.stderr).write(`${USAGE}\n`);
    return opts.help ? 0 : 2;
  }
  const log = io.log || makeLogger({ quiet: opts.quiet });
  return opts.baseline ? baseline(opts, log, stdout) : live(opts, log, stdout);
}

module.exports = { main, summarize };

if (require.main === module) {
  main(process.argv.slice(2)).then(
    code => process.exit(code),
    err => {
      process.stderr.write(`${err.stack || err}\n`);
      process.exit(2);
    },
  );
}
