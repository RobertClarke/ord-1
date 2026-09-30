'use strict';

// Incremental (SAX-style) JSON tokenizer: feed text chunks, get events.
// Used for ord's /output, whose `sat_ranges` can run to millions of ranges
// (an 814 MB response for one consolidation output was seen), far beyond
// what JSON.parse or a JS string can hold.
//
// handler: { startObject(), endObject(), startArray(), endArray(),
//            key(name), value(v) } (all optional)

const WS = new Set([0x20, 0x09, 0x0a, 0x0d]);

const S_VALUE = 0; // expecting a value
const S_KEY = 1; // in an object, expecting a key or '}'
const S_COLON = 2;
const S_AFTER = 3; // after a value: ',' or a closing bracket
const S_STRING = 4;
const S_NUMBER = 5;
const S_LITERAL = 6;
const S_DONE = 7;

class JsonTokenizer {
  constructor(handler) {
    this.h = handler;
    this.state = S_VALUE;
    this.stack = []; // '{' or '['
    this.buf = '';
    this.stringIsKey = false;
    this.escape = false;
    this.unicode = null;
  }

  fail(msg) {
    throw new Error(`JSON stream: ${msg}`);
  }

  emitValue(v) {
    if (this.h.value) this.h.value(v);
    this.state = this.stack.length ? S_AFTER : S_DONE;
  }

  write(chunk) {
    const n = chunk.length;
    let i = 0;
    while (i < n) {
      const state = this.state;
      if (state === S_STRING) {
        // Fast path: copy up to the next quote or backslash.
        if (!this.escape && this.unicode === null) {
          let j = i;
          while (j < n) {
            const c = chunk.charCodeAt(j);
            if (c === 0x22 || c === 0x5c) break;
            j++;
          }
          if (j > i) this.buf += chunk.slice(i, j);
          i = j;
          if (i >= n) break;
        }
        const ch = chunk[i++];
        if (this.unicode !== null) {
          this.unicode += ch;
          if (this.unicode.length === 4) {
            this.buf += String.fromCharCode(parseInt(this.unicode, 16));
            this.unicode = null;
          }
        } else if (this.escape) {
          this.escape = false;
          if (ch === 'u') this.unicode = '';
          else this.buf += { n: '\n', t: '\t', r: '\r', b: '\b', f: '\f' }[ch] ?? ch;
        } else if (ch === '\\') this.escape = true;
        else {
          // closing quote
          const s = this.buf;
          this.buf = '';
          if (this.stringIsKey) {
            if (this.h.key) this.h.key(s);
            this.state = S_COLON;
          } else this.emitValue(s);
        }
        continue;
      }
      if (state === S_NUMBER) {
        let j = i;
        while (j < n) {
          const c = chunk.charCodeAt(j);
          // 0-9 . - + e E
          if ((c >= 0x30 && c <= 0x39) || c === 0x2e || c === 0x2d || c === 0x2b || c === 0x65 || c === 0x45) j++;
          else break;
        }
        this.buf += chunk.slice(i, j);
        i = j;
        if (i >= n) break;
        const num = Number(this.buf);
        if (Number.isNaN(num)) this.fail(`bad number ${this.buf.slice(0, 40)}`);
        this.buf = '';
        this.emitValue(num);
        continue;
      }
      if (state === S_LITERAL) {
        let j = i;
        while (j < n) {
          const c = chunk.charCodeAt(j);
          if (c >= 0x61 && c <= 0x7a) j++;
          else break;
        }
        this.buf += chunk.slice(i, j);
        i = j;
        if (i >= n) break;
        const lit = this.buf;
        this.buf = '';
        if (lit === 'true') this.emitValue(true);
        else if (lit === 'false') this.emitValue(false);
        else if (lit === 'null') this.emitValue(null);
        else this.fail(`bad literal ${lit}`);
        continue;
      }
      const c = chunk.charCodeAt(i);
      if (WS.has(c)) {
        i++;
        continue;
      }
      const ch = chunk[i++];
      switch (state) {
        case S_VALUE:
          if (ch === '{') {
            this.stack.push('{');
            if (this.h.startObject) this.h.startObject();
            this.state = S_KEY;
          } else if (ch === '[') {
            this.stack.push('[');
            if (this.h.startArray) this.h.startArray();
            this.state = S_VALUE;
          } else if (ch === ']' && this.stack[this.stack.length - 1] === '[') {
            // empty array
            this.stack.pop();
            if (this.h.endArray) this.h.endArray();
            this.state = this.stack.length ? S_AFTER : S_DONE;
          } else if (ch === '"') {
            this.state = S_STRING;
            this.stringIsKey = false;
          } else if (ch === '-' || (ch >= '0' && ch <= '9')) {
            this.state = S_NUMBER;
            this.buf = ch;
          } else if (ch === 't' || ch === 'f' || ch === 'n') {
            this.state = S_LITERAL;
            this.buf = ch;
          } else this.fail(`unexpected ${ch}`);
          break;
        case S_KEY:
          if (ch === '"') {
            this.state = S_STRING;
            this.stringIsKey = true;
          } else if (ch === '}') {
            this.stack.pop();
            if (this.h.endObject) this.h.endObject();
            this.state = this.stack.length ? S_AFTER : S_DONE;
          } else this.fail(`expected key, got ${ch}`);
          break;
        case S_COLON:
          if (ch !== ':') this.fail(`expected :, got ${ch}`);
          this.state = S_VALUE;
          break;
        case S_AFTER: {
          const top = this.stack[this.stack.length - 1];
          if (ch === ',') this.state = top === '{' ? S_KEY : S_VALUE;
          else if (ch === '}' && top === '{') {
            this.stack.pop();
            if (this.h.endObject) this.h.endObject();
            this.state = this.stack.length ? S_AFTER : S_DONE;
          } else if (ch === ']' && top === '[') {
            this.stack.pop();
            if (this.h.endArray) this.h.endArray();
            this.state = this.stack.length ? S_AFTER : S_DONE;
          } else this.fail(`unexpected ${ch} after value`);
          break;
        }
        case S_DONE:
          this.fail(`trailing ${ch}`);
          break;
        default:
          this.fail(`bad state ${state}`);
      }
    }
  }

  end() {
    if (this.state === S_NUMBER) {
      const num = Number(this.buf);
      this.buf = '';
      this.emitValue(num);
    } else if (this.state === S_LITERAL) {
      const lit = this.buf;
      this.buf = '';
      this.emitValue(lit === 'true' ? true : lit === 'false' ? false : null);
    }
    if (this.state !== S_DONE) this.fail('truncated document');
  }
}

// Builds plain values for everything, except that arrays under the paths
// given in `special` are routed to callbacks instead of being built:
//   special = { sat_ranges: { onItem(v) } }  (items built normally)
// Only top-level keys are supported as special paths.
class ObjectBuilder {
  constructor({ special = {} } = {}) {
    this.special = special;
    this.stack = []; // { container, key }
    this.root = undefined;
    this.pendingKey = null;
    this.specialDepth = -1;
    this.specialName = null;
  }

  push(container) {
    this.stack.push({ container, key: this.pendingKey });
    this.pendingKey = null;
  }

  attach(v) {
    if (!this.stack.length) {
      this.root = v;
      return;
    }
    // Items of a special top-level array go to its callback.
    if (this.specialDepth !== -1 && this.stack.length === this.specialDepth + 1) {
      this.special[this.specialName].onItem(v);
      return;
    }
    const top = this.stack[this.stack.length - 1].container;
    if (Array.isArray(top)) top.push(v);
    else top[this.pendingKey] = v;
    this.pendingKey = null;
  }

  startObject() {
    this.push({});
  }

  startArray() {
    if (this.stack.length === 1 && this.special[this.pendingKey] && this.specialDepth === -1) {
      this.specialName = this.pendingKey;
      this.specialDepth = 1;
      this.special[this.specialName].onStart?.();
      this.push(null);
      return;
    }
    this.push([]);
  }

  endContainer() {
    const { container, key } = this.stack.pop();
    if (container === null) {
      // closing a special array
      const name = this.specialName;
      this.specialDepth = -1;
      this.specialName = null;
      this.special[name].onEnd?.();
      this.pendingKey = key;
      this.attach({ $special: name });
      return;
    }
    this.pendingKey = key;
    this.attach(container);
  }

  endObject() {
    this.endContainer();
  }

  endArray() {
    this.endContainer();
  }

  key(k) {
    this.pendingKey = k;
  }

  value(v) {
    this.attach(v);
  }
}

module.exports = { JsonTokenizer, ObjectBuilder };
