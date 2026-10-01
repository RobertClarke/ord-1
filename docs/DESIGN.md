# ord rare-sat tracker — design

This branch (`rare-sat-tracker`, cut from upstream `ordinals/ord` master
`03c9b87b`, 0.29.0+8) turns ord into a lean, per-block tracker of the rare
sats that the bitcoin-monitoring extraction runtime cares about. It replaces
ord's `--index-sats` approach (every sat range of every UTXO: terabytes of
index, >150 GB RAM to build the old `feature/sat-range-index`) with tracking of
a fixed, rule-defined set of sats only.

Everything inscription, rune, wallet, HTML explorer and BRC-style is removed.
What is kept from upstream: the `ordinals` crate (sat/height/epoch math) and
the `mockcore` crate (mock bitcoind for tests). Everything under `src/` is new.

Hard resource envelope (the target box, garmentperson): 32 vCPU, 64 GiB RAM,
no swap, 1.6 TiB disk. bitcoind is remote (Core 31.1, txindex, REST off),
reachable over RPC at ~1 ms RTT; `getblock <hash> 0` sustains ~550 MB/s of hex
with 8–24 parallel requests; `getblock <hash> 3` takes ~0.16 s / 13.5 MB for a
full block.

## 1. What is tracked

A sat is **tracked** when it satisfies the monitoring runtime's extraction
rules (`src/utils/psbtBuilder.js` → `SPECIAL_SAT_TYPES`,
`REQUIRED_COMBINATION_TYPES`, `satIsExtractableAlpha2009`) with the scope the
operator set:

* **Whole ranges** — every sat in:
  * `block9`: sats `[45_000_000_000, 50_000_000_000)`, all of block 9
    including its first BTC (`450x`).
  * The range list is configuration (`TrackerConfig::ranges`); the default is
    `[block9]`. Changing it requires a reindex (the index records it). Other
    large chunks the runtime extracts (vintage, block 78, block 286, Nakamoto,
    pizza, jpeg, hitman, Silk Road) are deliberately **not** tracked whole; only
    their individually special sats are.
* **Individual sats** anywhere in the chain whose types (see §2) include any of
  * standalone types: `uncommon` (so also `rare`, `epic`, `legendary`,
    `mythic`), `black`, `black_rare`, `black_epic`,
    `palindromes_integer_1d`, `palindromes_integer_2d`,
    `palindromes_perfect_palinception`, `bounty3_name`,
    `kilosat_alpha`, `decasat_alpha`, `centasat_alpha`, `megasat_alpha`,
    `kilosat_omega`, `decasat_omega`, `centasat_omega`, `megasat_omega`;
  * every type of any `REQUIRED_COMBINATION_TYPES` entry, e.g.
    `nakamoto`+`alpha`, `pizza`+`palindromes_integer`, `sr_1st_auction`+`black`,
    `palindromes_integer_3d`+`palindromes_integer`+`sr_1st_auction` (the full
    list is in §2.3);
  * an alpha mined in Jan–Feb 2009 (blocks 0–5923, `isAlpha2009Sat`).
* **Not tracked:** whole vintage / block 78 / block 286 / block 666 / pizza /
  jpeg / hitman / Silk Road / Nakamoto / 2009 ranges (only their special sats
  above; the runtime's standalone `block78`/`block286` ≥1M-sat chunks are out of
  scope), `common_name`, `requests`,
  inscriptions, luck (luck is a per-block property of already-tracked sats and
  can be derived from the stored headers).

Size at height ~969k: ≈2.8 M individual sats (≈969k uncommon + ≈969k black,
≈296k Jan–Feb 2009 alphas, ≈50k vintage omegas, ≈40k milestone
alphas/omegas, ≈35k 1d/2d palindromes, ≈12.5k bounty3 names, ≈400k other
combo sats) plus the block 9 range (5e9 sats).

The tracked set is a pure function of the sat number, so it can be enumerated
for any coinbase without chain data.

## 2. Sat types (exact port of the JS rules)

`types(sat) -> TypeSet` must return exactly what bitcoin-monitoring's
`getSatTypes(sat)` (`src/services/satRarityRules.js`) returns, plus exotic
range membership (`src/services/exoticSatRanges.js` → `EXOTIC_RANGES`), plus
`bounty3_name` (`data/bounty3-names-map.json`), plus the synthetic
`alpha_jan_feb_2009`. Parity is enforced by test vectors generated from the JS
(`tests/fixtures/sat-types.json`).

Assembly follows the runtime scanner for a point sat
(`rareSatScanner.js` → `analyzeSatRanges`): rule types, then `bounty3_name` if
named, then every exotic range type containing the sat, then
`normalizePaliblockTypes` over the combined set — so `paliblock` is dropped
whenever an exotic type or `bounty3_name` is present (e.g. block 9's first
sat). `alpha_jan_feb_2009` is added after normalization. `common_name`, name
palindromes and `inscription_*` types are not modelled.

### 2.1 Rule types (from `getSatTypes`)

For sat `s` in block `h` at `offset = s - first_sat(h)`, `subsidy = subsidy(h)`:

| type | rule |
|---|---|
| `mythic` | `s == 0` |
| `legendary` | `offset == 0 && h % 1_260_000 == 0` |
| `epic` | `offset == 0 && h % 210_000 == 0` |
| `rare` | `offset == 0 && h % 2016 == 0` |
| `uncommon` | `offset == 0` |
| `paliblock` | uncommon of palindromic `h` with `h != 0`, `h % 1_260_000 != 0`, `h % 210_000 != 0`, `h % 2016 != 0`; or black of palindromic `h` with `h != 0`, `(h+1) % 210_000 != 0`, `(h+1) % 2016 != 0`. Then `normalizePaliblockTypes`: drop `paliblock` unless the set has `uncommon` or `black` **and** every type is in {`paliblock`, `uncommon`, `black`, `alpha`, `omega`, the 8 milestone types}. |
| `black` | `offset == subsidy - 1` |
| `black_rare` | black and `(h+1) % 2016 == 0` |
| `black_epic` | black and `(h+1) % 210_000 == 0` |
| `alpha` | `s % 1e8 == 0`; plus exactly one tier: `megasat_alpha` (`s % 1e14 == 0`), else `centasat_alpha` (`1e13`), else `decasat_alpha` (`1e12`), else `kilosat_alpha` (`1e11`) |
| `omega` | `s % 1e8 == 1e8-1`; tiers on `s+1` the same way (`megasat_omega` …) |
| `palindromes_integer` | decimal digits of `s` read the same reversed |
| `palindromes_integer_1d` / `_2d` / `_3d` | palindrome with exactly 1 / 2 / 3 distinct digits |
| `palindromes_perfect_palinception` | palindrome whose digit string is `sub` repeated ≥2 times, `sub` itself a palindrome with ≥2 distinct digits (`isPerfectPalinception`) |

Note `s == 0` yields mythic, legendary, epic, rare, uncommon, alpha and
megasat_alpha, but **not** the palindrome types: the scanner's palindrome
generator (`generatePalindromesInRange`) never yields 0, although a bare
`getSatTypes(0)` would say it is one. The scanner is what the runtime and the
old supply script used, so it wins.

### 2.2 Exotic range types

`vintage`, `block9`, `450x`, `block78`, `block286`, `block_666`, `nakamoto`
(19 blocks), `jpeg` (10 ranges), `pizza` (825), `hitman` (6,200),
`sr_1st_auction` (405,534). Exported from the JS into `data/exotic-ranges.bin` (+ a
`data/exotic-ranges.json` manifest with counts and sha256s) by this repo's
`tools/export-monitoring-data.js <bitcoin-monitoring checkout>` (`--check`
reports drift) and embedded with `include_bytes!`.

### 2.3 Required combinations (from `REQUIRED_COMBINATION_TYPES`)

```
nakamoto+palindromes_integer   palindromes_integer+pizza   jpeg+palindromes_integer
alpha+pizza  alpha+jpeg  omega+jpeg  omega+pizza  alpha+nakamoto  omega+nakamoto
alpha+vintage  omega+vintage  block9+palindromes_integer  block9+alpha  block9+omega
alpha+block78  omega+block78  block78+palindromes_integer
alpha+block286  omega+block286  block286+palindromes_integer
alpha+hitman  omega+hitman  hitman+palindromes_integer
alpha+sr_1st_auction  omega+sr_1st_auction  black+sr_1st_auction
palindromes_integer_3d+palindromes_integer+sr_1st_auction
block_666+palindromes_integer  block_666+alpha  block_666+omega
```

## 3. Rows (reported categories)

A **row** is a named filter over types plus an optional height scope:
`{ id, label, all_of: [type], any_of: [type], heights: Option<(from, to)>,
range: bool }`. `range: true` rows are pure range membership (`block9`, `450x`)
and are counted by overlap length; everything else is counted per sat (all of
its sats are individually tracked). The default row set is
defined in `src/sats/rows.rs` (ids are the API contract with the dashboard):

```
uncommon rare epic legendary mythic black black_rare black_epic paliblock
alpha_jan_feb_2009 kilosat_alpha decasat_alpha centasat_alpha megasat_alpha
kilosat_omega decasat_omega centasat_omega megasat_omega
palindrome_1d palindrome_2d palindrome_perfect_palinception bounty3_name
vintage_uncommon vintage_black vintage_alpha vintage_omega
block9 450x block9_alpha block9_omega block9_palindrome
block78_alpha block78_omega block78_palindrome
block286_alpha block286_omega block286_palindrome
block_666_alpha block_666_omega block_666_palindrome
nakamoto_uncommon nakamoto_black nakamoto_alpha nakamoto_omega nakamoto_palindrome
pizza_uncommon pizza_alpha pizza_omega pizza_palindrome
jpeg_uncommon jpeg_alpha jpeg_omega jpeg_palindrome
hitman_uncommon hitman_alpha hitman_omega hitman_palindrome
sr_uncommon sr_alpha sr_omega sr_black sr_palindrome_3d
uncommon_2009 rare_2009 black_2009 palindrome_2d_2009
```

`*_palindrome` means `palindromes_integer`; `_2009` rows are heights
`0..=32489`.

For each row the tracker maintains (§5): `total` (all sats that will ever
exist, computed once), `mined`, `located` (in spendable outputs), `burned`
(in unspendable outputs), `lost`, `extracted` and holder statistics.

## 4. Sat flow (ordinal theory, identical to ord `--index-sats`)

For each block, transactions are processed in order `1..n`, then the coinbase:

1. The coinbase's input sats are the block subsidy `[first_sat(h),
   first_sat(h) + subsidy(h))` followed by each non-coinbase transaction's fee
   sats, in transaction order.
2. A transaction's input sats are its inputs' sats concatenated in input
   order. Outputs take sats first-in-first-out in output order. Sats left
   after the last output are the fee and go to the coinbase (1).
3. Sats the coinbase outputs do not claim are **lost**: they are placed at
   ord's null outpoint (`0000…0000:4294967295`) at offset = running total of
   all sats ever lost (all sats, not only tracked ones), exactly as ord does,
   so lost satpoints match ord's.
4. Every output receives sats, including OP_RETURN and other unspendable
   outputs; tracked sats landing in an unspendable output (script starts with
   OP_RETURN or is longer than 10,000 bytes — Core's `IsUnspendable`) are
   **burned**: they keep that location forever.
5. The two BIP30 duplicate coinbases (blocks 91842 and 91880 re-create the
   coinbase txids of 91812 and 91722): when an output is created at an
   outpoint that still holds a tracked UTXO, the old UTXO's pieces become
   **overwritten** (unspendable, reported separately), then the new one is
   stored.
6. The genesis coinbase output is treated like any other output (ord does the
   same); it can never be spent, so its sats never move.

### 4.1 Pieces

A **piece** is a run of consecutive tracked sats at consecutive offsets in one
output: `Piece { offset, start, len }` (sat `start + i` sits at `offset + i`).
A tracked UTXO holds its pieces sorted by offset, non-overlapping, merged when
both offsets and sats are contiguous. Individual sats are `len == 1` pieces;
block 9 pieces can be up to 5e9 long. When an output boundary cuts a piece it
is split.

Only pieces are moved. For transactions that spend no tracked UTXO the tracker
only needs `Σ inputs − Σ outputs` (the fee length) to keep coinbase fee
offsets right.

## 5. Aggregates ("circulating supply")

Per row, maintained incrementally as tracked UTXOs are created and removed:

* `mined`: cumulative count created by coinbases (rows are counted when their
  sats are born).
* `located`, `burned`, `lost`, `overwritten`: current counts by location.
* `extracted` (a.k.a. the gist's `n_seq`): count in UTXOs that are
  *extracted*. A tracked UTXO is extracted, for every row it holds, when
  it is spendable, was created by a block with header time
  ≥ 2022-12-14T00:00:00Z (`extracted_since`, the ordinal-theory launch the
  old script used), and its value is at most its **extraction budget**:

  ```
  budget = Σ over its pieces ( inside + M × runs_inside + M × outside )
  ```

  with `M = extracted_max_value` (10,000), `inside` the piece's sats inside
  `TrackerConfig::ranges` (block 9), `runs_inside` the number of maximal
  runs they form within the piece, and `outside` its other sats. In words:
  each individually tracked sat allows up to 10,000 sats of output value,
  and each contiguous run of block 9 sats allows its own length + 10,000.
  So a single rare sat still needs an output of ≤ 10,000 sats and a pure
  block 9 output is extracted, as before; collector consolidations now are
  too: a collector's address holds 2,298 outputs
  packing rare sats at ~330 sats of padding each (100 rare sats in 33,000
  sats, 200 in 66,000, 5,000 in 1,924,843) and near-pure block 9 chunks
  such as 100,000 tracked sats in 100,438 (budget 110,000), all extracted;
  while an exchange output holding a few uncommons in whole BTC is not
  (2 in 1 BTC: budget 20,000). `model::is_extracted` is the one
  implementation, used by the tracker and the API. Rules version 2 (the
  catalog fingerprint) introduced it, replacing the per-row test
  `value ≤ 10_000 || row_count == value`, which it subsumes; indexes built
  under version 1 are refused and must be rebuilt, which recomputes the
  per-block row history.
* Non-extracted located counts bucketed by the UTXO's creation height (a
  Fenwick tree over heights) so "moved within N years" is answerable for any N
  at any tip time (`n_active_non_seq_Ny` = non-extracted located sats whose
  UTXO was created at a block with time ≥ tip time − N·365 days).
* Holders: per `script_pubkey`, `{ count, extracted_count, utxos }`; exposes
  `holders`, `extracted_holders`, and on request the top N extracted /
  non-extracted holders. In each list, `utxos` and `last_received` (latest
  creation height/time) are computed over that list's outputs only: the
  holder's extracted outputs for the extracted list, the rest for the other.

Per block, the tracker emits the row deltas; the store keeps a per-row
history `(row, height) → RowStats` for every block where the row changed, so
supply can be charted over the whole chain.

## 6. Architecture

```
          ┌──────────── fast initial sync (genesis → tip−12) ───────────┐
bitcoind  │ getblockhash (batched) → N× getblock(h,0) → hex→Block→txids │
  RPC ───►│   → in-order → UtxoValueMap (in-memory, sharded)            │──► ResolvedBlock
          └─────────────────────────────────────────────────────────────┘        │
          ┌──────────── live (after sync, restarts, catch-up) ──────────┐        ▼
          │ getblock(h,3): prevout values from Core's undo data         │──► Tracker (in-memory
          └─────────────────────────────────────────────────────────────┘    authoritative state)
                                                                                 │ BlockDelta
                                                                                 ▼
                                                    Store (redb: tracked UTXOs, sat locator,
                                                    script index, moves, row history, headers,
                                                    undo for the last 144 blocks)
                                                                                 │
                                                    Server (axum, JSON API, same process) ◄── dashboard
```

* **Fast source** (`src/source/fast.rs`): only from genesis; owns the full
  UTXO *value* map (outpoint → sats and the script's 32-bit
  `model::script_hash`; ~180–190 M entries, 12.25 GiB in 2^28 buckets of 49
  bytes). Unspendable outputs are never inserted. Stops at tip − 12, then
  the map is dropped.
* **Live source** (`src/source/live.rs`): `getblock <hash> 3` gives every
  input's prevout value and script; no UTXO map needed. Used after the fast
  sync, after any restart, and for catch-up. Both sources produce identical
  `ResolvedBlock`s (tested on real blocks): every input carries the spent
  output's value and script hash, its signature's sighash flag and ord's
  inscription envelopes (read from the scriptSig and witness).
* **Tracker** (`src/tracker/`): pure, deterministic, no I/O. `apply_block`
  returns a `BlockDelta` (moves, net created/spent tracked UTXOs, lost/burned
  pieces, row deltas) and `undo_block` reverses one.
* **Store** (`src/store/`): redb. During the fast sync commits every
  `commit_interval` blocks (default 5,000) or 30 s; live: every block. Undo
  records are kept for the last 144 blocks for reorgs.
* **Indexer** (`src/indexer.rs`): orchestration: fresh index → fast sync; then
  live loop polling `getbestblockhash` every 2 s; reorg = prev-hash mismatch →
  undo blocks until the parent matches; shutdown on SIGINT/SIGTERM after the
  current commit.
* **Server** (`src/server/`): read-only JSON API over redb read transactions
  and the published in-memory supply snapshot.

## 7. Contracts

The shared types live in `src/model.rs` and are the interface between the
modules; see the rustdoc there. The HTTP API is specified in `docs/API.md`.

## 8. Validation

* Unit tests per module; JS parity fixtures for types/rows/tracked set.
* Integration tests against `mockcore` (fees into coinbase, lost sats,
  OP_RETURN burns, intra-block spends, reorgs, BIP30-style overwrite).
* Mainnet oracle: loopshock runs upstream ord with `--index-sats`
  (`http://100.121.138.22/sat/<n>` returns `satpoint`); a sampled comparison
  of tracked sats' satpoints at the same height must match exactly.
* Fast-source vs live-source equality on real mainnet block ranges.
