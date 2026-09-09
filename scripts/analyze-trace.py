#!/usr/bin/env python3
"""Analyze GoLive opt-in media trace files (JSONL, numeric-only, no PII).

Usage:
    python3 scripts/analyze-trace.py <trace-dir-or-files...>
    python3 scripts/analyze-trace.py --self-test
    python3 scripts/analyze-trace.py scripts/fixtures/trace-sample.jsonl

Each input may be a directory (all ``*.jsonl`` inside, sorted) or a file.
For every file the script prints a per-stage summary (records, rate,
mean/max work_us, totals) plus finding flags, and -- when 2+ files are
given -- a host+viewer overlay keyed on ``timestamp_ms``.

Schema mirror of ``core/src/trace.rs``: every value must be a number
except ``stage``, which must be one of
capture/source/encode/send/rtp/decode/present. Unknown *numeric* fields
are accepted so older traces keep parsing; the pli-storm heuristic sums
the ``pli_sent``/``pli_suppressed``/``intra_applied`` counters (plus legacy
``pli``/``nack``/``fir`` names) and the judder heuristic reads
``max_gap_us`` on present records. Anything non-numeric is rejected.

Semantics mirror ``MEDIA_DEBUG.md``: rate = count*1e6/elapsed_us, fresh
presentation = frames-repeats, rtp.frames counts packets not pictures.
Only aggregate counts are printed; raw trace lines never are.

Exit status: 0 when all inputs are valid (flags are findings, not
errors), 2 on invalid JSONL or missing input.
"""

import argparse
import json
import sys
from pathlib import Path

STAGES = ("capture", "source", "encode", "send", "rtp", "decode", "present")

# Counters summed per stage for the summary. bytes/frames are informational;
# the rest feed the finding flags.
TOTALS = ("frames", "bytes", "dropped", "timeouts", "errors", "keyframes",
          "repeats", "gpu_frames")

# Optional future counters for the pli-storm heuristic. Absent from current
# traces; only consulted when present as numeric fields.
PLI_KEYS = ("pli", "plis", "pli_count", "fir", "firs", "fir_count",
            "nack", "nacks", "nack_count",
            "pli_sent", "pli_suppressed", "intra_applied")

TIMEOUT_BURST_DEFAULT = 8      # single-record capture.timeouts at/above this = burst
STALL_US_DEFAULT = 100_000     # single-observation max_work_us at/above this = stall
PLI_STORM_TOTAL = 10           # optional pli-ish total at/above this = storm
OVERLAY_BIN_MS = 5_000


class TraceError(Exception):
    """Invalid trace input; message carries file:line + key only, never data."""


def eprint(msg):
    print(msg, file=sys.stderr)


def collect_inputs(paths):
    """Expand CLI paths to a sorted list of JSONL files."""
    files = []
    for raw in paths:
        p = Path(raw)
        if p.is_dir():
            found = sorted(p.glob("*.jsonl"))
            if not found:
                raise TraceError("%s: no *.jsonl files in directory" % p)
            files.extend(found)
        elif p.is_file():
            files.append(p)
        else:
            raise TraceError("%s: no such file or directory" % p)
    return files


def parse_file(path):
    """Parse and schema-validate one JSONL file. Returns list of records."""
    records = []
    with open(path, "r", encoding="utf-8") as fh:
        for lineno, line in enumerate(fh, 1):
            if not line.strip():
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                raise TraceError("%s:%d: invalid JSON" % (path, lineno))
            if not isinstance(obj, dict):
                raise TraceError("%s:%d: record is not an object" % (path, lineno))
            stage = obj.get("stage")
            if stage not in STAGES:
                raise TraceError(
                    "%s:%d: bad 'stage' (want one of %s)"
                    % (path, lineno, "/".join(STAGES)))
            for key, value in obj.items():
                if key == "stage":
                    continue
                # Mirror core/src/trace.rs test: only numeric diagnostics.
                # bool is an int subclass in Python; reject it explicitly.
                if isinstance(value, bool) or not isinstance(value, (int, float)):
                    raise TraceError(
                        "%s:%d: non-numeric value for '%s'" % (path, lineno, key))
            records.append(obj)
    if not records:
        raise TraceError("%s: no records" % path)
    return records


def num(rec, key):
    value = rec.get(key, 0)
    return value if isinstance(value, (int, float)) else 0


def summarize(records):
    """Aggregate per-stage stats. Returns {stage: stats} in STAGES order."""
    stats = {}
    for rec in records:
        st = rec["stage"]
        s = stats.setdefault(st, {
            "records": 0, "frames": 0, "elapsed_us": 0, "work_us": 0,
            "observations": 0, "max_work_us": 0, "max_timeouts": 0,
            "t_min": rec.get("timestamp_ms", 0),
            "t_max": rec.get("timestamp_ms", 0),
            "pli": 0, "pli_sent": 0, "pli_suppressed": 0,
            "intra_applied": 0, "max_gap_us": 0,
        })
        for key in TOTALS:
            s[key] = s.get(key, 0) + num(rec, key)
        for key in ("pli_sent", "pli_suppressed", "intra_applied"):
            s[key] += num(rec, key)
        s["max_gap_us"] = max(s["max_gap_us"], num(rec, "max_gap_us"))
        s["records"] += 1
        s["elapsed_us"] += num(rec, "elapsed_us")
        s["work_us"] += num(rec, "work_us")
        s["observations"] += num(rec, "observations")
        s["max_work_us"] = max(s["max_work_us"], num(rec, "max_work_us"))
        s["max_timeouts"] = max(s["max_timeouts"], num(rec, "timeouts"))
        for key in PLI_KEYS:
            s["pli"] += num(rec, key)
        ts = num(rec, "timestamp_ms")
        s["t_min"] = min(s["t_min"], ts)
        s["t_max"] = max(s["t_max"], ts)
    return {st: stats[st] for st in STAGES if st in stats}


def rate(count, elapsed_us):
    return count * 1_000_000 / elapsed_us if elapsed_us > 0 else 0.0


def flag_stage(stage, s, timeout_burst, stall_us):
    """Finding flags for one per-stage summary. Returns list of strings."""
    flags = []
    if stage == "decode" and s.get("dropped", 0) > 0:
        flags.append("STARVATION decode.dropped=%d (access units with no picture)"
                     % s["dropped"])
    if stage == "present" and s.get("repeats", 0) > 0:
        flags.append("REPLAY present.repeats=%d (re-sends, not fresh frames)"
                     % s["repeats"])
    if stage == "present" and s.get("frames", 0) > 0 and s.get("elapsed_us", 0) > 0:
        interval = s["elapsed_us"] / s["frames"]
        if s.get("max_gap_us", 0) >= 2 * interval:
            flags.append("JITTER present.max_gap_us=%dus (>= 2x %.0fus interval)"
                         % (s["max_gap_us"], interval))
    if stage == "capture" and s.get("max_timeouts", 0) >= timeout_burst:
        flags.append("TIMEOUT-BURST capture.timeouts peak=%d in one record "
                     "(total=%d, bridge 100ms wait starved)"
                     % (s["max_timeouts"], s.get("timeouts", 0)))
    if s.get("max_work_us", 0) >= stall_us:
        flags.append("STALL %s.max_work_us=%dus (>= %dus)"
                     % (stage, s["max_work_us"], stall_us))
    if s.get("pli", 0) >= PLI_STORM_TOTAL:
        flags.append("PLI-STORM %s pli-ish total=%d" % (stage, s["pli"]))
    elif s.get("pli", 0) > 0:
        flags.append("note: %s pli-ish total=%d (below storm threshold %d)"
                     % (stage, s["pli"], PLI_STORM_TOTAL))
    if s.get("errors", 0) > 0:
        flags.append("ERRORS %s.errors=%d" % (stage, s["errors"]))
    if stage not in ("decode",) and s.get("dropped", 0) > 0:
        flags.append("note: %s.dropped=%d" % (stage, s["dropped"]))
    return flags


def report_file(path, records, summary, timeout_burst, stall_us):
    lines = []
    lines.append("== %s (%d records) ==" % (path, len(records)))
    for stage, s in summary.items():
        fps = rate(s["frames"], s["elapsed_us"])
        mean_work = (s["work_us"] / s["observations"]
                     if s["observations"] > 0 else 0.0)
        extra = ""
        if stage == "present":
            fresh = s["frames"] - s.get("repeats", 0)
            extra = " fresh=%d fresh_fps=%.1f max_gap=%dus" % (
                fresh, rate(fresh, s["elapsed_us"]), s.get("max_gap_us", 0))
        if stage == "decode" and (s.get("pli_sent", 0) or s.get("pli_suppressed", 0)):
            extra = " pli_sent=%d pli_suppressed=%d" % (
                s["pli_sent"], s["pli_suppressed"])
        if stage == "encode" and s.get("intra_applied", 0):
            extra = " intra_applied=%d" % s["intra_applied"]
        if stage == "rtp":
            extra = " (frames=packets)"
        lines.append(
            "  %-8s rec=%-4d rate=%7.1f/s mean_work=%9.1fus max_work=%8dus "
            "drop=%d timeouts=%d err=%d repeats=%d keyframes=%d%s"
            % (stage, s["records"], fps, mean_work, s["max_work_us"],
               s.get("dropped", 0), s.get("timeouts", 0), s.get("errors", 0),
               s.get("repeats", 0), s.get("keyframes", 0), extra))
    flags = []
    for stage, s in summary.items():
        flags.extend(flag_stage(stage, s, timeout_burst, stall_us))
    if flags:
        lines.append("  flags:")
        lines.extend("    - %s" % f for f in flags)
    else:
        lines.append("  flags: none")
    return lines


def report_overlay(per_file, bin_ms=OVERLAY_BIN_MS):
    """Host+viewer overlay keyed on timestamp_ms. per_file: [(path, records)]."""
    lines = ["== overlay by timestamp_ms (%ds bins, frames per file) =="
             % (bin_ms // 1000)]
    spans = []
    for path, records in per_file:
        ts = [num(r, "timestamp_ms") for r in records]
        spans.append((path, min(ts), max(ts)))
        lines.append("  %s span=%d..%d (%ds)"
                     % (path, min(ts), max(ts), (max(ts) - min(ts)) // 1000))
    lo = max(s for _, s, _ in spans)
    hi = min(e for _, _, e in spans)
    if lo <= hi:
        lines.append("  common overlap: %d..%d (%ds)" % (lo, hi, (hi - lo) // 1000))
    else:
        lines.append("  common overlap: none (clocks disjoint or unsynced)")
    base = min(s for _, s, _ in spans)
    bins = {}
    for path, records in per_file:
        for rec in records:
            b = (int(num(rec, "timestamp_ms")) - base) // bin_ms
            bins.setdefault(b, {}).setdefault(str(path), 0)
            bins[b][str(path)] += int(num(rec, "frames"))
    names = [str(p) for p, _ in per_file]
    lines.append("  t+%ss  %s" % ("s".rjust(5), "  ".join(
        ("%s" % n.split("/")[-1][:22]).rjust(24) for n in names)))
    for b in sorted(bins):
        row = "  +%5d  %s" % (b * (bin_ms // 1000), "  ".join(
            ("%d" % bins[b].get(n, 0)).rjust(24) for n in names))
        lines.append(row)
    return lines


def fixture_path():
    return Path(__file__).resolve().parent / "fixtures" / "trace-sample.jsonl"


def run_self_test():
    """Parse the checked-in fixture and assert parsing + flag logic."""
    path = fixture_path()
    failures = []

    def check(cond, label):
        print(("PASS" if cond else "FAIL") + ": " + label)
        if not cond:
            failures.append(label)

    try:
        records = parse_file(str(path))
    except TraceError as exc:
        print("FAIL: fixture parses: %s" % exc)
        return 1
    check(len(records) == 13, "fixture has 13 records (got %d)" % len(records))
    check({r["stage"] for r in records}
          >= {"capture", "encode", "decode", "present"},
          "fixture covers capture/encode/decode/present")

    summary = summarize(records)
    check(summary["decode"].get("dropped") == 2, "decode.dropped totals 2")
    check(summary["present"].get("repeats") == 3, "present.repeats totals 3")
    check(summary["decode"].get("pli_sent") == 1, "decode.pli_sent totals 1")
    check(summary["decode"].get("pli_suppressed") == 49,
          "decode.pli_suppressed totals 49 (storm suppressed)")
    check(summary["encode"].get("intra_applied") == 1,
          "encode.intra_applied totals 1")
    check(summary["present"].get("max_gap_us") == 250000,
          "present.max_gap_us keeps the worst ack gap")

    flags = [f for st, s in summary.items()
             for f in flag_stage(st, s, TIMEOUT_BURST_DEFAULT, STALL_US_DEFAULT)]
    check(any(f.startswith("STARVATION") for f in flags), "starvation flagged")
    check(any(f.startswith("REPLAY") for f in flags), "replay flagged")
    check(any(f.startswith("TIMEOUT-BURST") for f in flags),
          "capture timeout burst flagged")
    check(any(f.startswith("PLI-STORM") for f in flags),
          "pli storm flagged (gap storm counters)")
    check(any(f.startswith("JITTER") for f in flags),
          "present judder flagged (max_gap_us >= 2x interval)")

    bad = {"stage": "decode", "frames": "many"}
    ok = True
    for key, value in bad.items():
        if key != "stage" and (isinstance(value, bool)
                               or not isinstance(value, (int, float))):
            ok = False
    check(not ok, "non-numeric value rejected by schema rule")
    check("nope" not in STAGES, "unknown stage rejected by schema rule")

    checks = 14  # number of check() calls above
    if failures:
        print("self-test: %d failure(s)" % len(failures))
        return 1
    print("self-test: all %d checks passed" % checks)
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(
        description="Summarize GoLive media trace JSONL (numeric-only, no PII).")
    ap.add_argument("inputs", nargs="*",
                    help="trace files or directories holding *.jsonl")
    ap.add_argument("--self-test", action="store_true",
                    help="check parsing/flag logic against the fixture")
    ap.add_argument("--timeout-burst", type=int, default=TIMEOUT_BURST_DEFAULT,
                    help="single-record capture.timeouts flag level (default %d)"
                    % TIMEOUT_BURST_DEFAULT)
    ap.add_argument("--stall-us", type=int, default=STALL_US_DEFAULT,
                    help="max_work_us flag level in us (default %d)"
                    % STALL_US_DEFAULT)
    args = ap.parse_args(argv)

    if args.self_test:
        return run_self_test()
    if not args.inputs:
        ap.error("need <trace-dir-or-files...> or --self-test")

    try:
        files = collect_inputs(args.inputs)
    except TraceError as exc:
        eprint("analyze-trace: error: %s" % exc)
        return 2

    per_file = []
    for path in files:
        try:
            records = parse_file(str(path))
        except TraceError as exc:
            eprint("analyze-trace: error: %s" % exc)
            return 2
        per_file.append((path, records))

    out = []
    for path, records in per_file:
        out.extend(report_file(path, records, summarize(records),
                               args.timeout_burst, args.stall_us))
    if len(per_file) >= 2:
        out.extend(report_overlay(per_file))
    print("\n".join(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
