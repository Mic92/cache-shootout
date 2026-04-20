# Results

Host `ryan` — AMD EPYC 7713P (64 cores), 991 GiB RAM, NixOS. All servers and
the benchmark client run on the same machine over loopback.

Closures:

| name | paths | NAR (none) | NAR (zstd) |
|---|---|---|---|
| firefox | 373 | 1541 MiB | ~520 MiB |
| nixos-minimal | 493 | 1033 MiB | ~420 MiB |

Files:

- `ryan-time.png` / `ryan-time-linear.png` — wall time per full closure
  pass (log / linear x-axis); **directly comparable across compression
  modes**, lower is better. Hatched bars = zstd.
- `ryan-throughput.png` — socket MiB/s = bytes read off the TCP socket /
  wall time. The client never decompresses, so for `*-zstd` this counts
  compressed bytes; the chart splits none/zstd into two blocks for that
  reason.
- `ryan.csv` — flat `(closure, metric, server, time_s, mibps)` table.
- `ryan.log` — raw `cargo bench` output.

Re-render from a fresh `target/criterion`:

```sh
python3 scripts/plot.py --unit time                       --out results/ryan-time.png --csv-out results/ryan.csv
python3 scripts/plot.py --unit time       --scale linear  --out results/ryan-time-linear.png
python3 scripts/plot.py --unit throughput                 --out results/ryan-throughput.png
```
