"""The tokenizer boundary never substitutes a mutable revision or model weights."""
import importlib.util
import sys
import types
import unittest
from pathlib import Path
from unittest import mock


class SnapshotTests(unittest.TestCase):
    def setUp(self):
        self.api = mock.Mock()
        self.hub = types.ModuleType("huggingface_hub")
        self.hub.HfApi = mock.Mock(return_value=self.api)
        self.hub.snapshot_download = mock.Mock()
        module_spec = importlib.util.spec_from_file_location("thalamus_snapshot", Path(__file__).with_name("snapshot.py"))
        self.snapshot = importlib.util.module_from_spec(module_spec)
        with mock.patch.dict(sys.modules, {"huggingface_hub": self.hub}):
            module_spec.loader.exec_module(self.snapshot)
        self.revision = "a" * 40
        self.api.model_info.return_value.sha = self.revision
        self.api.list_repo_tree.return_value = [
            types.SimpleNamespace(path="tokenizer.json", size=100),
            types.SimpleNamespace(path="tokenizer_config.json", size=20),
            types.SimpleNamespace(path="model.safetensors", size=999999999),
            types.SimpleNamespace(path="tokenization_remote.py", size=10),
        ]

    def run_snapshot(self, revision=None):
        with mock.patch.object(sys, "argv", ["snapshot.py", "org/model", revision or self.revision, "/tmp/work"]):
            return self.snapshot.main()

    def test_exact_commit_and_tokenizer_only(self):
        self.assertEqual(self.run_snapshot(), 0)
        self.hub.HfApi.assert_called_once_with(endpoint="https://huggingface.co", token=False)
        self.api.model_info.assert_called_once_with("org/model", revision=self.revision)
        self.api.list_repo_tree.assert_called_once_with("org/model", revision=self.revision, recursive=False)
        kwargs = self.hub.snapshot_download.call_args.kwargs
        self.assertEqual(kwargs["revision"], self.revision)
        self.assertEqual(kwargs["repo_id"], "org/model")
        self.assertIs(kwargs["token"], False)
        self.assertEqual(set(kwargs["allow_patterns"]), {"tokenizer.json", "tokenizer_config.json"})
        self.assertEqual(kwargs["local_dir"], "/tmp/work/tokenizer")

    def test_rejects_mutable_mismatched_or_oversize_snapshots(self):
        self.assertEqual(self.run_snapshot("main"), 1)
        self.api.model_info.return_value.sha = "b" * 40
        self.assertEqual(self.run_snapshot(), 1)
        self.api.model_info.return_value.sha = self.revision
        self.api.list_repo_tree.return_value = [types.SimpleNamespace(path="tokenizer.json", size=self.snapshot.MAX_BYTES + 1)]
        self.assertEqual(self.run_snapshot(), 1)
        self.hub.snapshot_download.assert_not_called()

    def test_empty_or_missing_file_sizes_fail_closed(self):
        for items in ([], [types.SimpleNamespace(path="tokenizer_config.json", size=3)],
                      [types.SimpleNamespace(path="tokenizer.json", size=None)]):
            self.api.list_repo_tree.return_value = items
            with self.subTest(items=items):
                self.assertEqual(self.run_snapshot(), 1)
        self.hub.snapshot_download.assert_not_called()


if __name__ == "__main__":
    unittest.main()
