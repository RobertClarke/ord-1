'use strict';

// supply-crosscheck.js against a hand-built mock of /status, /rows,
// /supply, /supply/{row}, /sats and /output whose expected numbers were
// worked out by hand (DESIGN.md §5, per-UTXO extraction), plus deliberate
// discrepancies.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { main } = require('../supply-crosscheck');
const sm = require('../lib/satmath');
const { startServer } = require('./mocks');
const { readJsonl } = require('../lib/util');

const SINCE = 1_670_976_000;
const MAX = 10_000;
const DAY = 86_400;
const TIP_TIME = SINCE + 3 * 365 * DAY + 1000; // 3 years after
const OLD = SINCE - 10 * 365 * DAY; // long before extraction
const RECENT = TIP_TIME - 100; // extracted era, within 1y
const RANGES = [{ id: 'block9', start: 45_000_000_000, end: 50_000_000_000 }];
const NULL_OUTPOINT = `${'0'.repeat(64)}:4294967295`;

const U = h => sm.uncommonSat(h);
const B = h => sm.blackSat(h);
const op = (byte, vout = 0) => `${byte.repeat(32)}:${vout}`;

// Located outputs (spendable unless noted) with their pieces. Block 9's
// 5e9 sats: C [45e9, 49_999_960_000), D, G, H, I, lost [49_999_989_995,
// 49_999_999_995) and J [49_999_999_995, 50e9).
const OUTPUTS = [
  // A: two uncommon sats in 546 sats. Budget 20,000: extracted.
  { op: op('aa'), value: 546, time: RECENT + 5, height: 905, script: '5120aa', pieces: [{ offset: 0, start: U(700_000), len: 1 }, { offset: 100, start: U(700_001), len: 1 }] },
  // B: one uncommon sat in 50,000. Budget 10,000: not extracted.
  { op: op('bb', 1), value: 50_000, time: RECENT, height: 901, script: '5120aa', pieces: [{ offset: 0, start: U(700_002), len: 1 }] },
  // C: most of block 9 (including its uncommon sat), old: not extracted.
  { op: op('cc'), value: 5_000_000_000, time: OLD, height: 9, script: '41cc', pieces: [{ offset: 0, start: 45_000_000_000, len: 4_999_960_000 }] },
  // D: 20,000 block-9 sats, pure. Budget 30,000: extracted.
  { op: op('dd'), value: 20_000, time: TIP_TIME - 2 * 365 * DAY + 50, height: 850, script: '0014dd', pieces: [{ offset: 0, start: 49_999_960_000, len: 20_000 }] },
  // E: an uncommon sat burned in an OP_RETURN output.
  { op: op('ee'), value: 1000, time: RECENT, height: 902, script: '6a', kind: 'unspendable', pieces: [{ offset: 0, start: U(700_004), len: 1 }] },
  // F: a collector's consolidation of three uncommon sats in 25,000.
  // Budget 30,000: extracted (the per-row rule said no).
  { op: op('f1'), value: 25_000, time: RECENT + 8, height: 908, script: '5120cc', pieces: [0, 546, 1092].map((offset, i) => ({ offset, start: U(700_005 + i), len: 1 })) },
  // G: 5,000 block-9 sats and a black sat in 20,000. Budget 15,000 +
  // 10,000 = 25,000: extracted, for both rows.
  { op: op('f2'), value: 20_000, time: RECENT + 3, height: 903, script: '0014ee', pieces: [{ offset: 0, start: 49_999_980_000, len: 5_000 }, { offset: 10_000, start: B(700_003), len: 1 }] },
  // H: two block-9 runs (contiguous sats, apart offsets) in 21,500.
  // Budget 2 x 11,000 = 22,000: extracted.
  { op: op('f3'), value: 21_500, time: RECENT + 4, height: 904, script: '0014ff', pieces: [{ offset: 0, start: 49_999_985_000, len: 1_000 }, { offset: 5_000, start: 49_999_986_000, len: 1_000 }] },
  // I: 2,995 block-9 sats in exactly their budget, 12,995: extracted.
  { op: op('f4'), value: 12_995, time: RECENT + 6, height: 906, script: '0014ff', pieces: [{ offset: 0, start: 49_999_987_000, len: 2_995 }] },
  // J: block 9's last 5 sats (its black sat among them) and block 10's
  // uncommon sat in one piece, 20,006 sats. Budget (5 + 10,000) + 10,000 =
  // 20,005: not extracted.
  { op: op('f5'), value: 20_006, time: RECENT + 7, height: 907, script: '5120bb', pieces: [{ offset: 3, start: 49_999_999_995, len: 6 }] },
];
// Lost pieces (ord lost offsets).
const LOST = [
  { offset: 7, start: U(700_010), len: 1 },
  { offset: 8, start: U(700_011), len: 1 },
  { offset: 9, start: U(700_012), len: 1 },
  { offset: 100, start: 49_999_989_995, len: 10_000 },
  { offset: 20_000, start: B(700_013), len: 1 },
];

// Sats of a row in [start, start + len).
function rowCount(row, start, len) {
  const end = start + len;
  if (row === 'block9') return Math.max(0, Math.min(end, 50_000_000_000) - Math.max(start, 45_000_000_000));
  let n = 0;
  for (let h = sm.heightOfSat(start); h <= sm.heightOfSat(end - 1); h++) {
    const s = row === 'uncommon' ? U(h) : B(h);
    if (s >= start && s < end) n++;
  }
  return n;
}

// Hand-computed (see the comments above).
const EXPECTED = {
  uncommon: {
    // Located A 2, B 1, C 1, F 3, J 1; burned E; lost 3.
    mined: 12,
    located: 8,
    burned: 1,
    lost: 3,
    overwritten: 0,
    extracted: 5, // A 2 + F 3 (the per-row rule: 2)
    holders: 4, // 5120aa (A, B), 41cc, 5120cc, 5120bb
    extracted_holders: 2, // 5120aa, 5120cc
  },
  black: {
    mined: 3, // G, J, lost 1
    located: 2,
    burned: 0,
    lost: 1,
    overwritten: 0,
    extracted: 1, // G (the per-row rule: 0)
    holders: 2,
    extracted_holders: 1,
  },
  block9: {
    mined: 5_000_000_000,
    located: 4_999_960_000 + 20_000 + 5_000 + 2_000 + 2_995 + 5,
    burned: 0,
    lost: 10_000,
    overwritten: 0,
    extracted: 20_000 + 5_000 + 2_000 + 2_995, // D, G, H, I (the per-row rule: 20,000)
    holders: 5, // 41cc, 0014dd, 0014ee, 0014ff, 5120bb
    extracted_holders: 3, // 0014dd, 0014ee, 0014ff
  },
};
const windows = n => ({ '1y': n, '2y': n, '3y': n, '4y': n, '5y': n, '7y': n });
// Non-extracted located sats by creation time (C is too old for 7y).
const ACTIVE = { uncommon: windows(2), black: windows(1), block9: windows(5) };
// DESIGN.md §5: each list's n, utxos and last_received over its own outputs.
const TOP = {
  uncommon: {
    top_extracted_holders: [
      { script_pubkey: '5120cc', n: 3, utxos: 1, last_received_height: 908, last_received_time: RECENT + 8 },
      { script_pubkey: '5120aa', n: 2, utxos: 1, last_received_height: 905, last_received_time: RECENT + 5 },
    ],
    // 5120aa's latest row output (A, 905) is an extracted one.
    top_non_extracted_holders: [
      { script_pubkey: '41cc', n: 1, utxos: 1, last_received_height: 9, last_received_time: OLD },
      { script_pubkey: '5120aa', n: 1, utxos: 1, last_received_height: 901, last_received_time: RECENT },
      { script_pubkey: '5120bb', n: 1, utxos: 1, last_received_height: 907, last_received_time: RECENT + 7 },
    ],
  },
  black: {
    top_extracted_holders: [{ script_pubkey: '0014ee', n: 1, utxos: 1, last_received_height: 903, last_received_time: RECENT + 3 }],
    top_non_extracted_holders: [{ script_pubkey: '5120bb', n: 1, utxos: 1, last_received_height: 907, last_received_time: RECENT + 7 }],
  },
  block9: {
    top_extracted_holders: [
      { script_pubkey: '0014dd', n: 20_000, utxos: 1, last_received_height: 850, last_received_time: TIP_TIME - 2 * 365 * DAY + 50 },
      { script_pubkey: '0014ee', n: 5_000, utxos: 1, last_received_height: 903, last_received_time: RECENT + 3 },
      { script_pubkey: '0014ff', n: 4_995, utxos: 2, last_received_height: 906, last_received_time: RECENT + 6 },
    ],
    top_non_extracted_holders: [
      { script_pubkey: '41cc', n: 4_999_960_000, utxos: 1, last_received_height: 9, last_received_time: OLD },
      { script_pubkey: '5120bb', n: 5, utxos: 1, last_received_height: 907, last_received_time: RECENT + 7 },
    ],
  },
};
// The fork's output-level flag under the per-UTXO rule.
const FLAGS = { [op('aa')]: true, [op('bb', 1)]: false, [op('cc')]: false, [op('dd')]: true, [op('ee')]: false, [op('f1')]: true, [op('f2')]: true, [op('f3')]: true, [op('f4')]: true, [op('f5')]: false };

// `tamper(obj, what)` may rewrite a /supply row ('row'), holder lists
// ('top'), a /sats line's output ('line') or an /output answer ('output';
// null = 404).
function mockOurs({ tamper = x => x, onStatus = null } = {}) {
  const state = { height: 1000, statusCalls: 0, outputRequests: [], satsRequests: [] };
  const outputJson = (o, withPieces) => ({
    outpoint: o.op,
    value: o.value,
    address: null,
    script_pubkey: o.script,
    kind: o.kind || 'spendable',
    created_height: o.height,
    created_time: o.time,
    extracted: FLAGS[o.op],
    ...(withPieces ? { pieces: o.pieces.map(p => ({ ...p, types: [] })) } : {}),
    rows: {},
  });
  const lines = (rowId, withPieces) => {
    const out = [];
    for (const o of OUTPUTS) {
      for (const p of o.pieces) {
        const count = rowCount(rowId, p.start, p.len);
        if (!count) continue;
        const kind = o.kind === 'unspendable' ? 'burned' : 'unspent';
        const output = tamper(outputJson(o, withPieces), 'line');
        out.push({ piece: { ...p, types: [rowId] }, count, location: { kind, outpoint: o.op, offset: p.offset, satpoint: `${o.op}:${p.offset}`, output } });
      }
    }
    for (const p of LOST) {
      const count = rowCount(rowId, p.start, p.len);
      if (count) out.push({ piece: { ...p, types: [rowId] }, count, location: { kind: 'lost', outpoint: NULL_OUTPOINT, offset: p.offset, satpoint: `${NULL_OUTPOINT}:${p.offset}`, output: null } });
    }
    return out.sort((a, b) => a.piece.start - b.piece.start);
  };
  const rowSupply = id => tamper({ id, total: { uncommon: 6_930_000, black: 6_930_000, block9: 5_000_000_000 }[id], stats: { ...EXPECTED[id] }, active_non_extracted: { ...ACTIVE[id] }, delta: null }, 'row');
  const handler = (req, res) => {
    const url = new URL(req.url, 'http://x');
    const send = (code, body) => {
      res.writeHead(code, { 'content-type': 'application/json' });
      res.end(JSON.stringify(body));
    };
    if (url.pathname === '/status') {
      state.statusCalls++;
      onStatus?.(state);
      return send(200, { height: state.height, extracted_max_value: MAX, extracted_since: SINCE, ranges: RANGES, lost_sats: 99 });
    }
    if (url.pathname === '/rows') {
      return send(200, [
        { id: 'uncommon', label: 'Uncommon', all_of: ['uncommon'], any_of: [], heights: null, range: false, total: 6_930_000 },
        { id: 'black', label: 'Black', all_of: ['black'], any_of: [], heights: null, range: false, total: 6_930_000 },
        { id: 'block9', label: 'Block 9', all_of: ['block9'], any_of: [], heights: null, range: true, total: 5_000_000_000 },
      ]);
    }
    if (url.pathname === '/supply') return send(200, { height: state.height, hash: 'h', time: TIP_TIME, rows: ['uncommon', 'black', 'block9'].map(rowSupply) });
    let m = /^\/supply\/([^/]+)$/.exec(url.pathname);
    if (m) {
      const top = Number(url.searchParams.get('top') || 100);
      const lists = tamper({ ...TOP[m[1]] }, 'top');
      return send(200, { ...rowSupply(m[1]), top_extracted_holders: lists.top_extracted_holders.slice(0, top), top_non_extracted_holders: lists.top_non_extracted_holders.slice(0, top) });
    }
    if (url.pathname === '/sats') {
      const withPieces = url.searchParams.get('pieces') === 'true';
      state.satsRequests.push({ row: url.searchParams.get('row'), pieces: withPieces });
      res.writeHead(200, { 'content-type': 'application/x-ndjson' });
      for (const l of lines(url.searchParams.get('row'), withPieces)) res.write(`${JSON.stringify(l)}\n`);
      return res.end();
    }
    m = /^\/output\/([0-9a-f]{64}:\d+)$/.exec(url.pathname);
    if (m) {
      state.outputRequests.push(m[1]);
      const o = OUTPUTS.find(x => x.op === m[1]);
      const body = o ? tamper(outputJson(o, true), 'output') : null;
      return body ? send(200, body) : send(404, { error: 'output holds no tracked sat' });
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

test('supply: recomputed counters equal /supply (per-UTXO extraction)', async () => {
  const mock = mockOurs();
  const { code, stdout, diffs, summary } = await run(mock, ['--top=10']);
  assert.equal(code, 0, `${stdout}\n${JSON.stringify(diffs, null, 1)}`);
  assert.deepEqual(
    summary.results.map(r => [r.row, r.computed.extracted]),
    [
      ['uncommon', 5],
      ['black', 1],
      ['block9', 29_995],
    ],
  );
  const readings = summary.results[0].holder_readings;
  assert.deepEqual(readings.top_extracted_holders, { count: 'own', last: 'own', time: 'block' });
  assert.deepEqual(readings.top_non_extracted_holders, { count: 'own', last: 'own', time: 'block' });
  // Pieces came from /output, once per located output for all three rows.
  assert.ok(mock.state.satsRequests.every(r => !r.pieces));
  const located = OUTPUTS.filter(o => o.kind !== 'unspendable').map(o => o.op).sort();
  assert.deepEqual([...mock.state.outputRequests].sort(), located);
  assert.deepEqual(summary.extraction, { since: SINCE, max_value: MAX, ranges: RANGES, budgets_fetched: located.length, budgets_inline: 0 });
});

test('supply: --inline-pieces takes the pieces from /sats', async () => {
  const mock = mockOurs();
  const { code, diffs } = await run(mock, ['--inline-pieces']);
  assert.equal(code, 0, JSON.stringify(diffs, null, 1));
  assert.ok(mock.state.satsRequests.every(r => r.pieces));
  assert.deepEqual(mock.state.outputRequests, []);
});

test('supply: a wrong counter is reported', async () => {
  const tamper = (s, what) => {
    if (what === 'row' && s.id === 'uncommon') s.stats.extracted += 1;
    if (what === 'row' && s.id === 'block9') s.active_non_extracted['7y'] = 6;
    return s;
  };
  const { code, diffs } = await run(mockOurs({ tamper }), ['--rows=uncommon,block9']);
  assert.equal(code, 1);
  assert.deepEqual(
    diffs.map(d => [d.row, d.field, d.reported, d.computed]),
    [
      ['uncommon', 'extracted', 6, 5],
      ['block9', 'active_non_extracted.7y', 6, 5],
    ],
  );
});

test('supply: output flags and piece lists are checked', async () => {
  const tamper = (o, what) => {
    // The fork calls F not extracted (the old per-row rule would).
    if (what === 'line' && o.outpoint === op('f1')) return { ...o, extracted: false };
    // I's piece split in two contiguous halves: same budget-worthy sats, but
    // not merged as DESIGN.md §4.1 requires.
    if (what === 'output' && o.outpoint === op('f4')) {
      return { ...o, pieces: [{ offset: 0, start: 49_999_987_000, len: 1_000 }, { offset: 1_000, start: 49_999_988_000, len: 1_995 }] };
    }
    return o;
  };
  const { code, diffs } = await run(mockOurs({ tamper }), ['--rows=uncommon,block9']);
  assert.equal(code, 1);
  const flag = diffs.find(d => d.field === 'output.extracted');
  assert.deepEqual([flag.row, flag.outpoint, flag.budget, flag.extracted, flag.expected], ['uncommon', op('f1'), 30_000, false, true]);
  const pieces = diffs.find(d => d.field === 'output.pieces');
  assert.deepEqual([pieces.row, pieces.outpoint, pieces.problem], ['block9', op('f4'), 'pieces_not_merged']);
  // Nothing else differs: I is still extracted with 22,995 of budget.
  assert.deepEqual([...new Set(diffs.map(d => d.field))].sort(), ['output.extracted', 'output.pieces']);
});

test('supply: an output /sats lists but /output does not know is reported', async () => {
  const tamper = (o, what) => (what === 'output' && o.outpoint === op('dd') ? null : o);
  const { code, diffs } = await run(mockOurs({ tamper }), ['--rows=block9']);
  assert.equal(code, 1);
  const fields = diffs.map(d => `${d.field}:${d.problem || ''}`);
  assert.ok(fields.includes('output:output_not_found'), fields.join(' '));
  assert.ok(fields.includes('extracted:budget_unknown'), fields.join(' '));
});

test('supply: conservation (mined) and holder lists are checked', async () => {
  const tamper = (s, what) => {
    if (what === 'top') {
      return { ...s, top_non_extracted_holders: s.top_non_extracted_holders.map((h, i) => (i === 0 ? { ...h, utxos: 7 } : h)) };
    }
    if (what === 'row' && s.id === 'uncommon') s.stats.mined += 5;
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
      if (st.statusCalls === 3 && !bumped) {
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
