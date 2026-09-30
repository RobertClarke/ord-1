'use strict';

const sm = require('./satmath');
const { parseSatpoint, isUnspendableScript } = require('./oracle');
const { mapLimit } = require('./util');

// Expectation precedence when a sat is in several sets.
const EXPECT_RANK = { known_diff: 4, tracked: 3, untracked: 2, maybe: 1 };

// sat -> { sat, sets: Set, expect }
class SampleSet {
  constructor() {
    this.map = new Map();
  }

  add(sat, set, expect = 'tracked') {
    if (!Number.isSafeInteger(sat) || sat < 0) return;
    // BIP30-overwritten sats are reported apart, whatever set picked them.
    if (sm.isBip30Sat(sat)) expect = 'known_diff';
    let s = this.map.get(sat);
    if (!s) {
      s = { sat, sets: new Set(), expect };
      this.map.set(sat, s);
    }
    s.sets.add(set);
    if (EXPECT_RANK[expect] > EXPECT_RANK[s.expect]) s.expect = expect;
  }

  get size() {
    return this.map.size;
  }

  list() {
    return [...this.map.values()].sort((a, b) => a.sat - b.sat);
  }
}

function compactOutput(out) {
  if (!out || typeof out !== 'object') return out ?? null;
  const pieces = Array.isArray(out.pieces) ? out.pieces : [];
  return {
    outpoint: out.outpoint,
    value: out.value,
    address: out.address ?? null,
    script_pubkey: out.script_pubkey,
    kind: out.kind,
    created_height: out.created_height,
    extracted: out.extracted,
    piece_count: pieces.length,
    pieces: pieces.slice(0, 20),
  };
}

function compactOurs(res) {
  if (!res) return null;
  if (res.status !== 200 || typeof res.body !== 'object' || res.body === null) {
    return { http_status: res.status, body: typeof res.body === 'string' ? res.body.slice(0, 200) : res.body };
  }
  const b = res.body;
  const loc = b.location;
  return {
    tracked: b.tracked,
    height: b.height,
    types: b.types,
    rows: b.rows,
    location: loc
      ? {
          kind: loc.kind,
          satpoint: loc.satpoint,
          outpoint: loc.outpoint,
          offset: loc.offset,
          output: compactOutput(loc.output),
        }
      : null,
  };
}

function compactScan(scan, sat) {
  if (!scan) return null;
  if (scan.error) return { error: scan.error };
  if (scan.status !== 200) return { status: scan.status };
  return {
    outpoint: scan.outpoint,
    value: scan.value,
    address: scan.address,
    script_pubkey: scan.script_pubkey,
    spent: scan.spent,
    indexed: scan.indexed,
    confirmations: scan.confirmations,
    sat_ranges: scan.sat_ranges && {
      count: scan.sat_ranges.count,
      total: scan.sat_ranges.total,
      ...(scan.sat_ranges.ranges ? { ranges: scan.sat_ranges.ranges } : {}),
    },
    sat_offset: sat === undefined ? undefined : scan.offsets?.[sat] ?? null,
    window: sat === undefined ? undefined : scan.windows?.[sat],
  };
}

// Live comparison: ours and the oracle at the same height.
class LiveChecker {
  constructor({ ours, oracle, log, concurrency = 16 }) {
    this.ours = ours;
    this.oracle = oracle;
    this.log = log;
    this.concurrency = concurrency;
  }

  async scan(outpoint, targets) {
    try {
      return await this.oracle.output(outpoint, targets);
    } catch (err) {
      return { error: err.message };
    }
  }

  // Resolves every sample to a result:
  // { sat, sets, expect, status: match|mismatch|known_diff|skip|error,
  //   reasons: [], method, ours, oracle, context }
  async check(samples) {
    const log = this.log;
    const results = new Map();
    const oursAnswers = new Map();
    const oracleAnswers = new Map();

    log.info(`querying ours /sat for ${samples.length} sats`);
    await mapLimit(samples, this.concurrency * 2, async s => {
      try {
        oursAnswers.set(s.sat, await this.ours.sat(s.sat));
      } catch (err) {
        oursAnswers.set(s.sat, { status: 'error', body: err.message });
      }
    });

    const needOracle = samples.filter(s => {
      if (s.expect === 'untracked') return false;
      const o = oursAnswers.get(s.sat);
      return s.expect !== 'maybe' || (o.status === 200 && o.body?.tracked);
    });
    const lost = await this.oracle.lostIndex();
    log.info(`querying oracle /sat for ${needOracle.length} sats`);
    await mapLimit(needOracle, this.concurrency, async s => {
      try {
        // A lost uncommon sat makes ord answer 500: don't retry those.
        const retry500 = lost.offset(s.sat) === null ? 1 : 0;
        oracleAnswers.set(s.sat, await this.oracle.sat(s.sat, { retry500 }));
      } catch (err) {
        oracleAnswers.set(s.sat, { status: 'error', body: err.message });
      }
    });

    // Claims to verify against the oracle's /output ranges, grouped by
    // outpoint so each output is streamed once.
    const claims = new Map();
    const claim = (outpoint, sat) => {
      if (!outpoint || outpoint.startsWith(sm.NULL_TXID)) return;
      if (!claims.has(outpoint)) claims.set(outpoint, new Set());
      claims.get(outpoint).add(sat);
    };
    for (const s of needOracle) {
      const o = oursAnswers.get(s.sat);
      const loc = o.status === 200 ? o.body?.location : null;
      if (!loc) continue;
      const orc = oracleAnswers.get(s.sat);
      const direct = orc?.status === 200 ? parseSatpoint(orc.body?.satpoint) : null;
      if (direct && direct.outpoint === loc.outpoint && direct.offset === loc.offset) continue; // direct match
      if (loc.kind !== 'lost') claim(loc.outpoint, s.sat);
      // A direct mismatch also needs the oracle's own output for context.
      if (direct) claim(direct.outpoint, s.sat);
    }
    log.info(`scanning ${claims.size} oracle outputs`);
    const scans = new Map();
    let done = 0;
    await mapLimit([...claims.keys()], Math.min(this.concurrency, 8), async op => {
      scans.set(op, await this.scan(op, [...claims.get(op)]));
      if (++done % 500 === 0) log.info(`  ${done}/${claims.size}`);
    });

    for (const s of samples) {
      results.set(s.sat, evaluateLive(s, oursAnswers.get(s.sat), oracleAnswers.get(s.sat), { lost, scans }));
    }
    return results;
  }
}

function evaluateLive(sample, oursRes, oracleRes, { lost, scans }) {
  const r = {
    sat: sample.sat,
    sets: [...sample.sets],
    expect: sample.expect,
    status: 'match',
    reasons: [],
    method: null,
    ours: compactOurs(oursRes),
    oracle: null,
    context: {},
  };
  const fail = reason => {
    r.status = 'mismatch';
    r.reasons.push(reason);
  };

  if (!oursRes || oursRes.status === 'error' || (oursRes.status !== 200 && oursRes.status !== 404)) {
    r.status = 'error';
    r.reasons.push('ours_error');
    return r;
  }
  const ours = oursRes.status === 200 ? oursRes.body : { tracked: false, location: null };
  const tracked = ours.tracked === true;

  if (sample.expect === 'untracked') {
    r.method = 'untracked';
    if (tracked || ours.location) fail('tracked_but_expected_untracked');
    return r;
  }
  if (!tracked) {
    r.method = 'untracked';
    if (sample.expect === 'tracked') fail('untracked_but_expected_tracked');
    else r.status = sample.expect === 'known_diff' ? 'known_diff' : 'skip';
    return r;
  }

  if (oracleRes && oracleRes.status === 'error') {
    r.status = 'error';
    r.reasons.push('oracle_error');
    r.oracle = { error: oracleRes.body };
    return r;
  }
  const orcBody = oracleRes?.status === 200 && typeof oracleRes.body === 'object' ? oracleRes.body : null;
  r.oracle = {
    http_status: oracleRes?.status ?? null,
    satpoint: orcBody?.satpoint ?? null,
    address: orcBody?.address ?? null,
    rarity: orcBody?.rarity ?? null,
    inscriptions: orcBody ? (orcBody.inscriptions || []).length : null,
  };
  const loc = ours.location;
  const lostOffset = lost.offset(sample.sat);
  if (lostOffset !== null) r.context.oracle_lost_offset = lostOffset;

  if (!loc) {
    fail('tracked_without_location');
    return finalizeKnown(r, sample);
  }
  // BIP30: sats of the overwritten coinbases (91812, 91722) are known to
  // differ. Sats of 91842 / 91880 live at the same outpoints and are
  // compared like any other.
  if (loc.kind === 'overwritten') {
    r.method = 'bip30';
    if (sample.expect === 'known_diff') r.status = 'known_diff';
    else fail('unexpected_overwritten');
    return r;
  }

  const oursOutput = loc.output || null;
  const direct = orcBody ? parseSatpoint(orcBody.satpoint) : null;
  // Our own answer must be coherent: burned exactly when our output's
  // script is unspendable.
  if (oursOutput && typeof oursOutput.script_pubkey === 'string' && (loc.kind === 'burned') !== isUnspendableScript(oursOutput.script_pubkey)) {
    fail('kind_inconsistent_with_script');
  }

  if (direct && !direct.outpoint.startsWith(sm.NULL_TXID)) {
    r.method = 'direct';
    if (loc.satpoint !== orcBody.satpoint) {
      // The oracle's SAT_TO_SATPOINT can be stale (BIP30); its ranges are
      // the ground truth, so accept ours if the ranges agree with it.
      const mine = scans.get(loc.outpoint);
      const theirs = scans.get(direct.outpoint);
      r.context.oracle_output_for_ours = compactScan(mine, sample.sat);
      r.context.oracle_output_for_oracle = compactScan(theirs, sample.sat);
      const mineOk = mine?.status === 200 && mine.offsets?.[sample.sat] === loc.offset;
      const theirsOk = theirs?.status === 200 && theirs.offsets?.[sample.sat] === direct.offset;
      if (mineOk && !theirsOk) {
        r.method = 'direct_stale_oracle';
        r.context.note = "the oracle's /sat satpoint is not backed by its own ranges; ours is";
      } else fail('satpoint_differs');
    }
    if (r.status === 'match' && oursOutput?.address && orcBody.address && oursOutput.address !== orcBody.address) {
      fail('address_differs');
    }
    return finalizeKnown(r, sample);
  }

  if (loc.kind === 'lost') {
    r.method = 'lost_ranges';
    const expected = lostOffset === null ? null : `${sm.NULL_OUTPOINT}:${lostOffset}`;
    r.oracle.satpoint_derived = expected;
    if (lostOffset === null) fail('not_lost_in_oracle');
    else if (loc.outpoint !== sm.NULL_OUTPOINT || loc.offset !== lostOffset || loc.satpoint !== expected) fail('lost_offset_differs');
    return finalizeKnown(r, sample);
  }

  // Claim: the sat is at loc.outpoint:loc.offset. The oracle's /output
  // ranges (present only while the output is unspent, or for OP_RETURN)
  // must put it exactly there.
  r.method = 'output_ranges';
  const scan = scans.get(loc.outpoint);
  r.context.oracle_output_for_ours = compactScan(scan, sample.sat);
  if (!scan || scan.error) {
    r.status = 'error';
    r.reasons.push('oracle_output_error');
    return r;
  }
  if (scan.status === 404) fail('claim_output_unknown');
  else if (!scan.sat_ranges) fail('claim_output_spent');
  else {
    const off = scan.offsets?.[sample.sat];
    if (off === null || off === undefined) fail('claim_sat_not_in_output');
    else if (off !== loc.offset) fail('claim_offset_differs');
    if (loc.satpoint !== `${loc.outpoint}:${loc.offset}`) fail('satpoint_malformed');
    if (oursOutput) {
      if (oursOutput.script_pubkey !== undefined && scan.script_pubkey !== undefined && oursOutput.script_pubkey !== scan.script_pubkey) fail('script_differs');
      if (oursOutput.value !== undefined && scan.value !== undefined && oursOutput.value !== scan.value) fail('value_differs');
      if (oursOutput.address && scan.address && oursOutput.address !== scan.address) fail('address_differs');
    }
    const unspendable = isUnspendableScript(scan.script_pubkey);
    if ((loc.kind === 'burned') !== unspendable) fail('kind_differs');
    if (scan.spent && !unspendable) r.context.note = 'oracle flags the output spent (mempool?) but still indexes it';
    r.oracle.satpoint_derived = off === null || off === undefined ? null : `${loc.outpoint}:${off}`;
  }
  if (lostOffset !== null) fail('oracle_has_it_lost');
  return finalizeKnown(r, sample);
}

function finalizeKnown(r, sample) {
  if (sample.expect === 'known_diff') r.status = 'known_diff';
  return r;
}

// Baseline comparison: ours at any height H1 against oracle answers
// recorded at H0, for sats whose recorded output provably still holds them
// at H1 (bitcoind gettxout: unspent now and created at or before H1).
class BaselineChecker {
  constructor({ ours, oracle = null, rpc, log, concurrency = 16 }) {
    this.ours = ours;
    this.oracle = oracle;
    this.rpc = rpc;
    this.log = log;
    this.concurrency = concurrency;
  }

  // Unspent-at-tip test for outpoints, returning outpoint -> { unspent,
  // created } (created height from confirmations).
  async utxoInfo(outpoints) {
    const list = [...outpoints];
    const replies = await this.rpc.batch(
      list.map(op => {
        const [txid, vout] = op.split(':');
        return ['gettxout', [txid, Number(vout), false]];
      }),
    );
    // Creation height = height(bestblock) - confirmations + 1, with the
    // bestblock each reply was computed against.
    const best = new Map();
    for (const r of replies) if (r && !r.error && r.bestblock) best.set(r.bestblock, null);
    for (const hash of best.keys()) best.set(hash, (await this.rpc.call('getblockheader', [hash])).height);
    const info = new Map();
    list.forEach((op, i) => {
      const r = replies[i];
      if (r && r.error) info.set(op, { error: r.error.message });
      else if (r === null) info.set(op, { unspent: false });
      else info.set(op, { unspent: true, created: best.get(r.bestblock) - r.confirmations + 1 });
    });
    const tip = best.size ? Math.max(...best.values()) : await this.rpc.call('getblockcount');
    return { tip, info };
  }

  async check(entries, { height, lostSats }) {
    const log = this.log;
    const results = new Map();
    const ours = new Map();
    log.info(`baseline: ${entries.length} entries against ours at height ${height}`);
    await mapLimit(entries, this.concurrency * 2, async e => {
      try {
        ours.set(e.sat, await this.ours.sat(e.sat));
      } catch (err) {
        ours.set(e.sat, { status: 'error', body: err.message });
      }
    });

    // Recorded outputs that must still exist unspent at `height`.
    const outpoints = new Set();
    for (const e of entries) {
      if (e.location && e.location.kind === 'unspent') outpoints.add(e.location.outpoint);
    }
    // Ours' claims, for entries the baseline could not locate (checked live
    // against the oracle's ranges when the claimed output is provably
    // unchanged since `height`).
    const claimOutpoints = new Set();
    for (const e of entries) {
      if (e.location || e.expect === 'untracked' || e.expect === 'known_diff') continue;
      const loc = ours.get(e.sat)?.body?.location;
      if (loc && loc.kind === 'unspent' && loc.outpoint) claimOutpoints.add(loc.outpoint);
    }
    const { tip, info } = await this.utxoInfo(new Set([...outpoints, ...(this.oracle ? claimOutpoints : [])]));
    log.info(`bitcoind tip ${tip}; ${outpoints.size} baseline outputs, ${claimOutpoints.size} claim outputs`);

    // Live claim checks against the oracle.
    const scans = new Map();
    if (this.oracle) {
      const byOp = new Map();
      for (const e of entries) {
        if (e.location || e.expect === 'untracked' || e.expect === 'known_diff') continue;
        const loc = ours.get(e.sat)?.body?.location;
        if (!loc || loc.kind !== 'unspent') continue;
        const u = info.get(loc.outpoint);
        if (!u?.unspent || u.created > height) continue;
        if (!byOp.has(loc.outpoint)) byOp.set(loc.outpoint, new Set());
        byOp.get(loc.outpoint).add(e.sat);
      }
      await mapLimit([...byOp.keys()], Math.min(this.concurrency, 8), async op => {
        try {
          scans.set(op, await this.oracle.output(op, [...byOp.get(op)]));
        } catch (err) {
          scans.set(op, { error: err.message });
        }
      });
    }

    for (const e of entries) {
      results.set(`${e.set}:${e.sat}`, evaluateBaseline(e, ours.get(e.sat), { height, lostSats, info, scans }));
    }
    return { results, tip };
  }
}

function evaluateBaseline(e, oursRes, { height, lostSats, info, scans }) {
  const r = {
    sat: e.sat,
    sets: [e.set],
    expect: e.expect,
    status: 'match',
    reasons: [],
    method: null,
    ours: compactOurs(oursRes),
    baseline: { oracle_height: e.oracle_height, location: e.location, oracle: e.oracle },
    context: {},
  };
  const fail = reason => {
    r.status = 'mismatch';
    r.reasons.push(reason);
  };
  const skip = why => {
    r.status = 'skip';
    r.reasons.push(why);
    return r;
  };
  if (!oursRes || oursRes.status === 'error' || (oursRes.status !== 200 && oursRes.status !== 404)) {
    r.status = 'error';
    r.reasons.push('ours_error');
    return r;
  }
  if (e.height > height) return skip('not_mined_at_our_height');
  const ours = oursRes.status === 200 ? oursRes.body : { tracked: false, location: null };
  const loc = ours.location;

  if (e.expect === 'untracked') {
    r.method = 'untracked';
    if (ours.tracked || loc) fail('tracked_but_expected_untracked');
    return r;
  }
  if (e.expect === 'known_diff') {
    r.method = 'bip30';
    r.status = 'known_diff';
    return r;
  }
  if (!ours.tracked) {
    r.method = 'untracked';
    if (e.expect === 'tracked') fail('untracked_but_expected_tracked');
    else r.status = 'skip';
    return r;
  }
  const b = e.location;
  if (b && b.kind === 'lost') {
    r.method = 'baseline_lost';
    if (typeof lostSats !== 'number') return skip('ours_lost_sats_unknown');
    if (b.offset >= lostSats) return skip('not_lost_yet_at_our_height');
    if (!loc || loc.kind !== 'lost' || loc.satpoint !== b.satpoint) fail('lost_satpoint_differs');
    return r;
  }
  if (b && b.kind === 'burned') {
    r.method = 'baseline_burned';
    if (typeof b.created_height === 'number' && b.created_height > height) return skip('not_created_at_our_height');
    if (!loc || loc.satpoint !== b.satpoint || loc.kind !== 'burned') fail('satpoint_differs');
    return r;
  }
  if (b) {
    r.method = 'baseline';
    const u = info.get(b.outpoint);
    r.context.utxo = u;
    if (!u || u.error) return skip('gettxout_failed');
    if (!u.unspent) return skip('baseline_output_spent_since');
    if (u.created > height) return skip('not_created_at_our_height');
    if (!loc || loc.satpoint !== b.satpoint) fail('satpoint_differs');
    else if (loc.kind !== 'unspent') fail('kind_differs');
    const oursAddr = loc?.output?.address;
    if (r.status === 'match' && oursAddr && b.address && oursAddr !== b.address) fail('address_differs');
    return r;
  }
  // Unlocated in the baseline: verify ours' claim live.
  r.method = 'claim';
  if (!loc) {
    fail('tracked_without_location');
    return r;
  }
  if (loc.kind !== 'unspent') return skip(`claim_kind_${loc.kind}`);
  const u = info.get(loc.outpoint);
  r.context.utxo = u;
  if (!u || u.error) return skip('gettxout_failed');
  if (!u.unspent) return skip('claim_output_spent_since');
  if (u.created > height) {
    fail('claim_output_created_after_our_height');
    return r;
  }
  const scan = scans.get(loc.outpoint);
  if (!scan) return skip('no_oracle');
  r.context.oracle_output_for_ours = compactScan(scan, e.sat);
  if (scan.error) {
    r.status = 'error';
    r.reasons.push('oracle_output_error');
    return r;
  }
  if (scan.status === 404 || !scan.sat_ranges) return skip('oracle_has_not_indexed_it');
  const off = scan.offsets?.[e.sat];
  if (off === null || off === undefined) fail('claim_sat_not_in_output');
  else if (off !== loc.offset) fail('claim_offset_differs');
  return r;
}

module.exports = { SampleSet, LiveChecker, BaselineChecker, evaluateLive, evaluateBaseline, compactScan, compactOurs };
