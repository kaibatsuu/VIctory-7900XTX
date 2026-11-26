"""Correlate private bridge requests, admission evictions and destination commits."""
from collections import Counter
import csv
from dataclasses import dataclass
import io
import json
import re


# Twice the normal producer budget. Bound retained events even on a custom or unfinished capture.
MAX_ROWS = 131072
EVENTS = {"gpu_submit", "reuse", "eviction", "verified_delivery", "delivery_failed"}
HEX_ID = re.compile(r"[0-9a-f]{16}")
METRICS = ("gpu_submits", "gpu_resubmits", "first_observed_verified_gpu_load", "verified_reload_after_eviction",
           "verified_repeat_without_observed_eviction", "reuse_attempts", "verified_cache_reuse",
           "evictions", "gpu_delivery_failed", "reuse_delivery_failed", "gpu_unfinished", "reuse_unfinished",
           "unpaired_terminal")


@dataclass(slots=True)
class Event:
    seq: int
    time: int
    request: int
    event: str
    block: int


def csv_text(headers, rows):
    stream = io.StringIO(newline="")
    writer = csv.writer(stream)
    writer.writerow(headers)
    writer.writerows(rows)
    return stream.getvalue()


def private_names(path):
    names = {}
    if path.is_file():
        with path.open(encoding="ascii", errors="replace", newline="") as stream:
            for i, row in enumerate(csv.DictReader(stream)):
                if i >= MAX_ROWS:
                    break
                value = row.get("name_hex", "")
                if HEX_ID.fullmatch(row.get("id", "")) and value and len(value) <= 8192:
                    try:
                        names[row["id"]] = bytes.fromhex(value).decode("utf8", errors="replace")
                    except ValueError:
                        continue
    return names


def analyze(path, outcome):
    """Return shareable tables, private tables and human-readable observations separately."""
    aliases = {}
    origins = {}
    blocks = []
    events = []
    problems = Counter()
    expected_seq = 1
    with path.open(encoding="ascii", errors="replace", newline="") as stream:
        reader = csv.DictReader(stream)
        required = {"seq", "utc_us", "request", "event", "asset_id", "source_id", "source_kind", "name_id",
                    "file_offset", "pack_offset", "decoded_bytes", "stored_bytes"}
        if not required.issubset(reader.fieldnames or ()):
            return {}, {}, ["Block reload analysis unavailable because the capture lacks request or block identities."]
        try:
            for i, row in enumerate(reader):
                if i == MAX_ROWS:
                    problems["analysis row limit"] += 1
                    break
                try:
                    seq, stamp = int(row["seq"]), int(row["utc_us"])
                    if seq < expected_seq or stamp < 1:
                        raise ValueError
                    if seq > expected_seq:
                        problems["missing sequence numbers"] += seq - expected_seq
                    expected_seq = seq + 1
                    event = row["event"]
                    if event == "limit":
                        problems["producer cap"] += 1
                        break
                    asset = row["asset_id"]
                    pack, request, decoded = int(row["pack_offset"]), int(row["request"]), int(row["decoded_bytes"])
                    if (event not in EVENTS or not HEX_ID.fullmatch(asset or "") or pack < 0 or request < 1 or decoded < 1):
                        raise ValueError
                except (ValueError, TypeError):
                    problems["invalid records"] += 1
                    continue
                # The marker supplies only a digest prefix. Distinguish indexed records by pack offset too.
                key = (asset, pack)
                if key not in aliases:
                    aliases[key] = len(blocks)
                    blocks.append({"key": key, "first": stamp, "last": stamp, "bytes": decoded,
                                   "origin": (row["source_id"], row["name_id"], row["file_offset"]),
                                   "counts": Counter(), "eviction": None, "admission": 0,
                                   "reload_wait": "", "last_reload": ""})
                block = aliases[key]
                blocks[block]["last"] = stamp
                origin = (block, row["source_kind"], row["source_id"], row["name_id"], row["file_offset"], row["stored_bytes"])
                origins.setdefault(origin, stamp)
                events.append(Event(seq, stamp, request, event, block))
        except csv.Error:
            problems["unreadable CSV tail"] += 1

    # A destination callback may arrive after a new request evicts its admission.
    # Pair first, then replay submits/evictions in producer sequence order.
    starts = {}
    terminals = {}
    for e in events:
        if e.event == "eviction":
            continue
        key = (e.request, e.block)
        target = starts if e.event in ("gpu_submit", "reuse") else terminals
        if key in target:
            problems["duplicate request events"] += 1
            target[key] = None
        else:
            target[key] = e

    timeline = []
    for e in events:
        b = blocks[e.block]
        c = b["counts"]
        key = (e.request, e.block)
        commit = eviction_time = delay = ""
        if e.event == "eviction":
            c["evictions"] += 1
            b["eviction"] = e
            label = "eviction"
        elif e.event in ("verified_delivery", "delivery_failed"):
            if starts.get(key) and starts[key].seq < e.seq:
                continue
            label = "unpaired_" + e.event
            c["unpaired_terminal"] += 1
        else:
            if starts.get(key) is not e:
                continue
            gpu = e.event == "gpu_submit"
            if gpu:
                c["gpu_resubmits"] += int(c["gpu_submits"] > 0)
                c["gpu_submits"] += 1
            else:
                c["reuse_attempts"] += 1
            terminal = terminals.get(key)
            if terminal is None or terminal.seq <= e.seq:
                label = "gpu_unfinished" if gpu else "reuse_unfinished"
            else:
                commit = terminal.time
                if terminal.event == "delivery_failed":
                    label = "gpu_delivery_failed" if gpu else "reuse_delivery_failed"
                elif not gpu:
                    label = "verified_cache_reuse"
                else:
                    evicted = b["eviction"]
                    if not b["admission"]:
                        label = "first_observed_verified_gpu_load"
                    elif evicted and evicted.seq > b["admission"]:
                        label = "verified_reload_after_eviction"
                        eviction_time = evicted.time
                        if e.time >= evicted.time:
                            delay = e.time - evicted.time
                        else:
                            problems["UTC clock reversals"] += 1
                        b["reload_wait"], b["last_reload"] = delay, e.time
                    else:
                        label = "verified_repeat_without_observed_eviction"
                    b["admission"] = e.seq
            c[label] += 1
        timeline.append((e.block + 1, e.time, label, commit, eviction_time, delay))

    counters = Counter()
    for b in blocks:
        counters.update(b["counts"])
    rank = sorted(range(len(blocks)), key=lambda i: (-blocks[i]["counts"]["verified_reload_after_eviction"],
                  -blocks[i]["counts"]["gpu_resubmits"], i))
    cols = ("block_alias", "first_utc_us", "last_utc_us", "decoded_bytes", *METRICS,
            "last_eviction_utc_us", "last_reload_submit_utc_us", "last_reload_us_since_observed_eviction")
    rows = [(i + 1, blocks[i]["first"], blocks[i]["last"], blocks[i]["bytes"],
             *(blocks[i]["counts"][k] for k in METRICS), blocks[i]["eviction"].time if blocks[i]["eviction"] else "",
             blocks[i]["last_reload"], blocks[i]["reload_wait"]) for i in rank]
    public = {"gdeflate-blocks.csv": csv_text(cols, rows),
              "gdeflate-block-events.csv": csv_text(("block_alias", "event_utc_us", "classification", "delivery_utc_us",
                "observed_eviction_utc_us", "submit_us_since_observed_eviction"), timeline)}
    names = private_names(path.with_name("gdeflate-names.csv"))
    private = {"gdeflate-blocks-private.csv": csv_text(("asset_id_prefix", "pack_offset", "first_source_id",
               "first_name_id", "first_file_offset", "first_source_path", *cols),
               ((*blocks[i]["key"], *blocks[i]["origin"], names.get(blocks[i]["origin"][0], ""), *row) for i, row in zip(rank, rows))),
               "gdeflate-block-origins-private.csv": csv_text(("block_alias", "asset_id_prefix", "pack_offset",
                "source_kind", "source_id", "source_path", "name_id", "request_name", "file_offset", "stored_bytes", "first_utc_us"),
                ((i + 1, *blocks[i]["key"], kind, source, names.get(source, ""), name, names.get(name, ""), offset, stored, stamp)
                 for (i, kind, source, name, offset, stored), stamp in origins.items()))}
    reloads = counters["verified_reload_after_eviction"]
    affected = sum(b["counts"]["verified_reload_after_eviction"] > 0 for b in blocks)
    public["gdeflate-coverage.json"] = json.dumps({
        "launcher_status": outcome, "analyzed_events": len(events), "distinct_blocks": len(blocks),
        "blocks_reloaded_after_eviction": affected, "observed_counts": {k:counters[k] for k in METRICS},
        "coverage_issues": dict(problems), "producer_capped": bool(problems["producer cap"]),
        "analysis_capped": bool(problems["analysis row limit"]),
        "unmatched_requests": counters["gpu_unfinished"] + counters["reuse_unfinished"],
    }, indent=2)
    notes = [f"Observed {len(blocks):,} indexed blocks, {counters['gpu_submits']:,} GPU submits, "
             f"{counters['gpu_resubmits']:,} resubmits, {reloads:,} verified reloads after eviction across {affected:,} blocks, "
             f"and {counters['verified_cache_reuse']:,} verified cache reuses.",
             f"Matched destination failures: {counters['gpu_delivery_failed'] + counters['reuse_delivery_failed']:,}. "
             f"Requests without a matched terminal event: {counters['gpu_unfinished'] + counters['reuse_unfinished']:,}. "
             f"Unpaired terminal events: {counters['unpaired_terminal']:,}.",
             "Block aliases apply within this capture. Reload delays run from the latest observed eviction to the GPU resubmit. "
             "UTC timestamps locate storage and destination-delivery events. Draw-level use requires separate evidence."]
    if reloads:
        top = rank[0]
        notes.append(f"Block alias {top + 1} had the most verified reloads after eviction ({blocks[top]['counts']['verified_reload_after_eviction']:,}).")
    if outcome != "completed":
        notes.append("Capture ended before a confirmed normal completion. Block counts cover the recorded prefix.")
    if problems:
        notes.append("Incomplete marker coverage: " + ", ".join(f"{k} ({n})" for k,n in sorted(problems.items())) + ".")
    return public, private, notes
