#!/usr/bin/env python3
"""Validates an index with the address history (docs/API.md "Address
history") against the retained moves, and against a reference fork without
it (python3, stdlib only).

    tools/validate/history-check.py --ours=http://127.0.0.1:8091 \
        [--reference=http://127.0.0.1:8090] [--sample=N] [--seed=S] \
        [--address=ADDR ...] [--concurrency=16] [--max-report=50]

Checks:
  1. moves vs history (ours): over the heights both cover, every
     (transaction, script) that a move's tracked inputs or spendable outputs
     name has exactly one history entry and every entry comes from a move;
     direction, sats_in/out, value_in/out, outputs_in, inputs_out,
     extracted_in/out and kind `mined` (coinbases) agree; every sale in
     /sales over the range gives its seller and buyer `sale` entries with
     their role. --sample checks N random scripts (default: all).
  2. with --reference, at equal heights: lost_sats, tracked UTXOs and
     pieces, every /supply row, and every /sales line are identical.
  3. --address (repeatable): prints each address's history summary (entries
     by kind, first and last heights) for a spot check.

Exit code: 0 all equal, 1 differences, 2 errors.
"""

import argparse
import collections
import concurrent.futures
import hashlib
import json
import random
import sys
import time
import urllib.error
import urllib.parse
import urllib.request


def get(base, path, retries=3):
    for attempt in range(retries):
        try:
            with urllib.request.urlopen(base + path, timeout=300) as response:
                return json.load(response)
        except urllib.error.HTTPError as err:
            body = err.read().decode(errors="replace")
            if attempt == retries - 1 or err.code < 500:
                raise RuntimeError(f"{path}: {err.code} {body}") from None
        except (urllib.error.URLError, TimeoutError):
            if attempt == retries - 1:
                raise
        time.sleep(1 + attempt)


def ndjson(base, path):
    with urllib.request.urlopen(base + path, timeout=600) as response:
        for line in response:
            if line.strip():
                value = json.loads(line)
                if isinstance(value, dict) and "error" in value and len(value) == 1:
                    raise RuntimeError(f"{path}: {value['error']}")
                yield value


class Report:
    def __init__(self, limit):
        self.limit = limit
        self.problems = 0

    def __call__(self, **problem):
        self.problems += 1
        if self.problems <= self.limit:
            print(json.dumps(problem), flush=True)


def expected_entries(height, move, expected):
    parties = {}

    def party(script):
        if script not in parties:
            parties[script] = {
                "txid": move["txid"], "coinbase": move["coinbase"],
                "sats_in": 0, "sats_out": 0, "value_in": 0, "value_out": 0,
                "outputs_in": 0, "inputs_out": 0,
                "extracted_in": False, "extracted_out": False,
            }
        return parties[script]

    for item in move["inputs"]:
        entry = party(item["script_pubkey"])
        entry["inputs_out"] += 1
        entry["value_out"] += item["value"]
        entry["sats_out"] += sum(piece["len"] for piece in item["pieces"])
        entry["extracted_out"] |= item["extracted"]
    for item in move["outputs"]:
        if item["kind"] != "spendable":
            continue
        entry = party(item["script_pubkey"])
        entry["outputs_in"] += 1
        entry["value_in"] += item["value"]
        entry["sats_in"] += sum(piece["len"] for piece in item["pieces"])
        entry["extracted_in"] |= item["extracted"]
    for script, entry in parties.items():
        if entry["outputs_in"] and entry["inputs_out"]:
            entry["direction"] = "self"
        else:
            entry["direction"] = "in" if entry["outputs_in"] else "out"
        expected.setdefault(script, {})[(height, move["tx_index"])] = entry


def script_history(base, script, low, high):
    found = {}
    duplicates = []
    before = ""
    while True:
        page = get(base, f"/script/{script}/history?from={low}&to={high}&limit=1000{before}")
        for entry in page["entries"]:
            key = (entry["height"], entry["tx_index"])
            if key in found:
                duplicates.append(key)
            found[key] = entry
        if not page["next"]:
            return found, duplicates
        before = "&before=" + urllib.parse.quote(page["next"])


FIELDS = ["direction", "sats_in", "sats_out", "value_in", "value_out",
          "outputs_in", "inputs_out", "extracted_in", "extracted_out", "txid"]


def check_moves(args, report):
    status = get(args.ours, "/status")
    if not status.get("address_history"):
        raise RuntimeError("ours has no address history")
    if status["address_history_building"]:
        raise RuntimeError("ours is still building the address history")
    history_from = status["address_history_from_height"]
    low = max(status["moves_from"], history_from)
    high = status["height"]
    print(f"moves vs history: heights {low}..={high} (moves from {status['moves_from']}, "
          f"history from {history_from})", file=sys.stderr)

    expected = {}
    moves = 0
    for start in range(low, high + 1, 10_001):
        end = min(high, start + 10_000)
        for line in ndjson(args.ours, f"/moves?from={start}&to={end}"):
            expected_entries(line["height"], line["move"], expected)
            moves += 1
    sales = list(ndjson(args.ours, f"/sales?from={low}&to={high}"))
    scripts = list(expected)
    total = sum(len(entries) for entries in expected.values())
    print(f"{moves} moves, {total} expected entries, {len(scripts)} scripts, "
          f"{len(sales)} sales", file=sys.stderr)
    if args.sample:
        random.Random(args.seed).shuffle(scripts)
        scripts = scripts[:args.sample]

    actual = {}
    errors = 0
    with concurrent.futures.ThreadPoolExecutor(args.concurrency) as pool:
        futures = {pool.submit(script_history, args.ours, script, low, high): script
                   for script in scripts}
        for done, future in enumerate(concurrent.futures.as_completed(futures), 1):
            script = futures[future]
            try:
                found, duplicates = future.result()
            except Exception as err:  # noqa: BLE001
                errors += 1
                print(f"{script}: {err}", file=sys.stderr)
                continue
            actual[script] = found
            for key in duplicates:
                report(script=script, key=key, problem="duplicate entry")
            wanted = expected[script]
            for key, want in wanted.items():
                got = found.get(key)
                if got is None:
                    report(script=script, key=key, txid=want["txid"], problem="missing entry")
                    continue
                diffs = {field: {"ours": got[field], "moves": want[field]}
                         for field in FIELDS if got[field] != want[field]}
                if (got["kind"] == "mined") != want["coinbase"]:
                    diffs["kind"] = {"ours": got["kind"], "coinbase": want["coinbase"]}
                if diffs:
                    report(script=script, key=key, problem="fields differ", diffs=diffs)
            for key in found:
                if key not in wanted:
                    report(script=script, key=key, problem="entry without a move")
            if done % 5000 == 0:
                print(f"  {done}/{len(scripts)} scripts", file=sys.stderr)

    sale_checks = 0
    for sale in sales:
        for role, side in (("seller", sale["seller"]), ("buyer", sale["buyer"])):
            if not side or side["script_pubkey"] not in actual:
                continue
            sale_checks += 1
            entry = actual[side["script_pubkey"]].get((sale["height"], sale["tx_index"]))
            if not entry or entry["kind"] != "sale" or role not in entry["roles"]:
                report(txid=sale["txid"], role=role, script=side["script_pubkey"],
                       problem="sale not in the history",
                       kind=entry and entry["kind"], roles=entry and entry["roles"])
    return {
        "from": low, "to": high, "moves": moves, "scripts_checked": len(scripts),
        "expected_entries_checked": sum(len(expected[s]) for s in scripts),
        "history_entries_read": sum(len(found) for found in actual.values()),
        "sale_roles_checked": sale_checks, "errors": errors,
    }


def same_height(args):
    for _ in range(120):
        ours, reference = get(args.ours, "/status"), get(args.reference, "/status")
        if ours["height"] == reference["height"] and ours["hash"] == reference["hash"]:
            return ours, reference
        time.sleep(2)
    raise RuntimeError("the two forks did not reach the same tip")


def check_reference(args, report):
    ours, reference = same_height(args)
    for field in ["lost_sats", "tracked_utxos", "tracked_pieces", "ranges",
                  "extracted_max_value", "extracted_since"]:
        if ours[field] != reference[field]:
            report(problem="status differs", field=field, ours=ours[field],
                   reference=reference[field])
    print(f"reference: both at {ours['height']}; fingerprints {ours['catalog_fingerprint'][:8]} "
          f"(ours) / {reference['catalog_fingerprint'][:8]}", file=sys.stderr)

    supply = [get(base, "/supply") for base in (args.ours, args.reference)]
    if supply[0]["height"] != supply[1]["height"]:
        report(problem="supply heights differ", ours=supply[0]["height"],
               reference=supply[1]["height"])
    rows = [{row["id"]: row for row in s["rows"]} for s in supply]
    for row_id in sorted(set(rows[0]) | set(rows[1])):
        if rows[0].get(row_id) != rows[1].get(row_id):
            report(problem="supply row differs", row=row_id)

    height = ours["height"]
    digests = []
    counts = []
    for base in (args.ours, args.reference):
        digest = hashlib.sha256()
        count = 0
        for sale in ndjson(base, f"/sales?from=0&to={height}"):
            digest.update(json.dumps(sale, sort_keys=True).encode())
            count += 1
        digests.append(digest.hexdigest())
        counts.append(count)
    if digests[0] != digests[1]:
        report(problem="sales differ", ours=counts[0], reference=counts[1])
    return {"height": height, "supply_rows": len(rows[0]), "sales": counts[0]}


def spot_check(args):
    for address in args.address:
        kinds = collections.Counter()
        heights = []
        before = ""
        while True:
            page = get(args.ours, f"/address/{address}/history?limit=1000{before}")
            for entry in page["entries"]:
                kinds[entry["kind"]] += 1
                heights.append(entry["height"])
            if not page["next"]:
                break
            before = "&before=" + urllib.parse.quote(page["next"])
        print(json.dumps({"address": address, "entries": len(heights), "kinds": kinds,
                          "newest": max(heights, default=None),
                          "oldest": min(heights, default=None)}))


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--ours", required=True)
    parser.add_argument("--reference")
    parser.add_argument("--sample", type=int)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--address", action="append", default=[])
    parser.add_argument("--concurrency", type=int, default=16)
    parser.add_argument("--max-report", type=int, default=50)
    args = parser.parse_args()
    args.ours = args.ours.rstrip("/")
    if args.reference:
        args.reference = args.reference.rstrip("/")

    report = Report(args.max_report)
    summary = {}
    try:
        summary["moves_vs_history"] = check_moves(args, report)
        if args.reference:
            summary["reference"] = check_reference(args, report)
        spot_check(args)
    except Exception as err:  # noqa: BLE001
        print(f"error: {err}", file=sys.stderr)
        summary["problems"] = report.problems
        print(json.dumps({"summary": summary}))
        sys.exit(2)
    summary["problems"] = report.problems
    print(json.dumps({"summary": summary}))
    errors = summary["moves_vs_history"]["errors"]
    sys.exit(2 if errors else 1 if report.problems else 0)


if __name__ == "__main__":
    main()
