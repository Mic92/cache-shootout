# Results

Host `ryan` — AMD EPYC 7713P (64 cores), 991 GiB RAM, NixOS. All servers and
the benchmark client run on the same machine over loopback.

Closures:

| name | paths | NAR (none) | NAR (zstd) |
|---|---|---|---|
| firefox | 373 | 1541 MiB | ~520 MiB |
| nixos-minimal | 493 | 1033 MiB | ~420 MiB |

Throughput is **wire bytes / wall time**; for zstd variants that is the
compressed size, so a higher number means the server pushed more compressed
bytes per second, not more uncompressed payload.

Files:

- `ryan.png` — seaborn grid (closure × metric, log scale, hatched = zstd)
- `ryan.csv` — flat `(closure, metric, server, time_s, mibps)` table
- `ryan.log` — raw `cargo bench` output

Re-render from a fresh `target/criterion`:

```sh
python3 scripts/plot.py --out results/ryan.png --csv-out results/ryan.csv
```
