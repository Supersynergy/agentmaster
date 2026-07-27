#!/usr/bin/env python3
"""Deterministic, zero-provider payload benchmark for llmadapter routing."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import statistics
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any


LINE_COUNT = 4_000
ERROR_COUNT = 100
MARKER_LINE = 3_777
MARKER = "FINAL_NEEDLE"
DEFAULT_RUNS = 501

OBJECTIVE = (
    "Using the supplied log, return JSON with exact error_count (lines whose "
    "level field is exactly ERROR) and the 1-based line number containing "
    "marker=FINAL_NEEDLE."
)
ORACLE = (
    "python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); "
    "raise SystemExit(d!={\"error_count\":100,\"marker_line\":3777})' "
    '"$AGENTMASTER_ANSWER_PATH"'
)
AWK_PROGRAM = (
    '$2=="ERROR"{e++} '
    'index($0,"marker=FINAL_NEEDLE"){m=NR} '
    'END{printf "{\\"error_count\\":%d,\\"marker_line\\":%d}\\n",e,m}'
)
FILTER_COMMAND = (
    f"awk '{AWK_PROGRAM}' "
    '"$AGENTMASTER_INPUT_PATH" > "$AGENTMASTER_PROJECTED_PATH"'
)


def fixture_text() -> str:
    """Return a stable log with 100 exact ERROR levels and one marker."""
    lines: list[str] = []
    for line_number in range(1, LINE_COUNT + 1):
        level = "ERROR" if line_number % 40 == 0 else "INFO"
        marker = MARKER if line_number == MARKER_LINE else "-"
        lines.append(
            f"{line_number:04d} {level} event=synthetic "
            f"bucket={line_number % 17:02d} marker={marker}"
        )
    return "\n".join(lines) + "\n"


def project_fixture(path: Path) -> dict[str, int]:
    """Apply the filter locally; return only facts needed by the objective."""
    errors = 0
    marker_line = 0
    with path.open("r", encoding="utf-8") as handle:
        for line_number, line in enumerate(handle, start=1):
            fields = line.split(maxsplit=2)
            if len(fields) >= 2 and fields[1] == "ERROR":
                errors += 1
            if f"marker={MARKER}" in line:
                marker_line = line_number
    return {"error_count": errors, "marker_line": marker_line}


def canonical_json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def build_naive_payload(log_text: str) -> str:
    return canonical_json(
        {
            "objective": OBJECTIVE,
            "oracle": ORACLE,
            "input": {"kind": "inline_log", "content": log_text},
        }
    )


def build_bounded_capsule(projected: dict[str, int]) -> str:
    return canonical_json(
        {
            "objective": OBJECTIVE,
            "local_filter": FILTER_COMMAND,
            "projected_evidence": projected,
            "oracle": ORACLE,
            "limits": {
                "max_workers": 3,
                "max_result_tokens": 500,
                "deadline_seconds": 120,
            },
            "artifacts": {
                "input": "$AGENTMASTER_INPUT_PATH",
                "projected": "$AGENTMASTER_PROJECTED_PATH",
                "answer": "$AGENTMASTER_ANSWER_PATH",
                "usage": "$AGENTMASTER_USAGE_PATH",
            },
        }
    )


def percentile_nearest_rank(samples: list[float], percentile: float) -> float:
    ordered = sorted(samples)
    index = max(0, math.ceil(percentile * len(ordered)) - 1)
    return ordered[index]


def self_check(
    fixture_path: Path,
    log_text: str,
    naive: str,
    bounded: str,
    projected: dict[str, int],
) -> None:
    lines = log_text.splitlines()
    exact_errors = sum(line.split(maxsplit=2)[1] == "ERROR" for line in lines)
    marker_lines = [
        index
        for index, line in enumerate(lines, start=1)
        if f"marker={MARKER}" in line
    ]
    assert len(lines) == LINE_COUNT
    assert exact_errors == ERROR_COUNT
    assert marker_lines == [MARKER_LINE]
    assert projected == {"error_count": ERROR_COUNT, "marker_line": MARKER_LINE}
    awk_result = subprocess.run(
        ["awk", AWK_PROGRAM, str(fixture_path)],
        check=True,
        capture_output=True,
        text=True,
    )
    assert json.loads(awk_result.stdout) == projected
    with tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        prefix="agentmaster-oracle-answer-",
        suffix=".json",
    ) as answer_file:
        json.dump(projected, answer_file)
        answer_file.flush()
        os.chmod(answer_file.name, 0o600)
        assert Path(answer_file.name).stat().st_mode & 0o777 == 0o600
        oracle_result = subprocess.run(
            ["/bin/sh", "-c", ORACLE],
            check=False,
            capture_output=True,
            text=True,
            env={**os.environ, "AGENTMASTER_ANSWER_PATH": answer_file.name},
        )
        assert oracle_result.returncode == 0, oracle_result.stderr

    naive_data = json.loads(naive)
    bounded_data = json.loads(bounded)
    assert naive_data["objective"] == bounded_data["objective"] == OBJECTIVE
    assert naive_data["oracle"] == bounded_data["oracle"] == ORACLE
    assert bounded_data["limits"] == {
        "max_workers": 3,
        "max_result_tokens": 500,
        "deadline_seconds": 120,
    }
    assert naive_data["input"]["content"] == log_text
    assert "input" not in bounded_data


def run_benchmark(fixture_path: Path, runs: int) -> dict[str, Any]:
    log_text = fixture_path.read_text(encoding="utf-8")
    naive = build_naive_payload(log_text)

    samples_ms: list[float] = []
    projected: dict[str, int] | None = None
    bounded = ""
    for _ in range(runs):
        started = time.perf_counter_ns()
        projected = project_fixture(fixture_path)
        bounded = build_bounded_capsule(projected)
        elapsed_ns = time.perf_counter_ns() - started
        samples_ms.append(elapsed_ns / 1_000_000)

    assert projected is not None
    self_check(fixture_path, log_text, naive, bounded, projected)

    naive_bytes = len(naive.encode("utf-8"))
    bounded_bytes = len(bounded.encode("utf-8"))
    reduction_percent = (1.0 - bounded_bytes / naive_bytes) * 100.0
    return {
        "benchmark": "llmadapter_payload_capacity_proxy",
        "provider_calls": 0,
        "fixture": {
            "lines": LINE_COUNT,
            "exact_error_lines": ERROR_COUNT,
            "marker_line": MARKER_LINE,
            "sha256": hashlib.sha256(log_text.encode("utf-8")).hexdigest(),
        },
        "semantic_equivalence": {
            "same_objective": True,
            "same_oracle": True,
            "expected_answer": projected,
        },
        "payload": {
            "naive_inline_bytes": naive_bytes,
            "bounded_capsule_bytes": bounded_bytes,
            "reduction_percent": round(reduction_percent, 4),
        },
        "bounded_build_runtime_ms": {
            "runs": runs,
            "p50": round(statistics.median(samples_ms), 4),
            "p95_nearest_rank": round(percentile_nearest_rank(samples_ms, 0.95), 4),
        },
        "self_check": "PASS",
        "claim_scope": (
            "Local projection payload-capacity case study only; not an "
            "equal-work agent A/B or provider token, cost, quality, or "
            "end-to-end latency measurement."
        ),
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--runs",
        type=int,
        default=DEFAULT_RUNS,
        help=f"Measured bounded-build iterations (default: {DEFAULT_RUNS}).",
    )
    parser.add_argument(
        "--fixture-out",
        type=Path,
        help="Optional path to retain the generated fixture.",
    )
    parser.add_argument(
        "--json-out",
        type=Path,
        help="Optional path to also write the JSON result.",
    )
    args = parser.parse_args()
    if args.runs < 20:
        parser.error("--runs must be at least 20")
    return args


def main() -> int:
    args = parse_args()
    text = fixture_text()

    if args.fixture_out:
        args.fixture_out.parent.mkdir(parents=True, exist_ok=True)
        args.fixture_out.write_text(text, encoding="utf-8")
        result = run_benchmark(args.fixture_out, args.runs)
    else:
        with tempfile.TemporaryDirectory(prefix="agentmaster-llmadapter-bench-") as temp:
            fixture_path = Path(temp) / "synthetic.log"
            fixture_path.write_text(text, encoding="utf-8")
            result = run_benchmark(fixture_path, args.runs)

    rendered = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.json_out:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        args.json_out.write_text(rendered, encoding="utf-8")
    print(rendered, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
