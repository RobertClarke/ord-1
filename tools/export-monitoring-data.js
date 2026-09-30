#!/usr/bin/env node
/**
 * Exports the rare-sat rule data of bitcoin-monitoring into the files the
 * tracker embeds, plus JS-generated test vectors.
 *
 * Usage: node tools/export-monitoring-data.js <path-to-bitcoin-monitoring>
 *
 * Reads (never writes) the bitcoin-monitoring checkout:
 *   src/services/exoticSatRanges.js  EXOTIC_RANGES (require()d)
 *   src/services/satRarityRules.js   the rule functions (require()d)
 *   data/bounty3-names-map.json      sat -> bounty3 name
 *   src/utils/psbtBuilder.js         SPECIAL_SAT_TYPES / REQUIRED_COMBINATION_TYPES,
 *                                    parsed from the source text (requiring it
 *                                    validates the runtime's .env)
 *
 * Writes, relative to this repository:
 *
 * data/exotic-ranges.bin, all integers little-endian:
 *   magic   8 bytes  "ORDEXRNG"
 *   version u32      1
 *   types   u32      number of type records
 *   per type, in EXOTIC_RANGES key order:
 *     name_len u8, name (ASCII)
 *     count    u32
 *     count x (start u64, end u64): half-open [start, end) sat ranges, sorted,
 *              overlapping and adjacent source ranges merged. The JS ranges
 *              are inclusive [start, end]; end here is the JS end + 1.
 * data/exotic-ranges.json: manifest (per-type source/merged range counts and
 *   sat totals, the .bin's sha256 and the source file's sha256).
 *
 * data/bounty3-names.bin:
 *   magic   8 bytes  "ORDB3NAM"
 *   version u32      1
 *   count   u32
 *   count x u64      sats, ascending
 *   count x (len u8, name UTF-8), in the same order
 * data/bounty3-names.json: manifest (count, sha256s).
 *
 * tests/fixtures/sat-types.json: test vectors. For every listed sat, the set
 *   of types the runtime's scanner gives that sat as a point sat
 *   (rareSatScanner.analyzeSatRanges): findSpecialSatsInRangesBatch rule
 *   types, plus bounty3_name, plus every EXOTIC_RANGES type containing the
 *   sat, then normalizePaliblockTypes over the combined list; the synthetic
 *   alpha_jan_feb_2009 (isAlpha2009Sat) is added afterwards. Stored as
 *   [sat, mask] pairs, bit i of mask = type_names[i]. Also carries the
 *   psbtBuilder.js arrays for the Rust drift check.
 *
 * Deterministic: the sat sample uses a fixed-seed PRNG, so re-running on the
 * same sources reproduces the files byte for byte.
 */

'use strict';

const crypto = require('crypto');
const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const SUPPLY = 2099999997690000n;
const COIN = 100000000n;
const HALVING = 210000;

const root = process.argv[2];
if (!root) {
  console.error('usage: node tools/export-monitoring-data.js <path-to-bitcoin-monitoring>');
  process.exit(2);
}
const monitoring = path.resolve(root);
const repo = path.resolve(__dirname, '..');

const SOURCES = {
  exotic: 'src/services/exoticSatRanges.js',
  rules: 'src/services/satRarityRules.js',
  bounty3: 'data/bounty3-names-map.json',
  psbt: 'src/utils/psbtBuilder.js',
};

function sha256(buffer) {
  return crypto.createHash('sha256').update(buffer).digest('hex');
}

function sourceHash(relative) {
  return sha256(fs.readFileSync(path.join(monitoring, relative)));
}

function monitoringCommit() {
  try {
    return execFileSync('git', ['-C', monitoring, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  } catch (_) {
    return null;
  }
}

const { EXOTIC_RANGES, getExoticRangeIndex, findOverlapWindow } = require(
  path.join(monitoring, SOURCES.exotic)
);
const rules = require(path.join(monitoring, SOURCES.rules));
const {
  findSpecialSatsInRangesBatch,
  getSatTypes,
  normalizePaliblockTypes,
  isAlpha2009Sat,
  getFirstSatOfBlock,
  getLastSatOfBlock,
  generatePalindromesInRange,
} = rules;
const bounty3Map = JSON.parse(fs.readFileSync(path.join(monitoring, SOURCES.bounty3), 'utf8'));

// ---------------------------------------------------------------------------
// psbtBuilder.js arrays, parsed from the source text.

function parseArrayLiteral(source, name) {
  const marker = `const ${name} = [`;
  const start = source.indexOf(marker);
  if (start < 0) {
    throw new Error(`${name} not found in ${SOURCES.psbt}`);
  }
  let depth = 0;
  let end = -1;
  for (let i = start + marker.length - 1; i < source.length; i++) {
    const c = source[i];
    if (c === '[') depth++;
    if (c === ']') {
      depth--;
      if (depth === 0) {
        end = i + 1;
        break;
      }
    }
  }
  if (end < 0) {
    throw new Error(`unterminated ${name}`);
  }
  const literal = source
    .slice(start + marker.length - 1, end)
    .replace(/\/\/[^\n]*/g, '')
    .replace(/'/g, '"')
    .replace(/,(\s*\])/g, '$1');
  return JSON.parse(literal);
}

const psbtSource = fs.readFileSync(path.join(monitoring, SOURCES.psbt), 'utf8');
const SPECIAL_SAT_TYPES = parseArrayLiteral(psbtSource, 'SPECIAL_SAT_TYPES');
const REQUIRED_COMBINATION_TYPES = parseArrayLiteral(psbtSource, 'REQUIRED_COMBINATION_TYPES');
const HISTORIC_BLOCK_TYPES = parseArrayLiteral(psbtSource, 'HISTORIC_BLOCK_TYPES');

// ---------------------------------------------------------------------------
// Exotic ranges.

function mergeRanges(ranges) {
  // Inclusive JS ranges -> sorted, merged half-open ranges.
  const sorted = ranges
    .map(([start, end]) => [BigInt(start), BigInt(end) + 1n])
    .sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
  const merged = [];
  for (const [start, end] of sorted) {
    if (end <= start) {
      throw new Error(`empty or inverted range [${start}, ${end})`);
    }
    const last = merged[merged.length - 1];
    if (last && start <= last[1]) {
      if (end > last[1]) last[1] = end;
    } else {
      merged.push([start, end]);
    }
  }
  return merged;
}

const exoticTypes = Object.keys(EXOTIC_RANGES);
const mergedExotic = {};
for (const name of exoticTypes) {
  mergedExotic[name] = mergeRanges(EXOTIC_RANGES[name]);
}

function u32(n) {
  const b = Buffer.alloc(4);
  b.writeUInt32LE(n);
  return b;
}

function u64(n) {
  const b = Buffer.alloc(8);
  b.writeBigUInt64LE(BigInt(n));
  return b;
}

function writeExoticRanges() {
  const parts = [Buffer.from('ORDEXRNG', 'ascii'), u32(1), u32(exoticTypes.length)];
  const manifestTypes = [];
  let totalSats = 0n;
  for (const name of exoticTypes) {
    const merged = mergedExotic[name];
    if (name.length > 255 || !/^[\x21-\x7e]+$/.test(name)) {
      throw new Error(`bad type name ${name}`);
    }
    parts.push(Buffer.from([name.length]), Buffer.from(name, 'ascii'), u32(merged.length));
    const body = Buffer.alloc(merged.length * 16);
    let sats = 0n;
    merged.forEach(([start, end], i) => {
      body.writeBigUInt64LE(start, i * 16);
      body.writeBigUInt64LE(end, i * 16 + 8);
      sats += end - start;
    });
    parts.push(body);
    totalSats += sats;
    manifestTypes.push({
      name,
      source_ranges: EXOTIC_RANGES[name].length,
      ranges: merged.length,
      sats: Number(sats),
      first: merged.length ? Number(merged[0][0]) : null,
      end: merged.length ? Number(merged[merged.length - 1][1]) : null,
    });
  }
  const bin = Buffer.concat(parts);
  fs.writeFileSync(path.join(repo, 'data/exotic-ranges.bin'), bin);
  const manifest = {
    format: 'ORDEXRNG v1: see tools/export-monitoring-data.js',
    generated_by: 'tools/export-monitoring-data.js',
    source: {
      file: SOURCES.exotic,
      sha256: sourceHash(SOURCES.exotic),
      commit: monitoringCommit(),
    },
    bin: { file: 'data/exotic-ranges.bin', bytes: bin.length, sha256: sha256(bin) },
    total_sats: Number(totalSats),
    types: manifestTypes,
  };
  fs.writeFileSync(
    path.join(repo, 'data/exotic-ranges.json'),
    JSON.stringify(manifest, null, 2) + '\n'
  );
  return manifest;
}

// ---------------------------------------------------------------------------
// Bounty3 names.

const bounty3 = Object.entries(bounty3Map)
  .map(([sat, name]) => [BigInt(sat), name])
  .sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
const bounty3Set = new Set(bounty3.map(([sat]) => sat));

function writeBounty3() {
  const parts = [Buffer.from('ORDB3NAM', 'ascii'), u32(1), u32(bounty3.length)];
  for (let i = 0; i < bounty3.length; i++) {
    if (i > 0 && bounty3[i][0] === bounty3[i - 1][0]) {
      throw new Error(`duplicate bounty3 sat ${bounty3[i][0]}`);
    }
    parts.push(u64(bounty3[i][0]));
  }
  for (const [sat, name] of bounty3) {
    const bytes = Buffer.from(String(name), 'utf8');
    if (bytes.length === 0 || bytes.length > 255) {
      throw new Error(`bad bounty3 name for ${sat}`);
    }
    parts.push(Buffer.from([bytes.length]), bytes);
  }
  const bin = Buffer.concat(parts);
  fs.writeFileSync(path.join(repo, 'data/bounty3-names.bin'), bin);
  const manifest = {
    format: 'ORDB3NAM v1: see tools/export-monitoring-data.js',
    generated_by: 'tools/export-monitoring-data.js',
    source: {
      file: SOURCES.bounty3,
      sha256: sourceHash(SOURCES.bounty3),
      commit: monitoringCommit(),
    },
    bin: { file: 'data/bounty3-names.bin', bytes: bin.length, sha256: sha256(bin) },
    count: bounty3.length,
    first: Number(bounty3[0][0]),
    last: Number(bounty3[bounty3.length - 1][0]),
  };
  fs.writeFileSync(
    path.join(repo, 'data/bounty3-names.json'),
    JSON.stringify(manifest, null, 2) + '\n'
  );
  return manifest;
}

// ---------------------------------------------------------------------------
// The runtime scanner's types for one point sat.

const rangeIndexes = {};
for (const name of exoticTypes) {
  rangeIndexes[name] = getExoticRangeIndex(EXOTIC_RANGES[name]);
}

// Exotic membership exactly as rareSatScanner's applyExoticTypesAndBuildLargeRanges
// finds it: the overlap window for [sat, sat + 1), then a per-range check.
function exoticTypesOf(sat) {
  const types = [];
  for (const name of exoticTypes) {
    const index = rangeIndexes[name];
    if (index.ranges.length === 0) continue;
    const [from, to] = findOverlapWindow(index, sat, sat + 1n);
    for (let i = from; i < to; i++) {
      const [start, end] = index.ranges[i];
      if (start <= sat && sat <= end) {
        types.push(name);
        break;
      }
    }
  }
  return types;
}

function scannerTypes(sat) {
  const special = findSpecialSatsInRangesBatch([[sat, sat]]);
  const types = special.has(sat) ? [...special.get(sat)] : [];
  if (bounty3Set.has(sat) && !types.includes('bounty3_name')) {
    types.push('bounty3_name');
  }
  for (const name of exoticTypesOf(sat)) {
    if (!types.includes(name)) types.push(name);
  }
  normalizePaliblockTypes(types);
  if (isAlpha2009Sat(sat)) {
    types.push('alpha_jan_feb_2009');
  }
  return types;
}

// ---------------------------------------------------------------------------
// Sat selection.

let rngState = 0x243f6a8885a308d3n;
function nextRandom() {
  // splitmix64
  rngState = (rngState + 0x9e3779b97f4a7c15n) & 0xffffffffffffffffn;
  let z = rngState;
  z = ((z ^ (z >> 30n)) * 0xbf58476d1ce4e5b9n) & 0xffffffffffffffffn;
  z = ((z ^ (z >> 27n)) * 0x94d049bb133111ebn) & 0xffffffffffffffffn;
  return z ^ (z >> 31n);
}
function randomBelow(n) {
  return nextRandom() % BigInt(n);
}
function randomIn(start, end) {
  // [start, end)
  return BigInt(start) + randomBelow(BigInt(end) - BigInt(start));
}
function sample(array, count) {
  if (array.length <= count) return array.slice();
  const picked = [];
  const step = array.length / count;
  for (let i = 0; i < count; i++) {
    const lo = Math.floor(i * step);
    const hi = Math.max(lo + 1, Math.floor((i + 1) * step));
    picked.push(array[lo + Number(randomBelow(hi - lo))]);
  }
  return picked;
}

const selected = new Set();
const reasons = {};
function add(reason, ...sats) {
  for (const s of sats) {
    const sat = BigInt(s);
    if (sat < 0n || sat >= SUPPLY) continue;
    if (!selected.has(sat)) {
      selected.add(sat);
      reasons[reason] = (reasons[reason] || 0) + 1;
    }
  }
}
function addAround(reason, sat, radius = 1) {
  for (let d = -radius; d <= radius; d++) add(reason, BigInt(sat) + BigInt(d));
}

function isPalindromeNumber(n) {
  const s = n.toString();
  return s === s.split('').reverse().join('');
}

const LAST_BLOCK = 6929999;

function addBlock(reason, height) {
  const first = getFirstSatOfBlock(height);
  const last = getLastSatOfBlock(height);
  addAround(reason, first, 2);
  addAround(reason, last, 2);
}

// 1. First/last sats of selected blocks, every epoch boundary and every
//    difficulty adjustment, Nakamoto/JPEG blocks.
const namedBlocks = [
  0, 1, 2, 8, 9, 10, 11, 77, 78, 79, 285, 286, 287, 665, 666, 667, 688, 877, 998, 999, 1000,
  1001, 2015, 2016, 2017, 4031, 4032, 5923, 5924, 32489, 32490, 91722, 91812, 91842, 91880,
  209999, 210000, 210001, 419999, 420000, 630000, 840000, 1049999, 1050000, 1259999, 1260000,
  1260001, 2519999, 2520000, 6719999, 6720000, 6720001, 6929998, 6929999,
];
for (const h of namedBlocks) addBlock('named-block', h);
for (let e = 1; e <= 33; e++) {
  addBlock('epoch-boundary', e * HALVING - 1);
  if (e * HALVING <= LAST_BLOCK) addBlock('epoch-boundary', e * HALVING);
}
for (let h = 0; h <= LAST_BLOCK; h += 2016) {
  add('difficulty-adjustment', getFirstSatOfBlock(h));
  if (h > 0) add('difficulty-adjustment', getLastSatOfBlock(h - 1));
}
for (const [start] of mergedExotic.nakamoto) {
  const h = Number(start / 5000000000n);
  addBlock('nakamoto-block', h);
}

// 2. Every palindromic block (paliblock), both boundary sats.
const palindromicHeights = [];
for (let h = 0; h <= LAST_BLOCK; h++) {
  if (isPalindromeNumber(h)) palindromicHeights.push(h);
}
for (const h of palindromicHeights) {
  add('palindromic-block', getFirstSatOfBlock(h), getLastSatOfBlock(h));
}
// Epoch 32 blocks are one sat each: uncommon and black at once.
for (let i = 0; i < 1500; i++) {
  const h = 32 * HALVING + Number(randomBelow(LAST_BLOCK - 32 * HALVING + 1));
  add('epoch-32', getFirstSatOfBlock(h));
}
for (let d = 1n; d <= 5n; d++) add('supply-end', SUPPLY - d);

// 3. Alphas and omegas across every milestone tier.
const tiers = [
  ['alpha-1e8', COIN, 3000],
  ['alpha-kilosat', COIN * 1000n, 3000],
  ['alpha-decasat', COIN * 10000n, 2100],
  ['alpha-centasat', COIN * 100000n, 210],
  ['alpha-megasat', COIN * 1000000n, 21],
];
for (const [reason, modulus, count] of tiers) {
  const multiples = SUPPLY / modulus;
  for (let i = 0; i < count; i++) {
    const k = count >= Number(multiples) ? BigInt(i) : randomBelow(multiples + 1n);
    const sat = k * modulus;
    add(reason, sat, sat - 1n);
    if (i % 8 === 0) add(reason, sat + 1n, sat - 2n);
  }
}
const FIRST_MARCH_2009_SAT = getFirstSatOfBlock(5924);
addAround('alpha-2009-boundary', FIRST_MARCH_2009_SAT, 2);
addAround('alpha-2009-boundary', FIRST_MARCH_2009_SAT - COIN, 2);
for (let i = 0; i < 2000; i++) {
  const sat = randomBelow(FIRST_MARCH_2009_SAT / COIN) * COIN;
  add('alpha-2009', sat, sat - 1n);
}

// 4. Palindromes of every kind.
function palindromesOfLength(length, halves) {
  const half = Math.ceil(length / 2);
  const result = [];
  for (const h of halves) {
    const s = h.toString();
    if (s.length !== half) continue;
    const mirror = s.slice(0, length % 2 === 1 ? -1 : undefined).split('').reverse().join('');
    const pal = BigInt(s + mirror);
    if (pal < SUPPLY) result.push(pal);
  }
  return result;
}
function distinctDigits(n) {
  return new Set(n.toString()).size;
}
// All 1-digit/2-digit-alphabet palindromes below the supply, found by
// enumerating digit patterns.
for (let length = 1; length <= 16; length++) {
  const half = Math.ceil(length / 2);
  for (let a = 0; a <= 9; a++) {
    for (let b = a; b <= 9; b++) {
      const digits = a === b ? [a] : [a, b];
      const patterns = digits.length === 1 ? 1 : 1 << half;
      for (let p = 0; p < patterns; p++) {
        let s = '';
        for (let i = 0; i < half; i++) {
          s += digits.length === 1 ? digits[0] : digits[(p >> (half - 1 - i)) & 1];
        }
        if (s[0] === '0') continue;
        const [pal] = palindromesOfLength(length, [BigInt(s)]);
        if (pal !== undefined && distinctDigits(pal) <= 2) add('palindrome-1d-2d', pal);
      }
    }
  }
}
// Perfect palinceptions: a palindrome with >= 2 distinct digits repeated.
for (let length = 4; length <= 16; length++) {
  for (let sub = 3; sub <= length / 2; sub++) {
    if (length % sub !== 0) continue;
    const subHalf = Math.ceil(sub / 2);
    for (let h = 10 ** (subHalf - 1); h < 10 ** subHalf; h++) {
      const [subPal] = palindromesOfLength(sub, [BigInt(h)]);
      const subStr = subPal === undefined ? null : subPal.toString();
      if (subStr === null || subStr.length !== sub || new Set(subStr).size < 2) continue;
      const full = BigInt(subStr.repeat(length / sub));
      if (full < SUPPLY) add('palinception', full);
    }
  }
}
// Other palindromes (3+ distinct digits), odd and even lengths.
for (let length = 1; length <= 16; length++) {
  const half = Math.ceil(length / 2);
  const lo = 10n ** BigInt(half - 1);
  const hi = 10n ** BigInt(half);
  for (let i = 0; i < 300; i++) {
    const [pal] = palindromesOfLength(length, [lo + randomBelow(hi - lo)]);
    if (pal !== undefined) add('palindrome-random', pal, pal + 1n);
  }
}
// Around every digit-length boundary.
for (let k = 1n; k <= 16n; k++) {
  const p = 10n ** k;
  for (const d of [-12n, -11n, -2n, -1n, 0n, 1n, 2n, 11n, 12n]) add('digit-boundary', p + d);
}

// 5. Exotic range boundaries (every range except the Silk Road auction's,
//    which are sampled) and sats inside the ranges.
for (const name of exoticTypes) {
  const source = EXOTIC_RANGES[name];
  const merged = mergedExotic[name];
  const pickedSource = name === 'sr_1st_auction' ? sample(source, 4000) : source;
  for (const [start, end] of pickedSource) {
    add(`exotic-boundary-${name}`, start - 1n, start, end, end + 1n);
  }
  const pickedMerged = name === 'sr_1st_auction' ? sample(merged, 1000) : merged;
  for (const [start, end] of pickedMerged) {
    add(`exotic-boundary-${name}`, start - 1n, start, end - 1n, end);
  }
}
for (const [start, end] of mergedExotic.sr_1st_auction.filter((_, i, all) =>
  i > 0 && all[i - 1][1] === _[0]
)) {
  add('exotic-sr-adjacent', start - 1n, start, end);
}
function specialsInside(name, perRangeLimit, rangeSample) {
  const ranges = rangeSample ? sample(mergedExotic[name], rangeSample) : mergedExotic[name];
  for (const [start, end] of ranges) {
    // Alphas, omegas and block boundaries inside the range.
    let alpha = ((start + COIN - 1n) / COIN) * COIN;
    let n = 0;
    for (; alpha < end && n < perRangeLimit; alpha += COIN, n++) {
      add(`inside-${name}`, alpha, alpha - 1n);
    }
    // Palindromes inside the range.
    const pals = generatePalindromesInRange(start, end - 1n);
    for (const pal of sample(pals, perRangeLimit)) add(`inside-${name}`, pal);
    // Random sats inside the range.
    for (let i = 0; i < 2; i++) add(`inside-${name}`, randomIn(start, end));
  }
}
specialsInside('nakamoto', 300);
specialsInside('jpeg', 300);
specialsInside('pizza', 12);
specialsInside('hitman', 3, 3000);
specialsInside('sr_1st_auction', 2, 6000);
specialsInside('block9', 2000);
specialsInside('block78', 400);
specialsInside('block286', 400);
specialsInside('block_666', 400);
// Blacks inside Silk Road auction ranges (the black + sr combination).
{
  let found = 0;
  for (const [start, end] of mergedExotic.sr_1st_auction) {
    const { block } = rules.getBlockForSat(end - 1n);
    const last = getLastSatOfBlock(block);
    if (last >= start && last < end) {
      add('sr-black', last, last + 1n);
      found++;
    }
  }
  reasons['sr-black-found'] = found;
}

// 6. Every bounty3 sat (and neighbours of a sample).
for (const [sat] of bounty3) add('bounty3', sat);
for (const [sat] of sample(bounty3, 1000)) add('bounty3-neighbour', sat - 1n, sat + 1n);

// 7. Random sats.
for (let i = 0; i < 20000; i++) add('random', randomBelow(SUPPLY));
for (let i = 0; i < 6000; i++) add('random-vintage', randomBelow(5000000000000n));
for (let i = 0; i < 4000; i++) add('random-2009', randomBelow(getFirstSatOfBlock(32490)));

// ---------------------------------------------------------------------------
// Fixture.

function writeFixture() {
  const sats = [...selected].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  const typeNames = [];
  const typeIndex = new Map();
  const lines = [];
  let getSatTypesDiffers = 0;
  const differing = [];
  for (const sat of sats) {
    const types = scannerTypes(sat);
    let mask = 0n;
    for (const type of types) {
      if (!typeIndex.has(type)) {
        typeIndex.set(type, typeNames.length);
        typeNames.push(type);
      }
      mask |= 1n << BigInt(typeIndex.get(type));
    }
    lines.push(`[${sat},${mask}]`);

    // Diagnostic only: where the plain getSatTypes() + exotic + bounty3
    // composition would differ from the scanner's.
    const plain = getSatTypes(sat);
    if (bounty3Set.has(sat)) plain.push('bounty3_name');
    plain.push(...exoticTypesOf(sat));
    normalizePaliblockTypes(plain);
    if (isAlpha2009Sat(sat)) plain.push('alpha_jan_feb_2009');
    const a = [...new Set(plain)].sort().join(',');
    const b = [...new Set(types)].sort().join(',');
    if (a !== b) {
      getSatTypesDiffers++;
      if (differing.length < 10) differing.push({ sat: sat.toString(), getSatTypes: a, scanner: b });
    }
  }
  if (typeNames.length > 52) {
    throw new Error('too many types for a JSON-safe mask');
  }
  const header = {
    generated_by: 'tools/export-monitoring-data.js',
    description:
      'Types the runtime scanner gives each sat as a point sat: findSpecialSatsInRangesBatch ' +
      'rule types + bounty3_name + EXOTIC_RANGES membership, normalizePaliblockTypes over ' +
      'the combined list, then alpha_jan_feb_2009 (isAlpha2009Sat). sats: [sat, mask] with ' +
      'bit i of mask = type_names[i].',
    sources: Object.fromEntries(
      Object.entries(SOURCES).map(([key, file]) => [key, { file, sha256: sourceHash(file) }])
    ),
    commit: monitoringCommit(),
    special_sat_types: SPECIAL_SAT_TYPES,
    required_combination_types: REQUIRED_COMBINATION_TYPES,
    historic_block_types: HISTORIC_BLOCK_TYPES,
    type_names: typeNames,
    reasons,
    get_sat_types_differs: { count: getSatTypesDiffers, examples: differing },
  };
  const json =
    JSON.stringify(header, null, 1).replace(/\n}$/, '') +
    ',\n "sats": [\n' +
    lines.join(',\n') +
    '\n ]\n}\n';
  JSON.parse(json);
  fs.mkdirSync(path.join(repo, 'tests/fixtures'), { recursive: true });
  fs.writeFileSync(path.join(repo, 'tests/fixtures/sat-types.json'), json);
  return { count: sats.length, bytes: json.length, getSatTypesDiffers, differing, typeNames };
}

fs.mkdirSync(path.join(repo, 'data'), { recursive: true });
const exotic = writeExoticRanges();
const names = writeBounty3();
const fixture = writeFixture();

console.log(
  JSON.stringify(
    {
      exotic_ranges: {
        bytes: exotic.bin.bytes,
        types: exotic.types.map(t => `${t.name}:${t.ranges}`).join(' '),
      },
      bounty3: { count: names.count, bytes: names.bin.bytes },
      fixture: {
        sats: fixture.count,
        bytes: fixture.bytes,
        types: fixture.typeNames.length,
        get_sat_types_differs: fixture.getSatTypesDiffers,
        examples: fixture.differing,
      },
      reasons,
    },
    null,
    2
  )
);
