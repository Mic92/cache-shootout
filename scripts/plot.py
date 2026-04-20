#!/usr/bin/env python3
"""Render seaborn charts (and a CSV) from criterion's on-disk JSON output.

Criterion already emits an HTML report, but a single PNG plus a flat CSV are
easier to commit and diff than the full ``target/criterion`` tree.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import matplotlib

matplotlib.use("Agg")  # headless: render to file, no $DISPLAY
import matplotlib.pyplot as plt  # noqa: E402
import pandas as pd  # noqa: E402
import seaborn as sns  # noqa: E402


def load_results(criterion_dir: Path) -> pd.DataFrame:
    """Walk target/criterion and return one row per (closure, metric, server).

    Criterion 0.5 lays out results as
    ``<group>/<id>/new/{benchmark,estimates}.json``; the bench encodes
    ``group_id = "<metric>/<closure>"`` and the server name in ``value_str``.
    """
    rows: list[dict[str, object]] = []
    for bench_json in criterion_dir.rglob("new/benchmark.json"):
        est_json = bench_json.with_name("estimates.json")
        if not est_json.exists():
            continue
        meta = json.loads(bench_json.read_text())
        est = json.loads(est_json.read_text())

        group = meta.get("group_id")
        server = meta.get("value_str") or meta.get("function_id")
        if not group or not server or "/" not in group:
            continue
        metric, closure = group.split("/", 1)

        mean_ns = est["mean"]["point_estimate"]
        time_s = mean_ns / 1e9

        tp = meta.get("throughput") or {}
        bytes_per_iter = tp.get("Bytes")
        mibps = (bytes_per_iter / time_s) / (1024 * 1024) if bytes_per_iter else None

        rows.append(
            {
                "closure": closure,
                "metric": metric,
                "server": server,
                "time_s": time_s,
                "mibps": mibps,
            }
        )

    if not rows:
        sys.exit(f"no criterion results found under {criterion_dir}")
    return pd.DataFrame(rows)


def split_server(name: str) -> tuple[str, str]:
    """Map a server label to (implementation, compression) for colouring."""
    if name.endswith("+nginx-zstd"):
        return name.removesuffix("+nginx-zstd"), "zstd"
    for suf in ("-none", "-zstd"):
        if name.endswith(suf):
            return name.removesuffix(suf), suf[1:]
    return name, "none"


def order_servers(df: pd.DataFrame, unit: str) -> list[str]:
    """One ordering shared by every subplot.

    For ``time`` the bars are directly comparable across compression modes, so
    just sort by mean sequential-NAR wall time. For ``throughput`` the zstd
    numerator is compressed bytes, so keep the two compression blocks apart
    and sort within each.
    """
    seq = df[df["metric"] == "nar_download_c1"]
    base = seq if not seq.empty else df
    if unit == "time":
        t = base.groupby("server")["time_s"].mean().sort_values()
        return list(t.index)
    speed = base.groupby("server")["mibps"].mean()
    servers = list(speed.index)
    servers.sort(
        key=lambda s: (split_server(s)[1] != "none", -float(speed.get(s, 0.0)))
    )
    return servers


def plot(df: pd.DataFrame, out: Path, unit: str, scale: str) -> None:
    sns.set_theme(style="whitegrid", context="notebook")
    server_order = order_servers(df, unit)
    # Colour by underlying implementation so the none/zstd pair of each server
    # share a hue; the compression axis is encoded as a hatch instead.
    impls = list(dict.fromkeys(split_server(s)[0] for s in server_order))
    impl_palette = dict(zip(impls, sns.color_palette("colorblind", len(impls))))
    palette = {s: impl_palette[split_server(s)[0]] for s in server_order}
    zstd_servers = {s for s in server_order if split_server(s)[1] == "zstd"}

    closures = sorted(df["closure"].unique())
    nar_metrics = sorted(
        (m for m in df["metric"].unique() if m.startswith("nar_download_")),
        key=lambda m: int(m.removeprefix("nar_download_c")),
    )
    cols = ["narinfo_all", *nar_metrics]

    fig, axes = plt.subplots(
        len(closures),
        len(cols),
        figsize=(5.2 * len(cols), 0.45 * len(server_order) * len(closures) + 1.5),
        squeeze=False,
        sharey=True,
    )

    for r, closure in enumerate(closures):
        for c, metric in enumerate(cols):
            ax = axes[r][c]
            d = df[(df["closure"] == closure) & (df["metric"] == metric)]
            if d.empty:
                ax.axis("off")
                continue
            if metric == "narinfo_all":
                d = d.assign(value=d["time_s"] * 1000)
                xlabel = "time [ms]  ↓"
                fmt = "{:.1f}"
            elif unit == "time":
                d = d.assign(value=d["time_s"])
                xlabel = "time [s]  ↓"
                fmt = "{:.2f}"
            else:
                d = d.assign(value=d["mibps"])
                xlabel = "socket MiB/s  ↑  (compressed for zstd)"
                fmt = "{:.0f}"
            # Reindex onto the full server list so every panel has exactly one
            # patch per server in a known order; this makes the manual
            # colour/hatch pass below robust to partial runs.
            d = (
                pd.DataFrame({"server": server_order})
                .merge(d[["server", "value"]], how="left")
                .set_index("server")
                .reindex(server_order)
                .reset_index()
            )
            sns.barplot(data=d, y="server", x="value", ax=ax, color="0.7")
            # Spread spans ~2-3 orders of magnitude (nginx sendfile vs.
            # on-the-fly NAR streamers / perl); log keeps the tail readable,
            # linear makes absolute gaps obvious at the cost of flattening the
            # fast end into the axis.
            ax.set_xscale(scale)
            # Leave headroom on the right so the value labels of the longest
            # bars are not clipped against the axis frame.
            lo, hi = ax.get_xlim()
            ax.set_xlim(lo, hi * (2 if scale == "log" else 1.12))
            _annotate_h(ax, fmt)
            # Hatch the zstd variants so compression reads as a texture, not a
            # separate colour family.
            for patch, server in zip(ax.patches, server_order):
                patch.set_facecolor(palette[server])
                patch.set_edgecolor("white")
                if server in zstd_servers:
                    patch.set_hatch("//")
            if unit == "throughput":
                # Visual divider between the uncompressed and zstd blocks.
                n_none = sum(1 for s in server_order if s not in zstd_servers)
                if 0 < n_none < len(server_order):
                    ax.axhline(n_none - 0.5, color="0.5", lw=0.8)
            conc = metric.removeprefix("nar_download_c")
            title = "narinfo" if metric == "narinfo_all" else f"NAR, {conc} conn"
            ax.set_title(f"{closure} — {title}")
            ax.set_xlabel(xlabel)
            ax.set_ylabel("")

    fig.suptitle(
        f"Nix binary cache shootout   (↓ lower is better, ↑ higher is better, {scale} scale)",
        y=1.02,
        fontsize=15,
    )
    fig.tight_layout()
    out.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(out, dpi=150, bbox_inches="tight")
    print(f"wrote {out}")


def _annotate_h(ax: plt.Axes, fmt: str) -> None:
    for p in ax.patches:
        w = p.get_width()
        if not w or pd.isna(w):
            continue
        ax.annotate(
            fmt.format(w),
            (w, p.get_y() + p.get_height() / 2),
            ha="left",
            va="center",
            fontsize=8,
            xytext=(3, 0),
            textcoords="offset points",
        )


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--criterion-dir", type=Path, default=Path("target/criterion"))
    ap.add_argument("--out", type=Path, default=Path("target/plots/shootout.png"))
    ap.add_argument(
        "--unit",
        choices=["time", "throughput"],
        default="time",
        help="NAR panels: wall time per closure (comparable across compression) "
        "or wire MiB/s (compressed bytes for zstd variants)",
    )
    ap.add_argument("--scale", choices=["log", "linear"], default="log")
    ap.add_argument(
        "--csv-out",
        type=Path,
        default=None,
        help="also write the parsed results as CSV (committable artefact)",
    )
    args = ap.parse_args()

    df = load_results(args.criterion_dir)
    if args.csv_out:
        args.csv_out.parent.mkdir(parents=True, exist_ok=True)
        df.sort_values(["closure", "metric", "server"]).to_csv(
            args.csv_out, index=False
        )
        print(f"wrote {args.csv_out}")
    plot(df, args.out, args.unit, args.scale)


if __name__ == "__main__":
    main()
