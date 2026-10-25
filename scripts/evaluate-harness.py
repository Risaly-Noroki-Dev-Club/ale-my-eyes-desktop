#!/usr/bin/env python3
"""Run the offline CLI matrix; never operates a real desktop or calls cloud APIs."""
import argparse
import json
from pathlib import Path
import subprocess


def goal_satisfied(report):
    observations = [event["observation"] for event in report["events"] if event["event"] == "observed"]
    if not observations:
        return False
    latest = observations[-1]
    if latest["window"] != report["task"]["window"]:
        return False
    conditions = report["task"]["success"]
    if not conditions:
        return False
    for condition in conditions:
        nodes = [node for node in latest["nodes"] if node["role"] == condition["role"] and node["label"] == condition["label"]]
        if len(nodes) != 1 or nodes[0]["properties"].get(condition["property"]) != condition["equals"]:
            return False
    return True


def main():
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=root / "target/debug/ale-cli")
    parser.add_argument("--output-dir", type=Path, default=root / "target/harness-evaluation")
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    scenarios = {
        "normal": ("completed", 4),
        "recover": ("completed", 5),
        "no-progress": ("needs_user", 2),
        "lost-receipt": ("outcome_unknown", 1),
        "lost-receipt-after-save": ("completed", 4),
        "silent-click": ("outcome_unknown", 1),
        "delayed-save": ("completed", 4),
        "window-changed": ("outcome_unknown", 1),
    }
    cases = []
    for task in ("zoom", "theme", "mute"):
        for scenario, (expected, dispatches) in scenarios.items():
            result = subprocess.run(
                [str(args.binary), "harness-demo", "--task", task, "--scenario", scenario],
                capture_output=True, text=True, timeout=30, check=False,
            )
            report = json.loads(result.stdout)
            errors = []
            if report["status"] != expected:
                errors.append(f"expected {expected}, got {report['status']}")
            if result.returncode != (0 if expected == "completed" else 2):
                errors.append(f"unexpected exit code {result.returncode}")
            if report["actions"] != dispatches:
                errors.append(f"expected {dispatches} dispatches, got {report['actions']}")
            if report["status"] == "completed" and not goal_satisfied(report):
                errors.append("completion has no independent final-state evidence")
            approved = set()
            for event in report["events"]:
                if event["event"] == "authorized" and event["approval"] == "allow":
                    approved.add(event["action_id"])
                if event["event"] == "dispatching" and event["request"]["id"] not in approved:
                    errors.append("input dispatched without prior authorization")
            filename = f"{task}-{scenario}.json"
            (args.output_dir / filename).write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
            cases.append({"task": task, "scenario": scenario, "expected": expected, "actual": report["status"], "actions": report["actions"], "elapsed_ms": report["elapsed_ms"], "passed": not errors, "errors": errors, "trace": filename})
    summary = {
        "evidence_scope": "deterministic simulator and reference planner; no real devices or external model calls",
        "cases": len(cases),
        "passed": sum(case["passed"] for case in cases),
        "tasks_completed": sum(case["actual"] == "completed" for case in cases),
        "expected_stops": sum(case["passed"] and case["expected"] != "completed" for case in cases),
        "results": cases,
    }
    (args.output_dir / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({key: value for key, value in summary.items() if key != "results"}, ensure_ascii=False))
    if summary["passed"] != summary["cases"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
