# HTTP API

JSON over HTTP, read-only, served by `ord server` (default `127.0.0.1:8090`).
Every response is `application/json` except the streaming endpoints, which
are newline-delimited JSON (`application/x-ndjson`, one object per line).
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
`extracted` follows DESIGN.md §5 for the output as a whole (created on/after
`extracted_since` and worth ≤ `extracted_max_value`). Per-row extraction
(which also counts outputs made only of that row's sats) is reflected in the
supply numbers, not here.

**RowStats**
```json
{ "mined": 969238, "located": 969100, "burned": 3, "lost": 120, "overwritten": 2,
  "extracted": 51234, "holders": 400000, "extracted_holders": 20000 }
```

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
               "last_commit_seconds": 0.02, "last_commit_at": 1790734130, "commits": 12,
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

### `GET /rows`
Row definitions, in `RowId` order.
```json
[{ "id": "uncommon", "label": "Uncommon", "all_of": ["uncommon"], "any_of": [],
   "heights": null, "range": false, "total": 6930000 }]
```

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
