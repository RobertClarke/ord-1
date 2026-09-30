#!/usr/bin/env node
'use strict';

// Records an oracle baseline: for ~6,000 chosen sats, where the oracle (ord
// --index-sats) puts them, so the fork can be checked later even when its
// height differs from the oracle's (see oracle-check.js --baseline).
//
//   set -a; source ~/.config/ord-sats/bitcoind.env; set +a
//   node tools/validate/build-baseline.js --oracle="$ORACLE_ORD_URL" \
//        [--out=tools/validate/baseline] [--sets=a,b] [--seed=baseline-v1] [--concurrency=16] \
//        [--blocks=2000] [--block9=500] [--vintage-plain=250] [--vintage-alpha-omega=250] \
//        [--untracked=250] [--trace-block9-to=150000] [--retrace-block9]
//
// Sets: random-blocks, nakamoto, milestones, recent, fee-heavy, bip30,
// vintage-plain, vintage-alpha-omega, untracked, lost, block9. With --sets
// only those files are rewritten and meta.json is merged.
//
// The oracle's /sat only knows where uncommon-or-rarer sats are, so every
// other sat is located by scanning /output ranges: the output holding the
// same block's uncommon sat (co-location), the coinbase output that got
// the sat (bitcoind), and for block 9 the outputs a forward trace from
// block 9's coinbase reaches (lib/tracer.js, bitcoind raw blocks). Every
// recorded location is confirmed by the oracle's own /output ranges.

const fs = require('node:fs');
const path = require('node:path');
const sm = require('./lib/satmath');
const { Oracle, parseSatpoint, isUnspendableScript } = require('./lib/oracle');
const { Rpc } = require('./lib/rpc');
const { trace } = require('./lib/tracer');
const { FEE_HEAVY_BLOCKS, lostCandidates } = require('./lib/sampling');
const { parseArgs, intOpt, listOpt, makeRng, mapLimit, makeLogger } = require('./lib/util');

const ALL_SETS = ['random-blocks', 'nakamoto', 'milestones', 'recent', 'fee-heavy', 'bip30', 'vintage-plain', 'vintage-alpha-omega', 'untracked', 'lost', 'block9'];
const BLOCK9_COINBASE = '0437cd7f8525ceed2324359c2d0ba26006d92d856a9c20fa0241106ee5a597c9:0';

function usage() {
  return [
    'usage: build-baseline.js --oracle=URL [--out=DIR] [--sets=a,b] [--seed=S] [--concurrency=N]',
    '       [--blocks=2000] [--block9=500] [--vintage-plain=250] [--vintage-alpha-omega=250] [--untracked=250]',
    '       [--trace-block9-to=150000] [--retrace-block9]',
    `sets: ${ALL_SETS.join(', ')}`,
    'bitcoind credentials: ORD_BITCOIN_RPC_URL / ORD_BITCOIN_RPC_USERNAME / ORD_BITCOIN_RPC_PASSWORD',
  ].join('\n');
}

class Baseline {
  constructor({ oracle, rpc, rng, log, sets }) {
    this.oracle = oracle;
    this.rpc = rpc;
    this.rng = rng;
    this.log = log;
    this.sets = sets;
    this.entries = new Map(); // set:sat -> entry
    this.satAnswers = new Map(); // sat -> promise of the /sat answer
    this.coinbases = new Map(); // height -> promise of the coinbase layout
  }

  wants(set) {
    return this.sets.includes(set);
  }

  add(set, sat, kind, expect) {
    if (!this.wants(set)) return;
    const key = `${set}:${sat}`;
    if (!this.entries.has(key)) this.entries.set(key, { sat, set, kind, height: sm.heightOfSat(sat), expect });
  }

  satAnswer(sat) {
    if (!this.satAnswers.has(sat)) {
      this.satAnswers.set(
        sat,
        this.oracle.sat(sat, { retry500: 0 }).then(({ status, body }) =>
          status === 200
            ? { status, satpoint: body.satpoint ?? null, address: body.address ?? null, rarity: body.rarity, inscriptions: (body.inscriptions || []).length }
            : { status, error: String(body).slice(0, 120) },
        ),
      );
    }
    return this.satAnswers.get(sat);
  }

  // { txid, outputs: [{vout, value, script_pubkey, start}] } of a block's coinbase.
  coinbaseOutputs(height) {
    if (!this.coinbases.has(height)) {
      this.coinbases.set(
        height,
        (async () => {
          const hash = await this.rpc.call('getblockhash', [height]);
          const block = await this.rpc.call('getblock', [hash, 1]);
          const tx = await this.rpc.call('getrawtransaction', [block.tx[0], 1, hash]);
          let start = 0;
          return {
            txid: tx.txid,
            outputs: tx.vout.map(o => {
              const value = Math.round(o.value * 1e8);
              const out = { vout: o.n, value, script_pubkey: o.scriptPubKey.hex, start };
              start += value;
              return out;
            }),
          };
        })(),
      );
    }
    return this.coinbases.get(height);
  }
}

// Picks every set's sats except block 9 (see the header).
function chooseSats(b, { tip, lost, opts }) {
  const { rng } = b;
  const mined = sm.firstSat(tip + 1);

  for (const h of rng.distinct(intOpt(opts, 'blocks', 2000), 0, tip - 5)) {
    b.add('random-blocks', sm.uncommonSat(h), 'uncommon', 'tracked');
    b.add('random-blocks', sm.blackSat(h), 'black', 'tracked');
  }

  for (const h of sm.NAKAMOTO_BLOCKS) {
    b.add('nakamoto', sm.uncommonSat(h), 'uncommon', 'tracked');
    b.add('nakamoto', sm.blackSat(h), 'black', 'tracked');
  }

  // Milestone alphas (block starts in epochs 0-4, so uncommon too) and
  // omegas (the previous block's black sat): every mega/centasat one, a
  // sample of deca/kilosat ones.
  for (const [unit, alphas, omegas] of [
    [1e14, Infinity, Infinity],
    [1e13, Infinity, Infinity],
    [1e12, 100, 50],
    [1e11, 100, 50],
  ]) {
    const last = Math.floor((mined - 1) / unit);
    const pick = n => (n >= last ? Array.from({ length: last }, (_, i) => i + 1) : rng.distinct(n, 1, last + 1));
    if (unit === 1e14) b.add('milestones', 0, 'megasat_alpha', 'tracked');
    for (const k of pick(alphas)) b.add('milestones', k * unit, `${sm.milestoneTier(k * unit)}_alpha`, 'tracked');
    for (const k of pick(omegas)) b.add('milestones', k * unit - 1, `${sm.milestoneTier(k * unit)}_omega`, 'tracked');
  }

  for (let h = tip - 4; h <= tip; h++) {
    b.add('recent', sm.uncommonSat(h), 'uncommon', 'tracked');
    b.add('recent', sm.blackSat(h), 'black', 'tracked');
  }

  for (const h of FEE_HEAVY_BLOCKS) {
    b.add('fee-heavy', sm.uncommonSat(h), 'uncommon', 'tracked');
    b.add('fee-heavy', sm.blackSat(h), 'black', 'tracked');
  }

  // BIP30: the overwritten coinbases (known differences) and the ones that
  // overwrote them (compared normally).
  for (const { height, by } of sm.BIP30) {
    for (const h of [height, by]) {
      const overwritten = h === height;
      b.add('bip30', sm.uncommonSat(h), 'uncommon', overwritten ? 'known_diff' : 'tracked');
      b.add('bip30', sm.blackSat(h), 'black', overwritten ? 'known_diff' : 'tracked');
      b.add('bip30', sm.firstSat(h) + 1 + rng.int(sm.subsidy(h) - 2), 'plain', overwritten ? 'known_diff' : 'untracked');
    }
  }

  // Vintage (blocks 0-999) outside block 9: plain sats are not tracked;
  // alphas and omegas are (alpha+vintage, omega+vintage).
  const randomUntracked = (set, count, lo, hi) => {
    let n = 0;
    for (let guard = 0; n < count && guard < 1e6; guard++) {
      const s = lo + rng.int(hi - lo);
      if (sm.isCertainlyUntracked(s) && !b.entries.has(`${set}:${s}`)) {
        b.add(set, s, 'plain', 'untracked');
        n++;
      }
    }
  };
  randomUntracked('vintage-plain', intOpt(opts, 'vintagePlain', 250), 0, sm.VINTAGE.end);
  const ao = intOpt(opts, 'vintageAlphaOmega', 250);
  for (const k of rng.distinct(Math.ceil(ao / 2), 0, sm.VINTAGE.end / sm.COIN)) b.add('vintage-alpha-omega', k * sm.COIN, 'alpha', 'tracked');
  for (const k of rng.distinct(Math.floor(ao / 2), 1, sm.VINTAGE.end / sm.COIN + 1)) b.add('vintage-alpha-omega', k * sm.COIN - 1, 'omega', 'tracked');

  randomUntracked('untracked', intOpt(opts, 'untracked', 250), sm.VINTAGE.end, mined);

  // Lost: tracked candidates inside the oracle's lost ranges.
  for (const c of lostCandidates(lost)) b.add('lost', c.sat, c.kind, c.certain ? 'tracked' : 'maybe');
}

// Block 9: follow its coinbase forward (bitcoind raw blocks) up to
// `traceTo`, then keep the pieces whose output the oracle still indexes
// with exactly the traced offsets; plus the outputs of the address holding
// sat 45e9. Returns [{outpoint, start, len, offset}] confirmed by the oracle.
async function block9Pieces(b, { traceTo, previous = null }) {
  const { log, oracle } = b;
  const found = new Map(); // outpoint -> { pieces, value, address }
  const candidates = new Map(); // outpoint -> pieces (expected)
  const info = { traced_to: traceTo, trace: null, outputs_traced: 0, outputs_confirmed: 0, outputs_spent_later: 0, disagreements: [] };
  if (previous && previous.traced_to === traceTo && Array.isArray(previous.outputs)) {
    // An earlier trace's confirmed outputs, re-confirmed below.
    log.info(`block 9: reusing ${previous.outputs.length} outputs traced to ${traceTo} (--retrace-block9 to trace again)`);
    info.trace = previous.trace;
    info.outputs_traced = previous.outputs_traced;
    info.reused = true;
    for (const o of previous.outputs) candidates.set(o.outpoint, o.pieces);
  } else if (traceTo > 9) {
    const state = new Map([[BLOCK9_COINBASE, { value: 5e9, pieces: [{ offset: 0, start: sm.BLOCK9.start, len: 5e9 }] }]]);
    const started = Date.now();
    log.info(`block 9: tracing forward to ${traceTo}`);
    const res = await trace({ rpc: b.rpc, state, from: 10, to: traceTo, log, progressEvery: 25_000 });
    info.trace = { ...res.stats, lost_pieces: res.lost.length, seconds: Math.round((Date.now() - started) / 1000) };
    info.outputs_traced = res.state.size;
    for (const [op, v] of res.state) candidates.set(op, v.pieces);
  }
  // The output holding 45e9 and its address's other outputs.
  const first = parseSatpoint((await b.satAnswer(sm.BLOCK9.start)).satpoint);
  if (first) {
    const res = await oracle.http.get(`/output/${first.outpoint}`);
    const address = res.json?.address;
    const outs = address ? await oracle.http.get(`/outputs/${address}?type=any`, { maxBytes: 64 << 20 }) : null;
    for (const out of [res.json, ...(Array.isArray(outs?.json) ? outs.json : [])]) {
      if (!out || !Array.isArray(out.sat_ranges) || candidates.has(out.outpoint)) continue;
      let off = 0;
      const pieces = [];
      for (const [s, e] of out.sat_ranges) {
        const lo = Math.max(s, sm.BLOCK9.start);
        const hi = Math.min(e, sm.BLOCK9.end);
        if (lo < hi) pieces.push({ offset: off + (lo - s), start: lo, len: hi - lo });
        off += e - s;
      }
      if (pieces.length) candidates.set(out.outpoint, pieces);
    }
  }
  await mapLimit([...candidates.entries()], 4, async ([op, pieces]) => {
    const targets = pieces.flatMap(p => [p.start, p.start + p.len - 1]);
    const scan = await oracle.output(op, targets);
    if (scan.status !== 200 || !scan.sat_ranges) {
      info.outputs_spent_later++;
      return;
    }
    const agree = pieces.every(p => scan.offsets[p.start] === p.offset && scan.offsets[p.start + p.len - 1] === p.offset + p.len - 1);
    if (!agree) {
      info.disagreements.push({ outpoint: op, traced: pieces.slice(0, 5), oracle_windows: scan.windows });
      return;
    }
    info.outputs_confirmed++;
    found.set(op, { pieces, value: scan.value, address: scan.address });
  });
  const pieces = [...found.entries()].flatMap(([outpoint, o]) => o.pieces.map(p => ({ outpoint, ...p })));
  info.confirmed_sats = pieces.reduce((a, p) => a + p.len, 0);
  info.outputs = [...found.entries()].map(([outpoint, o]) => ({ outpoint, address: o.address, value: o.value, pieces: o.pieces }));
  if (info.disagreements.length) log.warn(`block 9: ${info.disagreements.length} traced outputs disagree with the oracle`);
  log.info(`block 9: ${found.size} outputs confirmed, ${info.confirmed_sats} sats`);
  return { pieces, info };
}

function pickBlock9(b, pieces, wanted) {
  const picks = new Set();
  const total = pieces.reduce((a, p) => a + p.len, 0);
  if (!total) return picks;
  // Every piece's first and last sat, then half by piece, half by sat.
  for (const p of pieces) {
    if (picks.size >= wanted / 4) break;
    picks.add(p.start);
    picks.add(p.start + p.len - 1);
  }
  for (let guard = 0; picks.size < wanted && guard < 1e6; guard++) {
    if (guard % 2) {
      const p = b.rng.pick(pieces);
      picks.add(p.start + b.rng.int(p.len));
    } else {
      let x = b.rng.int(total);
      for (const p of pieces) {
        if (x < p.len) {
          picks.add(p.start + x);
          break;
        }
        x -= p.len;
      }
    }
  }
  return picks;
}

async function main() {
  const opts = parseArgs(process.argv.slice(2), { booleans: ['help', 'quiet', 'retraceBlock9'] });
  if (opts.help || !opts.oracle) {
    console.error(usage());
    process.exit(opts.help ? 0 : 2);
  }
  const log = makeLogger({ quiet: opts.quiet });
  const concurrency = Math.min(intOpt(opts, 'concurrency', 16), 32);
  const outDir = opts.out || path.join(__dirname, 'baseline');
  const seed = opts.seed || 'baseline-v1';
  const sets = listOpt(opts, 'sets') || ALL_SETS;
  for (const s of sets) if (!ALL_SETS.includes(s)) throw new Error(`unknown set ${s}`);
  const oracle = new Oracle(opts.oracle, { concurrency });
  const rpc = Rpc.fromEnv({ rpcUrl: opts.rpcUrl, concurrency: 8 });
  const b = new Baseline({ oracle, rpc, rng: makeRng(seed), log, sets });

  const status0 = await oracle.status();
  const H0 = status0.height;
  const bitcoindHeight = await rpc.call('getblockcount');
  log.info(`oracle height ${H0}, lost_sats ${status0.lost_sats}; bitcoind ${bitcoindHeight}`);
  const lost = await oracle.lostIndex();
  if (lost.total !== status0.lost_sats) log.warn(`lost ranges sum ${lost.total} != status lost_sats ${status0.lost_sats}`);

  let feeHeavy = null;
  if (b.wants('fee-heavy')) {
    const stats = await rpc.batch(FEE_HEAVY_BLOCKS.map(h => ['getblockstats', [h, ['height', 'subsidy', 'totalfee']]]));
    feeHeavy = stats.map(s => ({ height: s.height, subsidy: s.subsidy, totalfee: s.totalfee }));
    for (const s of feeHeavy) if (!(s.totalfee > s.subsidy)) log.warn(`block ${s.height} is not fee-heavy (${s.totalfee} <= ${s.subsidy})`);
  }

  chooseSats(b, { tip: H0, lost, opts });

  // Block 9's confirmed outputs locate block-9 sats of every set (the
  // Nakamoto blocks, vintage alphas/omegas and the block9 set).
  let block9 = null;
  if (b.wants('block9') || [...b.entries.values()].some(e => sm.inRange(e.sat, sm.BLOCK9))) {
    const metaPath = path.join(outDir, 'meta.json');
    const previous = !opts.retraceBlock9 && fs.existsSync(metaPath) ? JSON.parse(fs.readFileSync(metaPath, 'utf8')).block9 : null;
    block9 = await block9Pieces(b, { traceTo: Math.min(intOpt(opts, 'traceBlock9To', 150_000), H0), previous });
    if (b.wants('block9')) {
      for (const s of pickBlock9(b, block9.pieces, intOpt(opts, 'block9', 500))) b.add('block9', s, sm.inRange(s, sm.X450) ? '450x' : 'block9', 'tracked');
    }
  }
  const b9ByOutpoint = new Map();
  for (const p of block9?.pieces || []) {
    if (!b9ByOutpoint.has(p.outpoint)) b9ByOutpoint.set(p.outpoint, []);
    b9ByOutpoint.get(p.outpoint).push(p);
  }

  const entries = [...b.entries.values()];
  log.info(`${entries.length} entries; querying /sat`);

  // The oracle's own /sat answer for every entry, and for the uncommon sat
  // of every entry's block (co-location hint).
  const hintSats = [...new Set(entries.map(e => sm.uncommonSat(e.height)))];
  await mapLimit([...new Set([...entries.map(e => e.sat), ...hintSats])], concurrency, s => b.satAnswer(s));

  // Candidate outpoints per entry, scanned once per outpoint for all their
  // target sats. Round 1: the sat's own satpoint (uncommon and up), the
  // output holding the same block's uncommon sat (a coinbase output usually
  // carries the whole subsidy), the confirmed block-9 outputs. Round 2
  // (only for what is still unlocated): the coinbase output that received
  // the sat, from bitcoind (multi-output coinbases).
  const candidates = new Map();
  const scans = new Map();
  const keyOf = e => `${e.set}:${e.sat}`;
  const want = (pending, e, outpoint) => {
    if (!outpoint || outpoint.startsWith(sm.NULL_TXID)) return;
    const key = keyOf(e);
    if (!candidates.has(key)) candidates.set(key, []);
    if (candidates.get(key).includes(outpoint)) return;
    candidates.get(key).push(outpoint);
    if (!pending.has(outpoint)) pending.set(outpoint, new Set());
    pending.get(outpoint).add(e.sat);
  };
  const scanAll = async (pending, label) => {
    const outpoints = [...pending.keys()];
    log.info(`${label}: scanning ${outpoints.length} outputs`);
    let done = 0;
    await mapLimit(outpoints, Math.min(concurrency, 8), async op => {
      try {
        scans.set(op, await oracle.output(op, [...pending.get(op)]));
      } catch (err) {
        scans.set(op, { error: err.message });
        log.warn(`scan ${op}: ${err.message}`);
      }
      if (++done % 250 === 0) log.info(`  ${done}/${outpoints.length}`);
    });
  };
  const locate = e => {
    for (const op of candidates.get(keyOf(e)) || []) {
      const scan = scans.get(op);
      if (!scan || scan.status !== 200 || !scan.sat_ranges) continue;
      const off = scan.offsets[e.sat];
      if (off !== null && off !== undefined) return { op, scan, off };
    }
    return null;
  };

  const round1 = new Map();
  for (const e of entries) {
    e.oracle = await b.satAnswer(e.sat);
    if (e.expect === 'untracked' || lost.offset(e.sat) !== null) continue;
    const direct = parseSatpoint(e.oracle.satpoint);
    if (direct) want(round1, e, direct.outpoint);
    const hint = parseSatpoint((await b.satAnswer(sm.uncommonSat(e.height))).satpoint);
    if (hint) want(round1, e, hint.outpoint);
    if (sm.inRange(e.sat, sm.BLOCK9)) {
      for (const [op, ps] of b9ByOutpoint) if (ps.some(p => e.sat >= p.start && e.sat < p.start + p.len)) want(round1, e, op);
    }
  }
  await scanAll(round1, 'round 1');

  const round2 = new Map();
  const unlocated = entries.filter(e => e.expect !== 'untracked' && lost.offset(e.sat) === null && !locate(e));
  log.info(`round 2: coinbase layouts for ${unlocated.length} unlocated sats`);
  await mapLimit(unlocated, 8, async e => {
    try {
      const cb = await b.coinbaseOutputs(e.height);
      const off = sm.blockOffset(e.sat);
      const out = cb.outputs.find(o => off >= o.start && off < o.start + o.value);
      if (out) want(round2, e, `${cb.txid}:${out.vout}`);
      else e.coinbase_unclaimed = true;
    } catch (err) {
      log.warn(`coinbase of ${e.height}: ${err.message}`);
    }
  });
  await scanAll(round2, 'round 2');

  // Resolve each entry's location.
  const status1 = await oracle.status();
  for (const e of entries) {
    e.oracle_height = H0;
    e.location = null;
    const direct = parseSatpoint(e.oracle.satpoint);
    const lostOffset = lost.offset(e.sat);
    if (lostOffset !== null) {
      e.location = { kind: 'lost', satpoint: `${sm.NULL_OUTPOINT}:${lostOffset}`, outpoint: sm.NULL_OUTPOINT, offset: lostOffset, how: 'lost_ranges' };
    } else if (e.expect !== 'untracked') {
      const hit = locate(e);
      if (hit) {
        const { op, scan, off } = hit;
        e.location = {
          kind: isUnspendableScript(scan.script_pubkey) ? 'burned' : 'unspent',
          satpoint: `${op}:${off}`,
          outpoint: op,
          offset: off,
          how: direct?.outpoint === op ? 'sat+output' : 'output',
          value: scan.value,
          script_pubkey: scan.script_pubkey,
          address: scan.address,
          created_height: typeof scan.confirmations === 'number' ? H0 - scan.confirmations + 1 : null,
          oracle_spent_flag: scan.spent,
        };
      } else if (direct && scans.get(direct.outpoint)?.error) {
        // Could not scan the output: keep the oracle's word for it.
        e.location = { kind: 'unspent', satpoint: e.oracle.satpoint, outpoint: direct.outpoint, offset: direct.offset, how: 'sat', address: e.oracle.address };
      }
    }
    // The oracle's /sat disagreeing with its own ranges (e.g. BIP30).
    if (direct && (!e.location || e.location.satpoint !== e.oracle.satpoint)) e.oracle_inconsistent = true;
    if (e.oracle.status === 500) e.note = 'oracle /sat answered 500';
  }

  // Write the chosen sets; merge meta.json with what is already there.
  fs.mkdirSync(outDir, { recursive: true });
  const metaPath = path.join(outDir, 'meta.json');
  const old = fs.existsSync(metaPath) && sets.length < ALL_SETS.length ? JSON.parse(fs.readFileSync(metaPath, 'utf8')) : {};
  const counts = { ...(old.counts || {}) };
  const setInfo = { ...(old.sets || {}) };
  for (const set of sets) {
    const list = entries.filter(e => e.set === set).sort((x, y) => x.sat - y.sat);
    fs.writeFileSync(path.join(outDir, `${set}.jsonl`), list.map(e => JSON.stringify(e)).join('\n') + (list.length ? '\n' : ''));
    counts[set] = {
      entries: list.length,
      located: list.filter(e => e.location).length,
      direct: list.filter(e => parseSatpoint(e.oracle.satpoint)).length,
      lost: list.filter(e => e.location?.kind === 'lost').length,
      burned: list.filter(e => e.location?.kind === 'burned').length,
      untracked: list.filter(e => e.expect === 'untracked').length,
      oracle_inconsistent: list.filter(e => e.oracle_inconsistent).length,
    };
    setInfo[set] = { oracle_height: H0, oracle_height_end: status1.height, created: new Date().toISOString(), seed };
  }
  const meta = {
    ...old,
    created: old.created || new Date().toISOString(),
    updated: new Date().toISOString(),
    seed: old.seed || seed,
    oracle_height: Math.min(old.oracle_height ?? H0, H0),
    oracle_height_end: Math.max(old.oracle_height_end ?? status1.height, status1.height),
    oracle_lost_sats: status0.lost_sats,
    lost_ranges: lost.count,
    bitcoind_height: bitcoindHeight,
    ...(feeHeavy ? { fee_heavy_blocks: feeHeavy } : {}),
    ...(block9 && b.wants('block9') ? { block9: block9.info } : {}),
    sets: setInfo,
    counts,
    oracle_http: oracle.http.stats,
  };
  delete meta.block9_outputs;
  fs.writeFileSync(metaPath, `${JSON.stringify(meta, null, 2)}\n`);
  log.info(JSON.stringify(Object.fromEntries(sets.map(s => [s, counts[s]])), null, 2));
  log.info(`oracle height ${H0} -> ${status1.height}; wrote ${entries.length} entries to ${outDir}`);
}

main().catch(err => {
  console.error(err.stack || String(err));
  process.exit(2);
});
