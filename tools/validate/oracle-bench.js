#!/usr/bin/env node
'use strict';

// Measures the oracle's /sat and /output latency and throughput at a few
// concurrency levels (never above 32).
//
//   node tools/validate/oracle-bench.js --oracle=URL [--levels=1,4,8,16,32]
//        [--requests=300] [--seed=S] [--json]

const { JsonClient } = require('./lib/http');
const { scanOutputResponse } = require('./lib/oracle');
const sm = require('./lib/satmath');
const { parseArgs, intOpt, listOpt, makeRng, percentile, mapLimit } = require('./lib/util');

function usage() {
  return 'usage: oracle-bench.js --oracle=URL [--levels=1,4,8,16,32] [--requests=300] [--seed=S] [--json]';
}

// /sat: plain GET. /output: streamed and scanned like oracle-check does
// (some outputs carry millions of ranges); records the biggest ones.
async function measure(client, paths, concurrency, { stream = false, biggest = [] } = {}) {
  const latencies = [];
  let errors = 0;
  const statuses = {};
  const started = performance.now();
  await mapLimit(paths, concurrency, async path => {
    const t0 = performance.now();
    try {
      if (stream) {
        const scan = await client.request(path, res => scanOutputResponse(res, []), { retries: 0, retry500: 0 });
        statuses[scan.status] = (statuses[scan.status] || 0) + 1;
        const ms = performance.now() - t0;
        latencies.push(ms);
        if (scan.sat_ranges) biggest.push({ path, ranges: scan.sat_ranges.count, ms: Math.round(ms) });
      } else {
        const res = await client.get(path, { retries: 0, retry500: 0 });
        statuses[res.status] = (statuses[res.status] || 0) + 1;
        latencies.push(performance.now() - t0);
      }
    } catch {
      errors++;
    }
  });
  const wall = (performance.now() - started) / 1000;
  latencies.sort((a, b) => a - b);
  const r = x => Math.round(x * 10) / 10;
  return {
    concurrency,
    requests: paths.length,
    errors,
    statuses,
    wall_s: r(wall),
    throughput_rps: r(paths.length / wall),
    p50_ms: r(percentile(latencies, 50)),
    p90_ms: r(percentile(latencies, 90)),
    p99_ms: r(percentile(latencies, 99)),
    max_ms: r(latencies[latencies.length - 1]),
  };
}

async function main() {
  const opts = parseArgs(process.argv.slice(2), { booleans: ['json', 'help'] });
  if (opts.help || !opts.oracle) {
    console.error(usage());
    process.exit(opts.help ? 0 : 2);
  }
  const levels = (listOpt(opts, 'levels') || ['1', '4', '8', '16', '32']).map(Number).filter(n => n >= 1 && n <= 32);
  const requests = intOpt(opts, 'requests', 300);
  const rng = makeRng(opts.seed || 'oracle-bench');
  const client = new JsonClient(opts.oracle, { concurrency: 32, name: 'oracle' });
  const status = await client.getJson('/status');
  const tip = status.height;
  console.error(`oracle height ${tip}, lost_sats ${status.lost_sats}`);

  // A fresh random set per level so no level benefits from another's cache.
  const satPaths = () =>
    Array.from({ length: requests }, (_, i) => {
      const h = rng.int(tip + 1);
      if (i % 3 === 0) return `/sat/${sm.uncommonSat(h)}`;
      if (i % 3 === 1) return `/sat/${sm.blackSat(h)}`;
      return `/sat/${sm.firstSat(h) + rng.int(sm.subsidy(h))}`;
    });

  const results = { oracle_height: tip, sat: [], output: [], lost_output: null };
  for (const c of levels) {
    const m = await measure(client, satPaths(), c);
    results.sat.push(m);
    console.error(`/sat    c=${String(c).padStart(2)} ${JSON.stringify(m)}`);
  }

  // Outpoints of current uncommon-sat locations (a realistic /output mix:
  // coinbases, payouts, consolidations).
  const outpoints = new Set();
  const heights = rng.distinct(Math.min(levels.length * requests, 3000), 0, tip + 1);
  await mapLimit(heights, 16, async h => {
    const res = await client.get(`/sat/${sm.uncommonSat(h)}`, { retry500: 0 });
    const sp = res.json && res.json.satpoint;
    if (sp && !sp.startsWith(sm.NULL_TXID)) outpoints.add(sp.split(':').slice(0, 2).join(':'));
  });
  const pool = rng.shuffle([...outpoints]);
  let cursor = 0;
  const biggest = [];
  for (const c of levels) {
    const paths = [];
    for (let i = 0; i < requests && pool.length; i++) paths.push(`/output/${pool[cursor++ % pool.length]}`);
    const m = await measure(client, paths, c, { stream: true, biggest });
    results.output.push(m);
    console.error(`/output c=${String(c).padStart(2)} ${JSON.stringify(m)}`);
  }
  const seen = new Set();
  results.output_biggest = biggest
    .sort((a, b) => b.ranges - a.ranges)
    .filter(b => !seen.has(b.path) && seen.add(b.path))
    .slice(0, 10);
  const counts = biggest.map(b => b.ranges).sort((a, b) => a - b);
  results.output_ranges = { p50: percentile(counts, 50), p90: percentile(counts, 90), p99: percentile(counts, 99), max: counts[counts.length - 1] };
  console.error(`/output ranges per output ${JSON.stringify(results.output_ranges)}; biggest ${JSON.stringify(results.output_biggest.slice(0, 3))}`);

  const t0 = performance.now();
  const lost = await client.getJson(`/output/${sm.NULL_OUTPOINT}`);
  results.lost_output = {
    ms: Math.round(performance.now() - t0),
    ranges: (lost.sat_ranges || []).length,
    sats: (lost.sat_ranges || []).reduce((a, [s, e]) => a + e - s, 0),
    status_lost_sats: status.lost_sats,
  };
  console.error(`/output/<null outpoint> ${JSON.stringify(results.lost_output)}`);
  if (opts.json) process.stdout.write(`${JSON.stringify(results, null, 2)}\n`);
}

main().catch(err => {
  console.error(err.stack || String(err));
  process.exit(2);
});
