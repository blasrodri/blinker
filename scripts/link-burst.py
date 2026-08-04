#!/usr/bin/env python3
"""Read a `BLINKER_TRACE_WAIT` trace and report the queue in it.

    BLINKER_TRACE_WAIT=/tmp/t cargo build ... && scripts/link-burst.py /tmp/t

Finding 204 measured a cold build submitting eleven links at once and found
them served in a 35.7 -> 205.5 ms staircase. That staircase is invisible from
inside the daemon — each served link's own timings look healthy, because the
waiting happens in the listen backlog before the daemon ever sees the request.
The client is the only place it can be seen, so the client writes a line per
link and this reads them.

Each line is

    pid  start  end  waited_ms  worker  output

with `start` and `end` as absolute seconds, which is what makes overlap a fact
rather than an inference: two links overlap when one's interval contains the
other's start. The summary reports the burst — the widest run of overlapping
links — because that is the part concurrency changes. Sum-of-waits does not
distinguish eleven links waiting in a queue from eleven links running at once.
"""

import sys
from collections import defaultdict


def load(path):
    """Lines from before the worker pool carry no worker field; they read as
    worker "-", so a trace taken with either linker can be compared."""
    links = []
    for line in open(path):
        parts = line.split(None, 5)
        if len(parts) == 5:
            _, start, end, waited, output = parts
            worker = "-"
        elif len(parts) == 6:
            _, start, end, waited, worker, output = parts
        else:
            continue
        links.append((float(start), float(end), float(waited), worker, output.strip()))
    return sorted(links)


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    links = load(sys.argv[1])
    if not links:
        sys.exit("no links in the trace")
    origin = links[0][0]

    # How many other links were in flight when each one started.
    def overlapping(at):
        return sum(1 for s, e, *_ in links if s <= at <= e)

    print(f"{len(links)} links")
    per_worker = defaultdict(int)
    for start, end, waited, worker, output in links:
        per_worker[worker] += 1
        print(
            f"  {start - origin:7.3f}s -> {end - origin:7.3f}s "
            f"{waited:8.1f} ms  concurrent {overlapping(start):2d}  "
            f"worker {worker}  {output.rsplit('/', 1)[-1][:40]}"
        )

    span = max(e for _, e, *_ in links) - origin
    print()
    print(f"  span         {span * 1000:8.1f} ms   first submitted to last finished")
    print(f"  slowest      {max(w for _, _, w, _, _ in links):8.1f} ms   worst client wait")
    print(f"  median       {sorted(w for _, _, w, _, _ in links)[len(links) // 2]:8.1f} ms")
    print(f"  peak         {max(overlapping(s) for s, *_ in links):8d}      links in flight at once")
    print("  workers      " + " ".join(f"{k}:{v}" for k, v in sorted(per_worker.items())))


if __name__ == "__main__":
    main()
