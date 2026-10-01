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
{ "offset": 0, "start": 45000000000, "len": 1, "types": ["uncommon", "alpha", "vintage", "block9", "450x", "nakamoto", "alpha_jan_feb_2009"] }
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

### `GET /supply`
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

### `GET /supply/{row}?top=100`
One row as in `/supply` plus holders:
```json
{ …row…,
  "top_extracted_holders": [{ "address": "bc1p…", "script_pubkey": "…", "n": 12, "utxos": 3,
                              "last_received_height": 930000, "last_received_time": 1766000000 }],
  "top_non_extracted_holders": [ … ] }
```
`top` ≤ 1000, default 100.

### `GET /supply/{row}/history?from=&to=&every=`
Row counters over time (only heights where the row changed are stored; each
bucket of `every` blocks reports the last value at or before its end).
```json
{ "row": "uncommon", "points": [{ "height": 144, "time": 1231731025, "stats": RowStats }] }
```
Defaults: `from=0`, `to=tip`, `every` chosen so ≤ 2,000 points are returned.

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

### `GET /blocks?from=&to=`
`[{ "height": …, "hash": "…", "time": … }]`, `to - from` ≤ 10,000.

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
  "marketplace": "magisat" | "magic_eden" | "satflow" | "ordinals_wallet" | "ord_net" | "gamma" | "ordswap" | "goldmine" | "tx_dev" | "unisat" | "unknown",
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
* `price`: the sale price as the venue states it. Magisat: its listing price
  — the seller's payment plus Magisat's maker fee (⌊payment / 199⌋ from
  height 861,287), excluding the buyer's fee; Magisat offers: the seller's
  payment plus half the fee output. Elsewhere: the seller's payment plus the
  marketplace fees attributable to the listing (a shared fee output is split
  over a transaction's sales in proportion to their payments).
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
