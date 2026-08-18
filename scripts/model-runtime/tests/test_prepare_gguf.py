import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


MODULE_PATH = Path(__file__).parents[1] / "prepare_gguf.py"
SPEC = importlib.util.spec_from_file_location("prepare_gguf", MODULE_PATH)
assert SPEC and SPEC.loader
PREPARE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = PREPARE
SPEC.loader.exec_module(PREPARE)


class PrepareGgufTests(unittest.TestCase):
    def test_source_requires_matching_revision_and_architecture(self):
        spec = PREPARE.MODELS[0]
        with tempfile.TemporaryDirectory() as directory:
            model = Path(directory) / spec.directory
            model.mkdir()
            (model / "config.json").write_text(
                json.dumps({"architectures": [spec.architectures[0]]}), encoding="utf-8"
            )
            (model / ".ale-revision").write_text(spec.revision, encoding="ascii")
            (model / "weights.safetensors").write_bytes(b"weights")
            self.assertEqual(PREPARE.validate_source(Path(directory), spec), model)
            (model / ".ale-revision").write_text("wrong", encoding="ascii")
            with self.assertRaises(RuntimeError):
                PREPARE.validate_source(Path(directory), spec)

    def test_source_accepts_pytorch_bin_weights(self):
        spec = PREPARE.MODELS[1]
        with tempfile.TemporaryDirectory() as directory:
            model = Path(directory) / spec.directory
            model.mkdir()
            (model / "config.json").write_text(
                json.dumps({"architectures": [spec.architectures[0]]}), encoding="utf-8"
            )
            (model / ".ale-revision").write_text(spec.revision, encoding="ascii")
            (model / "pytorch_model.bin").write_bytes(b"weights")
            self.assertEqual(PREPARE.validate_source(Path(directory), spec), model)
            self.assertEqual(PREPARE.source_size(model), len(b"weights"))

    def test_completed_artifact_rejects_hash_mismatch(self):
        spec = PREPARE.MODELS[1]
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            model = output / "model-q4_k_m.gguf"
            mmproj = output / "mmproj-model-f16.gguf"
            model.write_bytes(b"model")
            mmproj.write_bytes(b"projector")
            marker = {
                "source_revision": spec.revision,
                "model": {
                    "file": model.name,
                    "size": model.stat().st_size,
                    "sha256": PREPARE.sha256(model),
                },
                "mmproj": {
                    "file": mmproj.name,
                    "size": mmproj.stat().st_size,
                    "sha256": PREPARE.sha256(mmproj),
                },
            }
            (output / "conversion.json").write_text(json.dumps(marker), encoding="utf-8")
            self.assertIsNotNone(PREPARE.completed_artifact(output, spec))
            model.write_bytes(b"other")
            self.assertIsNone(PREPARE.completed_artifact(output, spec))

    def test_mmproj_conversion_uses_a_distinct_output_path(self):
        spec = PREPARE.MODELS[0]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            output = root / "output"
            commands: list[list[str]] = []

            def fake_run(command, _log_path, _env):
                commands.append(command)
                if command[-1] == "Q4_K_M":
                    Path(command[2]).write_bytes(b"quantized")
                    return
                destination = Path(command[command.index("--outfile") + 1])
                destination.write_bytes(b"projector" if "--mmproj" in command else b"model")

            with patch.object(PREPARE, "run_logged", side_effect=fake_run):
                PREPARE.convert_model(
                    Path("python"),
                    Path("converter.py"),
                    Path("quantize"),
                    source,
                    output,
                    spec,
                    "test-build",
                )

            language_output = Path(commands[0][commands[0].index("--outfile") + 1])
            projector_output = Path(commands[1][commands[1].index("--outfile") + 1])
            self.assertNotEqual(language_output, projector_output)
            self.assertEqual(projector_output.name, "mmproj-model-f16.gguf")


if __name__ == "__main__":
    unittest.main()
