ord — rare-sat tracker fork
===========================

This fork of [ord](https://github.com/ordinals/ord) tracks, block by block,
where the rare sats that matter are:

* every uncommon sat (so every rare, epic, legendary and mythic) and every
  black sat (so every black rare and black epic);
* the alphas, omegas and palindromes the bitcoin-monitoring extraction
  runtime extracts: milestone alphas/omegas, Jan–Feb 2009 alphas, 1d/2d and
  perfect-palinception palindromes, bounty3 names, and the combo sats inside
  vintage, Nakamoto, pizza, jpeg, hitman, Silk Road and blocks 78/286/666;
* every sat of block 9, including its first BTC (450x).

It does not index inscriptions, runes or every sat range. Where upstream's
`--index-sats` needs terabytes and >150 GB of RAM to build, this fits a 64 GiB
RAM / 1.6 TiB box with room to spare (measured below).

* Design: [docs/DESIGN.md](docs/DESIGN.md)
* HTTP API: [docs/API.md](docs/API.md)

It keeps upstream's `ordinals` crate (sat math) and `mockcore` (mock bitcoind
for tests). Everything under `src/` is new.

Running it
----------

```sh
cargo build --release
ORD_BITCOIN_RPC_URL=http://<bitcoind>:8332 \
ORD_BITCOIN_RPC_USERNAME=... ORD_BITCOIN_RPC_PASSWORD=... \
  ./target/release/ord --data-dir /var/lib/ord-sats server --http-port 8090
```

bitcoind must be unpruned (the live source uses `getblock <hash> 3`, which
reads Core's undo data); `txindex` and REST are not needed. An empty index
fast-syncs from genesis (raw blocks, an in-memory UTXO value map), then follows
the tip block by block. `ord --help` lists every option; each also reads an
`ORD_*` environment variable.

Other subcommands: `ord index update` (index to the tip and exit),
`ord index info`, `ord find <sat>`, `ord types <sat>`, `ord rows`.

Changing the tracked set (`--tracked-range`, `--extracted-*`) or the rules
needs `--reindex`; the index records its configuration and the rules'
fingerprint and refuses to open with different ones.

Measured on mainnet
-------------------

garmentperson (32 vCPU, 62 GiB RAM, no swap), bitcoind on another host over
Tailscale, 2026-09-30:

| | |
|---|---|
| Fast sync genesis → 969,247 | 40.2 min (bound by bitcoind and Tailscale: 772 GB of raw blocks at ~320 MB/s) |
| Peak RSS during the fast sync | 16.9 GiB (the UTXO value map is ~11.3 GiB of it) |
| RSS live, after a restart | ~1.1 GB |
| Index on disk | 2.2 GB (moves kept for 4,032 blocks) |
| Restart (restore 1.01 M tracked UTXOs) | 1.6 s |
| Tracked at the tip | 1,014,851 UTXOs, 2,751,933 pieces |
| Lost sats | 2,895,844,904 — identical to upstream ord `--index-sats` |

Validated against an upstream ord with `--index-sats` at the same height
(`tools/validate/`, 2026-09-30, tip 969,264–969,267): 2,310 sampled tracked
sats across all 62 rows plus edge sets (burned sats, sats that passed through
fees, 450x, every milestone, lost-sat offsets) — 709 matched `/sat` satpoints
directly, 1,440 were confirmed inside the oracle's `/output` sat ranges at our
offset, 29 at ord's lost offsets; 0 mismatches. A baseline of 6,022 sats
recorded from the oracle matched too, and every row's `/supply` numbers equal
an independent recomputation from `/sats`. Run it with
`tools/validate/validate-all.sh --ours=http://127.0.0.1:8090 ...`.

Keeping the rules in sync
-------------------------

The sat types are an exact port of bitcoin-monitoring's JS rules. When
`satRarityRules.js`, `exoticSatRanges.js`, the bounty3 names or the
extraction type lists change there:

```sh
node tools/export-monitoring-data.js <bitcoin-monitoring checkout>          # regenerate data/ and fixtures
node tools/export-monitoring-data.js <bitcoin-monitoring checkout> --check  # or just check for drift
cargo test
```

then bump `RULES_VERSION` in `src/sats/mod.rs` if behaviour changed, and
reindex.
