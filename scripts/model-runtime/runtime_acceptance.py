#!/usr/bin/env python3
"""Run real multimodal llama.cpp inference and emit a portable acceptance report."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import time
from pathlib import Path
from typing import Any


COORDINATE = re.compile(
    r"[\[(]\s*(-?(?:\d+(?:\.\d+)?|\.\d+))\s*,\s*(-?(?:\d+(?:\.\d+)?|\.\d+))\s*[\])]"
)
OFFLOAD = re.compile(r"offloaded\s+(\d+)\s*/\s*(\d+)\s+layers?\s+to\s+GPU", re.IGNORECASE)


def extract_json_objects(text: str) -> list[Any]:
    decoder = json.JSONDecoder()
    values: list[Any] = []
    for index, character in enumerate(text):
        if character != "{":
            continue
        try:
            value, _ = decoder.raw_decode(text[index:])
        except json.JSONDecodeError:
            continue
        values.append(value)
    return values


def extract_coordinate(text: str) -> tuple[float, float] | None:
    matches = COORDINATE.findall(text)
    if not matches:
        return None
    x, y = matches[-1]
    return float(x), float(y)


def point_in_bbox(point: tuple[float, float] | None, bbox: list[float]) -> bool:
    if point is None:
        return False
    x, y = point
    return bbox[0] <= x <= bbox[2] and bbox[1] <= y <= bbox[3]


def center_error(point: tuple[float, float] | None, bbox: list[float]) -> float | None:
    if point is None:
        return None
    center_x = (bbox[0] + bbox[2]) / 2
    center_y = (bbox[1] + bbox[3]) / 2
    return round(((point[0] - center_x) ** 2 + (point[1] - center_y) ** 2) ** 0.5, 6)


def normalize_coordinate(
    point: tuple[float, float] | None, image_size: list[int]
) -> tuple[float, float] | None:
    if point is None:
        return None
    x, y = point
    if 0 <= x <= 1 and 0 <= y <= 1:
        return point
    width, height = image_size
    if width <= 0 or height <= 0 or not (0 <= x < width and 0 <= y < height):
        return None
    return x / width, y / height


def contains_coordinate_fields(value: Any) -> bool:
    if isinstance(value, dict):
        forbidden = {"x", "y", "position", "bbox", "bounds", "click_x", "click_y"}
        if any(str(key).lower() in forbidden for key in value):
            return True
        return any(contains_coordinate_fields(item) for item in value.values())
    if isinstance(value, list):
        return any(contains_coordinate_fields(item) for item in value)
    return False


def semantic_plan(text: str) -> dict[str, Any] | None:
    for value in reversed(extract_json_objects(text)):
        if not isinstance(value, dict):
            continue
        steps = value.get("steps")
        if not value.get("goal") or not isinstance(steps, list) or not steps:
            continue
        valid = all(
            isinstance(step, dict)
            and bool(step.get("operation"))
            and bool(step.get("expected_state"))
            and not contains_coordinate_fields(step)
            for step in steps
        )
        if valid:
            return value
    return None


def run_process(command: list[str], timeout_seconds: int) -> dict[str, Any]:
    started = time.monotonic()
    try:
        completed = subprocess.run(
            command,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout_seconds,
            check=False,
        )
        return {
            "command": command,
            "exit_code": completed.returncode,
            "timed_out": False,
            "duration_seconds": round(time.monotonic() - started, 3),
            "stdout": completed.stdout,
            "stderr": completed.stderr,
        }
    except subprocess.TimeoutExpired as error:
        return {
            "command": command,
            "exit_code": None,
            "timed_out": True,
            "duration_seconds": round(time.monotonic() - started, 3),
            "stdout": (error.stdout or "") if isinstance(error.stdout, str) else "",
            "stderr": (error.stderr or "") if isinstance(error.stderr, str) else "",
        }


def write_process_log(report_dir: Path, name: str, result: dict[str, Any]) -> None:
    payload = [
        f"COMMAND: {json.dumps(result['command'])}",
        f"EXIT_CODE: {result['exit_code']}",
        f"TIMED_OUT: {result['timed_out']}",
        f"DURATION_SECONDS: {result['duration_seconds']}",
        "",
        "--- STDOUT ---",
        result["stdout"],
        "",
        "--- STDERR ---",
        result["stderr"],
    ]
    (report_dir / f"{name}.log").write_text("\n".join(payload), encoding="utf-8")


def offload_evidence(result: dict[str, Any]) -> dict[str, Any]:
    text = f"{result['stdout']}\n{result['stderr']}"
    matches = OFFLOAD.findall(text)
    if matches:
        offloaded, total = matches[-1]
        return {"detected": int(offloaded) > 0, "offloaded_layers": int(offloaded), "total_layers": int(total)}
    lower = text.lower()
    return {
        "detected": "vulkan" in lower and any(token in lower for token in ("amd", "radeon", "w6800")),
        "offloaded_layers": None,
        "total_layers": None,
    }


def model_command(
    llama_cli: Path,
    model: dict[str, Any],
    image: Path,
    prompt: str,
    tokens: int,
) -> list[str]:
    return [
        str(llama_cli),
        "-m",
        model["model_path"],
        "--mmproj",
        model["mmproj_path"],
        "--image",
        str(image),
        "-p",
        prompt,
        "-n",
        str(tokens),
        "-c",
        "4096",
        "-ngl",
        "all",
        "--temp",
        "0",
        "--top-k",
        "1",
        "--seed",
        "42",
        "--image-max-tokens",
        "1024",
        "--verbose",
        "--no-display-prompt",
        "--single-turn",
    ]


def fixture_map(expected: dict[str, Any]) -> dict[str, dict[str, Any]]:
    if "fixtures" in expected:
        return expected["fixtures"]
    image_size = expected["image_size"]
    return {
        name: {**value, "image_size": image_size}
        for name, value in expected.items()
        if name != "image_size"
    }


def acceptance_tests(fixtures: dict[str, dict[str, Any]]) -> list[dict[str, Any]]:
    unique = sorted(name for name in fixtures if name.startswith("unique"))
    same_label = sorted(name for name in fixtures if name.startswith("same_label"))
    tests: list[dict[str, Any]] = [
        {
            "id": "QWEN-01",
            "model": "qwen",
            "fixture": unique[0],
            "tokens": 320,
            "prompt": (
                "Analyze this deterministic desktop screenshot. Return only one JSON object with keys "
                "goal, application_id, summary, and steps. steps must be an array with one object containing "
                "operation, target, and expected_state. Plan how to activate the DOWNLOAD MODELS button. "
                "This is a semantic plan: do not include coordinates, x, y, bounding boxes, or position."
            ),
            "validator": "semantic_plan",
        }
    ]
    for name in unique + same_label:
        target = fixtures[name]["target"]
        tests.append(
            {
                "id": f"SHOWUI-{name.replace('_', '-').upper()}",
                "model": "showui",
                "fixture": name,
                "tokens": 48,
                "prompt": (
                    "Based on the screenshot of the page, I give a text description and you give its "
                    "corresponding location. The coordinate represents a clickable location [x, y] for an "
                    "element, which is a relative coordinate on the screenshot, scaled from 0 to 1. "
                    f"{target}"
                ),
                "validator": "coordinate",
                "profile": "official_normalized",
            }
        )
    return tests


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--llama-cli", type=Path, required=True)
    parser.add_argument("--models-manifest", type=Path, required=True)
    parser.add_argument("--fixtures-dir", type=Path, required=True)
    parser.add_argument("--report-dir", type=Path, required=True)
    parser.add_argument("--capabilities-out", type=Path)
    parser.add_argument("--git-commit", default="unknown")
    parser.add_argument("--timeout-seconds", type=int, default=1800)
    args = parser.parse_args()
    args.report_dir.mkdir(parents=True, exist_ok=True)
    manifest = json.loads(args.models_manifest.read_text(encoding="utf-8"))
    expected = json.loads((args.fixtures_dir / "expected.json").read_text(encoding="utf-8"))
    fixtures = fixture_map(expected)

    device = run_process([str(args.llama_cli), "--list-devices"], 60)
    write_process_log(args.report_dir, "00-devices", device)
    device_text = f"{device['stdout']}\n{device['stderr']}".lower()
    amd_vulkan = (
        device["exit_code"] == 0
        and "vulkan" in device_text
        and any(token in device_text for token in ("amd", "radeon", "wx 9100"))
    )

    tests = acceptance_tests(fixtures)

    results: list[dict[str, Any]] = []
    for test in tests:
        fixture = fixtures[test["fixture"]]
        command = model_command(
            args.llama_cli,
            manifest["models"][test["model"]],
            args.fixtures_dir / fixture["file"],
            test["prompt"],
            test["tokens"],
        )
        print(f"[RUN] {test['id']} ({test['model']})", flush=True)
        process = run_process(command, args.timeout_seconds)
        write_process_log(args.report_dir, test["id"].lower(), process)
        combined = f"{process['stdout']}\n{process['stderr']}"
        generated = process["stdout"].strip() or combined
        validation: dict[str, Any]
        if test["validator"] == "semantic_plan":
            plan = semantic_plan(generated) or semantic_plan(combined)
            validation = {"type": "semantic_plan", "valid": plan is not None, "parsed": plan}
        else:
            raw_point = extract_coordinate(generated)
            if raw_point is None:
                raw_point = extract_coordinate(combined)
            point = normalize_coordinate(raw_point, fixture["image_size"])
            valid = point_in_bbox(point, fixture["bbox_normalized"])
            validation = {
                "type": "coordinate",
                "valid": valid,
                "point": list(point) if point else None,
                "raw_point": list(raw_point) if raw_point else None,
                "expected_bbox": fixture["bbox_normalized"],
                "center_error": center_error(point, fixture["bbox_normalized"]),
                "profile": test.get("profile"),
            }
        offload = offload_evidence(process)
        passed = (
            process["exit_code"] == 0
            and not process["timed_out"]
            and validation["valid"]
            and offload["detected"]
        )
        failure_reason = None
        if process["timed_out"]:
            failure_reason = "inference timed out"
        elif process["exit_code"] != 0:
            failure_reason = f"llama.cpp exited with {process['exit_code']}"
        elif not offload["detected"]:
            failure_reason = "GPU offload was not detected"
        elif not validation["valid"]:
            failure_reason = "model output did not satisfy the strict validator"
        results.append(
            {
                "id": test["id"],
                "model": test["model"],
                "passed": passed,
                "duration_seconds": process["duration_seconds"],
                "meets_30_second_stage_target": process["duration_seconds"] <= 30,
                "exit_code": process["exit_code"],
                "timed_out": process["timed_out"],
                "gpu_offload": offload,
                "validation": validation,
                "model_output": generated,
                "failure_reason": failure_reason,
                "log": f"{test['id'].lower()}.log",
            }
        )

    qwen_ready = all(item["passed"] for item in results if item["model"] == "qwen")
    showui_ready = all(item["passed"] for item in results if item["model"] == "showui")
    capabilities = {
        "schema_version": 1,
        "git_commit": args.git_commit,
        "llama_build": manifest["llama_build"],
        "amd_vulkan_device_detected": amd_vulkan,
        "models": {
            "qwen": {"ready": qwen_ready},
            "showui": {"ready": showui_ready},
        },
    }

    report = {
        "schema_version": 1,
        "backend": "llama.cpp-vulkan",
        "llama_build": manifest["llama_build"],
        "git_commit": args.git_commit,
        "amd_vulkan_device_detected": amd_vulkan,
        "device_probe_exit_code": device["exit_code"],
        "models": {
            key: {
                "display_name": value["display_name"],
                "source_revision": value["source_revision"],
                "model": value["model"],
                "mmproj": value["mmproj"],
            }
            for key, value in manifest["models"].items()
        },
        "tests": results,
        "capabilities": capabilities["models"],
        "passed": amd_vulkan and qwen_ready and showui_ready,
        "notes": [
            "The 30-second field is an SLA observation; cold model loading is included in this first-pass probe.",
            "No mouse or keyboard action is executed by this acceptance tool.",
        ],
    }
    (args.report_dir / "summary.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    if args.capabilities_out:
        args.capabilities_out.parent.mkdir(parents=True, exist_ok=True)
        args.capabilities_out.write_text(json.dumps(capabilities, indent=2), encoding="utf-8")
    lines = [
        "Ale, My Eyes! Windows AMD model runtime acceptance",
        f"Overall: {'PASS' if report['passed'] else 'FAIL'}",
        f"AMD Vulkan device: {'PASS' if amd_vulkan else 'FAIL'}",
        "",
    ]
    for result in results:
        lines.append(
            f"{result['id']}: {'PASS' if result['passed'] else 'FAIL'} "
            f"({result['duration_seconds']}s, GPU={result['gpu_offload']['detected']})"
        )
    (args.report_dir / "SUMMARY.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")
    print("\n".join(lines), flush=True)
    return 0 if report["passed"] else 2


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"[FAILED] {error}", file=sys.stderr)
        raise SystemExit(1)
