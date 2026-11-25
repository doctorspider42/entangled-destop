# SPDX-License-Identifier: Apache-2.0
"""Join the animation lab's host actions, WinEvents, scanouts and window pixels."""

import argparse
from bisect import bisect_left, bisect_right
from collections import defaultdict
import csv
from datetime import datetime
from pathlib import Path
import re
from statistics import median


def rows(path):
    with path.open(encoding="utf-8-sig", newline="") as stream:
        yield from csv.DictReader(stream)


def clock_anchor(path):
    samples = []
    for row in rows(path):
        frequency = int(row["guest_frequency"])
        guest_send = int(row["guest_send_qpc"]) / frequency
        guest_receive = int(row["guest_receive_qpc"]) / frequency
        host_receive = int(row["host_receive_ns"]) / 1e9
        host_send = int(row["host_send_ns"]) / 1e9
        guest = (guest_send + guest_receive) / 2
        host = (host_receive + host_send) / 2
        samples.append((guest_receive - guest_send - (host_send - host_receive), guest, host - guest, frequency))
    if not samples:
        raise ValueError(f"no clock samples in {path}")
    return min(samples)


def first_in(series, start, end):
    index = bisect_left(series, start)
    if index < len(series) and series[index] <= end:
        return series[index]
    return None


def milliseconds(start, end):
    return round((end - start) * 1e3, 1) if start is not None and end is not None else None


def summarize(values):
    available = [v for v in values if v is not None]
    return f"{median(available):.1f}/{max(available):.1f}" if available else "-"


def scene_intervals(path, pid, guest_time):
    intervals = defaultdict(list)
    record = re.compile(rf"\[{pid}\].*scene-qpc,(\d+),(\d+),(.+)$")
    with path.open(encoding="utf-8", errors="replace") as stream:
        for line in stream:
            if match := record.search(line.strip()):
                begin, end, name = match.groups()
                intervals[name].append((guest_time(begin), guest_time(end)))
    for spans in intervals.values():
        spans.sort()
    return intervals


def lamp_stages(intervals, key, win_event, pixel):
    begins = [span for span in intervals["efekt lamp: enter"] if key - 0.004 <= span[0] <= key + 0.45]
    if not begins:
        return (None,) * 6
    # A focus event may run a separate, short lamp entry to arm capture.
    entered, attached = max(begins, key=lambda span: span[1] - span[0])
    commit_begin, commit_end = next(
        ((a, b) for a, b in intervals["Commit"] if attached <= a <= key + 0.65), (None, None))
    first_frame = next((a for a, _ in intervals["efekt lamp: frame"] if attached <= a <= key + 0.60), None)
    z_order = [milliseconds(a, b) for a, b in intervals["węzeł: przypisanie viewportu"]
               if key - 0.005 <= a <= b <= entered]
    return (milliseconds(win_event, entered), sum(z_order) if z_order else None, milliseconds(entered, attached),
            milliseconds(commit_begin, commit_end), milliseconds(commit_end, first_frame), milliseconds(commit_end, pixel))


def main():
    cli = argparse.ArgumentParser(description=__doc__)
    cli.add_argument("tag")
    cli.add_argument("--host", type=Path, required=True, help="directory containing TAG-anchor.csv, TAG-actions.csv and TAG-frames.csv")
    cli.add_argument("--guest", type=Path, required=True, help="directory containing TAG-pre.csv, TAG-post.csv and TAG-keys.csv")
    cli.add_argument("--log", type=Path, required=True, help="host engine log covering the measurement")
    cli.add_argument("--threshold", type=int, default=150, help="minimum changed pixels per captured frame")
    cli.add_argument("--scene-log", type=Path, help="optional Wintanyl shell.log with WT_BENCH_SCENE_QPC=1")
    cli.add_argument("--scene-pid", type=int, help="Wintanyl process ID for the scene trace")
    args = cli.parse_args()
    if args.threshold < 1:
        cli.error("--threshold must be positive")
    if (args.scene_log is None) != (args.scene_pid is None):
        cli.error("--scene-log and --scene-pid are used together")

    host_prefix = args.host / args.tag
    guest_prefix = args.guest / args.tag
    pre, post = (clock_anchor(Path(f"{guest_prefix}-{phase}.csv")) for phase in ("pre", "post"))
    if pre[3] != post[3] or pre[1] >= post[1]:
        cli.error("clock samples must bracket the same guest boot and frequency")

    _, before, after, utc = map(int, Path(f"{host_prefix}-anchor.csv").read_text().strip().split(","))
    host_wall_offset = (utc - (before + after) / 2) / 1e9
    actions = [(int(r["qpc_100ns"]) / 1e7, r["action"]) for r in rows(Path(f"{host_prefix}-actions.csv"))]
    if not actions:
        cli.error("no host input markers")

    def guest_time(qpc):
        guest = int(qpc) / pre[3]
        mix = (guest - pre[1]) / (post[1] - pre[1])
        return guest + pre[2] + mix * (post[2] - pre[2])

    events = defaultdict(list)
    for row in rows(Path(f"{guest_prefix}-keys.csv")):
        events[row["event"]].append(guest_time(row["qpc"]))
    for times in events.values():
        times.sort()

    pixels = sorted(int(r["qpc_wgc"]) / 1e7 for r in rows(Path(f"{host_prefix}-frames.csv")) if int(r["changed"]) >= args.threshold)
    scanouts = []
    with args.log.open(encoding="utf-8", errors="replace") as stream:
        for line in stream:
            if "virtio-gpu flush resource=" in line and "width=1920 height=1080 three_d=true" in line:
                utc = datetime.fromisoformat(line.split()[0]).timestamp()
                host = utc - host_wall_offset
                if actions[0][0] - 1 <= host <= actions[-1][0] + 2:
                    scanouts.append(host)
    scanouts.sort()

    stats = defaultdict(list)
    trace = defaultdict(list)
    intervals = scene_intervals(args.scene_log, args.scene_pid, guest_time) if args.scene_log else None
    if args.scene_log and not intervals:
        cli.error("no scene timing rows for that PID")
    missing = 0
    print(f"Clock bridge minimum round trips {pre[0] * 1e3:.3f}/{post[0] * 1e3:.3f} ms, pixel threshold {args.threshold}")
    print("action host-key key-WinEvent WinEvent-flush key-flush flush-pixels host-pixels changes")
    for send, action in actions:
        key = first_in(events[f"key_{action}"], send - 0.003, send + 0.350)
        if key is None:
            print(action, "missing key")
            missing += 1
            continue
        win_event = first_in(events[f"window_{action}"], key - 0.004, key + 0.9)
        pixel = first_in(pixels, key - 0.003, key + 1.25)
        scanout = None
        if pixel is not None:
            # The last preceding full-screen flush is a timing proxy. The
            # window capture and virtio-gpu trace do not carry a shared frame ID.
            index = bisect_right(scanouts, pixel)
            if index and max(key - 0.002, pixel - 0.13) <= scanouts[index - 1]:
                scanout = scanouts[index - 1]
        changes = bisect_right(pixels, key + 1.20) - bisect_left(pixels, key - 0.003)
        result = (
            milliseconds(send, key), milliseconds(key, win_event), milliseconds(win_event, scanout),
            milliseconds(key, scanout), milliseconds(scanout, pixel), milliseconds(send, pixel), changes,
        )
        print(action, *result)
        stats[action.split("_")[0]].append(result)
        if intervals and action in ("minimize", "restore"):
            trace[action].append(lamp_stages(intervals, key, win_event, pixel))
    print("Median / max by gesture")
    for action, group in stats.items():
        print(action, len(group), *(summarize(column) for column in zip(*group)))
    if trace:
        print("Scene med/max: WinEvent-enter Z-placement enter Commit return-first-frame return-pixels")
        for action, group in trace.items():
            print(action, len(group), *(summarize(column) for column in zip(*group)))
    return 1 if missing else 0


if __name__ == "__main__":
    raise SystemExit(main())
