'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const sm = require('../lib/satmath');
const { makeRng, Reservoir } = require('../lib/util');
const { LostIndex, offsetInRanges } = require('../lib/oracle');

test('epoch starting sats match the ordinals crate', () => {
  // Values from crates/ordinals/src/epoch.rs.
  assert.equal(sm.EPOCH_STARTING_SATS[1], 1050000000000000);
  assert.equal(sm.EPOCH_STARTING_SATS[2], 1575000000000000);
  assert.equal(sm.EPOCH_STARTING_SATS[10], 2097949218750000);
  assert.equal(sm.EPOCH_STARTING_SATS[11], 2098974609270000);
  assert.equal(sm.EPOCH_STARTING_SATS[32], 2099999997480000);
  assert.equal(sm.EPOCH_STARTING_SATS[33], sm.SUPPLY);
});

test('first sat, subsidy and height round trip', () => {
  assert.equal(sm.firstSat(9), 45_000_000_000);
  assert.equal(sm.firstSat(210_000), 1_050_000_000_000_000);
  assert.equal(sm.firstSat(840_000), 1_968_750_000_000_000);
  assert.equal(sm.subsidy(839_999), 625_000_000);
  assert.equal(sm.subsidy(840_000), 312_500_000);
  assert.equal(sm.blackSat(124_724), 623_624_999_999_999);
  assert.equal(sm.uncommonSat(501_726), 1_677_157_500_000_000);
  assert.equal(sm.firstSat(33 * 210_000), sm.SUPPLY);
  const rng = makeRng('rt');
  for (let i = 0; i < 2000; i++) {
    const h = rng.int(6_929_999);
    const s = sm.firstSat(h) + rng.int(sm.subsidy(h));
    assert.equal(sm.heightOfSat(s), h);
    assert.equal(sm.heightOfSat(sm.firstSat(h)), h);
    assert.equal(sm.heightOfSat(sm.blackSat(h)), h);
  }
});

test('rarity and predicates', () => {
  assert.equal(sm.rarity(0), 'mythic');
  assert.equal(sm.rarity(sm.firstSat(1_260_000)), 'legendary');
  assert.equal(sm.rarity(sm.firstSat(840_000)), 'epic');
  assert.equal(sm.rarity(sm.firstSat(2016 * 400)), 'rare');
  assert.equal(sm.rarity(sm.firstSat(9)), 'uncommon');
  assert.equal(sm.rarity(sm.firstSat(9) + 1), 'common');
  assert.ok(sm.isBlack(sm.blackSat(840_000)));
  assert.equal(sm.milestoneTier(1e14), 'megasat');
  assert.equal(sm.milestoneTier(3e13), 'centasat');
  assert.equal(sm.milestoneTier(7e11), 'kilosat');
  assert.equal(sm.milestoneTier(7e8), null);
  assert.ok(sm.isBip30Sat(sm.firstSat(91_812) + 5));
  assert.ok(!sm.isBip30Sat(sm.firstSat(91_842)));
  assert.ok(!sm.isCertainlyUntracked(46_000_000_123)); // block 9
  assert.ok(!sm.isCertainlyUntracked(12_345_654_321)); // palindrome
  assert.ok(!sm.isCertainlyUntracked(7 * sm.COIN)); // alpha
  assert.ok(sm.isCertainlyUntracked(12_345_678_901));
});

test('palindrome enumeration', () => {
  const brute = (a, b) => {
    const out = [];
    for (let n = a; n < b; n++) if (sm.isPalindrome(n)) out.push(n);
    return out;
  };
  for (const [a, b] of [
    [0, 1000],
    [95, 1234],
    [9_990, 100_200],
    [123_456, 124_000],
  ]) {
    assert.deepEqual([...sm.palindromesIn(a, b)], brute(a, b));
  }
  const big = [...sm.palindromesIn(1_677_157_500_000_000, 1_677_158_750_000_000)];
  assert.ok(big.length > 0);
  for (const p of big) assert.ok(sm.isPalindrome(p));
  const low = sm.lowDigitPalindromes(1_000_000);
  assert.deepEqual(
    low,
    brute(0, 1_000_000).filter(n => sm.distinctDigits(n) <= 2),
  );
});

test('lost index and range offsets', () => {
  const idx = LostIndex.fromRanges([
    [100, 101],
    [50, 60],
    [200, 250],
  ]);
  assert.equal(idx.total, 61);
  assert.equal(idx.offset(100), 0);
  assert.equal(idx.offset(50), 1);
  assert.equal(idx.offset(59), 10);
  assert.equal(idx.offset(200), 11);
  assert.equal(idx.offset(249), 60);
  assert.equal(idx.offset(60), null);
  assert.equal(offsetInRanges([[10, 20], [5, 6], [30, 40]], 5), 10);
  assert.equal(offsetInRanges([[10, 20], [5, 6], [30, 40]], 35), 16);
  assert.equal(offsetInRanges(null, 35), null);
});

test('reservoir sampling is uniform enough', () => {
  const rng = makeRng('res');
  const hits = new Array(10).fill(0);
  for (let t = 0; t < 4000; t++) {
    const r = new Reservoir(2, rng);
    for (let i = 0; i < 10; i++) r.offer(i);
    for (const x of r.items) hits[x]++;
  }
  for (const h of hits) assert.ok(h > 650 && h < 950, `bucket ${h}`);
});
