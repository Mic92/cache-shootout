# Results

Host `ryan` — AMD EPYC 7713P (64 cores), 991 GiB RAM, NixOS. All servers and
the benchmark client run on the same machine over loopback.

Closures:

| name | paths | NAR (none) | NAR (zstd) |
|---|---|---|---|
| firefox | 373 | 1541 MiB | ~520 MiB |
| nixos-minimal | 493 | 1033 MiB | ~420 MiB |

**Wire MiB/s** = bytes read off the TCP socket / wall-clock for one full
closure pass. The client never decompresses, so:

- `*-none` rows: raw NAR throughput (numerator = uncompressed NAR size).
- `*-zstd` rows: compressed throughput (numerator = zstd bytes on the wire).

The two blocks are therefore not directly comparable bar-to-bar; the chart
separates them with a divider. To compare end-to-end, look at `time_s` in
`ryan.csv` instead.

Files:

- `ryan.png` — seaborn grid (closure × metric, log scale, hatched = zstd)
- `ryan.csv` — flat `(closure, metric, server, time_s, mibps)` table
- `ryan.log` — raw `cargo bench` output

Re-render from a fresh `target/criterion`:

```sh
python3 scripts/plot.py --out results/ryan.png --csv-out results/ryan.csv
```
