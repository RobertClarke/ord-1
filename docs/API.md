# HTTP API

JSON over HTTP, read-only, served by `ord server` (default `127.0.0.1:8090`).
Every response is `application/json` except the streaming endpoints, which
are newline-delimited JSON (`application/x-ndjson`, one object per line).
Streams are read chunk by chunk from the live index, so a long stream can
include blocks committed after it started; one that fails midway ends with
an `{"error": …}` line and a truncated body, and a client that reads nothing
for 60 s has its stream cut off the same way.
Errors: `{"error": "<message>"}` with 400 (bad input), 404 (unknown / not
tracked) or 503 (index not ready). Sat numbers and values are JSON numbers
(all < 2^53). Hashes and txids are hex in RPC byte order. `address` is the
mainnet (or `--chain`) address of the script, or `null` for scripts without
one (P2PK, bare multisig, nonstandard); `script_pubkey` is always present
(hex). Times are unix seconds (block header time).

This document is the contract with the bitcoin-monitoring supply dashboard
(`npm run supply`). Additive changes are fine; renames are breaking.

## Common objects

**Piece**
```json
{ "offset": 0, "start": 45000000000, "len": 1, "types": ["uncommon", "alpha", "vintage", "block9", "450x", "nakamoto"] }
```
`types` are the types of the piece's first sat (for `len > 1` pieces the
other sats may have fewer types). `offset` meaning depends on the holder
(see docs/DESIGN.md §4.1).

**Output** (a tracked UTXO)
```json
{
  "outpoint": "txid:vout",
  "value": 546,
  "address": "bc1p…" ,
  "script_pubkey": "5120…",
  "kind": "spendable" | "unspendable",
  "created_height": 800000,
  "created_time": 1690000000,
  "extracted": true,
  "pieces": [Piece],
  "rows": { "uncommon": 1, "black": 1 }
}
```
`extracted` follows DESIGN.md §5, the same verdict for every row the output
holds: spendable, created on/after `extracted_since`, and worth at most its
extraction budget: `extracted_max_value` (10,000) sats per individually
tracked sat, plus, for each run of block 9 (range) sats in a piece, the run's
length + 10,000. A collector consolidation of 200 rare sats in 66,000 sats,
or 100,000 block 9 sats in 100,438, is extracted; 2 uncommons in 1 BTC are
not. Move inputs and outputs carry the same flag.

**RowStats**
```json
{ "mined": 969238, "located": 969100, "burned": 3, "lost": 120, "overwritten": 2,
  "extracted": 51234, "holders": 400000, "extracted_holders": 20000 }
```
`extracted` counts the row's located sats in extracted outputs (the
Output's `extracted` flag above; DESIGN.md §5), `extracted_holders` the
scripts holding at least one of them; `active_non_extracted` and the
non-extracted holder list cover the rest. Since rules version 2 a collector
consolidation (many rare sats at ~330 sats of padding each) counts as
extracted.

## Endpoints

### `GET /status`
```json
{
  "chain": "mainnet",
  "version": "0.29.0-sats.1", "commit": "abc123…",
  "height": 969237, "hash": "…", "time": 1790734122,
  "bitcoind_height": 969240,
  "mode": "starting" | "fast_sync" | "catch_up" | "live" | "serving",
  "fast_sync_complete": true,
  "started": 1790734000,
  "tracked_utxos": 1234567, "tracked_pieces": 2345678,
  "lost_sats": 2895844904,
  "ranges": [{ "id": "block9", "start": 45000000000, "end": 50000000000 }],
  "extracted_max_value": 10000, "extracted_since": 1670976000,
  "catalog_fingerprint": "…",
  "moves_retention": "4032", "moves_from": 965206,
  "address_history": true, "address_history_from": "ordinals",
  "address_history_from_height": 767430, "address_history_from_time": 1670976283,
  "address_history_building": false,
  "year_stats": true,
  "years": [{ "year": 2009, "from_height": 0, "to_height": 32489 }, …,
            { "year": 2026, "from_height": 930341, "to_height": 969237 }],
  "luck_stats": true, "max_luck": 31,
  "lucks": [{ "luck": 0, "blocks": 272084 }, …, { "luck": 21, "blocks": 1 }],
  "timings": { "blocks_per_second": 0.1, "last_commit_blocks": 1,
               "last_commit_seconds": 0.02, "last_commit_at": 1790734130,
               "last_poll_ok_at": 1790734141, "commits": 12,
               "commit_seconds_total": 0.4, "supply_seconds": 0.05,
               "fast_sync_seconds": null, "restore_seconds": 2.1 },
  "error": null
}
```
`height` is `null` until the first commit. `mode`: `starting` (opening the
index, reaching bitcoind), `fast_sync`, `catch_up` (more than one block
behind), `live` (following the tip), or `serving` (`ord server --no-sync`:
the index as it is, read-only, not indexing). `moves_retention` is
`--moves-retention` (a number of blocks or `"all"`); `moves_from` is the
lowest height whose moves are kept. `error` is the last indexing error
(e.g. bitcoind unreachable), cleared by the next commit.
`address_history` (additive) is `true` on builds that serve
`/address/{address}/history`; `address_history_from` is
`--address-history-from` (`ordinals`, `all` or a height),
`address_history_from_height` / `_time` the first block the history covers
(`null` until one is indexed), and `address_history_building` is `true`
while the initial sync is still staging it (history requests answer 503).
`year_stats` (additive) is `true` on builds that serve the supply by mint
year (below); `years` are the mint years of the indexed blocks (empty until
the first commit). `luck_stats` (additive, index schema 6) is `true` on
builds that serve the supply by block luck; `max_luck` is the highest luck
broken down (it also counts any luckier block) and `lucks` the indexed
blocks of each luck (lucks without a block left out).

Staleness: `mode` stays `live` while bitcoind is unreachable; `error` then
says why, and two unix times tell how stale the index may be:
`timings.last_commit_at` (the last block committed) and
`timings.last_poll_ok_at` (the last time bitcoind answered: a poll of its
tip every 2 s when live, or blocks delivered during the fast sync). Both are
`null` until they first happen, and stay `null` in `serving` mode.

### `GET /rows`
Row definitions, in `RowId` order.
```json
[{ "id": "uncommon", "label": "Uncommon", "all_of": ["uncommon"], "any_of": [],
   "heights": null, "range": false, "market": true, "total": 6930000 }]
```
`market` (additive): whether sales are priced per the row's sats (`unit` in
`Sale`). `false` for label rows that name the same circulating sats as
another row: `block9_1st_tx` holds every block 9 sat that ever moved (the
other 40 BTC of block 9 never did), so its sales are priced as `block9`.

The row set is docs/DESIGN.md §3. Rules version 4 (a new
`catalog_fingerprint` in `/status`, so a reindex) removed the
`alpha_jan_feb_2009` row and type (an extraction criterion of the runtime,
not a rarity): an index built under version 4 never reports it, in `/rows`,
`/supply`, `types`, `rows` or sales, and no longer tracks the Jan–Feb 2009
alphas that nothing else tracks. Clients that also read older indexes should
ignore that row and type. Version 4 also makes the rarity tiers exclusive:
`types` keep only the highest of mythic > legendary > epic > rare >
uncommon and of black_epic > black_rare > black (sat 0 is `mythic`, not
also `uncommon`; a rare sat is `rare` only), so the `uncommon`, `black`,
`rare`, `epic`, `black_rare` rows and every row combining them count each
sat under its highest tier only (totals in docs/DESIGN.md §2).

### `GET /supply[?years=|?luck=]`
```json
{
  "height": 969237, "hash": "…", "time": 1790734122,
  "rows": [{
    "id": "uncommon",
    "total": 6930000,
    "stats": RowStats,
    "active_non_extracted": { "1y": 1, "2y": 2, "3y": 3, "4y": 4, "5y": 5, "7y": 7 },
    "delta": { "height": 969237, "before": RowStats, "after": RowStats } | null
  }]
}
```
`delta` is the change the tip block made to the row (`null` if none).

### `GET /supply/{row}?top=100[&years=|&luck=]`
One row as in `/supply` plus holders:
```json
{ …row…,
  "top_extracted_holders": [{ "address": "bc1p…", "script_pubkey": "…", "n": 12, "utxos": 3,
                              "last_received_height": 930000, "last_received_time": 1766000000 }],
  "top_non_extracted_holders": [ … ] }
```
`top` ≤ 1000, default 100.

### `GET /supply/{row}/history?from=&to=&every=[&years=|&luck=]`
Row counters over time (only heights where the row changed are stored; each
bucket of `every` blocks reports the last value at or before its end).
```json
{ "row": "uncommon", "points": [{ "height": 144, "time": 1231731025, "stats": RowStats }] }
```
Defaults: `from=0`, `to=tip`, `every` chosen so ≤ 2,000 points are returned.

## Supply by mint year

Every row's counters are also kept per **mint year**: the UTC year of the
block that mined the sat (by the running maximum of the header times, so a
year is a contiguous range of heights; on mainnet it is every block's own
year). 2009 is heights 0–32,489, the scope of the `_2009` rows: `years=2009`
reports exactly their numbers for `uncommon`, `rare`, `black` and
`palindrome_2d`. Additive since index schema version 5 (a new
`catalog_fingerprint`: reindex); `/status` says `"year_stats": true`.

`years=2009,2010` (or `year=`) on `/supply`, `/supply/{row}` and
`/supply/{row}/history` restricts every row to the sats mined in those
years. Each must be a year blocks were mined in (`/status` `years`), at most
64; anything else is a 400.

* `total` and `mined`: the row's sats mined in those years so far (the
  whole year for a past year).
* `located`, `burned`, `lost`, `overwritten`, `extracted`, `inscribed` and
  `active_non_extracted`: the years' counts added up.
* `holders` and `extracted_holders`: the scripts holding sats of any of the
  years, exact for any set (a script holding sats of two of them counts
  once).
* `delta`: the tip block's change to those years' counters; for more than
  one year its holder counts are reported unchanged (`before` = `after`).
* `/supply/{row}?years=` lists top holders for a **single** year only (at
  most 100 per list), and leaves both lists out for a set of years.
* `/supply/{row}/history?years=` adds up the years' history point by point
  and echoes `"years": [2009, 2010]`; `holders` and `extracted_holders` are
  `null` for more than one year.
* A row with no sats of those years is reported with zeros (`delta`
  `null`).

### `GET /supply/years`
Every row's counters for each year it has mined sats of:
```json
{
  "height": 969237, "hash": "…", "time": 1790734122,
  "years": [{ "year": 2009, "from_height": 0, "to_height": 32489 }, …],
  "rows": [{
    "id": "uncommon",
    "years": {
      "2009": { "total": 32473, "stats": RowStats + "inscribed",
                "active_non_extracted": { "1y": …, … } },
      …
    }
  }]
}
```

## Supply by luck

Every row's counters are also kept per **block luck**: the leading zero
bits of the block's hash (as shown, big-endian) less the leading zero bits
of the target its header's `bits` encode, the definition and values of
bitcoin-monitoring's sat luck (`calculateLuckFromHashAndTarget`). Most
blocks are luck 0–3; on mainnet (to 969,652) 1,328 blocks have luck 10 or
more and the luckiest (756,951) has 21. Lucks are counted up to `max_luck` (31), a luckier block
in 31; a block whose hash misses its target (none on a valid chain; mock
chains in tests) counts as 0. Additive since index schema version 6 (a new
`catalog_fingerprint`: reindex); `/status` says `"luck_stats": true`.

`luck=N` (luck N or more) or `luck=N-M` (N to M), from 0 to 31, on
`/supply`, `/supply/{row}` and `/supply/{row}/history` restricts every row
to the sats mined in blocks of those lucks; anything else is a 400, and so
is `luck` together with `years` (the supply is broken down by one or the
other, not both).

* The counters, `total`, `inscribed`, `active_non_extracted` and `delta`
  are as for `years` (the selected lucks added up), and so are the holders:
  `holders` and `extracted_holders` count the scripts holding sats of any
  of the selected lucks, exact for any range (a script holding sats of two
  of them counts once).
* `/supply/{row}?luck=` lists no top holders.
* `/supply/{row}/history?luck=` adds up the lucks' history point by point
  and echoes `"luck": [N, M]` (`[N, 31]` for `luck=N`); `holders` and
  `extracted_holders` are those of the row for `luck=0` (every luck: the
  row's own history), of the luck for `N-N`, and `null` otherwise.

### `GET /supply/lucks`
Every row's counters for each luck it has mined sats of:
```json
{
  "height": 969237, "hash": "…", "time": 1790734122,
  "lucks": [{ "luck": 0, "blocks": 272084 }, …],
  "rows": [{
    "id": "uncommon",
    "lucks": {
      "12": { "total": 172, "stats": RowStats + "inscribed",
              "active_non_extracted": { "1y": …, … } },
      …
    }
  }]
}
```

### `GET /sat/{n}`
```json
{
  "sat": 45000000000, "height": 9, "types": ["uncommon", …], "tracked": true,
  "rows": ["uncommon", "block9", …],
  "location": {
    "kind": "unspent" | "burned" | "lost" | "overwritten",
    "satpoint": "txid:vout:offset", "outpoint": "txid:vout", "offset": 1000000,
    "output": Output | null
  } | null
}
```
Untracked sats: `"tracked": false, "location": null`. `output` is `null` for
`lost`.

### `GET /output/{txid}:{vout}`
`Output`, or 404 when the output holds no tracked sat (or never existed).

### `GET /address/{address}` (or `/script/{hex}`)
```json
{ "address": "…", "script_pubkey": "…", "outputs": [Output], "rows": { "uncommon": 3 } }
```

### `GET /block/{height}`
```json
{ "height": 9, "hash": "…", "prev_hash": "…", "time": 1231473279,
  "moves": 1, "rows_moved": { "uncommon": 1 } }
```
`moves` and `rows_moved` are `null` below `moves_from` (moves not retained).

### `GET /block/{height}/moves`
```json
{ "height": 170, "hash": "…", "time": 1231731025, "moves": [Move] }
```
404 below `moves_from`.
**Move**
```json
{
  "txid": "…", "tx_index": 1, "coinbase": false,
  "input_value": 5000000000, "output_value": 5000000000, "fee": 0,
  "inputs": [{ "vin": 0, "outpoint": "…:0", "value": 5000000000, "address": null,
               "script_pubkey": "41…ac", "created_height": 9, "created_time": 1231473279,
               "extracted": false, "pieces": [Piece] }],
  "outputs": [{ "vout": 0, "value": 1000000000, "address": null, "script_pubkey": "41…ac",
                "kind": "spendable", "extracted": false, "pieces": [Piece] }],
  "fee_pieces": [Piece],
  "mined_pieces": [Piece],
  "lost_pieces": [Piece],
  "rows": { "block9": 5000000000, "uncommon": 1 }
}
```
`rows` counts the row sats the transaction moved (for the coinbase: minted
or received).

### `GET /moves?from=&to=[&row=]` (NDJSON)
One line per move, oldest first: `{ "height": 170, "time": …, "move": Move }`.
`to - from` ≤ 10,000 blocks (`to` is inclusive and defaults to the tip);
404 when `from` is below `moves_from`. With `row`, only moves that moved sats
of that row.

### `GET /sats?row=[&from=&to=]` (NDJSON)
Every tracked piece of the row (optionally only sats in `[from, to)`), in sat
order, one line per piece:
`{ "piece": Piece, "count": 1, "location": { …as in /sat… } }` where `count`
is the number of the row's sats in the piece. Used for ad-hoc reports.
The location's `output` leaves out its `pieces` array (mainnet outputs hold
up to tens of thousands of pieces, which every line would repeat); add
`&pieces=true` to include it.

`&count=1` (additive; also `count=true`, and `count=0` is the stream): the
same scan counted instead of streamed, one JSON object:
```json
{ "row": "block9", "from": 45000000000, "to": 50000000000, "count": 5000000000,
  "by_kind": { "unspent": 4999999000, "burned": 0, "lost": 1000, "overwritten": 0 },
  "pieces": 267000 }
```
`count` is the row's tracked sats in `[from, to)` (`to` capped at the
supply), `by_kind` the same by location kind, `pieces` the tracked pieces
the scan went through (of any row). `from` and `to` are sats, as in the
stream: the sats mined at heights `a..=b` are `[first sat of a, first sat
of b + 1)`. Same validation and errors as the stream. It scans the same
locator range in one read transaction, without output lookups or JSON per
piece.

### `GET /blocks?from=&to=`
`[{ "height": …, "hash": "…", "time": …, "bits": "1d00ffff", "luck": 11 }]`,
`to - from` ≤ 10,000. `bits` is the header's compact target (8 hex digits),
`luck` the block's luck ("Supply by luck"; `null` for a hash above its
target). Both are additive.

## Address history

Every transaction that moved tracked sats, per address: one **entry** per
transaction and script that sent or received tracked sats, kept forever
(whatever `--moves-retention`) and rolled back with its block on a reorg.
It covers the blocks from `address_history_from_height` on (`/status`):
by default (`--address-history-from ordinals`) the first block whose
header time is at least `extracted_since` (2022-12-14) and every block
after it, which holds every extraction (no output counts as extracted
before then) and the sales of the ordinals era; `all` covers every block
from genesis (about ten times the size on mainnet), a height every block
from it. The setting is index-defining: changing it means a reindex.

The history came with index schema version 4 and a new
`catalog_fingerprint` (the fingerprint covers `address_history_from`):
indexes built before it are refused and must be rebuilt (`--reindex`).

### `GET /address/{address}/history?limit=&before=&kinds=&row=&from=&to=` (or `/script/{hex}/history`)
```json
{
  "address": "bc1p…", "script_pubkey": "5120…",
  "history_from": 767430,
  "entries": [HistoryEntry],
  "next": "969001:12" | null
}
```
Entries newest first (height descending, then `tx_index` descending).
* `limit`: 1–1000, default 100.
* `before`: the `next` cursor of the previous page (an opaque token; today
  `height:tx_index`, exclusive). `next` is `null` on the last page. A page
  reads at most 10,000 entries: a filtered page may hold fewer than `limit`
  entries (even none) with a non-null `next`; keep following it.
* `kinds`: comma-separated kinds (below); default all.
* `row`: only entries whose `rows_in` or `rows_out` hold that row.
* `from`, `to`: heights, inclusive (default the whole history).

An address without history is `200` with `"entries": [], "next": null`.
Errors: 400 for a bad address, script, `limit`, `before`, `kinds` or
`from` > `to`; 404 for an unknown `row`; 503 before the first block is
indexed and while `address_history_building`. Builds without the history
answer the generic 404 `no such endpoint` (how a client detects it, with
`/status` `address_history`).

**HistoryEntry**
```json
{
  "txid": "…", "height": 969001, "time": 1790000000, "tx_index": 12,
  "direction": "in" | "out" | "self",
  "kind": "extraction" | "sale" | "transfer" | "consolidation" | "burn" | "mined",
  "rows_in": { "uncommon": 1 }, "rows_out": {},
  "sats_in": 1, "sats_out": 0,
  "value_in": 546, "value_out": 0,
  "outputs_in": 1, "inputs_out": 0,
  "extracted_in": true, "extracted_out": false,
  "extracted_sats": 1, "isolated": 1, "burned": 0,
  "roles": [] | ["seller"] | ["buyer"],
  "counterparty": "bc1q…" | null, "counterparty_script_pubkey": "0014…" | null,
  "counterparties": 1,
  "lead_sat": { "sat": 1968750000000000, "types": ["epic", "alpha", …] } | null,
  "sales": [Sale]
}
```
Of one transaction, for this address (script):
* **in** is what it received: its spendable outputs that received tracked
  sats; **out** what it sent: the tracked UTXOs it spent. `rows_in` /
  `rows_out` are their row counts (as `rows` in `Output`), `sats_in` /
  `sats_out` their tracked sats, `value_in` / `value_out` the values of
  those outputs and UTXOs (outputs and inputs without tracked sats, such
  as change or a buyer's payment, are not counted), `outputs_in` /
  `inputs_out` how many there are. Unspendable outputs (OP_RETURN, …) are
  nobody's: their sats are `burned`, counted for the addresses whose inputs
  held them.
* `direction`: `in` (received only), `out` (sent only), `self` (both).
* `extracted_in`: some output it received is extracted (`Output`'s
  `extracted`); `extracted_out`: some UTXO it spent was. `extracted_sats`:
  the tracked sats it received into extracted outputs.
* `isolated`: of those, the sats that came (by sat number) from inputs that
  were not extracted: the sats this transaction isolated. Sats moved between
  extracted outputs were isolated already, so consolidating extracted
  outputs isolates nothing, while pulling rare sats out of an exchange's
  output into a small one does (the dashboard's notable-extraction rule,
  without its notability thresholds). 0 for the coinbase.
* `kind`, the first that applies:
  1. `mined`: a coinbase receipt (subsidy and fee sats);
  2. `sale`: the address is the seller (`roles: ["seller"]`) or the buyer
     (`["buyer"]`) of a sale detected in the transaction (`/sales`);
     `sales` holds those sales (empty for other kinds);
  3. `extraction`: `isolated > 0`;
  4. `burn`: `burned > 0`;
  5. `consolidation`: `self`, at least two tracked UTXOs spent into exactly
     one tracked output of its own, and no tracked sat sent to another
     address (fees aside);
  6. `transfer`: anything else (a self transfer is `transfer` with
     direction `self`).
* `counterparties`: the other addresses (scripts) it exchanged tracked sats
  with: those whose inputs' sats it received, and those whose outputs
  received its sats. `counterparty` is the main one, its address (`null`
  for a script without one; `counterparty_script_pubkey` is always set when
  there is one): for `in` the one that sent it the most tracked sats, for
  `out` the one it sent the most to, ties to the lowest vin or vout; for
  `self` the main receiver, else the main sender; `null` when there is none
  (the coinbase, sats it kept or burned).
* `lead_sat`: its rarest tracked sat in the transaction (received or sent),
  a sat of the row with the fewest sats that will ever exist (`total` in
  `/rows`), with its types (exclusive tiers).

Addresses are keyed by the first 8 bytes of the script's SHA-256: two
scripts sharing them (2^-64 odds per pair) would share a history.

## Sales and inscriptions

Sales and inscriptions of tracked sats, detected per block by
`market::analyze` and stored with the block: kept forever (whatever
`--moves-retention`), rolled back with it on a reorg. Field names are the
contract with the dashboard; the detection rules and price semantics are
calibrated against Magisat's sales history (docs/SALES-SHAPES.md) and
on-chain marketplace fee addresses (data/marketplaces.json).

**Sale** — one tracked-sat sale: a seller's input carrying tracked sats, paid
for in the same transaction (a marketplace listing signed
SINGLE|ANYONECANPAY, an accepted offer, a legacy Magic Eden trade, …).
```json
{
  "txid": "…", "height": 968899, "time": 1790000000, "tx_index": 12, "vin": 0,
  "marketplace": "magisat" | "magic_eden" | "satflow" | "ordinals_wallet" | "ord_net" | "gamma" | "raresats" | "ordswap" | "goldmine" | "tx_dev" | "unisat" | "unknown",
  "kind": "listing" | "offer" | "unknown",
  "seller": { "address": "bc1p…", "script_pubkey": "…", "outpoint": "txid:vout", "value": 546 },
  "buyer": { "address": "bc1p…", "script_pubkey": "…", "outpoint": "txid:vout", "value": 546 } | null,
  "payment_vout": 1 | null,
  "price": 45546,
  "seller_proceeds": 45319,
  "fees": 227,
  "premium": 45000,
  "pieces": [Piece],
  "sats": [{ "sat": 1150805000000000, "height": 250322, "time": 1375694270, "inscribed": false }],
  "rows": { "uncommon": 1, "alpha": 1, "alpha_uncommon": 1 },
  "unit": { "row": "alpha_uncommon", "count": 1, "price": 45000 } | null,
  "pure_rows": ["alpha_uncommon", "uncommon", "alpha"],
  "inscribed": false
}
```
* `time` is the time of the block of the sale (`height`).
* `seller`: the sold UTXO (`seller.value` its value). `buyer`: the output
  that received the sold sats (`null` if none did, e.g. they went to fees);
  its `value` is `null` when unknown. `payment_vout`: the output paying the
  seller, when identified (additive).
* `marketplace`: an id from `data/marketplaces.json`, by the fee output (or
  marker / layout) the transaction carries; `unknown` when it carries none
  we know (e.g. OKX fills and Magisat fees below dust have no fee output).
  `raresats` (raresats.net, from height 969,480): its listings (`kind`
  `listing`), by its fee output or its OP_RETURN `raresats.net` marker; its
  accepted offers (`kind` `offer`), only in its exact offer layout carrying
  both: the seller's UTXO at vin 0 passing whole to the bidder at vout 0,
  the price to the seller's own address at vout 1, its fee at vout 2, the
  bidder's optional change, the marker last (`payment_vout` 1, `buyer`
  vout 0). A transaction paying its fee address or carrying its marker in
  any other shape (a transfer) is no sale, and its fee never attributes or
  prices a two-party (ALL|ANYONECANPAY) sale.
* `price`: the sale price as the venue states it. Magisat: its listing price
  — the seller's payment plus Magisat's maker fee (⌊payment / 199⌋ from
  height 861,287), excluding the buyer's fee; Magisat offers: the seller's
  payment plus half the fee output. raresats: its listing or offer price,
  the seller's payment (no maker fee; the buyer's or bidder's fee is on top
  of the price), so `fees` is 0. Elsewhere: the seller's payment plus the marketplace fees
  attributable to the listing (a shared fee output is split over a
  transaction's sales in proportion to their payments).
  `seller_proceeds`: what the seller netted (the payment output, less the
  seller's own sats that went into it). `premium` = `price` − `seller.value`
  (what was paid for the sats themselves; a signed number);
  `unit.price` = `premium` / `unit.count` (integer division) for the sale's
  unit row: its rarest pure market row (`market` in `/rows`; pure: below),
  or, for a mixed bundle that no market row holds whole, its rarest market
  row with sold sats (rarest: fewest sats that will ever exist, `total` in
  `/rows`, ties to the first row). So a block 9 chunk gets a per-sat price
  as `block9` (or `450x` inside block 9's first BTC) even when it holds a
  palindrome or an alpha. `unit` is `null` only for a sale whose sats count
  toward no row.
* `pure_rows` (additive): the rows every sold sat belongs to, rarest first,
  label rows included — a block 9 chunk is pure for `block9` (and
  `block9_1st_tx` / `450x` when inside them), a single 2009 alpha uncommon
  for `uncommon`, `alpha`, `alpha_uncommon`, `uncommon_2009`, …; empty for
  a mixed bundle. The sale can be charted under each of them, at `premium`
  / its `rows` count.
* `pieces`: the sold UTXO's tracked pieces (offsets within that UTXO); for a
  sale that sells only part of a UTXO (a split sale), only the pieces that
  left the seller.
* `rows`: the sold sats per row, as in `Output`.
* `sats`: the individually special sats sold — sats that count toward a row
  other than the pure-range rows (`range: true` in `/rows`), so a block 9
  chunk lists its alphas, palindromes, … but not every block 9 sat — in sat
  order within each piece, at most 100; each with the height and time of
  the block that mined it and whether it carried an inscription when sold.
* `inscribed`: some sold tracked sat carried an inscription when sold (an
  outlier for rare-sat pricing; the fork tracks inscriptions only on tracked
  sats). "When sold" means inscribed by an earlier transaction: one later
  in the sale's own block (a buyer inscribing right away) does not count.

### `GET /sales?row=&from=&to=` (NDJSON)
Sales oldest first (by height, transaction, input), heights `from..=to`
(default `0` to the tip), optionally only those whose pieces include sats of
`row`. One `Sale` per line. 400 for `from` > `to`, 404 for an unknown row,
503 before the first commit.

### `GET /sales/tx/{txid}`
`{ "txid": "…", "sales": [Sale] }` by input (404 when the transaction sold
nothing tracked).

### Additions to existing objects
* `/sat/{n}`: `"inscribed": bool` (the sat carries an inscription now),
  `"inscriptions": n` (envelopes that landed on it, counting
  reinscriptions), `"sales": [Sale]` (sales whose pieces include it, oldest
  first).
* `Output`: `"inscribed_sats": [n, …]` (tracked sats in the output that carry
  inscriptions, in sat order).
* `RowStats` in `/supply` and `/supply/{row}` (`stats`): `"inscribed"` —
  located sats of the row that carry inscriptions now, from the inscribed
  sats and their current locations (recomputed once per block). History
  points and `delta` stats do not have it.

## Mempool

Unconfirmed transactions that move tracked sats, classified exactly as a
block would classify them: their sat flow (`Move`), sales (`Sale`, every
marketplace) and address history (`HistoryEntry`), as if mined at the next
height. They come from an in-memory overlay that follows bitcoind's mempool
(docs/DESIGN.md §6.2) and are never written to the index: `/supply*`,
`/sales`, `/sales/tx`, `/moves`, `/block*` and `/rows` never hold them, so
no total, volume or holder count includes a pending item. A pending
transaction that confirms becomes the normal confirmed record, with the
same `txid` (and `vin` for a sale).

`/status` says `"mempool": true` on builds that serve the endpoints below
(the way a client detects them) and `"mempool_enabled"`, whether the
overlay runs (`false` with `--no-mempool` or `--no-sync`; every endpoint
then answers as with an empty mempool).

**A pending object** is its confirmed shape plus `"pending": true`,
`"first_seen"` (unix seconds bitcoind first saw the transaction) and
`"fee_rate"` (the transaction's sat/vB, one decimal), with `"height": null`,
`"tx_index": null` and `"time"` = `first_seen`. Pending sales, entries and
outputs are judged at `first_seen` where a block time would be used (the
`extracted` verdict).

### `GET /mempool/status`
The change feed: poll it every few seconds and refetch the rest when `seq`
changes.
```json
{
  "enabled": true, "ready": true,
  "seq": 42,
  "height": 969650, "hash": "…",
  "mempool_sequence": 5966578, "mempool_txs": 36111, "scanned_txs": 36111,
  "pending_txs": 105, "pending_sales": 3, "pending_extractions": 1, "removed": 7,
  "interval_ms": 5000, "last_poll_at": 1790734141, "last_poll_ms": 312,
  "last_change_at": 1790734100, "error": null
}
```
* `seq`: bumped whenever the pending set changes (a transaction added,
  reclassified or removed); it starts at 0 with the process.
* `height`, `hash`: the index tip the pending set is consistent with
  (pending transactions are classified as at `height + 1`). The overlay
  only advances while bitcoind's best block is the index's tip, so a
  transaction that just confirmed is never reported dropped before its
  block is indexed.
* `ready`: the first poll is done. `mempool_txs`: transactions in bitcoind's
  mempool at the last poll; `scanned_txs`: those whose inputs were checked
  (fewer only while a huge mempool is being read, 200,000 per poll).
* `error`: the last poll's error (bitcoind unreachable…), cleared by the
  next good poll.

### `GET /mempool[?kind=][&row=]`
Every pending transaction, newest `first_seen` first, and the recently
removed ones. Answers carry `ETag: "mempool-<seq>"`; a request with
`If-None-Match` of the current tag gets 304 (also on the lists below).
```json
{ "seq": 42, "height": 969650, "hash": "…", "txs": [PendingTx], "removed": [Removed] }
```
`kind` (`sale`, `extraction`, `transfer`, `consolidation`, `burn`) keeps
transactions with an entry of that kind (400 for another); `row` those
that moved sats of the row (404 for an unknown row).

**PendingTx**
```json
{
  "txid": "…", "pending": true, "first_seen": 1790734000,
  "fee": 2100, "vsize": 210, "fee_rate": 10.0,
  "depends": ["txid"],
  "kinds": ["sale"],
  "move": Move,
  "sales": [Sale],
  "entries": [PendingEntry]
}
```
* `depends`: the pending transactions whose tracked outputs it spends (a
  chain; `[]` if none). Chains of any depth are followed.
* `kinds`: its entries' kinds, distinct, in the order `mined`, `sale`,
  `extraction`, `burn`, `consolidation`, `transfer`.
* `move`: as in `/block/{height}/moves`, with `tx_index` `null`; an input
  that is a pending parent's output has `created_height` and
  `created_time` `null`. Tracked sats paid as fee are its `fee_pieces`:
  they go to a coinbase not mined yet.
* `sales`: pending Sales (the Sale shape of `/sales`, marked pending), by
  input.
* `entries`: one PendingEntry per address (script) that sent or received
  tracked sats: the `HistoryEntry` of the address history marked pending,
  plus `address` and `script_pubkey`; its `sales` are pending Sales.

**Removed**
```json
{ "txid": "…", "status": "confirmed" | "replaced" | "conflicted" | "evicted",
  "height": 969651 | null, "replaced_by": "txid" | null, "at": 1790734200, "seq": 43 }
```
A pending transaction that left the mempool, kept for an hour (at most
1,000): `confirmed` in block `height` (indexed since); `conflicted`, a
block spent one of its tracked inputs in another transaction; `replaced`,
a transaction now in the mempool (`replaced_by`) spends one of its inputs
(replace-by-fee); `evicted`, none of these (expiry, the size limit).
A transaction whose pending parent left unconfirmed shares the parent's
status. `at`: when it was seen gone; `seq`: the `seq` that removed it.

### Lists
* `GET /mempool/sales[?row=]`: `{ "seq", "height", "hash", "sales": [Sale] }`,
  every pending sale (newest first); `row`: sales that sold sats of the row.
* `GET /mempool/extractions[?row=]`: `{ "seq", "height", "hash", "txs":
  [PendingTx] }`, transactions with an `extraction` entry (`isolated > 0`).
* `GET /mempool/transfers[?row=]`: the same, transactions with neither a
  `sale` nor an `extraction` entry (transfers, consolidations, burns).
* `GET /mempool/tx/{txid}`: one PendingTx; 404 when it is not pending,
  with `"removed": Removed` in the body when it left recently.
* `GET /mempool/address/{address}` (or `/mempool/script/{hex}`):
  ```json
  { "address": "…", "script_pubkey": "…", "seq": 42,
    "entries": [PendingEntry],
    "spent": [Spend + "outpoint"],
    "outputs": [PendingOutput] }
  ```
  the address's pending entries, its confirmed tracked outputs being spent,
  and the pending tracked outputs paying it.

### Additions to other endpoints
* **Output** (`/output`, `/address`, `/sat` `location.output`):
  `"spent_by_pending": Spend` when a pending transaction spends it; the key
  is absent otherwise.
  **Spend**: `{ "txid": "…", "vin": 1, "first_seen": …, "fee_rate": 10.0,
  "sale": true, "replaceable": true }`; `sale`: the spend is a pending sale
  of this output; `replaceable` is always `true` (bitcoind replaces by fee
  whether or not a transaction signals it).
* `GET /output/{outpoint}` of a tracked output a pending transaction
  creates (not in the index) is a **PendingOutput**: the Output shape with
  `"pending": true`, `"txid"`, `"first_seen"`, `"fee_rate"`,
  `"created_height": null` and `"created_time": null` (it may itself carry
  `spent_by_pending`, a chain).
* `GET /address/{address}` (and `/script/{hex}`): `"pending_outputs":
  [PendingOutput]`, present when the overlay runs; `outputs` and `rows`
  stay confirmed (their outputs carry `spent_by_pending`).
* `GET /address/{address}/history?pending=true` (and the script form):
  the address's PendingEntries first, newest first, on the first page only
  (no `before`) and only when `to` is not given; they follow `kinds` and
  `row`, do not count toward `limit` and do not move `next`. Without
  `pending=true` the answer is unchanged.
* `GET /sat/{n}`: `"pending"` when a pending transaction moves the sat
  (absent otherwise), its latest pending location (the end of a chain):
  ```json
  "pending": { "txid": "…", "first_seen": …, "fee_rate": …,
               "kind": "unspent" | "burned" | "fee",
               "satpoint": "txid:vout:offset" | null, "outpoint": "txid:vout" | null,
               "offset": 330 | null, "output": PendingOutput | null,
               "sales": [Sale] }
  ```
  `sales` are its pending sales; `location` and `sales` of the sat stay
  confirmed.
