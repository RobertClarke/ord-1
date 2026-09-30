'use strict';

// lib/extracted.js: the per-UTXO extraction rule.

const test = require('node:test');
const assert = require('node:assert/strict');
const { normalizeRanges, extractionBudget, isExtracted, pieceProblems } = require('../lib/extracted');

const MAX = 10_000;
const BLOCK9 = [{ id: 'block9', start: 45_000_000_000, end: 50_000_000_000 }];

test('ranges are sorted and merged when they overlap or touch', () => {
  assert.deepEqual(
    normalizeRanges([
      { start: 50, end: 60 },
      { start: 10, end: 20 },
      { start: 20, end: 30 },
      { start: 55, end: 70 },
      { start: 5, end: 5 },
    ]),
    [
      [10, 30],
      [50, 70],
    ],
  );
  assert.deepEqual(normalizeRanges([]), []);
  assert.deepEqual(normalizeRanges(undefined), []);
});

test('budget: individually tracked sats get extracted_max_value each', () => {
  const r = normalizeRanges(BLOCK9);
  assert.equal(extractionBudget([{ start: 1_000_000_000_000_000, len: 1 }], r, MAX), 10_000);
  // A collector's consolidation of three uncommon sats.
  const three = [0, 1, 2].map(i => ({ start: 1_000_000_000_000_000 + i * 1e9, len: 1 }));
  assert.equal(extractionBudget(three, r, MAX), 30_000);
  assert.equal(extractionBudget([], r, MAX), 0);
});

test('budget: in-range runs count their sats plus one allowance each', () => {
  const r = normalizeRanges(BLOCK9);
  assert.equal(extractionBudget([{ start: 45_000_000_000, len: 1_000_000 }], r, MAX), 1_010_000);
  // Two pieces (not merged: their offsets are apart) are two runs.
  assert.equal(
    extractionBudget(
      [
        { offset: 0, start: 49_999_985_000, len: 1_000 },
        { offset: 5_000, start: 49_999_986_000, len: 1_000 },
      ],
      r,
      MAX,
    ),
    22_000,
  );
  // A piece crossing the end of block 9: 5 sats in range, block 10's first
  // sat outside.
  assert.equal(extractionBudget([{ start: 49_999_999_995, len: 6 }], r, MAX), 5 + MAX + MAX);
  // Crossing the start of block 9: 3 outside (block 8's last sats), 2 in.
  assert.equal(extractionBudget([{ start: 44_999_999_997, len: 5 }], r, MAX), 3 * MAX + 2 + MAX);
});

test('budget: adjacent ranges form one run, disjoint ones two', () => {
  const adjacent = normalizeRanges([
    { start: 100, end: 200 },
    { start: 200, end: 300 },
  ]);
  assert.equal(extractionBudget([{ start: 150, len: 100 }], adjacent, MAX), 100 + MAX);
  const disjoint = normalizeRanges([
    { start: 100, end: 200 },
    { start: 210, end: 300 },
  ]);
  // 50 + 40 in range (two runs), 10 outside.
  assert.equal(extractionBudget([{ start: 150, len: 100 }], disjoint, MAX), 50 + MAX + 40 + MAX + 10 * MAX);
});

test('extracted: spendable, created since, value within the budget', () => {
  const since = 1_670_976_000;
  const base = { spendable: true, createdTime: since, value: 12_995, budget: 12_995 };
  assert.equal(isExtracted(base, { extractedSince: since }), true);
  assert.equal(isExtracted({ ...base, value: 12_996 }, { extractedSince: since }), false);
  assert.equal(isExtracted({ ...base, createdTime: since - 1 }, { extractedSince: since }), false);
  assert.equal(isExtracted({ ...base, spendable: false }, { extractedSince: since }), false);
});

test('piece lists are checked against DESIGN.md §4.1', () => {
  assert.deepEqual(
    pieceProblems(
      [
        { offset: 0, start: 10, len: 5 },
        { offset: 5, start: 100, len: 5 },
      ],
      10,
    ),
    [],
  );
  const kinds = list => pieceProblems(list, 100).map(p => p.problem);
  assert.deepEqual(
    kinds([
      { offset: 0, start: 10, len: 5 },
      { offset: 5, start: 15, len: 5 },
    ]),
    ['pieces_not_merged'],
  );
  assert.deepEqual(
    kinds([
      { offset: 0, start: 10, len: 5 },
      { offset: 3, start: 40, len: 5 },
    ]),
    ['pieces_overlap'],
  );
  assert.deepEqual(
    kinds([
      { offset: 50, start: 10, len: 5 },
      { offset: 3, start: 40, len: 5 },
    ]),
    ['pieces_not_sorted'],
  );
  assert.deepEqual(kinds([{ offset: 98, start: 10, len: 5 }]), ['piece_past_value']);
  assert.deepEqual(kinds([{ offset: 0, start: 10, len: 0 }]), ['bad_piece']);
});
