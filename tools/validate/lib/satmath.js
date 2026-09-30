'use strict';

// Sat/height/epoch math (same as the `ordinals` crate) plus the handful of
// type predicates the validator needs to pick samples. All values stay below
// 2^53, so plain JS numbers are exact.

const COIN = 100_000_000;
const HALVING_INTERVAL = 210_000;
const DIFFCHANGE_INTERVAL = 2016;
const CYCLE_INTERVAL = 1_260_000;
const FIRST_POST_SUBSIDY_EPOCH = 33;
const SUPPLY = 2_099_999_997_690_000;

// ord puts lost sats here, at offset = running total of all lost sats.
const NULL_TXID = '0'.repeat(64);
const NULL_OUTPOINT = `${NULL_TXID}:4294967295`;

// Only block 9 is tracked as a whole range (sats [45e9, 50e9)); 450x is its
// first BTC.
const BLOCK9 = { start: 45_000_000_000, end: 50_000_000_000 };
const X450 = { start: 45_000_000_000, end: 45_100_000_000 };
const VINTAGE = { start: 0, end: 5_000_000_000_000 };

// BIP30: blocks 91842 and 91880 re-created the coinbase txids of 91812 and
// 91722, overwriting those outputs.
const BIP30 = [
  { height: 91812, txid: 'd5d27987d2a3dfc724e359870c6644b40e497bdc0589a033220fe15429d88599', by: 91842 },
  { height: 91722, txid: 'e3bf3d07d4b0375638d5f1db5255fe07ba2c4cb067cd81b84ee974b6585fb468', by: 91880 },
];

// bitcoin-monitoring EXOTIC_RANGES.nakamoto (blocks mined by Satoshi).
const NAKAMOTO_BLOCKS = [
  9, 286, 688, 877, 1760, 2459, 2485, 3479, 5326, 9443, 9925, 10645, 14450, 15625, 15817, 19093,
  23014, 28593, 29097,
];

// Blocks 0-5923 are Jan-Feb 2009 (isAlpha2009Sat).
const FIRST_MARCH_2009_BLOCK = 5924;

function epochSubsidy(epoch) {
  return epoch < FIRST_POST_SUBSIDY_EPOCH ? Math.floor((50 * COIN) / 2 ** epoch) : 0;
}

const EPOCH_STARTING_SATS = (() => {
  const starts = [0];
  for (let e = 0; e < FIRST_POST_SUBSIDY_EPOCH; e++) {
    starts.push(starts[e] + HALVING_INTERVAL * epochSubsidy(e));
  }
  return starts;
})();

function epochOfHeight(height) {
  return Math.floor(height / HALVING_INTERVAL);
}

function subsidy(height) {
  return epochSubsidy(epochOfHeight(height));
}

function firstSat(height) {
  const epoch = Math.min(epochOfHeight(height), FIRST_POST_SUBSIDY_EPOCH);
  if (epoch >= FIRST_POST_SUBSIDY_EPOCH) return SUPPLY;
  return EPOCH_STARTING_SATS[epoch] + (height - epoch * HALVING_INTERVAL) * epochSubsidy(epoch);
}

function epochOfSat(sat) {
  let epoch = 0;
  while (epoch + 1 < EPOCH_STARTING_SATS.length && EPOCH_STARTING_SATS[epoch + 1] <= sat) epoch++;
  return epoch;
}

function heightOfSat(sat) {
  const epoch = epochOfSat(sat);
  if (epoch >= FIRST_POST_SUBSIDY_EPOCH) throw new Error(`sat ${sat} is beyond the supply`);
  return epoch * HALVING_INTERVAL + Math.floor((sat - EPOCH_STARTING_SATS[epoch]) / epochSubsidy(epoch));
}

// Offset of the sat inside its block's subsidy.
function blockOffset(sat) {
  return sat - firstSat(heightOfSat(sat));
}

function uncommonSat(height) {
  return firstSat(height);
}

function blackSat(height) {
  return firstSat(height) + subsidy(height) - 1;
}

function isUncommon(sat) {
  return blockOffset(sat) === 0;
}

function isBlack(sat) {
  const h = heightOfSat(sat);
  return sat === firstSat(h) + subsidy(h) - 1;
}

function isAlpha(sat) {
  return sat % COIN === 0;
}

function isOmega(sat) {
  return sat % COIN === COIN - 1;
}

// Milestone tier of an alpha (sat) or omega (sat + 1): 'kilosat' (1e11) …
// 'megasat' (1e14), or null.
function milestoneTier(n) {
  if (n % 1e14 === 0) return 'megasat';
  if (n % 1e13 === 0) return 'centasat';
  if (n % 1e12 === 0) return 'decasat';
  if (n % 1e11 === 0) return 'kilosat';
  return null;
}

function isPalindrome(n) {
  const s = String(n);
  for (let i = 0, j = s.length - 1; i < j; i++, j--) if (s[i] !== s[j]) return false;
  return true;
}

function distinctDigits(n) {
  return new Set(String(n)).size;
}

// Rarity as ord names it (only uncommon and up have a SAT_TO_SATPOINT entry
// on an ord --index-sats server).
function rarity(sat) {
  if (sat === 0) return 'mythic';
  const h = heightOfSat(sat);
  if (sat !== firstSat(h)) return 'common';
  if (h % CYCLE_INTERVAL === 0) return 'legendary';
  if (h % HALVING_INTERVAL === 0) return 'epic';
  if (h % DIFFCHANGE_INTERVAL === 0) return 'rare';
  return 'uncommon';
}

function inRange(sat, range) {
  return sat >= range.start && sat < range.end;
}

function isBip30Sat(sat) {
  return BIP30.some(({ height }) => sat >= firstSat(height) && sat < firstSat(height) + subsidy(height));
}

function isBip30Outpoint(outpoint) {
  return typeof outpoint === 'string' && BIP30.some(({ txid }) => outpoint.startsWith(`${txid}:`));
}

// A sat that is certainly not tracked by any rule of DESIGN.md §1 (as scoped:
// only block 9 is a whole range). Conservative: excludes every palindrome,
// every alpha/omega, uncommon, black and block 9. Bounty3 names are ~12.5k
// scattered sats and cannot be excluded without the name list; a random pick
// hits one with probability ~6e-12.
function isCertainlyUntracked(sat) {
  if (inRange(sat, BLOCK9)) return false;
  if (isAlpha(sat) || isOmega(sat)) return false;
  if (isPalindrome(sat)) return false;
  const off = blockOffset(sat);
  if (off === 0) return false;
  if (off === subsidy(heightOfSat(sat)) - 1) return false;
  return true;
}

// Palindromes in [from, to), ascending.
function* palindromesIn(from, to) {
  if (to <= from) return;
  let len = String(Math.max(from, 0)).length;
  while (true) {
    const halfLen = Math.ceil(len / 2);
    let half = len === 1 ? 0 : 10 ** (halfLen - 1);
    const halfEnd = 10 ** halfLen;
    // Smaller leading halves give palindromes below `from`.
    if (String(from).length === len) {
      half = Math.max(half, Math.floor(from / 10 ** (len - halfLen)));
    }
    for (; half < halfEnd; half++) {
      const h = String(half);
      const mirrored = h.slice(0, len - halfLen).split('').reverse().join('');
      const p = Number(h + mirrored);
      if (p >= to) return;
      if (p >= from) yield p;
    }
    len++;
    if (10 ** (len - 1) >= to) return;
  }
}

// Every palindrome with exactly 1 or 2 distinct digits below `limit`.
function lowDigitPalindromes(limit) {
  const out = new Set();
  const maxLen = String(limit).length;
  for (let len = 1; len <= maxLen; len++) {
    const halfLen = Math.ceil(len / 2);
    for (let a = 0; a <= 9; a++) {
      for (let b = 0; b <= 9; b++) {
        if (b < a) continue;
        // Bit i of mask picks digit b (1) or a (0) for half position i.
        const combos = a === b ? 1 : 2 ** halfLen;
        for (let mask = 0; mask < combos; mask++) {
          let half = '';
          for (let i = 0; i < halfLen; i++) half += (mask >> i) & 1 ? b : a;
          if (half[0] === '0' && len > 1) continue;
          const full = half + half.slice(0, len - halfLen).split('').reverse().join('');
          const n = Number(full);
          if (n < limit && distinctDigits(n) <= 2) out.add(n);
        }
      }
    }
  }
  return [...out].sort((x, y) => x - y);
}

module.exports = {
  COIN,
  HALVING_INTERVAL,
  DIFFCHANGE_INTERVAL,
  CYCLE_INTERVAL,
  SUPPLY,
  NULL_TXID,
  NULL_OUTPOINT,
  BLOCK9,
  X450,
  VINTAGE,
  BIP30,
  NAKAMOTO_BLOCKS,
  FIRST_MARCH_2009_BLOCK,
  EPOCH_STARTING_SATS,
  epochSubsidy,
  epochOfHeight,
  subsidy,
  firstSat,
  epochOfSat,
  heightOfSat,
  blockOffset,
  uncommonSat,
  blackSat,
  isUncommon,
  isBlack,
  isAlpha,
  isOmega,
  milestoneTier,
  isPalindrome,
  distinctDigits,
  rarity,
  inRange,
  isBip30Sat,
  isBip30Outpoint,
  isCertainlyUntracked,
  palindromesIn,
  lowDigitPalindromes,
};
