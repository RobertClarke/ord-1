'use strict';

const fs = require('node:fs');

// --key=value / --flag / positional arguments.
function parseArgs(argv, { booleans = [] } = {}) {
  const opts = { _: [] };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (!arg.startsWith('--')) {
      opts._.push(arg);
      continue;
    }
    const eq = arg.indexOf('=');
    const key = (eq === -1 ? arg.slice(2) : arg.slice(2, eq)).replace(/-([a-z0-9])/g, (_, c) => c.toUpperCase());
    if (eq !== -1) opts[key] = arg.slice(eq + 1);
    else if (booleans.includes(key) || i + 1 >= argv.length || argv[i + 1].startsWith('--')) opts[key] = true;
    else opts[key] = argv[++i];
  }
  return opts;
}

function intOpt(opts, key, fallback) {
  if (opts[key] === undefined || opts[key] === true) return fallback;
  const n = Number(opts[key]);
  if (!Number.isFinite(n)) throw new Error(`--${key} must be a number, got ${opts[key]}`);
  return n;
}

function listOpt(opts, key) {
  if (opts[key] === undefined || opts[key] === true) return null;
  return String(opts[key])
    .split(',')
    .map(s => s.trim())
    .filter(Boolean);
}

// xoshiro128** seeded from a string (cyrb128), with 53-bit floats.
function makeRng(seed) {
  let h1 = 1779033703, h2 = 3144134277, h3 = 1013904242, h4 = 2773480762;
  const str = String(seed);
  for (let i = 0; i < str.length; i++) {
    const k = str.charCodeAt(i);
    h1 = h2 ^ Math.imul(h1 ^ k, 597399067);
    h2 = h3 ^ Math.imul(h2 ^ k, 2869860233);
    h3 = h4 ^ Math.imul(h3 ^ k, 951274213);
    h4 = h1 ^ Math.imul(h4 ^ k, 2716044179);
  }
  h1 = Math.imul(h3 ^ (h1 >>> 18), 597399067);
  h2 = Math.imul(h4 ^ (h2 >>> 22), 2869860233);
  h3 = Math.imul(h1 ^ (h3 >>> 17), 951274213);
  h4 = Math.imul(h2 ^ (h4 >>> 19), 2716044179);
  let a = (h1 ^ h2 ^ h3 ^ h4) >>> 0, b = (h2 ^ h1) >>> 0, c = (h3 ^ h1) >>> 0, d = (h4 ^ h1) >>> 0;
  function next32() {
    const t = b << 9;
    let r = Math.imul(b, 5);
    r = Math.imul((r << 7) | (r >>> 25), 9);
    c ^= a;
    d ^= b;
    b ^= c;
    a ^= d;
    c ^= t;
    d = (d << 11) | (d >>> 21);
    return r >>> 0;
  }
  for (let i = 0; i < 16; i++) next32();
  const rng = {
    float() {
      return ((next32() >>> 5) * 67108864 + (next32() >>> 6)) / 9007199254740992;
    },
    // Integer in [0, n).
    int(n) {
      return Math.floor(rng.float() * n);
    },
    // Integer in [lo, hi).
    range(lo, hi) {
      return lo + rng.int(hi - lo);
    },
    pick(array) {
      return array[rng.int(array.length)];
    },
    shuffle(array) {
      for (let i = array.length - 1; i > 0; i--) {
        const j = rng.int(i + 1);
        [array[i], array[j]] = [array[j], array[i]];
      }
      return array;
    },
    // k distinct integers from [lo, hi).
    distinct(k, lo, hi) {
      const out = new Set();
      const span = hi - lo;
      if (k >= span) {
        for (let i = lo; i < hi; i++) out.add(i);
        return [...out];
      }
      while (out.size < k) out.add(lo + rng.int(span));
      return [...out];
    },
  };
  return rng;
}

// Uniform sample of `size` items from a stream of unknown length.
class Reservoir {
  constructor(size, rng) {
    this.size = size;
    this.rng = rng;
    this.items = [];
    this.seen = 0;
  }

  offer(item) {
    this.seen++;
    if (this.items.length < this.size) this.items.push(item);
    else {
      const j = this.rng.int(this.seen);
      if (j < this.size) this.items[j] = item;
    }
  }
}

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

// Runs at most `max` async jobs at once.
class Limiter {
  constructor(max) {
    this.max = Math.max(1, max);
    this.active = 0;
    this.queue = [];
  }

  run(fn) {
    return new Promise((resolve, reject) => {
      this.queue.push({ fn, resolve, reject });
      this.pump();
    });
  }

  pump() {
    while (this.active < this.max && this.queue.length) {
      const { fn, resolve, reject } = this.queue.shift();
      this.active++;
      Promise.resolve()
        .then(fn)
        .then(resolve, reject)
        .finally(() => {
          this.active--;
          this.pump();
        });
    }
  }
}

// Maps `items` through async `fn` with bounded concurrency, preserving order.
async function mapLimit(items, concurrency, fn) {
  const results = new Array(items.length);
  let next = 0;
  async function worker() {
    while (next < items.length) {
      const i = next++;
      results[i] = await fn(items[i], i);
    }
  }
  await Promise.all(Array.from({ length: Math.min(concurrency, items.length) }, worker));
  return results;
}

function makeLogger({ quiet = false, stream = process.stderr } = {}) {
  return {
    info: (...args) => {
      if (!quiet) stream.write(`${args.join(' ')}\n`);
    },
    warn: (...args) => stream.write(`warning: ${args.join(' ')}\n`),
  };
}

class JsonlWriter {
  constructor(path) {
    this.path = path;
    this.fd = path ? fs.openSync(path, 'w') : null;
    this.count = 0;
  }

  write(obj) {
    this.count++;
    if (this.fd !== null) fs.writeSync(this.fd, `${JSON.stringify(obj)}\n`);
  }

  close() {
    if (this.fd !== null) fs.closeSync(this.fd);
    this.fd = null;
  }
}

function readJsonl(path) {
  return fs
    .readFileSync(path, 'utf8')
    .split('\n')
    .filter(line => line.trim())
    .map(line => JSON.parse(line));
}

function percentile(sorted, p) {
  if (!sorted.length) return NaN;
  const i = Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1));
  return sorted[i];
}

module.exports = {
  parseArgs,
  intOpt,
  listOpt,
  makeRng,
  Reservoir,
  sleep,
  Limiter,
  mapLimit,
  makeLogger,
  JsonlWriter,
  readJsonl,
  percentile,
};
