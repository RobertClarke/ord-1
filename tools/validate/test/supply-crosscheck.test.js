'use strict';

// supply-crosscheck.js against a hand-built mock of /status, /rows,
// /supply, /supply/{row} and /sats whose expected numbers were worked out
// by hand (DESIGN.md §5), plus deliberate discrepancies.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { main } = require('../supply-crosscheck');
const { startServer } = require('./mocks');
const { readJsonl } = require('../lib/util');

const SINCE = 1_670_976_000;
const TIP_TIME = SINCE + 3 * 365 * 86_400 + 1000; // 3 years after
const OLD = SINCE - 10 * 365 * 86_400; // long before extraction
const RECENT = TIP_TIME - 100; // extracted era, within 1y

// Outputs of row "uncommon" (1 sat each unless noted) and "block9".
const OUTPUTS = [
  // A: 546 sats, recent, 2 uncommons -> extracted (small)
  { op: 'aa'.repeat(32) + ':0', value: 546, time: RECENT + 5, height: 905, script: '5120aa', rows: { uncommon: 2 } },
  // B: 50,000 sats, recent, 1 uncommon -> not extracted (big, mixed)
  { op: 'bb'.repeat(32) + ':1', value: 50_000, time: RECENT, height: 901, script: '5120aa', rows: { uncommon: 1 } },
  // C: old coinbase, 5e9 sats, 1 uncommon + most of block 9 -> not extracted (old)
  { op: 'cc'.repeat(32) + ':0', value: 5_000_000_000, time: OLD, height: 9, script: '41cc', rows: { uncommon: 1, block9: 4_999_979_999 } },
  // D: recent, 20,000 sats all block9 -> extracted for block9 (pure), 2y window
  { op: 'dd'.repeat(32) + ':0', value: 20_000, time: TIP_TIME - 2 * 365 * 86_400 + 50, height: 850, script: '0014dd', rows: { block9: 20_000 } },
  // E: burned, 1 uncommon
  { op: 'ee'.repeat(32) + ':0', value: 1000, time: RECENT, height: 902, script: '6a', kind: 'burned', rows: { uncommon: 1 } },
];
const LOST = { uncommon: 3, block9: 1 };

// Hand-computed (see the comments above).
const EXPECTED = {
  uncommon: {
    mined: 2 + 1 + 1 + 1 + 3,
    located: 4,
    burned: 1,
    lost: 3,
    overwritten: 0,
    extracted: 2,
    holders: 2, // 5120aa (A, B) and 41cc (C)
    extracted_holders: 1,
  },
  block9: {
    mined: 5_000_000_000,
    located: 4_999_979_999 + 20_000,
    burned: 0,
    lost: 1,
    overwritten: 0,
    extracted: 20_000,
    holders: 2,
    extracted_holders: 1,
  },
};
const ACTIVE = {
  // uncommon: non-extracted located = B (recent, 1) + C (old, 1)
  uncommon: { '1y': 1, '2y': 1, '3y': 1, '4y': 1, '5y': 1, '7y': 1 },
  // block9: non-extracted located = C (old) only
  block9: { '1y': 0, '2y': 0, '3y': 0, '4y': 0, '5y': 0, '7y': 0 },
};
// Dashboard reading (supply/mock-fork.js): n/utxos of the list's own
// outputs, last_received over all the holder's row outputs.
const TOP = {
  uncommon: {
    top_extracted_holders: [{ script_pubkey: '5120aa', n: 2, utxos: 1, last_received_height: 905, last_received_time: RECENT + 5 }],
    // 5120aa's latest row output (A, 905) is an extracted one.
    top_non_extracted_holders: [
      { script_pubkey: '41cc', n: 1, utxos: 1, last_received_height: 9, last_received_time: OLD },
      { script_pubkey: '5120aa', n: 1, utxos: 1, last_received_height: 905, last_received_time: RECENT + 5 },
    ],
  },
  block9: {
    top_extracted_holders: [{ script_pubkey: '0014dd', n: 20_000, utxos: 1, last_received_height: 850, last_received_time: TIP_TIME - 2 * 365 * 86_400 + 50 }],
    top_non_extracted_holders: [{ script_pubkey: '41cc', n: 4_999_979_999, utxos: 1, last_received_height: 9, last_received_time: OLD }],
  },
};

function mockOurs({ tamper = s => s, onStatus = null } = {}) {
  const state = { height: 1000, statusCalls: 0 };
  const lines = rowId => {
    const out = [];
    let sat = rowId === 'uncommon' ? 1000 : 45_000_000_000;
    for (const o of OUTPUTS) {
      const count = o.rows[rowId];
      if (!count) continue;
      // Two pieces for the 2-uncommon output (pieces are listed per sat run).
      const parts = rowId === 'uncommon' && count === 2 ? [1, 1] : [count];
      for (const n of parts) {
        const kind = o.kind || 'unspent';
        out.push({
          piece: { offset: 0, start: sat, len: n, types: [rowId] },
          count: n,
          location: {
            kind,
            outpoint: o.op,
            offset: 0,
            satpoint: `${o.op}:0`,
            output: { outpoint: o.op, value: o.value, created_time: o.time, created_height: o.height, script_pubkey: o.script, address: null, kind: kind === 'burned' ? 'unspendable' : 'spendable', extracted: o.time >= SINCE && o.value <= 10_000 },
          },
        });
        sat += n;
      }
    }
    out.push({ piece: { offset: 7, start: sat, len: LOST[rowId], types: [rowId] }, count: LOST[rowId], location: { kind: 'lost', outpoint: `${'0'.repeat(64)}:4294967295`, offset: 7, output: null } });
    return out.sort((a, b) => a.piece.start - b.piece.start);
  };
  const rowSupply = id => tamper({ id, total: id === 'uncommon' ? 100 : 5_000_000_000, stats: { ...EXPECTED[id] }, active_non_extracted: { ...ACTIVE[id] }, delta: null });
  const handler = (req, res) => {
    const url = new URL(req.url, 'http://x');
    const send = (code, body) => {
      res.writeHead(code, { 'content-type': 'application/json' });
      res.end(JSON.stringify(body));
    };
    if (url.pathname === '/status') {
      state.statusCalls++;
      onStatus?.(state);
      return send(200, { height: state.height, extracted_max_value: 10_000, extracted_since: SINCE, lost_sats: 99 });
    }
    if (url.pathname === '/rows') {
      return send(200, [
        { id: 'uncommon', label: 'Uncommon', all_of: ['uncommon'], any_of: [], heights: null, range: false, total: 100 },
        { id: 'block9', label: 'Block 9', all_of: ['block9'], any_of: [], heights: null, range: true, total: 5_000_000_000 },
      ]);
    }
    if (url.pathname === '/supply') return send(200, { height: state.height, hash: 'h', time: TIP_TIME, rows: ['uncommon', 'block9'].map(rowSupply) });
    const m = /^\/supply\/([^/]+)$/.exec(url.pathname);
    if (m) {
      const top = Number(url.searchParams.get('top') || 100);
      const base = rowSupply(m[1]);
      const lists = tamper({ ...TOP[m[1]] }, 'top');
      return send(200, { ...base, top_extracted_holders: lists.top_extracted_holders.slice(0, top), top_non_extracted_holders: lists.top_non_extracted_holders.slice(0, top) });
    }
    if (url.pathname === '/sats') {
      res.writeHead(200, { 'content-type': 'application/x-ndjson' });
      for (const l of lines(url.searchParams.get('row'))) res.write(`${JSON.stringify(l)}\n`);
      return res.end();
    }
    send(404, { error: 'not found' });
  };
  return { handler, state };
}

async function run(mock, extra = []) {
  const server = await startServer(mock.handler);
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'supply-test-'));
  const out = path.join(dir, 'diffs.jsonl');
  const summaryJson = path.join(dir, 'summary.json');
  let stdout = '';
  try {
    const code = await main([`--ours=${server.url}`, `--out=${out}`, `--summary-json=${summaryJson}`, ...extra], {
      stdout: { write: s => (stdout += s) },
      log: { info() {}, warn() {} },
    });
    return { code, stdout, diffs: readJsonl(out), summary: JSON.parse(fs.readFileSync(summaryJson, 'utf8')) };
  } finally {
    await server.close();
  }
}

test('supply: recomputed counters equal /supply', async () => {
  const { code, stdout, diffs, summary } = await run(mockOurs(), ['--top=10']);
  assert.equal(code, 0, `${stdout}\n${JSON.stringify(diffs, null, 1)}`);
  assert.deepEqual(summary.results.map(r => r.row), ['uncommon', 'block9']);
  const readings = summary.results[0].holder_readings;
  assert.deepEqual(readings.top_non_extracted_holders, { count: 'own', last: 'any', time: 'block' });
});

test('supply: a wrong counter is reported', async () => {
  const tamper = (s, what) => {
    if (what !== 'top' && s.id === 'uncommon') s.stats.extracted += 1;
    if (what !== 'top' && s.id === 'block9') s.active_non_extracted['7y'] = 5;
    return s;
  };
  const { code, diffs } = await run(mockOurs({ tamper }), ['--rows=uncommon,block9']);
  assert.equal(code, 1);
  assert.deepEqual(
    diffs.map(d => [d.row, d.field, d.reported, d.computed]),
    [
      ['uncommon', 'extracted', 3, 2],
      ['block9', 'active_non_extracted.7y', 5, 0],
    ],
  );
});

test('supply: conservation (mined) and holder lists are checked', async () => {
  const tamper = (s, what) => {
    if (what === 'top') {
      return { ...s, top_non_extracted_holders: s.top_non_extracted_holders.map((h, i) => (i === 0 ? { ...h, utxos: 7 } : h)) };
    }
    if (s.id === 'uncommon') s.stats.mined += 5;
    return s;
  };
  const { code, diffs } = await run(mockOurs({ tamper }), ['--rows=uncommon', '--top=10']);
  assert.equal(code, 1);
  const fields = diffs.map(d => `${d.field}:${d.problem || ''}`);
  assert.ok(fields.includes('mined:'), fields.join(' '));
  assert.ok(fields.includes('top_non_extracted_holders:holder_utxos'), fields.join(' '));
});

test('supply: a height change mid-row retries the row', async () => {
  let bumped = false;
  const mock = mockOurs({
    onStatus: st => {
      if (st.statusCalls === 2 && !bumped) {
        bumped = true;
        st.height++;
      }
    },
  });
  const { code, summary } = await run(mock, ['--rows=uncommon']);
  assert.equal(code, 0);
  assert.equal(summary.results.length, 1);
  assert.equal(summary.results[0].height, 1001);
});
