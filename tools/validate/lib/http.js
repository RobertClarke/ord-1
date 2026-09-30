'use strict';

const readline = require('node:readline');
const { Readable } = require('node:stream');
const { Limiter, sleep } = require('./util');

const RETRY_STATUS = new Set([429, 502, 503, 504]);

class HttpError extends Error {
  constructor(message, { status = null, url = null, body = null, retryable = false } = {}) {
    super(message);
    this.status = status;
    this.url = url;
    this.body = body;
    this.retryable = retryable;
  }
}

// Reads a fetch body as text, refusing to buffer more than `maxBytes`.
async function readCapped(res, maxBytes) {
  const reader = res.body.getReader();
  const chunks = [];
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.byteLength;
    if (size > maxBytes) {
      await reader.cancel().catch(() => {});
      throw new HttpError(`response larger than ${maxBytes} bytes`, { status: res.status });
    }
    chunks.push(value);
  }
  return Buffer.concat(chunks).toString('utf8');
}

// A JSON-over-HTTP client with bounded concurrency and a shared backoff:
// after a transient failure every request of the client waits, so a
// struggling server sees less traffic, not retries piling up.
class JsonClient {
  constructor(
    base,
    { concurrency = 16, timeoutMs = 60_000, retries = 6, headers = {}, name = base, maxBytes = 128 << 20 } = {},
  ) {
    this.base = base.replace(/\/+$/, '');
    this.limiter = new Limiter(concurrency);
    this.timeoutMs = timeoutMs;
    this.retries = retries;
    this.headers = { accept: 'application/json', ...headers };
    this.name = name;
    this.maxBytes = maxBytes;
    this.pausedUntil = 0;
    this.stats = { requests: 0, retries: 0, errors: 0 };
  }

  url(path) {
    return `${this.base}${path}`;
  }

  async waitGate() {
    const wait = this.pausedUntil - Date.now();
    if (wait > 0) await sleep(wait);
  }

  backoff(attempt) {
    const ms = Math.min(30_000, 250 * 2 ** attempt) * (0.5 + Math.random() / 2);
    this.pausedUntil = Math.max(this.pausedUntil, Date.now() + ms);
    return ms;
  }

  // Issues a GET and hands the Response to `consume` (inside the concurrency
  // limit), retrying connection failures, timeouts and 429/502/503/504 with
  // a shared backoff (and a 500 up to `retry500` times). `consume` errors
  // flagged `retryable` are retried too; others propagate.
  request(path, consume, { retries = this.retries, retry500 = 1, timeoutMs = this.timeoutMs, accept } = {}) {
    return this.limiter.run(async () => {
      const url = this.url(path);
      let attempt = 0;
      let fives = 0;
      for (;;) {
        await this.waitGate();
        const started = performance.now();
        let res;
        try {
          this.stats.requests++;
          res = await fetch(url, {
            headers: accept ? { ...this.headers, accept } : this.headers,
            signal: AbortSignal.timeout(timeoutMs),
          });
        } catch (err) {
          if (attempt++ >= retries) {
            this.stats.errors++;
            throw new HttpError(`${this.name}: GET ${path} failed: ${err.message}`, { url });
          }
          this.stats.retries++;
          await sleep(this.backoff(attempt));
          continue;
        }
        if (RETRY_STATUS.has(res.status) || (res.status === 500 && fives++ < retry500)) {
          await res.body?.cancel().catch(() => {});
          if (attempt++ >= retries) {
            this.stats.errors++;
            throw new HttpError(`${this.name}: GET ${path} -> ${res.status}`, { status: res.status, url });
          }
          this.stats.retries++;
          await sleep(this.backoff(attempt));
          continue;
        }
        try {
          return await consume(res, started);
        } catch (err) {
          const transient = err.retryable || err.name === 'TimeoutError' || err.name === 'AbortError' || /terminated|ECONNRESET|socket/i.test(err.message);
          if (!transient || attempt++ >= retries) {
            this.stats.errors++;
            if (err instanceof HttpError) throw err;
            throw new HttpError(`${this.name}: GET ${path}: ${err.message}`, { status: res.status, url });
          }
          this.stats.retries++;
          await sleep(this.backoff(attempt));
        }
      }
    });
  }

  // GET a JSON document. Resolves { status, json, text, ms } for any HTTP
  // status that is not transient (so 404 and 500 are answers, not throws).
  get(path, opts = {}) {
    return this.request(
      path,
      async (res, started) => {
        const text = await readCapped(res, opts.maxBytes ?? this.maxBytes);
        let json = null;
        if (/^\s*[[{"]/.test(text)) {
          try {
            json = JSON.parse(text);
          } catch {
            json = null;
          }
        }
        return { status: res.status, json, text, ms: performance.now() - started };
      },
      opts,
    );
  }

  async getJson(path, opts) {
    const res = await this.get(path, opts);
    if (res.status !== 200 || res.json === null) {
      throw new HttpError(`${this.name}: GET ${path} -> ${res.status}: ${String(res.text).slice(0, 200)}`, {
        status: res.status,
        body: res.text,
      });
    }
    return res.json;
  }

  // Streams an NDJSON response, calling `onObject` for each line. Not
  // retried (a stream can't be resumed); the caller decides.
  async stream(path, onObject, { idleTimeoutMs = 300_000 } = {}) {
    await this.waitGate();
    const url = this.url(path);
    const controller = new AbortController();
    let idle = setTimeout(() => controller.abort(new Error('idle timeout')), idleTimeoutMs);
    const bump = () => {
      clearTimeout(idle);
      idle = setTimeout(() => controller.abort(new Error('idle timeout')), idleTimeoutMs);
    };
    try {
      this.stats.requests++;
      const res = await fetch(url, {
        headers: { ...this.headers, accept: 'application/x-ndjson, application/json' },
        signal: controller.signal,
      });
      if (res.status !== 200) {
        const text = await readCapped(res, 1 << 20).catch(() => '');
        throw new HttpError(`${this.name}: GET ${path} -> ${res.status}: ${text.slice(0, 200)}`, {
          status: res.status,
          body: text,
        });
      }
      const rl = readline.createInterface({ input: Readable.fromWeb(res.body), crlfDelay: Infinity });
      let lines = 0;
      for await (const line of rl) {
        bump();
        if (!line.trim()) continue;
        lines++;
        await onObject(JSON.parse(line));
      }
      return lines;
    } finally {
      clearTimeout(idle);
    }
  }
}

module.exports = { JsonClient, HttpError, readCapped };
