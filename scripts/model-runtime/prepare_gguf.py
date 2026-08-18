#!/usr/bin/env python3
"""Convert pinned Hugging Face VLM snapshots into restart-safe GGUF test artifacts."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path


GIB = 1024**3


@dataclass(frozen=True)
class ModelSpec:
    key: str
    directory: str
    revision: str
    architectures: tuple[str, ...]


MODELS = (
    ModelSpec(
        "qwen",
        "Qwen2.5-VL-7B-Instruct",
        "cc594898137f460bfe9f0759e9844b3ce807cfb5",
        ("Qwen2_5_VLForConditionalGeneration",),
    ),
    ModelSpec(
        "showui",
        "ShowUI-2B",
        "cabec4fcc48d15ffd3efe0b33ea9bc7d41509d60",
        ("Qwen2VLForConditionalGeneration",),
    ),
)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(8 * 1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def weight_files(path: Path) -> tuple[Path, ...]:
    return tuple(path.glob("*.safetensors")) + tuple(path.glob("pytorch_model*.bin"))


def source_size(path: Path) -> int:
    return sum(item.stat().st_size for item in weight_files(path))


def validate_source(root: Path, spec: ModelSpec) -> Path:
    model = root / spec.directory
    config_path = model / "config.json"
    if not config_path.is_file():
        raise RuntimeError(f"missing {config_path}")
    config = json.loads(config_path.read_text(encoding="utf-8"))
    architectures = tuple(config.get("architectures", ()))
    if not any(name in spec.architectures for name in architectures):
        raise RuntimeError(f"unexpected architecture for {spec.directory}: {architectures}")
    marker = model / ".ale-revision"
    if not marker.is_file() or marker.read_text(encoding="ascii").strip() != spec.revision:
        raise RuntimeError(f"pinned revision marker missing or incorrect for {spec.directory}")
    if source_size(model) == 0:
        raise RuntimeError(f"no supported Hugging Face weights found in {model}")
    return model


def run_logged(command: list[str], log_path: Path, env: dict[str, str]) -> None:
    with log_path.open("a", encoding="utf-8", errors="replace") as log:
        log.write(f"\n[{time.strftime('%Y-%m-%d %H:%M:%S')}] COMMAND: {json.dumps(command)}\n")
        log.flush()
        completed = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, env=env, check=False)
        log.write(f"[{time.strftime('%Y-%m-%d %H:%M:%S')}] EXIT: {completed.returncode}\n")
        log.flush()
    if completed.returncode != 0:
        raise RuntimeError(f"command failed with exit code {completed.returncode}; see {log_path}")


def completed_artifact(output: Path, spec: ModelSpec) -> dict[str, object] | None:
    marker = output / "conversion.json"
    if not marker.is_file():
        return None
    try:
        data = json.loads(marker.read_text(encoding="utf-8"))
        if data.get("source_revision") != spec.revision:
            return None
        for key in ("model", "mmproj"):
            artifact = data[key]
            path = output / artifact["file"]
            if not path.is_file() or path.stat().st_size != artifact["size"]:
                return None
            if sha256(path) != artifact["sha256"]:
                return None
        return data
    except (KeyError, OSError, TypeError, ValueError, json.JSONDecodeError):
        return None


def convert_model(
    python: Path,
    converter: Path,
    quantize: Path,
    source: Path,
    output_root: Path,
    spec: ModelSpec,
    llama_build: str,
) -> dict[str, object]:
    output = output_root / spec.directory
    existing = completed_artifact(output, spec)
    if existing is not None:
        print(f"[READY] {spec.directory} conversion hashes verified", flush=True)
        return existing

    stage = output_root / f".{spec.directory}.stage"
    if stage.exists():
        shutil.rmtree(stage)
    stage.mkdir(parents=True)
    log_path = output_root / f"{spec.directory}.conversion.log"
    f16 = stage / "model-f16.gguf"
    q4 = stage / "model-q4_k_m.gguf"
    mmproj = stage / "mmproj-model-f16.gguf"
    env = os.environ.copy()
    env.update(
        {
            "HF_HUB_OFFLINE": "1",
            "TRANSFORMERS_OFFLINE": "1",
            "TOKENIZERS_PARALLELISM": "false",
        }
    )
    print(f"[CONVERT] {spec.directory}: language model", flush=True)
    run_logged(
        [str(python), str(converter), str(source), "--outfile", str(f16), "--outtype", "f16"],
        log_path,
        env,
    )
    print(f"[CONVERT] {spec.directory}: multimodal projector", flush=True)
    run_logged(
        [
            str(python),
            str(converter),
            str(source),
            "--outfile",
            str(mmproj),
            "--outtype",
            "f16",
            "--mmproj",
        ],
        log_path,
        env,
    )
    if not mmproj.is_file():
        matches = list(stage.glob("mmproj-*.gguf"))
        if len(matches) != 1:
            raise RuntimeError(f"mmproj conversion produced an unexpected layout in {stage}")
        matches[0].replace(mmproj)
    print(f"[QUANTIZE] {spec.directory}: Q4_K_M", flush=True)
    run_logged([str(quantize), str(f16), str(q4), "Q4_K_M"], log_path, env)
    f16.unlink(missing_ok=True)

    data: dict[str, object] = {
        "key": spec.key,
        "display_name": spec.directory,
        "source_revision": spec.revision,
        "llama_build": llama_build,
        "model": {"file": q4.name, "size": q4.stat().st_size, "sha256": sha256(q4)},
        "mmproj": {"file": mmproj.name, "size": mmproj.stat().st_size, "sha256": sha256(mmproj)},
    }
    (stage / "conversion.json").write_text(json.dumps(data, indent=2), encoding="utf-8")
    if output.exists():
        shutil.rmtree(output)
    stage.replace(output)
    return data


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--models-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--llama-source", type=Path, required=True)
    parser.add_argument("--quantize", type=Path, required=True)
    parser.add_argument("--llama-build", required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    args = parser.parse_args()
    converter = args.llama_source / "convert_hf_to_gguf.py"
    if not converter.is_file() or not args.quantize.is_file():
        raise RuntimeError("llama.cpp converter or quantizer is missing")
    sources = [(spec, validate_source(args.models_dir, spec)) for spec in MODELS]
    largest = max(source_size(path) for _, path in sources)
    required = largest * 2 + 8 * GIB
    args.output_dir.mkdir(parents=True, exist_ok=True)
    free = shutil.disk_usage(args.output_dir).free
    if free < required:
        raise RuntimeError(
            f"insufficient conversion space: need {required / GIB:.1f} GiB, have {free / GIB:.1f} GiB"
        )

    manifest: dict[str, object] = {"schema_version": 1, "llama_build": args.llama_build, "models": {}}
    for spec, source in sources:
        data = convert_model(
            Path(sys.executable), converter, args.quantize, source, args.output_dir, spec, args.llama_build
        )
        output = args.output_dir / spec.directory
        manifest["models"][spec.key] = {
            **data,
            "model_path": str((output / data["model"]["file"]).resolve()),
            "mmproj_path": str((output / data["mmproj"]["file"]).resolve()),
        }
    args.manifest.parent.mkdir(parents=True, exist_ok=True)
    args.manifest.write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    print(f"[READY] GGUF manifest: {args.manifest}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"[FAILED] {error}", file=sys.stderr)
        raise SystemExit(1)
