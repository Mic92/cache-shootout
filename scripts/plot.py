#!/usr/bin/env python3
"""Render seaborn charts from criterion's on-disk JSON output.

Criterion already emits an HTML report, but a single side-by-side PNG is
easier to drop into a README or share, so we re-read the raw estimates and
plot them ourselves.
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


def order_servers(df: pd.DataFrame) -> list[str]:
    """Stable server ordering across every subplot.

    Sort by mean sequential-NAR time so the visual left-to-right roughly tracks
    "fast to slow"; falls back to overall mean if that metric is missing.
    """
    seq = df[df["metric"] == "nar_download_c1"]
    base = seq if not seq.empty else df
    return list(base.groupby("server")["time_s"].mean().sort_values().index)


def plot(df: pd.DataFrame, out: Path) -> None:
    sns.set_theme(style="whitegrid", context="notebook")
    server_order = order_servers(df)
    palette = dict(
        zip(server_order, sns.color_palette("colorblind", len(server_order)))
    )

    closures = sorted(df["closure"].unique())
    nar_metrics = sorted(
        (m for m in df["metric"].unique() if m.startswith("nar_download_")),
        key=lambda m: int(m.removeprefix("nar_download_c")),
    )
    cols = ["narinfo_all", *nar_metrics]

    fig, axes = plt.subplots(
        len(closures),
        len(cols),
        figsize=(4.8 * len(cols), 4.5 * len(closures)),
        squeeze=False,
    )

    for r, closure in enumerate(closures):
        for c, metric in enumerate(cols):
            ax = axes[r][c]
            d = df[(df["closure"] == closure) & (df["metric"] == metric)]
            if d.empty:
                ax.axis("off")
                continue
            if metric == "narinfo_all":
                d = d.assign(ms=d["time_s"] * 1000)
                sns.barplot(
                    data=d,
                    x="server",
                    y="ms",
                    hue="server",
                    ax=ax,
                    order=server_order,
                    palette=palette,
                    legend=False,
                )
                ax.set_ylabel("time [ms] ↓")
                # nix-serve is two orders of magnitude slower; linear scale
                # would flatten everything else into the x-axis.
                ax.set_yscale("log")
                _annotate(ax, "{:.0f}")
            else:
                sns.barplot(
                    data=d,
                    x="server",
                    y="mibps",
                    hue="server",
                    ax=ax,
                    order=server_order,
                    palette=palette,
                    legend=False,
                )
                ax.set_ylabel("MiB/s ↑")
                # nginx sendfile vs. on-the-fly NAR streamers spans ~100x.
                ax.set_yscale("log")
                _annotate(ax, "{:.0f}")
            conc = metric.removeprefix("nar_download_c")
            title = "narinfo" if metric == "narinfo_all" else f"NAR, {conc} conn"
            ax.set_title(f"{closure} — {title}")
            ax.set_xlabel("")
            ax.tick_params(axis="x", rotation=40)
            for lbl in ax.get_xticklabels():
                lbl.set_ha("right")

    fig.suptitle("Nix binary cache shootout", y=1.02, fontsize=16)
    fig.tight_layout()
    out.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(out, dpi=150, bbox_inches="tight")
    print(f"wrote {out}")


def _annotate(ax: plt.Axes, fmt: str) -> None:
    for p in ax.patches:
        h = p.get_height()
        if not h or pd.isna(h):
            continue
        ax.annotate(
            fmt.format(h),
            (p.get_x() + p.get_width() / 2, h),
            ha="center",
            va="bottom",
            fontsize=8,
        )


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--criterion-dir", type=Path, default=Path("target/criterion"))
    ap.add_argument("--out", type=Path, default=Path("target/plots/shootout.png"))
    args = ap.parse_args()

    df = load_results(args.criterion_dir)
    plot(df, args.out)


if __name__ == "__main__":
    main()
