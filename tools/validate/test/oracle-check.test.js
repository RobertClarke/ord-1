'use strict';

// oracle-check.js against mock servers built from the recorded baseline:
// ours and the oracle agree everywhere the baseline has an answer, so a
// clean run proves the matching paths (direct satpoints, /output range
// claims, lost ranges, BIP30, untracked); deliberate mutations prove the
// mismatch paths.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { main } = require('../oracle-check');
const sm = require('../lib/satmath');
const { startServer, loadBaseline, mockOracle, mockOurs, mockBitcoind, BASELINE_DIR } = require('./mocks');
const { readJsonl } = require('../lib/util');

const DIR = process.env.VALIDATE_BASELINE_DIR || BASELINE_DIR;
const { meta, entries: allEntries } = loadBaseline(DIR);
const HEIGHT = meta.oracle_height;

// A deterministic subset: every lost / bip30 / fee-heavy / recent entry,
// up to 40 of the others per set, always keeping some of each method.
function subset(entries, perSet = 40) {
  const bySet = new Map();
  for (const e of entries) {
    if (!bySet.has(e.set)) bySet.set(e.set, []);
    bySet.get(e.set).push(e);
  }
  const out = [];
  for (const [set, list] of bySet) {
    if (['lost', 'bip30', 'fee-heavy', 'recent'].includes(set)) out.push(...list);
    else {
      const located = list.filter(e => e.location);
      const rest = list.filter(e => !e.location);
      out.push(...located.slice(0, perSet), ...rest.slice(0, Math.max(5, perSet - located.length)));
    }
  }
  return out;
}

const ENTRIES = subset(allEntries);
const quiet = { info() {}, warn() {} };

function tmpdir() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'oracle-check-test-'));
}

async function withServers(opts, fn) {
  const oracle = mockOracle({ entries: ENTRIES, height: HEIGHT, lostSats: meta.oracle_lost_sats, ...opts.oracle });
  const ours = mockOurs({ entries: ENTRIES, height: HEIGHT, lostSats: meta.oracle_lost_sats, ...opts.ours });
  const servers = [await startServer(oracle.handler), await startServer(ours.handler)];
  try {
    return await fn({ oracle, ours, oracleUrl: servers[0].url, oursUrl: servers[1].url });
  } finally {
    await Promise.all(servers.map(s => s.close()));
  }
}

async function runLive(oursUrl, oracleUrl, extra = []) {
  const dir = tmpdir();
  const out = path.join(dir, 'mismatches.jsonl');
  const summaryJson = path.join(dir, 'summary.json');
  let stdout = '';
  const code = await main(
    [
      `--ours=${oursUrl}`,
      `--oracle=${oracleUrl}`,
      '--samples-per-row=100000',
      '--edges=untracked,bip30,block9,lost,burned',
      '--seed=selftest',
      `--out=${out}`,
      `--summary-json=${summaryJson}`,
      '--sync-timeout=20',
      '--poll=0.1',
      ...extra,
    ],
    { stdout: { write: s => (stdout += s) }, log: quiet },
  );
  const report = JSON.parse(fs.readFileSync(summaryJson, 'utf8'));
  return { code, stdout, report, mismatches: readJsonl(out), known: readJsonl(out.replace(/\.jsonl$/, '.known.jsonl')) };
}

const pick = pred => ENTRIES.find(e => pred(e));

test('live: a mock built from the baseline matches everywhere', async () => {
  await withServers({}, async ({ oursUrl, oracleUrl }) => {
    const { code, report, mismatches, known, stdout } = await runLive(oursUrl, oracleUrl);
    assert.equal(code, 0, `${stdout}\n${JSON.stringify(mismatches.slice(0, 3), null, 1)}`);
    assert.equal(report.summary.total.mismatched, 0);
    assert.equal(report.summary.total.errors, 0);
    const m = report.methods;
    assert.ok(m['direct:match'] > 0, JSON.stringify(m));
    assert.ok(m['output_ranges:match'] > 0, JSON.stringify(m));
    assert.ok(m['lost_ranges:match'] > 0, JSON.stringify(m));
    assert.ok(m['untracked:match'] > 0, JSON.stringify(m));
    assert.ok(known.length > 0, 'BIP30 known differences are reported');
    assert.ok(known.every(r => r.sets.includes('bip30') || sm.isBip30Sat(r.sat)));
    assert.match(stdout, /row:uncommon/);
  });
});

test('live: deliberate mismatches are caught and explained', async () => {
  const claim = pick(e => e.location && e.location.kind === 'unspent' && e.location.how === 'output');
  const direct = pick(e => e.location && e.location.how === 'sat+output' && sm.rarity(e.sat) !== 'common');
  const lost = pick(e => e.location && e.location.kind === 'lost' && e.expect === 'tracked');
  const untracked = pick(e => e.expect === 'untracked' && e.set === 'vintage-plain');
  assert.ok(claim && direct && lost && untracked, 'baseline has every kind of entry');
  const mutate = (sat, body) => {
    const loc = body.location;
    if (sat === claim.sat || sat === lost.sat) {
      loc.offset += 1;
      loc.satpoint = `${loc.outpoint}:${loc.offset}`;
    } else if (sat === direct.sat) {
      const [txid, vout] = loc.outpoint.split(':');
      loc.outpoint = `${txid}:${Number(vout) + 1}`;
      loc.satpoint = `${loc.outpoint}:${loc.offset}`;
    } else if (sat === untracked.sat) {
      body.tracked = true;
      body.types = ['uncommon'];
    }
    return body;
  };
  await withServers({ ours: { mutate } }, async ({ oursUrl, oracleUrl }) => {
    const { code, mismatches, report } = await runLive(oursUrl, oracleUrl, ['--edges=untracked,bip30,lost', `--untracked-sats=${untracked.sat}`]);
    assert.equal(code, 1);
    const bySat = new Map(mismatches.map(r => [r.sat, r]));
    assert.deepEqual(bySat.get(claim.sat)?.reasons, ['claim_offset_differs']);
    assert.ok(bySat.get(claim.sat).context.oracle_output_for_ours.window, 'mismatch carries the oracle ranges around the sat');
    assert.deepEqual(bySat.get(lost.sat)?.reasons, ['lost_offset_differs']);
    assert.deepEqual(bySat.get(direct.sat)?.reasons, ['satpoint_differs']);
    assert.equal(bySat.get(direct.sat).oracle.satpoint, direct.oracle.satpoint);
    assert.ok('oracle_output_for_oracle' in bySat.get(direct.sat).context);
    assert.equal(report.summary.total.mismatched, mismatches.length);
    assert.deepEqual(bySat.get(untracked.sat)?.reasons, ['tracked_but_expected_untracked']);
  });
});

test('live: an untracked sat reported as tracked is a mismatch', async () => {
  const target = pick(e => e.expect === 'untracked');
  const other = ENTRIES.filter(e => e.expect === 'untracked' && e.sat !== target.sat).slice(0, 5);
  const mutate = (sat, body) => (sat === target.sat ? { ...body, tracked: true, types: ['black'] } : body);
  await withServers({ ours: { mutate } }, async ({ oursUrl, oracleUrl }) => {
    const dir = tmpdir();
    const out = path.join(dir, 'm.jsonl');
    const code = await main(
      [
        `--ours=${oursUrl}`,
        `--oracle=${oracleUrl}`,
        '--skip-rows',
        '--edges=none',
        `--untracked-sats=${[target, ...other].map(e => e.sat).join(',')}`,
        `--out=${out}`,
        '--sync-timeout=5',
      ],
      { stdout: { write() {} }, log: quiet },
    );
    assert.equal(code, 1);
    const rows = readJsonl(out);
    assert.equal(rows.length, 1);
    assert.equal(rows[0].sat, target.sat);
    assert.deepEqual(rows[0].reasons, ['tracked_but_expected_untracked']);
  });
});

test('live: waits until both heights match', async () => {
  await withServers({ ours: { height: HEIGHT - 3 } }, async ({ oursUrl, oracleUrl, ours }) => {
    ours.state.onStatus = st => {
      if (st.height < HEIGHT) st.height++;
    };
    const { code, report } = await runLive(oursUrl, oracleUrl, ['--edges=untracked', '--rows=uncommon']);
    assert.equal(code, 0);
    assert.equal(report.height, HEIGHT);
    assert.ok(ours.state.statusCalls >= 4);
  });
});

test('live: a block landing mid-run triggers a re-check', async () => {
  const target = pick(e => e.location && e.location.how === 'output' && e.kind !== 'uncommon');
  let fixed = false;
  const mutate = (sat, body) => {
    if (sat !== target.sat || fixed) return body;
    body.location.offset += 7;
    body.location.satpoint = `${body.location.outpoint}:${body.location.offset}`;
    return body;
  };
  await withServers({ ours: { mutate } }, async ({ oursUrl, oracleUrl, ours, oracle }) => {
    ours.state.onSat = sat => {
      if (sat === target.sat && !fixed) {
        // The block that "moved" it lands on both sides after this answer.
        setTimeout(() => {
          fixed = true;
          ours.state.height = HEIGHT + 1;
          oracle.state.height = HEIGHT + 1;
        }, 0);
      }
    };
    const { code, report } = await runLive(oursUrl, oracleUrl, [`--rows=${target.kind === '450x' ? 'block9' : target.kind}`, '--edges=untracked']);
    assert.equal(code, 0, JSON.stringify(report.summary.total));
    assert.equal(report.passes.length, 2);
    assert.equal(report.passes[0].moved_during, true);
  });
});

test('live: without a new block the mismatch stands', async () => {
  const target = pick(e => e.location && e.location.how === 'output' && e.kind !== 'uncommon');
  const mutate = (sat, body) => {
    if (sat === target.sat) {
      body.location.offset += 7;
      body.location.satpoint = `${body.location.outpoint}:${body.location.offset}`;
    }
    return body;
  };
  await withServers({ ours: { mutate } }, async ({ oursUrl, oracleUrl }) => {
    const { code, report, mismatches } = await runLive(oursUrl, oracleUrl, [`--rows=${target.kind === '450x' ? 'block9' : target.kind}`, '--edges=untracked']);
    assert.equal(code, 1);
    assert.equal(report.passes.length, 1);
    assert.equal(mismatches[0].sat, target.sat);
  });
});

test('baseline: checks sats whose recorded output is still unspent at our height', async () => {
  const dir = tmpdir();
  const bySet = new Map();
  for (const e of ENTRIES) {
    if (!bySet.has(e.set)) bySet.set(e.set, []);
    bySet.get(e.set).push(e);
  }
  for (const [set, list] of bySet) fs.writeFileSync(path.join(dir, `${set}.jsonl`), list.map(e => JSON.stringify(e)).join('\n') + '\n');
  // bitcoind: every recorded output is unspent, created at its recorded
  // height; one recorded output was spent since.
  const unspent = ENTRIES.filter(e => e.location && e.location.kind === 'unspent' && e.expect === 'tracked');
  const spentOne = unspent.find(e => e.height < HEIGHT - 3000);
  const utxos = new Map();
  for (const e of unspent) utxos.set(e.location.outpoint, e.location.created_height ?? 0);
  utxos.delete(spentOne.location.outpoint);
  const ourHeight = HEIGHT - 2000;
  const rpc = mockBitcoind({ utxos, tip: HEIGHT + 5 });
  const rpcServer = await startServer(rpc.handler);
  try {
    const run = async mutate =>
      withServers({ ours: { height: ourHeight, mutate, unlocated: 'fake' } }, async ({ oursUrl, oracleUrl }) => {
        const out = path.join(tmpdir(), 'b.jsonl');
        const summaryJson = path.join(path.dirname(out), 's.json');
        let stdout = '';
        const code = await main(
          [`--ours=${oursUrl}`, `--baseline=${dir}`, `--oracle=${oracleUrl}`, `--rpc-url=${rpcServer.url}`, `--out=${out}`, `--summary-json=${summaryJson}`],
          { stdout: { write: s => (stdout += s) }, log: quiet },
        );
        return { code, stdout, report: JSON.parse(fs.readFileSync(summaryJson, 'utf8')), mismatches: readJsonl(out) };
      });
    const clean = await run(null);
    assert.equal(clean.code, 0, `${clean.stdout}\n${JSON.stringify(clean.mismatches.slice(0, 3), null, 1)}`);
    assert.ok(clean.report.methods['baseline:match'] > 0, JSON.stringify(clean.report.methods));
    assert.ok(clean.report.skips.not_mined_at_our_height > 0);
    assert.ok(clean.report.skips.baseline_output_spent_since >= 1);
    // A recorded output created after our height is skipped, not compared.
    const late = unspent.find(e => utxos.has(e.location.outpoint) && e.location.created_height > ourHeight && e.height <= ourHeight);
    if (late) assert.ok(clean.report.skips.not_created_at_our_height >= 1);

    const target = unspent.find(e => utxos.has(e.location.outpoint) && (e.location.created_height ?? 0) <= ourHeight && e.height <= ourHeight);
    const bad = await run((sat, body) => {
      if (sat === target.sat) body.location.satpoint = `${body.location.outpoint}:${body.location.offset + 1}`;
      return body;
    });
    assert.equal(bad.code, 1);
    assert.equal(bad.mismatches.length, 1);
    assert.equal(bad.mismatches[0].sat, target.sat);
    assert.deepEqual(bad.mismatches[0].reasons, ['satpoint_differs']);
  } finally {
    await rpcServer.close();
  }
});

test('live: ours losing a different number of sats than ord fails the run', async () => {
  await withServers({ ours: { lostSats: meta.oracle_lost_sats - 5 } }, async ({ oursUrl, oracleUrl }) => {
    const { code, report } = await runLive(oursUrl, oracleUrl, ['--edges=untracked', '--rows=uncommon']);
    assert.equal(code, 1);
    assert.deepEqual(
      report.globals.map(g => [g.check, g.ok]),
      [['lost_sats_total', false]],
    );
    assert.equal(report.summary.total.mismatched, 0);
  });
});

test('live: BIP30 sats must be reported overwritten where they were', async () => {
  const target = sm.uncommonSat(91_812);
  const mutate = (sat, body) => {
    if (sat === target) body.location = { ...body.location, kind: 'unspent' };
    return body;
  };
  await withServers({ ours: { mutate } }, async ({ oursUrl, oracleUrl }) => {
    const { code, mismatches, known } = await runLive(oursUrl, oracleUrl, ['--skip-rows', '--edges=bip30']);
    assert.equal(code, 1);
    assert.deepEqual(
      mismatches.map(r => [r.sat, r.reasons]),
      [[target, ['bip30_not_overwritten']]],
    );
    assert.ok(known.some(r => r.sat === sm.blackSat(91_812)), 'the black sat is still a known difference');
  });
});

test('live: a seed reproduces the sample', async () => {
  await withServers({}, async ({ oursUrl, oracleUrl }) => {
    const sample = async seed => {
      const file = path.join(tmpdir(), 'samples.jsonl');
      await runLive(oursUrl, oracleUrl, ['--samples-per-row=3', '--edges=untracked,block9', `--seed=${seed}`, `--samples-out=${file}`]);
      return fs.readFileSync(file, 'utf8');
    };
    const a = await sample('same');
    assert.equal(await sample('same'), a);
    assert.notEqual(await sample('other'), a);
  });
});

async function runBaseline({ ours = {}, rpc, sets, extra = [] }) {
  const dir = tmpdir();
  const bySet = new Map();
  for (const e of ENTRIES) {
    if (sets && !sets.includes(e.set)) continue;
    if (!bySet.has(e.set)) bySet.set(e.set, []);
    bySet.get(e.set).push(e);
  }
  for (const [set, list] of bySet) fs.writeFileSync(path.join(dir, `${set}.jsonl`), list.map(e => JSON.stringify(e)).join('\n') + '\n');
  fs.writeFileSync(path.join(dir, 'meta.json'), JSON.stringify({ oracle_height: HEIGHT, oracle_lost_sats: meta.oracle_lost_sats }));
  const rpcServer = await startServer(rpc.handler);
  try {
    return await withServers({ ours }, async ({ oursUrl, oracleUrl, ours: mock }) => {
      if (ours.onSat) mock.state.onSat = ours.onSat;
      const out = path.join(tmpdir(), 'b.jsonl');
      const summaryJson = path.join(path.dirname(out), 's.json');
      let stdout = '';
      const code = await main([`--ours=${oursUrl}`, `--baseline=${dir}`, `--oracle=${oracleUrl}`, `--rpc-url=${rpcServer.url}`, `--out=${out}`, `--summary-json=${summaryJson}`, ...extra], {
        stdout: { write: s => (stdout += s) },
        log: quiet,
      });
      return { code, stdout, report: JSON.parse(fs.readFileSync(summaryJson, 'utf8')), mismatches: readJsonl(out) };
    });
  } finally {
    await rpcServer.close();
  }
}

const recordedUtxos = () => {
  const utxos = new Map();
  for (const e of ENTRIES) if (e.location && e.location.kind === 'unspent') utxos.set(e.location.outpoint, e.location.created_height ?? 0);
  return utxos;
};

test('baseline: gettxout failures are errors, and comparing nothing is not a pass', async () => {
  const utxos = recordedUtxos();
  const failing = new Set([ENTRIES.find(e => e.set === 'random-blocks' && e.location?.kind === 'unspent' && e.expect === 'tracked').location.outpoint]);
  const failed = await runBaseline({ ours: { height: HEIGHT, unlocated: 'fake' }, rpc: mockBitcoind({ utxos, tip: HEIGHT, failing }), sets: ['random-blocks', 'vintage-alpha-omega', 'nakamoto'] });
  assert.equal(failed.code, 2, failed.stdout);
  assert.ok(failed.mismatches.some(r => r.reasons.includes('gettxout_failed')));

  const nothing = await runBaseline({ ours: { height: 1000 }, rpc: mockBitcoind({ utxos, tip: HEIGHT }), sets: ['recent'] });
  assert.equal(nothing.code, 2);
  assert.equal(nothing.report.summary.total.checked, 0);
});

test('baseline: the genesis output counts as unspent', async () => {
  const zero = ENTRIES.find(e => e.sat === 0 && e.location);
  assert.ok(zero, 'sat 0 is in the baseline');
  const mutate = (sat, body) => {
    if (sat === 0) {
      body.location.offset = 1;
      body.location.satpoint = `${body.location.outpoint}:1`;
    }
    return body;
  };
  // bitcoind never reports the genesis output; the check must still run.
  const res = await runBaseline({ ours: { height: 10, mutate }, rpc: mockBitcoind({ utxos: new Map(), tip: HEIGHT }), sets: ['milestones'] });
  assert.equal(res.code, 1);
  assert.deepEqual(
    res.mismatches.map(r => [r.sat, r.reasons]),
    [[0, ['satpoint_differs']]],
  );
});

test('baseline: claims newer than our height fail unless ours moved during the run', async () => {
  const ourHeight = HEIGHT - 5000;
  const fake = new Map();
  for (const e of ENTRIES) if (!e.location && e.expect === 'tracked' && e.height <= ourHeight) fake.set(`${'ff'.repeat(32)}:${e.sat % 1000}`, ourHeight + 2);
  assert.ok(fake.size > 0);
  const utxos = new Map([...recordedUtxos(), ...fake]);
  const stuck = await runBaseline({ ours: { height: ourHeight, unlocated: 'fake' }, rpc: mockBitcoind({ utxos, tip: HEIGHT }), sets: ['random-blocks'] });
  assert.equal(stuck.code, 1);
  assert.ok(stuck.mismatches.every(r => r.reasons[0] === 'claim_output_created_after_our_height'));
  // Ours advanced 5 blocks while answering: those claims are skipped.
  const moving = await runBaseline({
    ours: { height: ourHeight, unlocated: 'fake', onSat: (sat, st) => (st.height = ourHeight + 5) },
    rpc: mockBitcoind({ utxos, tip: HEIGHT }),
    sets: ['random-blocks'],
  });
  assert.equal(moving.code, 0, moving.stdout);
  assert.ok(moving.report.skips.ours_moved_during_run > 0);
});

test('baseline: at or past the baseline height ours must have lost at least as much', async () => {
  const res = await runBaseline({ ours: { height: HEIGHT, lostSats: meta.oracle_lost_sats - 1 }, rpc: mockBitcoind({ utxos: recordedUtxos(), tip: HEIGHT }), sets: ['lost'] });
  assert.equal(res.code, 1);
  assert.deepEqual(
    res.report.globals.map(g => [g.check, g.ok]),
    [['lost_sats_at_least_baseline', false]],
  );
});
