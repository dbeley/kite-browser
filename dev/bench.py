#!/usr/bin/env python3
"""Measure cold start and total memory for a browser, honestly.

Not a microbenchmark: it starts the real binary under a real X server, waits for
a window to appear, then sums RSS across the whole process tree (browser +
renderers + GPU + zygote). That last part matters — a "10 MB browser" that leaves
600 MB of renderers running is not a light browser.

usage: bench.py <label> <command...>
"""
import json
import os
import subprocess
import sys
import time


def children(pid: int) -> list[int]:
    out = [pid]
    try:
        tasks = os.listdir(f"/proc/{pid}/task")
    except OSError:
        return out
    for t in tasks:
        try:
            kids = open(f"/proc/{pid}/task/{t}/children").read().split()
        except OSError:
            continue
        for k in kids:
            try:
                out.extend(children(int(k)))
            except (ValueError, RecursionError):
                pass
    return out


def window_for(cls: str, timeout: float) -> float | None:
    t0 = time.time()
    while time.time() - t0 < timeout:
        r = subprocess.run(
            ["xdotool", "search", "--class", cls],
            capture_output=True, text=True,
        )
        if r.returncode == 0 and r.stdout.strip():
            return time.time() - t0
        time.sleep(0.05)
    return None


def tree_mem(pid: int) -> dict:
    """RSS and PSS across the process tree.

    Summed RSS double-counts shared pages (every process that maps a library
    counts it again), so PSS — which splits shared pages proportionally — is the
    number worth quoting.
    """
    rss_total, pss_total, n = 0, 0, 0
    seen = set()
    ui_rss = 0
    for p in children(pid):
        if p in seen:
            continue
        seen.add(p)
        try:
            with open(f"/proc/{p}/statm") as f:
                pages = int(f.read().split()[1])
            kb = pages * (os.sysconf("SC_PAGE_SIZE") // 1024)
            rss_total += kb
            if p == pid:
                ui_rss = kb
            n += 1
        except (OSError, IndexError, ValueError):
            pass
        try:
            with open(f"/proc/{p}/smaps_rollup") as f:
                for line in f:
                    if line.startswith("Pss:"):
                        pss_total += int(line.split()[1])
                        break
        except OSError:
            pss_total += 0
    return {"rss_kb": rss_total, "pss_kb": pss_total, "procs": n, "ui_kb": ui_rss}


def main() -> int:
    label = sys.argv[1]
    cmd = sys.argv[2:]
    cls = os.environ.get("BENCH_CLASS", "Chromium-browser")
    settle = float(os.environ.get("BENCH_SETTLE", "12"))
    timeout = float(os.environ.get("BENCH_TIMEOUT", "90"))

    t0 = time.time()
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                            start_new_session=True)
    win_s = window_for(cls, timeout)
    spawned = time.time() - t0
    time.sleep(max(0.0, settle - (time.time() - t0)))
    m1 = tree_mem(proc.pid)
    time.sleep(2)
    m2 = tree_mem(proc.pid)
    pick = lambda k: max(m1[k], m2[k])  # noqa: E731
    result = {
        "label": label,
        "window_s": round(win_s, 2) if win_s is not None else None,
        "spawn_return_s": round(spawned, 2),
        "ui_rss_mb": round(pick("ui_kb") / 1024, 1),
        "pss_mb": round(pick("pss_kb") / 1024, 1),
        "rss_mb": round(pick("rss_kb") / 1024, 1),
        "processes": pick("procs"),
        "pid": proc.pid,
    }
    print(json.dumps(result))
    try:
        os.killpg(os.getpgid(proc.pid), 15)
    except OSError:
        pass
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        os.killpg(os.getpgid(proc.pid), 9)
    subprocess.run(["pkill", "-f", f"user-data-dir=/tmp/kite-bench"], capture_output=True)
    pat = os.environ.get("BENCH_KILLPAT")
    if pat:
        subprocess.run(["pkill", "-f", pat], capture_output=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
