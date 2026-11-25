# SPDX-License-Identifier: Apache-2.0
"""Join animation input, guest composition, scanouts and host window pixels."""

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


def dwm_frames(path, pid, guest_time):
    # Dwm-Core draw (59/60) and Present (15/16) scopes on the same render
    # thread. Consume the draw once so an unrelated later Present cannot use it.
    threads = defaultdict(list)
    for row in rows(path):
        event = int(row["event"])
        if row["provider"] == "dwm" and int(row["pid"]) == pid and event in (59, 60, 15, 16):
            threads[int(row["tid"])].append((int(row["qpc"]), event))
    frames = []
    for events in threads.values():
        draw_start = draw = present_start = present_draw = None
        for stamp, event in sorted(events, key=lambda row: row[0]):
            when = guest_time(stamp)
            if event == 59:
                draw_start = when
                draw = None
            elif event == 60 and draw_start is not None:
                draw = (draw_start, when)
                draw_start = None
            elif event == 15:
                present_start = when
                present_draw = draw
                draw = None
            elif event == 16 and present_start is not None:
                frames.append((present_start, when, present_draw))
                present_start = present_draw = None
    return frames


def dwm_stages(frames, send, win_event, scanout):
    if scanout is None:
        return (None,) * 5
    # Prefer containment to a clock-alignment tolerance at adjacent scopes.
    candidates = [frame for frame in frames if frame[0] <= scanout <= frame[1]]
    if not candidates:
        candidates = [frame for frame in frames if frame[0] - 0.004 <= scanout <= frame[1] + 0.004]
    if len(candidates) != 1:
        return (None,) * 5
    present, _, draw = candidates[0]
    if draw is None:
        return (None, None, None, None, milliseconds(present, scanout))
    start, end = draw
    return (milliseconds(send, start), milliseconds(win_event, start), milliseconds(start, end),
            milliseconds(end, present), milliseconds(present, scanout))


def main():
    cli = argparse.ArgumentParser(description=__doc__)
    cli.add_argument("tag")
    cli.add_argument("--host", type=Path, required=True, help="directory containing TAG-anchor.csv, TAG-actions.csv and TAG-frames.csv")
    cli.add_argument("--guest", type=Path, required=True, help="directory containing TAG-pre.csv, TAG-post.csv and TAG-keys.csv")
    cli.add_argument("--log", type=Path, required=True, help="host engine log covering the measurement")
    cli.add_argument("--threshold", type=int, default=150, help="minimum changed pixels per captured frame")
    cli.add_argument("--scene-log", type=Path, help="optional Wintanyl shell.log with WT_BENCH_SCENE_QPC=1")
    cli.add_argument("--scene-pid", type=int, help="Wintanyl process ID for the scene trace")
    cli.add_argument("--dwm-log", type=Path, help="optional CSV from etw-display-qpc.exe")
    cli.add_argument("--dwm-pid", type=int, help="guest DWM process ID for the ETW trace")
    cli.add_argument("--host-input-only", action="store_true", help="use host action timestamps when the guest input probe was absent")
    args = cli.parse_args()
    if args.threshold < 1:
        cli.error("--threshold must be positive")
    if (args.scene_log is None) != (args.scene_pid is None):
        cli.error("--scene-log and --scene-pid are used together")
    if (args.dwm_log is None) != (args.dwm_pid is None):
        cli.error("--dwm-log and --dwm-pid are used together")

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
    if not args.host_input_only:
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
    dwm_trace = defaultdict(list)
    dwm_longest = defaultdict(list)
    intervals = scene_intervals(args.scene_log, args.scene_pid, guest_time) if args.scene_log else None
    if args.scene_log and not intervals:
        cli.error("no scene timing rows for that PID")
    composition = dwm_frames(args.dwm_log, args.dwm_pid, guest_time) if args.dwm_log else None
    if args.dwm_log and not composition:
        cli.error("no paired DWM Present events for that PID")
    missing = 0
    print(f"Clock bridge minimum round trips {pre[0] * 1e3:.3f}/{post[0] * 1e3:.3f} ms, pixel threshold {args.threshold}")
    if args.host_input_only:
        print("Using host action timestamps for matching")
    print("action host-key key-WinEvent WinEvent-flush key-flush flush-pixels host-pixels changes")
    for send, action in actions:
        key = first_in(events[f"key_{action}"], send - 0.003, send + 0.350)
        if key is None and not args.host_input_only:
            print(action, "missing key")
            missing += 1
            continue
        trigger = send if key is None else key
        win_event = first_in(events[f"window_{action}"], trigger - 0.004, trigger + 0.9)
        pixel = first_in(pixels, trigger - 0.003, trigger + 1.25)
        scanout = None
        if pixel is not None:
            # The last preceding full-screen flush is a timing proxy. The
            # window capture and virtio-gpu trace do not carry a shared frame ID.
            index = bisect_right(scanouts, pixel)
            if index and max(trigger - 0.002, pixel - 0.13) <= scanouts[index - 1]:
                scanout = scanouts[index - 1]
        changes = bisect_right(pixels, trigger + 1.20) - bisect_left(pixels, trigger - 0.003)
        result = (
            milliseconds(send, key), milliseconds(key, win_event), milliseconds(win_event, scanout),
            milliseconds(key, scanout), milliseconds(scanout, pixel), milliseconds(send, pixel), changes,
        )
        print(action, *result)
        stats[action.split("_")[0]].append(result)
        if intervals and action in ("minimize", "restore"):
            trace[action].append(lamp_stages(intervals, trigger, win_event, pixel))
        if composition:
            dwm_trace[action].append(dwm_stages(composition, send, win_event, scanout))
            if pixel is not None:
                # An earlier Present can block even when its frame produces
                # no changed pixels in the capture region.
                longest = max((end - begin for begin, end, _ in composition
                               if trigger <= begin <= end <= pixel), default=None)
                dwm_longest[action].append(None if longest is None else round(longest * 1e3, 1))
    print("Median / max by gesture")
    for action, group in stats.items():
        print(action, len(group), *(summarize(column) for column in zip(*group)))
    if trace:
        print("Scene med/max: WinEvent-enter Z-placement enter Commit return-first-frame return-pixels")
        for action, group in trace.items():
            print(action, len(group), *(summarize(column) for column in zip(*group)))
    if dwm_trace:
        print("DWM med/max: host-draw WinEvent-draw draw draw-Present Present-flush")
        for action, group in dwm_trace.items():
            matched = sum(stages[-1] is not None for stages in group)
            print(action, f"{matched}/{len(group)}", *(summarize(column) for column in zip(*group)))
        print("DWM med/max: longest complete Present between input and changed pixels")
        for action, group in dwm_longest.items():
            measured = sum(duration is not None for duration in group)
            print(action, f"{measured}/{len(dwm_trace[action])}", summarize(group))
    return 1 if missing else 0


if __name__ == "__main__":
    raise SystemExit(main())
