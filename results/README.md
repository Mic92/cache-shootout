# Results

![Nix binary cache shootout](shootout.png)

Measured on an AMD EPYC 7713P (64 cores), 991 GiB RAM, NixOS. All servers
and the benchmark client run on the same machine over loopback.

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
