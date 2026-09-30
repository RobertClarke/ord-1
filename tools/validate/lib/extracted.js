'use strict';

// The fork's extraction rule, per UTXO and the same for every row: a
// tracked UTXO is extracted iff
//   * it is spendable,
//   * its creation block's time is ≥ extracted_since, and
//   * its value is ≤ its budget, where the budget is the sum over its
//     pieces of
//       - (sats inside the tracked ranges + extracted_max_value) for each
//         contiguous in-range run, plus
//       - extracted_max_value × sats outside the ranges.
// So three individually tracked sats get 30,000 sats of room (a collector's
// consolidation), a 1M-sat block-9 run 1,010,000.

// Ranges as sorted [start, end) pairs, overlapping or touching ones merged
// (sats of adjacent ranges form one contiguous run).
function normalizeRanges(ranges) {
  const sorted = (ranges || [])
    .map(r => (Array.isArray(r) ? [r[0], r[1]] : [r.start, r.end]))
    .filter(([s, e]) => Number.isSafeInteger(s) && Number.isSafeInteger(e) && e > s)
    .sort((a, b) => a[0] - b[0]);
  const out = [];
  for (const [s, e] of sorted) {
    const last = out[out.length - 1];
    if (last && s <= last[1]) last[1] = Math.max(last[1], e);
    else out.push([s, e]);
  }
  return out;
}

// `pieces`: [{ start, len }] of one UTXO; `ranges`: normalizeRanges output.
function extractionBudget(pieces, ranges, maxValue) {
  let budget = 0;
  for (const p of pieces) {
    const start = p.start;
    const end = p.start + p.len;
    let inside = 0;
    for (const [s, e] of ranges) {
      if (s >= end) break;
      const lo = Math.max(start, s);
      const hi = Math.min(end, e);
      if (lo < hi) {
        budget += hi - lo + maxValue;
        inside += hi - lo;
      }
    }
    budget += maxValue * (p.len - inside);
  }
  return budget;
}

function isExtracted({ spendable, createdTime, value, budget }, { extractedSince }) {
  return spendable && createdTime >= extractedSince && value <= budget;
}

// What is wrong with a UTXO's piece list (DESIGN.md §4.1: sorted by
// offset, non-overlapping, merged where offsets and sats are both
// contiguous, inside the output's value). Empty when nothing is.
function pieceProblems(pieces, value) {
  const problems = [];
  let prev = null;
  for (const p of pieces) {
    if (!Number.isSafeInteger(p.offset) || !Number.isSafeInteger(p.start) || !(p.len >= 1)) {
      problems.push({ problem: 'bad_piece', piece: p });
      continue;
    }
    if (typeof value === 'number' && p.offset + p.len > value) problems.push({ problem: 'piece_past_value', piece: p, value });
    if (prev) {
      if (p.offset < prev.offset) problems.push({ problem: 'pieces_not_sorted', piece: p, previous: prev });
      else if (p.offset < prev.offset + prev.len) problems.push({ problem: 'pieces_overlap', piece: p, previous: prev });
      else if (p.offset === prev.offset + prev.len && p.start === prev.start + prev.len) problems.push({ problem: 'pieces_not_merged', piece: p, previous: prev });
    }
    prev = p;
  }
  return problems;
}

module.exports = { normalizeRanges, extractionBudget, isExtracted, pieceProblems };
