ord — rare-sat tracker fork
===========================

This fork of [ord](https://github.com/ordinals/ord) tracks, block by block,
where the rare sats that matter are:

* every block's first sat (uncommon, rare, epic, legendary or mythic: each
  sat counts under its highest tier only) and last sat (black, black rare
  or black epic);
* the alphas, omegas and palindromes the bitcoin-monitoring extraction
  runtime extracts: milestone alphas/omegas, 1d/2d and
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

The index also keeps a per-address history of tracked-sat activity
(extractions, sales, transfers, consolidations, burns, coinbase receipts;
`GET /address/{address}/history`), from the ordinals era by default
(`--address-history-from <height|ordinals|all>`).

Changing the tracked set (`--tracked-range`, `--extracted-*`), the address
history's coverage (`--address-history-from`) or the rules
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

With the address history (index schema 4, 2026-10-02, same box, to
969,602): fast sync 43.9 min (2,635 s, including 49 s to move 25.9 M staged
history entries into place), peak RSS 19.6 GiB, RSS live after a restart
~1.1 GB, index 5.2 GB (the `history` table 3.1 GB: 26.4 M entries from
height 767,304, the first block of the ordinals era). Checked with
`tools/validate/history-check.py`: every move of the last 4,032 blocks is in
the history (407,095 entries, 90,753 scripts) and nothing else, every sale's
seller and buyer have their `sale` entry, and `/supply`, `/sales` and the
lost sats equal those of the previous index.

With the year rows (index schema 5, 2026-10-02, same box, to 969,629):
fast sync 46.5 min (2,790 s), peak memory 26.6 GiB, RSS live after a
restart ~1.3 GB (restore 1.9 s), index 7.3 GB (`row_history` 23.5 M
entries, 1.1 GB on disk), the published supply ~1.0 s a block (0.26 s
before: top holders for every year row). Checked against the schema 4
index at the same height (969,644): every row's `/supply` is identical;
`years=2009` equals `uncommon_2009`, `rare_2009`, `black_2009` and
`palindrome_2d_2009` counter for counter (holders, inscribed and active
windows included), and so does their history (194 points); for all 70
rows the years add up to the row (mined, located, burned, lost,
overwritten, extracted, inscribed, active) and the holders of every year
equal the row's.

With the luck rows (index schema 6, 2026-10-03, same box, to 969,638):
fast sync 48.1 min (2,884 s; +3% on schema 5), peak memory 28.2 GB
(cgroup), RSS live after a restart ~1.4 GB (restore 2.5 s, which now
hashes every stored header for its luck), index 6.5 GB (`row_history` 37.4 M
entries, 0.74 GB stored), the published supply ~1.3–1.7 s a block (1.6 s on
schema 5: luck rows keep no top holders). Checked at 969,652: every block's
luck and bits equal those bitcoin-monitoring's luck table derives from the
headers (969,653 heights, `GET /blocks`); for all 70 rows the lucks add up
to the row (mined, located, burned, lost, overwritten, extracted,
inscribed, active, totals) and `luck=0` is the row, holders included;
`/supply`, `/supply/years` and `years=` answers equal the schema 5 index's
at the same height; the holders of luck N or more and of N-N (N 0–21, both
lists) equal a brute-force count over `/sats` for rare, epic, black rare,
Nakamoto alpha, vintage uncommon, megasat alpha, alpha uncommon (195,831
holders) and uncommon (256,807).

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
