'use strict';

// lib/tracer.js: raw block parsing and txids on real data, and sat flow
// (FIFO outputs, fees into the coinbase after the subsidy and earlier fees,
// unclaimed sats lost, intra-block spends) on a synthetic chain.

const test = require('node:test');
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const { parseBlock, txid, distribute, trace } = require('../lib/tracer');
const sm = require('../lib/satmath');

// Block 170: the coinbase and Satoshi's 10 BTC to Hal Finney.
const BLOCK_170 =
  '0100000055bd840a78798ad0da853f68974f3d183e2bd1db6a842c1feecf222a00000000ff104ccb05421ab93e63f8c3ce5c2c2e9dbb37de2764b3a3175c8166562cac7d51b96a49ffff001d283e9e700201000000010000000000000000000000000000000000000000000000000000000000000000ffffffff0704ffff001d0102ffffffff0100f2052a01000000434104d46c4968bde02899d2aa0963367c7a6ce34eec332b32e42e5f3407e052d64ac625da6f0718e7b302140434bd725706957c092db53805b821a85b23a7ac61725bac000000000100000001c997a5e56e104102fa209c6a852dd90660a20b2d9c352423edce25857fcd3704000000004847304402204e45e16932b8af514961a1d3a1a25fdf3f4f7732e9d624c6c61548ab5fb8cd410220181522ec8eca07de4860a4acdd12909d831cc56cbbac4622082221a8768d1d0901ffffffff0200ca9a3b00000000434104ae1a62fe09c5f51b13905f07f06b99a2f7159b2225f374cd378d71302fa28414e7aab37397f554a7df5f142c21c1b7303b8a0626f1baded5c72a704f7e6cd84cac00286bee0000000043410411db93e1dcdb8a016b49840f8c53bc1eb68a382e97b1482ecad7b148a6909a5cb2e0eaddfb84ccf9744464f82e160bfa9b8b64f9d4c03f999b8643f656b412a3ac00000000';

// A segwit (P2SH-P2WPKH) spend, c586389e…fc3a.
const SEGWIT_TX =
  '0200000000010140d43a99926d43eb0e619bf0b3d83b4a31f60c176beecfb9d35bf45e54d0f7420100000017160014a4b4ca48de0b3fffc15404a1acdc8dbaae226955ffffffff0100e1f5050000000017a9144a1154d50b03292b3024370901711946cb7cccc387024830450221008604ef8f6d8afa892dee0f31259b6ce02dd70c545cfcfed8148179971876c54a022076d771d6e91bed212783c9b06e0de600fab2d518fad6f15a2b191d7fbd262a3e0121039d25ab79f41f75ceaf882411fd41fa670a4c672c23ffaf0e361a969cde0692e800000000';

test('parses block 170 and computes its txids', () => {
  const txs = parseBlock(Buffer.from(BLOCK_170, 'hex'));
  assert.equal(txs.length, 2);
  assert.equal(txid(txs[0]), 'b1fea52486ce0c62bb442b530a3f0132b826c74e473d1f2c220bfa78111c5082');
  assert.equal(txid(txs[1]), 'f4184fc596403b9d638783cf57adfe4c75c605f6356fbc91338530e9831e9e16');
  assert.deepEqual(txs[1].inputs, [{ txid: '0437cd7f8525ceed2324359c2d0ba26006d92d856a9c20fa0241106ee5a597c9', vout: 0 }]);
  assert.deepEqual(
    txs[1].outputs.map(o => o.value),
    [1_000_000_000, 4_000_000_000],
  );
});

test('computes a segwit txid without the witness', () => {
  const header = Buffer.alloc(80);
  const txs = parseBlock(Buffer.concat([header, Buffer.from([1]), Buffer.from(SEGWIT_TX, 'hex')]));
  assert.equal(txs.length, 1);
  assert.equal(txs[0].witness, true);
  assert.equal(txid(txs[0]), 'c586389e5e4b3acb9d6c8be1c19ae8ab2795397633176f5a6442a261bbdefc3a');
  assert.equal(txs[0].outputs[0].value, 100_000_000);
});

test('distributes pieces over outputs first-in-first-out', () => {
  const slots = [
    { from: 0, to: 10, key: 'a' },
    { from: 10, to: 10, key: 'zero' },
    { from: 10, to: 25, key: 'b' },
  ];
  const { placed, rest } = distribute(
    [
      { pos: 5, start: 1000, len: 10 },
      { pos: 24, start: 5000, len: 3 },
    ],
    slots,
  );
  assert.deepEqual(placed, [
    { key: 'a', offset: 5, start: 1000, len: 5 },
    { key: 'b', offset: 0, start: 1005, len: 5 },
    { key: 'b', offset: 14, start: 5000, len: 1 },
  ]);
  assert.deepEqual(rest, [{ pos: 25, start: 5001, len: 2 }]);
});

// --- a synthetic chain -----------------------------------------------------

function varint(n) {
  if (n < 0xfd) return Buffer.from([n]);
  const b = Buffer.alloc(3);
  b[0] = 0xfd;
  b.writeUInt16LE(n, 1);
  return b;
}

function serializeTx({ inputs, outputs }) {
  const parts = [Buffer.from('01000000', 'hex'), varint(inputs.length)];
  for (const i of inputs) {
    parts.push(Buffer.from(i.txid, 'hex').reverse());
    const v = Buffer.alloc(4);
    v.writeUInt32LE(i.vout);
    parts.push(v, varint(0), Buffer.from('ffffffff', 'hex'));
  }
  parts.push(varint(outputs.length));
  for (const value of outputs) {
    const v = Buffer.alloc(8);
    v.writeBigUInt64LE(BigInt(value));
    parts.push(v, varint(1), Buffer.from([0x51]));
  }
  parts.push(Buffer.alloc(4));
  return Buffer.concat(parts);
}

const idOf = raw => Buffer.from(crypto.createHash('sha256').update(crypto.createHash('sha256').update(raw).digest()).digest()).reverse().toString('hex');

// Chain from height 10: blocks of { txs: [{inputs, outputs}] } (tx 0 is the
// coinbase, whose single null input is added here).
function fakeChain(blocks, prevouts) {
  const byHash = new Map();
  const hashes = new Map();
  const values = new Map(prevouts); // outpoint -> value
  blocks.forEach((blk, i) => {
    const height = 10 + i;
    const hash = height.toString(16).padStart(64, '0');
    const raws = blk.txs.map((tx, t) => serializeTx(t === 0 ? { inputs: [{ txid: '0'.repeat(64), vout: 0xffffffff }], outputs: tx.outputs } : tx));
    const ids = raws.map(idOf);
    ids.forEach((id, t) => blk.txs[t].outputs.forEach((v, vout) => values.set(`${id}:${vout}`, v)));
    const json = {
      tx: blk.txs.map((tx, t) => ({
        txid: ids[t],
        vin: t === 0 ? [] : tx.inputs.map(inp => ({ prevout: { value: values.get(`${inp.txid}:${inp.vout}`) / 1e8 } })),
        vout: tx.outputs.map(v => ({ value: v / 1e8 })),
      })),
    };
    const hex = Buffer.concat([Buffer.alloc(80), varint(raws.length), ...raws]).toString('hex');
    hashes.set(height, hash);
    byHash.set(hash, { hex, json });
    blk.ids = ids;
  });
  const rpc = {
    calls: [],
    async call(method, params) {
      this.calls.push(method);
      if (method === 'getblock' && params[1] === 3) return byHash.get(params[0]).json;
      if (method === 'getrawtransaction') {
        const [id] = params;
        const outs = [];
        for (const [op, v] of values) if (op.startsWith(`${id}:`)) outs[Number(op.split(':')[1])] = { value: v / 1e8 };
        return { vout: outs };
      }
      throw new Error(`unexpected ${method}`);
    },
    async batch(calls) {
      return calls.map(([method, params]) => {
        if (method === 'getblockhash') return hashes.get(params[0]);
        if (method === 'getblock') return byHash.get(params[0]).hex;
        return { error: { message: method } };
      });
    },
  };
  return { rpc, values };
}

test('traces pieces through spends, fees and lost sats', async () => {
  const START = 'aa'.repeat(32);
  const OTHER = 'bb'.repeat(32);
  // Tracked: 1,000 sats (7,000,000..) at offsets 100..1099 of START:0 (5,000).
  //
  // Height 10, tx1 spends OTHER:0 (untracked, 300) then START:0: tracked at
  // input-stream 400..1399. Outputs 1,000 / 3,000: 600 sats to out0 at
  // offset 400, 400 sats to out1 at offset 0; the 1,300 fee is untracked.
  // tx2 spends tx1:1 in the same block: 2,500 out, tracked at 0..399.
  const tx1 = { inputs: [{ txid: OTHER, vout: 0 }, { txid: START, vout: 0 }], outputs: [1000, 3000] };
  const tx1id = idOf(serializeTx(tx1));
  // Height 11, tx1 spends tx1(10):0 (tracked 400..999) into one 450-sat
  // output: 50 tracked sats stay at 400..449, 550 become the fee. The
  // coinbase claims the subsidy plus 300 fee sats: those land after the
  // subsidy, the last 250 are lost.
  const blocks = [
    { txs: [{ outputs: [5_000_000_000] }, tx1, { inputs: [{ txid: tx1id, vout: 1 }], outputs: [2500] }] },
    { txs: [{ outputs: [5_000_000_000 + 300] }, { inputs: [{ txid: tx1id, vout: 0 }], outputs: [450] }] },
  ];
  const { rpc } = fakeChain(blocks, [
    [`${OTHER}:0`, 300],
    [`${START}:0`, 5000],
  ]);
  const state = new Map([[`${START}:0`, { value: 5000, pieces: [{ offset: 100, start: 7_000_000, len: 1000 }] }]]);
  const res = await trace({ rpc, state, from: 10, to: 11 });
  const tx2id = blocks[0].ids[2];
  const b11tx1 = blocks[1].ids[1];
  const cb11 = blocks[1].ids[0];
  const got = Object.fromEntries([...res.state].map(([op, v]) => [op, v.pieces]));
  assert.deepEqual(got, {
    // Block 10 tx2 output 0: sats 7,000,600.. (400 of them) at 0..399.
    [`${tx2id}:0`]: [{ offset: 0, start: 7_000_600, len: 400 }],
    // Block 11 tx1 output 0: tracked 400..449 of its input = sats 7,000,000..049.
    [`${b11tx1}:0`]: [{ offset: 400, start: 7_000_000, len: 50 }],
    // Block 11 coinbase: fee stream offsets 0..299 after the subsidy.
    [`${cb11}:0`]: [{ offset: sm.subsidy(11), start: 7_000_050, len: 300 }],
  });
  assert.deepEqual(res.lost, [{ height: 11, start: 7_000_350, len: 250 }]);
  assert.equal(res.stats.feeEvents, 1);
  assert.ok(rpc.calls.includes('getrawtransaction'), 'untracked input values come from getrawtransaction');
});
