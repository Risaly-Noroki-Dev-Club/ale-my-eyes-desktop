import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


MODULE_PATH = Path(__file__).parents[1] / "runtime_acceptance.py"
SPEC = importlib.util.spec_from_file_location("runtime_acceptance", MODULE_PATH)
assert SPEC and SPEC.loader
RUNTIME = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNTIME)


class RuntimeAcceptanceTests(unittest.TestCase):
    def test_coordinate_uses_last_normalized_pair(self):
        self.assertEqual(RUNTIME.extract_coordinate("noise [12, 9] answer [0.8, 0.7]"), (0.8, 0.7))

    def test_coordinate_accepts_parentheses_from_uitars_stdout(self):
        stdout = "(820,515)\n"
        verbose_stderr = "llama_context: preserved_tokens = [151657,151658]"
        self.assertEqual(RUNTIME.extract_coordinate(stdout), (820.0, 515.0))
        self.assertEqual(
            RUNTIME.extract_coordinate(stdout) or RUNTIME.extract_coordinate(verbose_stderr),
            (820.0, 515.0),
        )

    def test_point_must_be_inside_bbox(self):
        self.assertTrue(RUNTIME.point_in_bbox((0.8, 0.7), [0.7, 0.6, 0.9, 0.8]))
        self.assertFalse(RUNTIME.point_in_bbox((0.2, 0.7), [0.7, 0.6, 0.9, 0.8]))

    def test_pixel_coordinate_is_normalized_against_fixture_size(self):
        self.assertEqual(
            RUNTIME.normalize_coordinate((893, 549), [1280, 720]),
            (893 / 1280, 549 / 720),
        )
        self.assertIsNone(RUNTIME.normalize_coordinate((1280, 549), [1280, 720]))

    def test_model_command_enables_verbose_gpu_logs(self):
        command = RUNTIME.model_command(
            Path("llama-cli"),
            {"model_path": "model.gguf", "mmproj_path": "mmproj.gguf"},
            Path("fixture.png"),
            "prompt",
            1,
        )
        self.assertIn("--verbose", command)

    def test_semantic_plan_rejects_coordinates(self):
        valid = '{"goal":"download","steps":[{"operation":"click","target":"button","expected_state":"dialog"}]}'
        invalid = '{"goal":"download","steps":[{"operation":"click","target":{"bbox":[0.7,0.6,0.9,0.8]},"expected_state":"dialog"}]}'
        self.assertIsNotNone(RUNTIME.semantic_plan(valid))
        self.assertIsNone(RUNTIME.semantic_plan(invalid))

    def test_offload_evidence_parses_layer_count(self):
        result = {"stdout": "offloaded 29/29 layers to GPU", "stderr": ""}
        self.assertEqual(
            RUNTIME.offload_evidence(result),
            {"detected": True, "offloaded_layers": 29, "total_layers": 29},
        )

    def test_main_writes_passing_report_for_valid_real_process_shapes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixtures = root / "fixtures"
            report = root / "report"
            fixtures.mkdir()
            expected = {
                "image_size": [1280, 720],
                "unique": {
                    "file": "unique.png",
                    "bbox_normalized": [0.68, 0.77, 0.91, 0.89],
                },
                "ambiguous": {
                    "file": "ambiguous.png",
                    "bbox_normalized": [0.63, 0.72, 0.76, 0.81],
                },
            }
            (fixtures / "expected.json").write_text(json.dumps(expected), encoding="utf-8")
            model_data = {
                "display_name": "test",
                "source_revision": "a" * 40,
                "model": {"file": "model.gguf", "size": 1, "sha256": "b" * 64},
                "mmproj": {"file": "mmproj.gguf", "size": 1, "sha256": "c" * 64},
                "model_path": str(root / "model.gguf"),
                "mmproj_path": str(root / "mmproj.gguf"),
            }
            manifest = root / "manifest.json"
            manifest.write_text(
                json.dumps(
                    {
                        "llama_build": "test-build",
                        "models": {key: dict(model_data) for key in ("qwen", "showui", "uitars")},
                    }
                ),
                encoding="utf-8",
            )

            def result(stdout: str, stderr: str = "offloaded 29/29 layers to GPU"):
                return {
                    "command": ["fake"],
                    "exit_code": 0,
                    "timed_out": False,
                    "duration_seconds": 3.0,
                    "stdout": stdout,
                    "stderr": stderr,
                }

            process_results = [
                result("Vulkan0: AMD Radeon Pro WX 9100", ""),
                result(
                    '{"goal":"download","steps":[{"operation":"click","target":"button",'
                    '"expected_state":"dialog opens"}]}'
                ),
                result("[0.80, 0.82]"),
                result("[0.70, 0.77]"),
            ]
            argv = [
                "runtime_acceptance.py",
                "--llama-cli",
                str(root / "llama-cli.exe"),
                "--models-manifest",
                str(manifest),
                "--fixtures-dir",
                str(fixtures),
                "--report-dir",
                str(report),
            ]
            with mock.patch.object(RUNTIME, "run_process", side_effect=process_results), mock.patch.object(
                sys, "argv", argv
            ):
                self.assertEqual(RUNTIME.main(), 0)
            summary = json.loads((report / "summary.json").read_text(encoding="utf-8"))
            self.assertTrue(summary["passed"])
            self.assertTrue(all(item["passed"] for item in summary["tests"]))
            self.assertNotIn("model_path", summary["models"]["qwen"])


if __name__ == "__main__":
    unittest.main()
