# cache-shootout

Criterion benchmark comparing Nix binary cache servers over raw HTTP
(narinfo + NAR download, sequential and concurrent).

Servers under test:

- harmonia
- nix-serve (perl/starman)
- nix-serve-ng
- ncps (proxying a local harmonia, measured warm)
- nix-serve-ng behind nginx with on-the-fly zstd transfer encoding
- nginx (static flat-file `file://` cache; `none` and `zstd` NAR compression)
- attic (sqlite + local storage, closure pushed up-front; `none` and `zstd`)

Each server is built from its own upstream flake (see `flake.nix` inputs),
so `nix flake update <input>` bumps an individual implementation.

## Run

```sh
nix develop -c cargo bench
```

HTML report: `target/criterion/report/index.html`.

Seaborn wall-time chart + CSV:

```sh
nix develop -c python3 scripts/plot.py --out results/shootout.png --csv-out results/shootout.csv
```

See `results/` for the committed run.

## Knobs

- `BENCH_CLOSURES` — comma-separated closure names
  (default: `firefox,nixos-minimal`). Each name resolves to
  `.#packages.<system>.closure-<name>`; a literal flake ref containing `#`
  is used as-is.
- `*_BIN` — absolute paths to each server binary, set by the devShell so the
  harness runs the exact upstream-flake builds regardless of `PATH`.
