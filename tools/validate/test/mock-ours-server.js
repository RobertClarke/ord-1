#!/usr/bin/env node
'use strict';

// Serves a stand-in for the fork's API (the subset oracle-check uses) from
// baseline data, so oracle-check can be rehearsed against the real oracle
// before the fork runs:
//
//   node tools/validate/test/mock-ours-server.js [--baseline=DIR] [--port=0] \
//        [--follow=ORACLE_URL] [--mutate=SAT,…] [--unlocated=untracked|fake]
//   node tools/validate/oracle-check.js --ours=http://127.0.0.1:PORT --oracle=… \
//        --edges=untracked,bip30,block9,lost,burned
//
// --follow reports the oracle's current height (so the sync wait passes);
// sats that moved since the baseline was recorded then show up as genuine
// mismatches. --mutate shifts those sats' offsets by one (deliberate
// mismatches).

const { startServer, loadBaseline, mockOurs, BASELINE_DIR } = require('./mocks');
const { parseArgs, intOpt, listOpt } = require('../lib/util');

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  const { meta, entries } = loadBaseline(opts.baseline || BASELINE_DIR);
  const mutated = new Set((listOpt(opts, 'mutate') || []).map(Number));
  const mutate = mutated.size
    ? (sat, body) => {
        if (mutated.has(sat) && body.location) {
          body.location.offset += 1;
          body.location.satpoint = `${body.location.outpoint}:${body.location.offset}`;
        }
        return body;
      }
    : null;
  // --unlocated=fake: report sats the baseline could not locate as tracked
  // in an output bitcoind does not know (baseline mode then skips them).
  const unlocated = opts.unlocated === 'fake' ? 'fake' : 'untracked';
  const ours = mockOurs({ entries, height: meta.oracle_height, lostSats: meta.oracle_lost_sats, mutate, unlocated });
  if (opts.follow) {
    const base = String(opts.follow).replace(/\/+$/, '');
    const poll = async () => {
      try {
        const res = await fetch(`${base}/status`, { headers: { accept: 'application/json' } });
        const st = await res.json();
        ours.state.height = st.height;
        ours.state.lostSats = st.lost_sats;
      } catch (err) {
        process.stderr.write(`follow: ${err.message}\n`);
      }
    };
    await poll();
    setInterval(poll, 2000).unref();
  }
  const server = await startServer(ours.handler, { port: intOpt(opts, 'port', 0) });
  process.stdout.write(`${server.url}\n`);
  process.stderr.write(`mock ours: ${entries.length} baseline entries, rows ${[...ours.rows.keys()].join(', ')}, height ${ours.state.height}\n`);
}

main().catch(err => {
  process.stderr.write(`${err.stack || err}\n`);
  process.exit(2);
});
