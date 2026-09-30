'use strict';

const fs = require('node:fs');
const { Limiter, sleep } = require('./util');

// Minimal bitcoind JSON-RPC client (batches, retries). Credentials come from
// the environment only (ORD_BITCOIN_RPC_USERNAME / ORD_BITCOIN_RPC_PASSWORD,
// or ORD_BITCOIN_RPC_COOKIE_FILE) so they never show up in `ps` or logs.
class Rpc {
  constructor({ url, username, password, cookieFile, concurrency = 8, timeoutMs = 120_000 } = {}) {
    if (!url) throw new Error('bitcoind RPC URL missing (set ORD_BITCOIN_RPC_URL or --rpc-url)');
    this.url = url;
    this.cookieFile = cookieFile;
    this.username = username;
    this.password = password;
    this.limiter = new Limiter(concurrency);
    this.timeoutMs = timeoutMs;
    this.id = 0;
  }

  static fromEnv(opts = {}) {
    return new Rpc({
      url: opts.rpcUrl || process.env.ORD_BITCOIN_RPC_URL,
      username: process.env.ORD_BITCOIN_RPC_USERNAME,
      password: process.env.ORD_BITCOIN_RPC_PASSWORD,
      cookieFile: process.env.ORD_BITCOIN_RPC_COOKIE_FILE,
      concurrency: opts.concurrency,
    });
  }

  authHeader() {
    let user = this.username;
    let pass = this.password;
    if (this.cookieFile) [user, pass] = fs.readFileSync(this.cookieFile, 'utf8').trim().split(':');
    if (user === undefined) return {};
    return { authorization: `Basic ${Buffer.from(`${user}:${pass ?? ''}`).toString('base64')}` };
  }

  async post(body, label) {
    return this.limiter.run(async () => {
      for (let attempt = 0; ; attempt++) {
        let res;
        let text;
        try {
          res = await fetch(this.url, {
            method: 'POST',
            headers: { 'content-type': 'application/json', ...this.authHeader() },
            body: JSON.stringify(body),
            signal: AbortSignal.timeout(this.timeoutMs),
          });
          text = await res.text();
        } catch (err) {
          if (attempt >= 5) throw new Error(`bitcoind ${label}: ${err.message}`);
          await sleep(500 * 2 ** attempt);
          continue;
        }
        if (res.status === 401 || res.status === 403) throw new Error(`bitcoind ${label}: HTTP ${res.status} (check credentials)`);
        // 503 = work queue full; 500 with a JSON body is an RPC error answer.
        if (res.status === 503 || (res.status >= 500 && !/^\s*[[{]/.test(text))) {
          if (attempt >= 5) throw new Error(`bitcoind ${label}: HTTP ${res.status}`);
          await sleep(500 * 2 ** attempt);
          continue;
        }
        return JSON.parse(text);
      }
    });
  }

  async call(method, params = []) {
    const reply = await this.post({ jsonrpc: '1.0', id: ++this.id, method, params }, method);
    if (reply.error) {
      const err = new Error(`bitcoind ${method}: ${reply.error.message} (${reply.error.code})`);
      err.code = reply.error.code;
      throw err;
    }
    return reply.result;
  }

  // [[method, params], …] -> results in order; RPC errors become
  // { error: {code, message} } entries instead of throwing.
  async batch(calls, chunk = 250) {
    const out = [];
    for (let i = 0; i < calls.length; i += chunk) {
      const slice = calls.slice(i, i + chunk);
      const base = this.id;
      this.id += slice.length;
      const body = slice.map(([method, params], j) => ({ jsonrpc: '1.0', id: base + j + 1, method, params }));
      const replies = await this.post(body, `batch of ${slice.length} ${slice[0][0]}`);
      const byId = new Map(replies.map(r => [r.id, r]));
      for (let j = 0; j < slice.length; j++) {
        const r = byId.get(base + j + 1);
        out.push(r && !r.error ? r.result : { error: r ? r.error : { message: 'missing reply' } });
      }
    }
    return out;
  }
}

module.exports = { Rpc };
