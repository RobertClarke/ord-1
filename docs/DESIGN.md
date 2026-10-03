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
`REQUIRED_COMBINATION_TYPES`) with the scope the operator set. The runtime
also extracts every alpha mined in January–February 2009
(`satIsExtractableAlpha2009`, `isAlpha2009Sat`: blocks 0–5923); that is an
extraction criterion, not a rarity, so the tracker does not follow it (rules
version 4): such an alpha is tracked only when something else tracks it (a
vintage, Nakamoto, block 9, 78, 286 or 666 alpha, a milestone alpha or an
uncommon).

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
    `mythic`, which the runtime's cumulative tiers make uncommon too),
    `black`, `black_rare`, `black_epic`,
    `palindromes_integer_1d`, `palindromes_integer_2d`,
    `palindromes_perfect_palinception`, `bounty3_name`,
    `kilosat_alpha`, `decasat_alpha`, `centasat_alpha`, `megasat_alpha`,
    `kilosat_omega`, `decasat_omega`, `centasat_omega`, `megasat_omega`;
  * every type of any `REQUIRED_COMBINATION_TYPES` entry, e.g.
    `nakamoto`+`alpha`, `pizza`+`palindromes_integer`, `sr_1st_auction`+`black`,
    `palindromes_integer_3d`+`palindromes_integer`+`sr_1st_auction` (the full
    list is in §2.3).
* **Not tracked:** whole vintage / block 78 / block 286 / block 666 / pizza /
  jpeg / hitman / Silk Road / Nakamoto / 2009 ranges (only their special sats
  above; the runtime's standalone `block78`/`block286` ≥1M-sat chunks are out of
  scope), `common_name`, `requests`,
  inscriptions, luck (luck is a per-block property of already-tracked sats and
  can be derived from the stored headers).

Size at height ~969k under rules version 3: ≈2.8 M individual sats (≈969k
uncommon + ≈969k black, ≈296k Jan–Feb 2009 alphas, ≈50k vintage omegas,
≈40k milestone alphas/omegas, ≈35k 1d/2d palindromes, ≈12.5k bounty3 names,
≈400k other combo sats) plus the block 9 range (5e9 sats). Version 4 drops
the 240,921 Jan–Feb 2009 alphas nothing else tracks; 55,279 of the 296,200
stay tracked (the 50,000 vintage alphas of blocks 0–999, and the uncommon,
Nakamoto and milestone alphas of blocks 1000–5923).

The tracked set is a pure function of the sat number, so it can be enumerated
for any coinbase without chain data.

## 2. Sat types (exact port of the JS rules)

`types(sat) -> TypeSet` must return exactly what bitcoin-monitoring's
`getSatTypes(sat)` (`src/services/satRarityRules.js`) returns, plus exotic
range membership (`src/services/exoticSatRanges.js` → `EXOTIC_RANGES`), plus
`bounty3_name` (`data/bounty3-names-map.json`). Parity is enforced by test
vectors generated from the JS (`tests/fixtures/sat-types.json`).

**Two deliberate differences** (rules version 4):

* The runtime's scanner adds a synthetic `alpha_jan_feb_2009` type
  (`isAlpha2009Sat`) after normalization. It names an extraction criterion
  of the runtime, not a rarity, so the port has no such type (nor row), and
  the export tool leaves it out of the test vectors (the Jan–Feb 2009
  alphas stay in them, as plain alphas).
* The runtime's rarity tiers are cumulative: `getSatTypes` says a rare sat
  is also uncommon, sat 0 mythic, legendary, epic, rare and uncommon, a
  black rare also black. The port keeps only the highest tier of each
  family: mythic > legendary > epic > rare > uncommon, and black_epic >
  black_rare > black (a rare sat is just rare, as ord and Magisat count
  them). The test vectors keep the runtime's cumulative tiers; the parity
  test compares the port with them made exclusive, and the tracked set with
  the runtime's own rule on them (unchanged: every tier is tracked).

  Exclusive row totals: mythic 1, legendary 5, epic 27, rare 3,432,
  uncommon 6,926,535 (6,930,000 blocks less the 3,465 with a higher tier;
  epics are not rares except at multiples of 1,260,000), black_epic 33,
  black_rare 3,432, black 6,926,535; uncommon_2009 32,473, rare_2009 16,
  black_2009 32,474, vintage_uncommon 999. `alpha_uncommon`, `black_omega`
  and the place `_uncommon` / `_black` rows lose their higher-tier sats
  likewise.

Assembly follows the runtime scanner for a point sat
(`rareSatScanner.js` → `analyzeSatRanges`): rule types, then `bounty3_name` if
named, then every exotic range type containing the sat, then
`normalizePaliblockTypes` over the combined set — so `paliblock` is dropped
whenever an exotic type or `bounty3_name` is present (e.g. block 9's first
sat). `common_name`, name
palindromes and `inscription_*` types are not modelled.

### 2.1 Rule types (from `getSatTypes`)

For sat `s` in block `h` at `offset = s - first_sat(h)`, `subsidy = subsidy(h)`
(the runtime's cumulative tiers; the port then keeps only the highest tier
of each family, see the differences above):

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

The JS holds inclusive `[start, end]` ranges; the export writes half-open
`[start, end + 1)` ones. The pizza, jpeg, hitman and Silk Road lists are
copies of published half-open lists, which the JS read as inclusive until it
converted them at load: each range also claimed the sat after it, an
uncommon or alpha whenever the range ended on a block or BTC (pizza 378
uncommons instead of 201). Since rules version 3 they hold exactly 1e12,
5e10, 1.67e11 and 2,965,651,306,529 sats; the manifest's `special` counts
(uncommon, black, alpha, omega per type, by the JS's own block math) are
what the row tests check against.

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
alpha_uncommon kilosat_alpha decasat_alpha centasat_alpha megasat_alpha
kilosat_omega decasat_omega centasat_omega megasat_omega black_omega
palindrome_1d palindrome_2d palindrome_perfect_palinception bounty3_name
vintage_uncommon vintage_black vintage_alpha vintage_omega
block9 450x block9_1st_tx block9_alpha block9_omega block9_palindrome
block78_alpha block78_omega block78_palindrome
block286_alpha block286_omega block286_palindrome
block_666_alpha block_666_omega block_666_palindrome
nakamoto_uncommon nakamoto_black nakamoto_alpha nakamoto_omega nakamoto_palindrome
pizza_uncommon pizza_black pizza_alpha pizza_omega pizza_palindrome
jpeg_uncommon jpeg_alpha jpeg_omega jpeg_palindrome
hitman_uncommon hitman_black hitman_alpha hitman_omega hitman_palindrome
sr_uncommon sr_alpha sr_omega sr_black sr_palindrome_3d
uncommon_2009 rare_2009 black_2009 palindrome_2d_2009
```

`*_palindrome` means `palindromes_integer`; `_2009` rows are heights
`0..=32489`. A row may also have a sat-number scope (`Row::sats`):
`block9_1st_tx` is the block 9 sats of the first bitcoin transaction,
`[45e9, 46e9)` (block 170 sent them to Hal Finney).

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
  too: one collector's address holds 2,298 outputs packing rare sats at
  ~330 sats of padding each (100 rare sats in 33,000
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

### 5.1 Mint years

Every row is also kept per **mint year**, the UTC year of the block that
mined the sat (index schema 5). Next to the catalog's rows `0..R` the tracker
counts one **year row** per row and year: id `R + row × 128 + (year −
2009)` (`model::year_row`; 128 years reach past the last subsidy). They are
plain tracker rows: a piece's row counts are taken per year (the piece split
at the years' first sats) and each `(row, n)` is counted for the row and
for its year row, so a UTXO caches both and every aggregate above
(`mined` … `extracted`, holders, the Fenwick trees, the row history, undo)
holds for the year rows without code of its own.

A year starts at the first height whose running-max header time
(`max_times`, as for the active windows) falls in it, so years are
contiguous heights and sats. The tracker derives the starts from the header
times as blocks are applied and drops them as they are undone; they are not
part of the catalog (2027 has no start yet). On mainnet the running-max year
equals every block's own year (checked on all 969,639 headers), so 2009 is
heights 0–32,489: a year row of 2009 counts exactly the sats of the
`heights: (0, 32489)` rows (`uncommon_2009` = `uncommon` in 2009).

Holders do not add up across years. The supply snapshot therefore also
counts, per row, the scripts by the **set of years** they hold sats of (a
128-bit mask → number of scripts; a few hundred distinct masks per row),
from the per-script tallies the holders are counted from: the holders of
any set of years are the scripts whose mask meets it, exact. Top holders are
kept for each year row too, 100 per list. Measured on mainnet: see the
README.

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

### 6.1 Address history

`src/history.rs` derives, per block, one entry per (transaction, script)
that sent or received tracked sats from the block's `BlockDelta` and
`MarketDelta` (definitions in its module docs and docs/API.md "Address
history"); the indexer computes them after `market::analyze` and commits
them with the block. The store keys them by (script key, height,
tx_index), the script key being the first 8 bytes of its SHA-256, and
never prunes them; a rollback deletes the keys of the undone block's moves.

Coverage is index-defining (`TrackerConfig::address_history_from`, in the
catalog fingerprint): by default from the first block whose time reaches
`extracted_since` (the ordinals era). Measured on mainnet (2026-10-02), a
history from genesis would hold about 150–200 M entries (2.1 per moving
transaction; 138 M by height 715,000), some 15–20 GB on disk; the ordinals
era (from height 767,304) 26.4 M entries, 94 bytes each as stored: the
`history` table is 3.1 GB on disk, the whole index 5.2 GB (README).

During the initial sync entries are appended, keyed by height first
(sequential writes), to a separate staging file next to the index
(`index.history-staging.redb`), and the commit that completes the sync
moves them into `history` in key order, one bucket of script keys (about
512 MiB of staged records) per scan, then deletes the file. This avoids
rewriting B-tree pages all over the per-script table at every fast-sync
commit, and leaves no free space behind in the index. redb splits a full
leaf in two halves and never writes the left one again during a sorted
bulk insert, which leaves leaves half full; the bulk inserts write three
entries in five first and the other two in a second pass, which fills
them to about 80%.

### 6.2 Mempool overlay

`src/mempool/` (docs/API.md "Mempool") shows unconfirmed transactions that
move tracked sats, everywhere confirmed activity shows, without touching the
index: an in-memory overlay rebuilt from bitcoind at every start, so it
needs no reindex and no schema change.

* **Source.** Polling, not ZMQ: the target's bitcoind publishes no `rawtx`
  over ZMQ, and polling recovers from anything by itself. Every
  `--mempool-interval` (5 s), and within ~0.25 s of each index commit, the
  overlay thread calls `getbestblockhash` (it goes on only when it is the
  index's tip, before and after the listing), `getrawmempool false true`
  (txids and `mempool_sequence`; nothing more to do when the sequence and
  the tip are unchanged) and `getrawtransaction` for the new txids
  (batches of 1,000, 4 in flight, 200,000 per poll at most; one gone in the
  meantime is skipped). It runs only while the indexer is `live`, and not
  with `--no-sync` or `--no-mempool`.
* **State.** Every mempool transaction's input outpoints (~9 MB at 100k
  transactions); the pending set (transactions spending tracked sats) with
  their raw transactions, moves and classification; the values of their
  untracked inputs; tombstones of the last hour.
* **Candidates.** A transaction is pending when an input is a tracked UTXO
  of the index (a redb point lookup in one read transaction per poll) or a
  tracked output of another pending transaction. New transactions are
  examined parents first (a topological order within the poll), so chains
  of any depth are followed. On a new tip every mempool transaction is
  examined again: parents that confirmed now come from the index, and a
  reorg may track other inputs.
* **Classification**, as at the next height, with the block pipeline's own
  pure functions: `mempool::flow::tx_move` (a read-only port of the
  tracker's sat flow, §4; a test checks it against the tracker's moves on
  randomly cut block 9 pieces), `market::analyze` on a pseudo-block
  `[placeholder coinbase, tx, its pending spenders]` (so a two-transaction
  sale settled in the mempool is attributed as in a block; a test checks
  every sale fixture classifies as in its block), and
  `history::move_entries`. Untracked inputs are valued with `gettxout
  <txid> <n> false` (the chainstate) or, for unconfirmed parents, the
  parent's raw transaction (`getrawtransaction` reads mempool transactions
  without `txindex`; a confirmed parent that is spent in the chainstate
  needs `txindex`); `getrawtransaction … 2` gives no prevouts for mempool
  transactions. `getmempoolentry` gives the fee, vsize and first-seen time.
  A transaction whose inputs or entry cannot be resolved yet is retried at
  the next poll.
* **Removal.** A pending transaction that leaves the mempool is
  `confirmed` (a move of a block indexed since the last poll, looked up in
  at most 24 blocks), `conflicted` (such a block spent one of its tracked
  inputs), `replaced` (a mempool transaction spends one of its inputs) or
  `evicted`; a child of a parent that left unconfirmed shares its fate.
* **Publication.** After every poll the thread publishes an immutable
  `Snapshot` (`Published::mempool`); the handlers render only that, with
  the same renderers as confirmed records, then mark the result pending.
  `seq` changes only when the pending set does, so clients poll
  `/mempool/status` and refetch on change (ETag `"mempool-<seq>"`).

Limits: a pending transaction is classified for the next height (fee
tables are per height; the next block may be later); tracked sats paid as
fee have no location until a coinbase claims them; inscriptions revealed in
the mempool are reported as the block would (`market::analyze`), but the
index's inscribed counters ignore them.

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
