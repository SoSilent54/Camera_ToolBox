#!/usr/bin/env python3
"""Generate matplotlib figures for the H2 observability simulation report."""

from __future__ import annotations

import argparse
import csv
import math
from collections import defaultdict
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Sequence, Tuple

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib import cm
from matplotlib.patches import Rectangle

IMAGE_WIDTH = 1920
IMAGE_HEIGHT = 1080
BOARD_COLS = 11
BOARD_ROWS = 8
TRUE_FX = 1200.0
TRUE_FY = 1180.0
TRUE_CX = 960.0
TRUE_CY = 540.0
THRESHOLDS = {
    "cond_h": 1.0e8,
    "focal_std_max_pct": 0.5,
    "principal_std_max_px": 2.0,
    "d5_edge_std_px": 2.0,
}
SCENARIO_LABELS = {
    "fronto_parallel_only": "Fronto-parallel only",
    "same_depth_pose_diverse": "Same depth, pose diverse",
    "progressive_full_coverage_true_D12": "Progressive coverage, true D12",
    "progressive_full_coverage_true_D5": "Progressive coverage, true D5",
    "aggressive_edge_coverage_true_D5": "Aggressive edge coverage, true D5",
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--input-dir",
        default="/tmp/h2_observability_sim",
        type=Path,
        help="Directory containing metrics.csv and corners.csv from the Rust simulation test.",
    )
    parser.add_argument(
        "--output-dir",
        default=Path(".ai_doc/experiments/figures/h2_observability"),
        type=Path,
        help="Directory for generated PNG figures and milestone markdown.",
    )
    return parser.parse_args()


def read_csv(path: Path) -> List[Dict[str, str]]:
    with path.open(newline="", encoding="utf-8") as handle:
        return list(csv.DictReader(handle))


def f(row: Dict[str, str], key: str) -> Optional[float]:
    value = row.get(key, "")
    if value == "":
        return None
    try:
        parsed = float(value)
    except ValueError:
        return None
    return parsed if math.isfinite(parsed) else None


def b(row: Dict[str, str], key: str) -> bool:
    return row.get(key, "").lower() == "true"


def scenario_title(name: str) -> str:
    return SCENARIO_LABELS.get(name, name)


def group_by(rows: Iterable[Dict[str, str]], key: str) -> Dict[str, List[Dict[str, str]]]:
    grouped: Dict[str, List[Dict[str, str]]] = defaultdict(list)
    for row in rows:
        grouped[row[key]].append(row)
    for values in grouped.values():
        values.sort(key=lambda item: int(item.get("views") or item.get("view") or 0))
    return dict(grouped)


def finite_xy(rows: Sequence[Dict[str, str]], key: str) -> Tuple[List[int], List[float]]:
    xs: List[int] = []
    ys: List[float] = []
    for row in rows:
        value = f(row, key)
        if value is not None:
            xs.append(int(row["views"]))
            ys.append(value)
    return xs, ys


def plot_board_overviews(corners: List[Dict[str, str]], output_dir: Path) -> List[Path]:
    paths: List[Path] = []
    grouped = group_by(corners, "scenario")
    for scenario, rows in grouped.items():
        by_view: Dict[int, List[Dict[str, str]]] = defaultdict(list)
        for row in rows:
            by_view[int(row["view"])].append(row)
        view_ids = sorted(by_view)
        fig, ax = plt.subplots(figsize=(10, 5.8), dpi=160)
        ax.add_patch(Rectangle((0, 0), IMAGE_WIDTH, IMAGE_HEIGHT, fill=False, lw=1.5, color="#111111"))
        ax.axvline(IMAGE_WIDTH / 2, color="#999999", lw=0.6, ls="--")
        ax.axhline(IMAGE_HEIGHT / 2, color="#999999", lw=0.6, ls="--")
        colors = cm.viridis([i / max(1, len(view_ids) - 1) for i in range(len(view_ids))])
        for color, view in zip(colors, view_ids):
            pts = sorted(by_view[view], key=lambda row: int(row["corner"]))
            xs = [float(row["x"]) for row in pts]
            ys = [float(row["y"]) for row in pts]
            if not xs:
                continue
            outline_indices = list(range(BOARD_COLS))
            outline_indices += [row * BOARD_COLS + BOARD_COLS - 1 for row in range(1, BOARD_ROWS)]
            outline_indices += list(range(BOARD_COLS * BOARD_ROWS - 2, BOARD_COLS * (BOARD_ROWS - 1) - 1, -1))
            outline_indices += [row * BOARD_COLS for row in range(BOARD_ROWS - 2, 0, -1)]
            outline_indices.append(0)
            ox = [xs[index] for index in outline_indices]
            oy = [ys[index] for index in outline_indices]
            ax.plot(ox, oy, color=color, lw=1.0, alpha=0.75)
            ax.scatter(xs, ys, s=3, color=color, alpha=0.25)
        ax.set_xlim(0, IMAGE_WIDTH)
        ax.set_ylim(IMAGE_HEIGHT, 0)
        ax.set_aspect("equal", adjustable="box")
        ax.set_title(f"Board projection overview — {scenario_title(scenario)}")
        ax.set_xlabel("image x [px]")
        ax.set_ylabel("image y [px]")
        ax.grid(True, lw=0.3, alpha=0.4)
        mappable = cm.ScalarMappable(cmap="viridis")
        mappable.set_array(view_ids)
        colorbar = fig.colorbar(mappable, ax=ax, fraction=0.035, pad=0.02)
        colorbar.set_label("view index")
        path = output_dir / f"{scenario}_overview.png"
        fig.tight_layout()
        fig.savefig(path)
        plt.close(fig)
        paths.append(path)
    return paths


def plot_metric_dashboard(scenario: str, rows: Sequence[Dict[str, str]], output_dir: Path) -> Path:
    fig, axes = plt.subplots(2, 2, figsize=(11, 7), dpi=160, sharex=True)
    fig.suptitle(f"H2 metrics while adding images — {scenario_title(scenario)}")
    specs = [
        ("cond_h", "cond(H)", True),
        ("focal_std_max_pct", "max fx/fy std [%]", True),
        ("principal_std_max_px", "max cx/cy std [px]", True),
        ("d5_edge_std_px", "D5 edge std [px]", True),
    ]
    for ax, (key, label, threshold) in zip(axes.flat, specs):
        xs, ys = finite_xy(rows, key)
        ax.plot(xs, ys, marker="o", ms=3, lw=1.3, color="#1f77b4")
        if key in {"cond_h", "d5_edge_std_px"}:
            ax.set_yscale("log")
        if threshold and key in THRESHOLDS:
            ax.axhline(THRESHOLDS[key], color="#d62728", lw=1.0, ls="--", label="target")
            ax.legend(loc="best", fontsize=7)
        ax.set_ylabel(label)
        ax.grid(True, which="both", lw=0.3, alpha=0.4)
    axes[-1, 0].set_xlabel("views added")
    axes[-1, 1].set_xlabel("views added")
    path = output_dir / f"{scenario}_h2_metrics.png"
    fig.tight_layout(rect=(0, 0, 1, 0.95))
    fig.savefig(path)
    plt.close(fig)
    return path


def plot_intrinsics_error_compare(grouped: Dict[str, List[Dict[str, str]]], output_dir: Path) -> Path:
    fig, axes = plt.subplots(2, 2, figsize=(12, 7), dpi=160, sharex=False)
    fig.suptitle("Actual OpenCV calibration result error while adding images")
    specs = [
        ("fx_error_pct", "fx error [%]"),
        ("fy_error_pct", "fy error [%]"),
        ("cx_error_px", "cx error [px]"),
        ("cy_error_px", "cy error [px]"),
    ]
    for ax, (key, label) in zip(axes.flat, specs):
        for scenario, rows in grouped.items():
            xs, ys = finite_xy(rows, key)
            if xs:
                ax.plot(xs, ys, marker="o", ms=2.5, lw=1.0, label=scenario_title(scenario))
        ax.axhline(0.0, color="#111111", lw=0.8, ls="--")
        ax.set_ylabel(label)
        ax.grid(True, lw=0.3, alpha=0.4)
    axes[-1, 0].set_xlabel("views added")
    axes[-1, 1].set_xlabel("views added")
    axes[0, 1].legend(fontsize=7, loc="best")
    path = output_dir / "actual_intrinsics_error_compare.png"
    fig.tight_layout(rect=(0, 0, 1, 0.95))
    fig.savefig(path)
    plt.close(fig)
    return path


def plot_distortion_error_compare(grouped: Dict[str, List[Dict[str, str]]], output_dir: Path) -> Path:
    fig, axes = plt.subplots(3, 2, figsize=(12, 8), dpi=160, sharex=False)
    fig.suptitle("Actual OpenCV D5 coefficient error while adding images")
    specs = [
        ("k1_error", "k1 error"),
        ("k2_error", "k2 error"),
        ("p1_error", "p1 error"),
        ("p2_error", "p2 error"),
        ("k3_error", "k3 error"),
    ]
    for ax, (key, label) in zip(axes.flat, specs):
        for scenario, rows in grouped.items():
            xs, ys = finite_xy(rows, key)
            if xs:
                ax.plot(xs, ys, marker="o", ms=2.5, lw=1.0, label=scenario_title(scenario))
        ax.axhline(0.0, color="#111111", lw=0.8, ls="--")
        ax.set_ylabel(label)
        ax.grid(True, lw=0.3, alpha=0.4)
    axes.flat[-1].axis("off")
    axes[0, 1].legend(fontsize=7, loc="best")
    for ax in axes[-1, :]:
        ax.set_xlabel("views added")
    path = output_dir / "actual_d5_distortion_error_compare.png"
    fig.tight_layout(rect=(0, 0, 1, 0.95))
    fig.savefig(path)
    plt.close(fig)
    return path


def best_row(rows: Sequence[Dict[str, str]], key: str, smaller: bool = True) -> Optional[Dict[str, str]]:
    candidates = [(f(row, key), row) for row in rows if f(row, key) is not None]
    if not candidates:
        return None
    return min(candidates, key=lambda item: item[0])[1] if smaller else max(candidates, key=lambda item: item[0])[1]


def first_row(rows: Sequence[Dict[str, str]], predicate) -> Optional[Dict[str, str]]:
    for row in rows:
        if predicate(row):
            return row
    return None


def row_value(row: Optional[Dict[str, str]], key: str) -> str:
    if row is None:
        return "--"
    value = f(row, key)
    if value is None:
        return "--"
    if abs(value) >= 1e5 or (abs(value) > 0 and abs(value) < 1e-3):
        return f"{value:.2e}"
    return f"{value:.3f}"


def write_milestones(grouped: Dict[str, List[Dict[str, str]]], output_dir: Path) -> Path:
    lines = [
        "| 数据集 | OpenCV 首次可解 | H2 首次可分析 | H2 首次达标 | 最小 cond(H) | 最小 D5 edge σ(px) | 最终 fx/fy 误差 | 最终 cx/cy 误差(px) |",
        "|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for scenario, rows in grouped.items():
        first_solve = first_row(rows, lambda row: b(row, "solve_ok"))
        first_h2 = first_row(rows, lambda row: b(row, "h2_ok"))
        first_goal = first_row(rows, lambda row: b(row, "goal_met"))
        min_cond = best_row(rows, "cond_h")
        min_d5 = best_row(rows, "d5_edge_std_px")
        final_solve = None
        for row in reversed(rows):
            if b(row, "solve_ok"):
                final_solve = row
                break
        def view_cell(row: Optional[Dict[str, str]]) -> str:
            return row["views"] if row is not None else "--"
        lines.append(
            "| {name} | {first_solve} | {first_h2} | {first_goal} | {cond} | {d5} | {ferr} | {cerr} |".format(
                name=scenario_title(scenario),
                first_solve=view_cell(first_solve),
                first_h2=view_cell(first_h2),
                first_goal=view_cell(first_goal),
                cond=row_value(min_cond, "cond_h"),
                d5=row_value(min_d5, "d5_edge_std_px"),
                ferr=(
                    f"{row_value(final_solve, 'fx_error_pct')}/{row_value(final_solve, 'fy_error_pct')}"
                    if final_solve is not None
                    else "--"
                ),
                cerr=(
                    f"{row_value(final_solve, 'cx_error_px')}/{row_value(final_solve, 'cy_error_px')}"
                    if final_solve is not None
                    else "--"
                ),
            )
        )
    path = output_dir / "milestones.md"
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def main() -> None:
    args = parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    metrics = read_csv(args.input_dir / "metrics.csv")
    corners = read_csv(args.input_dir / "corners.csv")
    grouped_metrics = group_by(metrics, "scenario")

    generated = []
    generated.extend(plot_board_overviews(corners, args.output_dir))
    for scenario, rows in grouped_metrics.items():
        generated.append(plot_metric_dashboard(scenario, rows, args.output_dir))
    generated.append(plot_intrinsics_error_compare(grouped_metrics, args.output_dir))
    generated.append(plot_distortion_error_compare(grouped_metrics, args.output_dir))
    generated.append(write_milestones(grouped_metrics, args.output_dir))

    for path in generated:
        print(path)


if __name__ == "__main__":
    main()
