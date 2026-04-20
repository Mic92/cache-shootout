# Results

![Nix binary cache shootout](shootout.png)

## Machine

| | |
|---|---|
| CPU | AMD EPYC 7713P, 64 cores / 128 threads |
| RAM | 991 GiB |
| OS | NixOS 25.11 (Xantusia), Linux 6.8.0 |
| Nix | 2.30.0pre |
| Storage | ZFS `zroot` on Dell Ent NVMe AGN MU AIC 1.6 TB |
| `/nix/store`, `/scratch`, `/tmp` | all on the same ZFS pool (no tmpfs) |

All servers and the benchmark client run on this machine over loopback. The
closure is hot in ARC/page cache after the warm-up pass, so results reflect
CPU and software overhead rather than disk bandwidth.

## Workload

Closures:

| name | paths | NAR (none) | NAR (zstd) |
|---|---|---|---|
| firefox | 373 | 1541 MiB | ~520 MiB |
| nixos-minimal | 493 | 1033 MiB | ~420 MiB |

Files:

- `shootout.png` — wall time per full closure pass, linear scale, lower is
  better. Hatched bars = zstd. Directly comparable across compression
  modes since the unit is seconds, not bytes-on-socket.
- `shootout.csv` — flat `(closure, metric, server, time_s)` table.
- `bench.log` — raw `cargo bench` output.

Re-render from a fresh `target/criterion`:

```sh
python3 scripts/plot.py --out results/shootout.png --csv-out results/shootout.csv
```
