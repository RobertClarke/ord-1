'use strict';

const sm = require('./satmath');
const { Reservoir, mapLimit, makeRng } = require('./util');

// Blocks whose fees exceeded the subsidy (bitcoind getblockstats, checked
// 2026-09-29): 409008 291.5 BTC of fees, 818087 85.2, 840000 37.6 (halving
// block, epic sat), 840001 4.5, 840002 7.0, 788695 6.7.
const FEE_HEAVY_BLOCKS = [409008, 788695, 818087, 840000, 840001, 840002];

const ALL_EDGES = ['epochs', 'block9', 'vintage', 'milestones', 'fee-heavy', 'fees', 'lost', 'burned', 'bip30', 'untracked'];

const randomSatIn = (rng, piece) => piece.start + rng.int(piece.len);

// Sat bounds implied by a type (only for types confined to known blocks).
const TYPE_BOUNDS = {
  block9: [sm.BLOCK9.start, sm.BLOCK9.end],
  '450x': [sm.X450.start, sm.X450.end],
  vintage: [sm.VINTAGE.start, sm.VINTAGE.end],
  block78: [sm.firstSat(78), sm.firstSat(79)],
  block286: [sm.firstSat(286), sm.firstSat(287)],
  block_666: [sm.firstSat(666), sm.firstSat(667)],
  nakamoto: [sm.firstSat(9), sm.firstSat(29_098)],
  alpha_jan_feb_2009: [0, sm.firstSat(sm.FIRST_MARCH_2009_BLOCK)],
};

// [from, to) holding every sat of the row, when the row's height scope or
// its all_of types confine it; else null. The fork's /sats walks every
// tracked piece in [from, to), so bounds make a row stream much cheaper.
function rowSatBounds(row) {
  let lo = 0;
  let hi = sm.SUPPLY;
  let bounded = false;
  if (Array.isArray(row.heights) && row.heights.length === 2) {
    lo = Math.max(lo, sm.firstSat(row.heights[0]));
    hi = Math.min(hi, sm.firstSat(row.heights[1] + 1));
    bounded = true;
  }
  const types = [...(row.all_of || []), ...(row.range ? [row.id] : [])];
  for (const t of types) {
    const b = TYPE_BOUNDS[t];
    if (!b) continue;
    lo = Math.max(lo, b[0]);
    hi = Math.min(hi, b[1]);
    bounded = true;
  }
  return bounded && lo < hi ? [lo, hi] : null;
}

// Streams /sats for each row: a uniform sample of `perRow` pieces (then a
// random sat inside each), every lost / burned / overwritten piece seen,
// and all pieces of the range rows (block9, 450x). Each row draws from its
// own generator (`${seed}:${row.id}`), so a seed reproduces the sample
// whatever order the streams interleave in.
async function sampleRows(ours, rows, { perRow, seed, log, parallel = 3, keepRangeRows = ['block9', '450x'] }) {
  const samples = new Map();
  const special = { lost: [], burned: [], overwritten: [] };
  const rangePieces = new Map();
  const lines = new Map();
  const errors = [];
  await mapLimit(rows, parallel, async row => {
    const rng = makeRng(`${seed}:${row.id}`);
    const reservoir = new Reservoir(perRow, rng);
    const keep = keepRangeRows.includes(row.id) ? [] : null;
    const bounds = rowSatBounds(row);
    try {
      const n = await ours.sats(row.id, line => {
        const piece = line.piece;
        if (!piece || !Number.isSafeInteger(piece.start) || !(piece.len >= 1)) return;
        reservoir.offer(piece);
        const kind = line.location?.kind;
        if (kind && kind !== 'unspent' && special[kind] && special[kind].length < 2000) special[kind].push({ row: row.id, piece });
        if (keep && keep.length < 500_000) keep.push({ start: piece.start, len: piece.len, offset: piece.offset });
      }, bounds ? { from: bounds[0], to: bounds[1] } : {});
      lines.set(row.id, n);
    } catch (err) {
      errors.push({ row: row.id, error: err.message });
      log.warn(`/sats?row=${row.id}: ${err.message}`);
    }
    const sats = [];
    for (const piece of reservoir.items) {
      sats.push(randomSatIn(rng, piece));
      if (row.range && piece.len > 1) sats.push(piece.start, piece.start + piece.len - 1);
    }
    samples.set(row.id, sats);
    if (keep) rangePieces.set(row.id, keep);
    log.info(`  row ${row.id}: ${lines.get(row.id) ?? '?'} pieces, sampled ${sats.length}`);
  });
  return { samples, special, rangePieces, lines, errors };
}

// Tracked-candidate sats inside the oracle's lost ranges. `certain` ones
// are tracked by rule (uncommon, black, block 9, vintage or milestone
// alpha/omega, Jan-Feb 2009 alpha, 1d/2d palindrome); the others depend on
// exotic-range combos.
function lostCandidates(lostIndex, { palindromesPerRange = 50 } = {}) {
  const out = [];
  for (const { start, end } of lostIndex.ranges()) {
    for (let h = sm.heightOfSat(start); h <= sm.heightOfSat(end - 1); h++) {
      const u = sm.uncommonSat(h);
      const b = sm.blackSat(h);
      if (u >= start && u < end) out.push({ sat: u, kind: 'uncommon', certain: true });
      if (b >= start && b < end) out.push({ sat: b, kind: 'black', certain: true });
    }
    const lo9 = Math.max(start, sm.BLOCK9.start);
    const hi9 = Math.min(end, sm.BLOCK9.end);
    if (lo9 < hi9) {
      out.push({ sat: lo9, kind: 'block9', certain: true });
      if (hi9 - 1 !== lo9) out.push({ sat: hi9 - 1, kind: 'block9', certain: true });
    }
    for (let a = Math.ceil(start / sm.COIN) * sm.COIN; a < end; a += sm.COIN) {
      const certain = a < sm.VINTAGE.end || !!sm.milestoneTier(a) || sm.heightOfSat(a) < sm.FIRST_MARCH_2009_BLOCK;
      out.push({ sat: a, kind: 'alpha', certain });
    }
    for (let o = Math.ceil((start + 1) / sm.COIN) * sm.COIN - 1; o < end; o += sm.COIN) {
      if (o < start) continue;
      out.push({ sat: o, kind: 'omega', certain: o < sm.VINTAGE.end || !!sm.milestoneTier(o + 1) });
    }
    let n = 0;
    for (const p of sm.palindromesIn(start, end)) {
      if (n++ >= palindromesPerRange) break;
      out.push({ sat: p, kind: 'palindrome', certain: sm.distinctDigits(p) <= 2 });
    }
  }
  return out;
}

// Edge sets. `add(sat, set, expect)` receives every pick.
async function buildEdges(ctx, add) {
  const { ours, oracle, seed, log, height, edges, rowData, feeHeavy = FEE_HEAVY_BLOCKS, feesRecentBlocks = 200 } = ctx;
  const rng = makeRng(`${seed}:edges`);
  // Whole ranges the index tracks (block 9 by default).
  const ranges = sm.rangesFromStatus(ctx.oursStatus);
  const untracked = s => sm.isCertainlyUntracked(s, ranges);
  // Moves are only kept from `moves_from` up (the fork's --moves-retention).
  const movesFrom = Number.isInteger(ctx.oursStatus?.moves_from) ? ctx.oursStatus.moves_from : 0;
  const on = name => edges.includes(name);
  const mined = sm.firstSat(height + 1);
  const both = (h, set) => {
    add(sm.uncommonSat(h), set);
    add(sm.blackSat(h), set);
  };

  if (on('epochs')) {
    const heights = new Set([0, 1, 9, 170]);
    for (let e = 0; e * sm.HALVING_INTERVAL <= height; e++) {
      const first = e * sm.HALVING_INTERVAL;
      const last = Math.min(first + sm.HALVING_INTERVAL - 1, height);
      for (const h of [first, first + 1, last, last - 1]) if (h >= 0 && h <= height) heights.add(h);
      for (const h of rng.distinct(8, first, last + 1)) heights.add(h);
    }
    // Difficulty-adjustment boundaries (rare / black_rare).
    for (const k of rng.distinct(6, 1, Math.floor(height / sm.DIFFCHANGE_INTERVAL) + 1)) {
      heights.add(k * sm.DIFFCHANGE_INTERVAL);
      heights.add(k * sm.DIFFCHANGE_INTERVAL - 1);
    }
    for (let h = Math.max(0, height - 4); h <= height; h++) heights.add(h);
    for (const h of heights) both(h, 'edge:epochs');
  }

  if (on('vintage')) {
    // Only block 9 is tracked whole; other vintage sats only when special.
    for (const h of rng.distinct(20, 0, 1000)) both(h, 'edge:vintage');
    for (const k of rng.distinct(20, 0, sm.VINTAGE.end / sm.COIN)) add(k * sm.COIN, 'edge:vintage');
    for (const k of rng.distinct(20, 1, sm.VINTAGE.end / sm.COIN + 1)) add(k * sm.COIN - 1, 'edge:vintage');
    // Jan-Feb 2009 alphas after the vintage blocks.
    for (const k of rng.distinct(20, sm.VINTAGE.end / sm.COIN, sm.firstSat(sm.FIRST_MARCH_2009_BLOCK) / sm.COIN)) add(k * sm.COIN, 'edge:vintage');
    // Palindromes of blocks 78, 286, 666 and the Nakamoto blocks.
    for (const h of [78, 286, 666, ...sm.NAKAMOTO_BLOCKS]) {
      const pals = [...sm.palindromesIn(sm.firstSat(h), sm.firstSat(h) + sm.subsidy(h))];
      for (const i of rng.distinct(Math.min(3, pals.length), 0, pals.length)) add(pals[i], 'edge:vintage');
    }
    // 1d/2d palindromes anywhere.
    const low = sm.lowDigitPalindromes(mined);
    for (const i of rng.distinct(Math.min(30, low.length), 0, low.length)) add(low[i], 'edge:vintage');
    // Plain vintage sats outside block 9 are not tracked.
    let n = 0;
    for (let guard = 0; n < 30 && guard < 100_000; guard++) {
      const s = rng.int(sm.VINTAGE.end);
      if (untracked(s)) {
        add(s, 'edge:vintage-plain', 'untracked');
        n++;
      }
    }
  }

  if (on('milestones')) {
    for (const [unit, count] of [
      [1e14, Infinity],
      [1e13, 30],
      [1e12, 15],
      [1e11, 15],
    ]) {
      const last = Math.floor((mined - 1) / unit);
      const ks = count >= last ? Array.from({ length: last }, (_, i) => i + 1) : rng.distinct(count, 1, last + 1);
      for (const k of ks) {
        add(k * unit, 'edge:milestones');
        add(k * unit - 1, 'edge:milestones');
      }
    }
    add(0, 'edge:milestones');
  }

  if (on('block9')) {
    for (const rowId of ['block9', '450x']) {
      let pieces = rowData.rangePieces.get(rowId);
      if (!pieces) {
        pieces = [];
        try {
          await ours.sats(rowId, line => {
            if (line.piece && pieces.length < 500_000) pieces.push({ start: line.piece.start, len: line.piece.len, offset: line.piece.offset });
          });
        } catch (err) {
          log.warn(`/sats?row=${rowId}: ${err.message}`);
        }
      }
      if (!pieces.length) continue;
      const set = `edge:${rowId}`;
      // Random offsets inside the largest pieces, piece boundaries, and
      // random small pieces.
      const bySize = [...pieces].sort((a, b) => b.len - a.len);
      for (const p of bySize.slice(0, 25)) {
        add(randomSatIn(rng, p), set);
        add(randomSatIn(rng, p), set);
        add(p.start, set);
        add(p.start + p.len - 1, set);
      }
      for (const i of rng.distinct(Math.min(40, pieces.length), 0, pieces.length)) {
        add(pieces[i].start, set);
        add(pieces[i].start + pieces[i].len - 1, set);
      }
    }
  }

  if (on('fee-heavy')) {
    const unretained = feeHeavy.filter(h => h <= height && h < movesFrom);
    if (unretained.length) log.info(`  fee-heavy blocks ${unretained.join(', ')} are below moves_from ${movesFrom}: only their uncommon/black sats`);
    for (const h of feeHeavy.filter(h => h <= height)) {
      both(h, 'edge:fee-heavy');
      if (h < movesFrom) continue;
      const moves = await ours.blockMoves(h).catch(() => null);
      if (!moves) {
        log.warn(`/block/${h}/moves unavailable`);
        continue;
      }
      let picked = 0;
      for (const m of moves.moves || []) {
        const pieces = m.coinbase
          ? (m.outputs || []).flatMap(o => o.pieces || [])
          : m.fee_pieces || [];
        for (const p of pieces) {
          if (picked >= 60) break;
          add(p.start, 'edge:fee-heavy');
          if (p.len > 1) add(p.start + p.len - 1, 'edge:fee-heavy');
          picked++;
        }
      }
    }
  }

  if (on('fees')) {
    // Sats that went through a fee (or got lost): every block-9 move the
    // fork kept, and all moves of the most recent blocks; plus what their
    // coinbases received from fees (pieces outside the block's subsidy).
    const seen = new Set();
    let picked = 0;
    const take = (p, cap) => {
      if (picked >= cap) return;
      add(p.start, 'edge:fees');
      if (p.len > 1) add(p.start + p.len - 1, 'edge:fees');
      picked++;
    };
    const onMove = cap => line => {
      const m = line.move || {};
      const key = `${line.height}:${m.txid}`;
      if (seen.has(key)) return;
      seen.add(key);
      for (const p of [...(m.fee_pieces || []), ...(m.lost_pieces || [])]) take(p, cap);
      if (m.coinbase) {
        const lo = sm.firstSat(line.height);
        const hi = lo + sm.subsidy(line.height);
        for (const o of m.outputs || []) for (const p of o.pieces || []) if (p.start < lo || p.start >= hi) take(p, cap);
      }
    };
    let windows = 0;
    for (let from = Math.max(0, movesFrom); from <= height; from += 10_000) {
      const to = Math.min(from + 9_999, height);
      try {
        await ours.moves(from, to, onMove(Infinity), { row: 'block9' });
        windows++;
      } catch (err) {
        log.warn(`/moves?row=block9 ${from}-${to}: ${err.message}`);
      }
    }
    const recentFrom = Math.max(movesFrom, height - feesRecentBlocks + 1, 0);
    const before = picked;
    try {
      await ours.moves(recentFrom, height, onMove(picked + 300));
    } catch (err) {
      log.warn(`/moves ${recentFrom}-${height}: ${err.message}`);
    }
    log.info(`  fees: block-9 moves from ${Math.max(0, movesFrom)} in ${windows} windows, all moves from ${recentFrom}: ${seen.size} moves, ${picked} fee/lost pieces (${picked - before} recent)`);
  }

  if (on('lost')) {
    for (const { piece } of rowData.special.lost.slice(0, 500)) {
      add(piece.start, 'edge:lost');
      if (piece.len > 1) add(piece.start + piece.len - 1, 'edge:lost');
    }
    const lost = await oracle.lostIndex();
    for (const c of lostCandidates(lost)) add(c.sat, 'edge:oracle-lost', c.certain ? 'tracked' : 'maybe');
  }

  if (on('burned')) {
    for (const { piece } of rowData.special.burned.slice(0, 500)) {
      add(piece.start, 'edge:burned');
      if (piece.len > 1) add(piece.start + piece.len - 1, 'edge:burned');
    }
  }

  if (on('bip30')) {
    for (const { height: h } of sm.BIP30) {
      both(h, 'bip30');
      add(sm.firstSat(h) + 1 + rng.int(sm.subsidy(h) - 2), 'bip30');
    }
    for (const { piece } of rowData.special.overwritten.slice(0, 200)) add(piece.start, 'bip30');
  }

  if (on('untracked')) {
    let n = 0;
    for (let guard = 0; n < 100 && guard < 100_000; guard++) {
      const s = sm.VINTAGE.end + rng.int(mined - sm.VINTAGE.end);
      if (untracked(s)) {
        add(s, 'edge:untracked', 'untracked');
        n++;
      }
    }
  }
}

module.exports = { FEE_HEAVY_BLOCKS, ALL_EDGES, sampleRows, lostCandidates, buildEdges, rowSatBounds };
