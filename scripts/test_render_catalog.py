"""Consumer-facing catalog publisher tests using the real KCL recipes.

Run from the repository root with Python 3 and KCL 0.13.2 on PATH:
    python3 -m unittest discover -s scripts -p 'test_render_catalog.py'
The tests render copies of the checked-in illustrative catalog in temporary
worktrees; they do not require a cluster or alter the checkout's output.
"""

import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent.parent


def invoke(root, *arguments):
    return subprocess.run(
        [sys.executable, "-I", str(ROOT / "scripts/render-catalog.py"), *arguments],
        cwd=root, capture_output=True, text=True, timeout=600,
    )


def snapshot(root):
    result = {}
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix()
        if path.is_dir():
            result[relative + "/"] = None
        elif path.is_file():
            result[relative] = path.read_bytes()
    return result


def generated_objects(root):
    return [json.loads(path.read_text(encoding="utf-8")) for path in sorted(root.glob("*.json"))]


class CatalogPublicationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.fixture = tempfile.TemporaryDirectory(prefix="thalamus-publisher-fixture-")
        cls.base = Path(cls.fixture.name) / "repository"
        cls.base.mkdir()
        for directory in ("catalog", "templates"):
            shutil.copytree(ROOT / directory, cls.base / directory)
        for name in ("main.k", "limits.k", "pool.k", "kcl.mod", "kcl.mod.lock"):
            shutil.copy2(ROOT / name, cls.base / name)
        (cls.base / "scripts").mkdir()
        shutil.copy2(ROOT / "scripts/render-catalog.py", cls.base / "scripts/render-catalog.py")
        initial = invoke(cls.base, "--write")
        if initial.returncode != 0:
            cls.fixture.cleanup()
            raise AssertionError(f"real fixture render failed:\n{initial.stderr}")
        cls.initial_list = json.loads(initial.stdout)

    @classmethod
    def tearDownClass(cls):
        cls.fixture.cleanup()

    def setUp(self):
        self.work = tempfile.TemporaryDirectory(prefix="thalamus-publisher-test-")
        self.root = Path(self.work.name) / "repository"
        shutil.copytree(self.base, self.root)
        self.rendered = self.root / "rendered"
        self.before = snapshot(self.rendered)

    def tearDown(self):
        self.work.cleanup()

    def success(self, *arguments):
        result = invoke(self.root, *arguments)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def failure(self, *arguments):
        result = invoke(self.root, *arguments)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual(result.stdout, "")
        return result

    def test_full_tree_check_write_and_prune(self):
        inference = self.rendered / "inference"
        pools = self.rendered / "pools"
        inference_objects = generated_objects(inference)
        pool_objects = generated_objects(pools)
        objects = inference_objects + pool_objects
        actual = sorted(json.dumps(item, sort_keys=True) for item in objects)
        combined = self.initial_list
        self.assertEqual((combined["apiVersion"], combined["kind"]), ("v1", "List"))
        self.assertEqual(actual, sorted(json.dumps(item, sort_keys=True) for item in combined["items"]))
        for directory in (inference, pools):
            kustomization = json.loads((directory / "kustomization.yaml").read_text())
            self.assertEqual(kustomization["resources"], sorted(path.name for path in directory.glob("*.json")))
        self.assertEqual(sum(item["kind"] == "Service" and item["metadata"]["name"] == "example-ratelimit" for item in objects), 1)
        # The example's pooled workloads and their LocalQueues must be in the
        # ordinary inventory; the selected ClusterQueues and ResourceFlavor
        # must be in the admin inventory and describe the same topology.
        namespace = "thalamus-inference"
        self.assertTrue(all(item["metadata"].get("namespace") == namespace for item in inference_objects))
        self.assertTrue(all("namespace" not in item["metadata"] for item in pool_objects))
        local_queues = {item["metadata"]["name"]: item for item in inference_objects if item["kind"] == "LocalQueue"}
        cluster_queues = {item["metadata"]["name"]: item for item in pool_objects if item["kind"] == "ClusterQueue"}
        flavors = {item["metadata"]["name"]: item for item in pool_objects if item["kind"] == "ResourceFlavor"}
        workloads = [
            item for item in inference_objects
            if item["kind"] == "Deployment"
            and "kueue.x-k8s.io/queue-name" in item["spec"]["template"]["metadata"]["labels"]
        ]
        self.assertTrue(local_queues and cluster_queues and flavors and workloads)
        self.assertEqual(
            set(local_queues),
            {item["spec"]["template"]["metadata"]["labels"]["kueue.x-k8s.io/queue-name"] for item in workloads},
        )
        for workload in workloads:
            queue_name = workload["spec"]["template"]["metadata"]["labels"]["kueue.x-k8s.io/queue-name"]
            local_queue = local_queues[queue_name]
            cluster_queue = cluster_queues[local_queue["spec"]["clusterQueue"]]
            selector = cluster_queue["spec"]["namespaceSelector"]
            matches_label = selector.get("matchLabels", {}).get("kubernetes.io/metadata.name") == namespace
            matches_expression = any(
                expr.get("key") == "kubernetes.io/metadata.name"
                and expr.get("operator") == "In"
                and namespace in expr.get("values", [])
                for expr in selector.get("matchExpressions", [])
            )
            self.assertTrue(matches_label or matches_expression)
            flavor_names = [
                flavor["name"]
                for group in cluster_queue["spec"]["resourceGroups"]
                for flavor in group["flavors"]
            ]
            self.assertTrue(flavor_names)
            for name in flavor_names:
                self.assertEqual(
                    workload["spec"]["template"]["spec"]["nodeSelector"],
                    flavors[name]["spec"]["nodeLabels"],
                )
        checkout_before = snapshot(self.root)
        self.success("--check")
        self.assertEqual(snapshot(self.root), checkout_before)
        self.assertEqual(snapshot(self.rendered), self.before)
        local = subprocess.run(
            [sys.executable, "scripts/render-catalog.py", "--check"],
            cwd=self.root, capture_output=True, text=True, timeout=600,
        )
        self.assertEqual(local.returncode, 0, local.stderr)
        self.assertEqual(snapshot(self.root), checkout_before)

        # Missing, changed, additional files and even empty stale directories
        # are drift; the non-mutating check must preserve that exact state.
        manifest = next(inference.glob("*.json"))
        manifest.write_text("{}\n", encoding="utf-8")
        (pools / "stale.json").write_text("{}\n", encoding="utf-8")
        (inference / "stale-directory").mkdir()
        (pools / "kustomization.yaml").unlink()
        (inference / "kustomization.yaml").chmod(0o755)
        drifted = snapshot(self.rendered)
        result = self.failure("--check")
        self.assertIn("out of date", result.stderr)
        self.assertIn("changed rendered/inference/kustomization.yaml", result.stderr)
        self.assertEqual(snapshot(self.rendered), drifted)
        self.assertTrue((inference / "stale-directory").is_dir())
        rewritten = self.success("--write")
        self.assertEqual(json.loads(rewritten.stdout), combined)
        self.assertEqual(snapshot(self.rendered), self.before)
        self.assertFalse((inference / "stale-directory").exists())
        self.assertFalse((inference / "kustomization.yaml").stat().st_mode & 0o111)

        # Unreferenced shared profiles and pools, and nested .yml services,
        # are catalog inputs too, not just dependencies of the example service.
        examples = (
            ("services", "example-service.yaml", "additional.yml",
             (("example-service", "additional-service"), ("example-deployment", "additional-deployment"), ("inference.example.invalid", "additional.example.invalid"))),
            ("limits", "example-shared.yaml", "additional.yml",
             (("example-shared", "additional-shared"), ("example-ratelimit", "additional-ratelimit"))),
            ("pools", "example-pool.yaml", "additional.yml",
             (("example-pool", "additional-pool"),)),
        )
        added_sources = []
        for category, example, filename, replacements in examples:
            source = self.root / "catalog" / category / example
            text = source.read_text(encoding="utf-8")
            for old, new in replacements:
                text = text.replace(old, new)
            nested = source.parent / "nested"
            nested.mkdir()
            added = nested / filename
            added.write_text(text, encoding="utf-8")
            added_sources.append(added)
        expanded = json.loads(self.success("--write").stdout)["items"]
        self.assertTrue(any(item["kind"] == "HTTPRoute" and item["metadata"]["name"] == "additional-service" for item in expanded))
        self.assertTrue(any(item["kind"] == "Service" and item["metadata"]["name"] == "additional-ratelimit" for item in expanded))
        self.assertTrue(any(item["kind"] == "ClusterQueue" and item["metadata"]["name"] == "additional-pool-filler" for item in expanded))
        self.assertEqual(sum(item["kind"] == "Service" and item["metadata"]["name"] == "example-ratelimit" for item in expanded), 1)
        for source in added_sources:
            source.unlink()

        # Removing reviewed inputs removes their objects, including every
        # cluster pool resource, rather than preserving the previous output.
        (self.root / "catalog/services/example-pool-service.yaml").unlink()
        (self.root / "catalog/pools/example-pool.yaml").unlink()
        reduced = json.loads(self.success("--write").stdout)
        self.assertFalse(any(item["kind"] == "HTTPRoute" and item["metadata"]["name"] == "example-pool-service" for item in reduced["items"]))
        self.assertFalse(any(item["metadata"]["name"] in ("additional-service", "additional-ratelimit", "additional-pool-filler") for item in reduced["items"]))
        self.assertTrue(any(item["kind"] == "HTTPRoute" and item["metadata"]["name"] == "example-service" for item in reduced["items"]))
        self.assertEqual(generated_objects(pools), [])
        self.assertEqual(json.loads((pools / "kustomization.yaml").read_text())["resources"], [])
        self.success("--check")

    def test_invalid_sources_and_collisions_preserve_both_trees(self):
        service = self.root / "catalog/services/example-service.yaml"
        original = service.read_text(encoding="utf-8")
        service.write_text(original.replace("rateLimit: example-shared", "rateLimit: missing-profile"), encoding="utf-8")
        self.failure("--write")
        self.assertEqual(snapshot(self.rendered), self.before)
        service.write_text(original.replace("contextLength: 4096", "contextLength: -1"), encoding="utf-8")
        self.failure("--write")
        self.assertEqual(snapshot(self.rendered), self.before)
        service.write_text(original, encoding="utf-8")
        duplicate = service.with_name("duplicate.yaml")
        shutil.copy2(service, duplicate)
        result = self.failure("--write")
        self.assertIn("duplicate Kubernetes object", result.stderr)
        self.assertEqual(snapshot(self.rendered), self.before)
        duplicate.unlink()
        self.failure("--write", "--workload-namespace", "other-namespace")
        self.assertEqual(snapshot(self.rendered), self.before)
        for category in ("services", "limits", "pools"):
            for source in (self.root / "catalog" / category).glob("*.yaml"):
                source.write_text(
                    source.read_text(encoding="utf-8").replace("thalamus-inference", "other-namespace"),
                    encoding="utf-8",
                )
        custom = json.loads(self.success("--write", "--workload-namespace", "other-namespace").stdout)
        self.assertTrue(all(
            item["metadata"].get("namespace") in (None, "other-namespace")
            for item in custom["items"]
        ))
        self.success("--check", "--workload-namespace", "other-namespace")

    def test_rejects_path_traversal_and_symlinks_without_touching_targets(self):
        service = self.root / "catalog/services/example-service.yaml"
        original = service.read_text(encoding="utf-8")
        outside = self.root / "outside.yaml"
        outside.write_text("SECRET-OUTSIDE-CATALOG\n", encoding="utf-8")
        service.write_text(original.replace("rateLimit: example-shared", "rateLimit: ../../outside"), encoding="utf-8")
        result = self.failure("--write")
        self.assertIn("validating", result.stderr)
        self.assertNotIn("SECRET-OUTSIDE-CATALOG", result.stderr)
        self.assertEqual(snapshot(self.rendered), self.before)
        service.write_text(
            original.replace("namespace: thalamus-inference\n", "namespace: thalamus-inference\npool: ../../outside\n", 1),
            encoding="utf-8",
        )
        result = self.failure("--write")
        self.assertIn("validating", result.stderr)
        self.assertNotIn("SECRET-OUTSIDE-CATALOG", result.stderr)
        self.assertEqual(snapshot(self.rendered), self.before)
        service.write_text(original, encoding="utf-8")
        source_link = service.with_name("escape.yaml")
        source_link.symlink_to(outside)
        self.assertIn("symlink", self.failure("--write").stderr)
        self.assertEqual(snapshot(self.rendered), self.before)
        source_link.unlink()

        output_link = self.rendered / "inference/escape.json"
        output_link.symlink_to(outside)
        self.assertIn("symlink", self.failure("--check").stderr)
        self.assertIn("symlink", self.failure("--write").stderr)
        self.assertEqual(outside.read_text(encoding="utf-8"), "SECRET-OUTSIDE-CATALOG\n")
        output_link.unlink()
        self.assertEqual(snapshot(self.rendered), self.before)

        foreign = Path(self.work.name) / "foreign"
        foreign.mkdir()
        sentinel = foreign / "keep.txt"
        sentinel.write_text("untouched", encoding="utf-8")
        inference = self.rendered / "inference"
        saved = self.rendered / "saved-inference"
        inference.rename(saved)
        inference.symlink_to(foreign, target_is_directory=True)
        self.assertIn("symlink", self.failure("--write").stderr)
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "untouched")
        inference.unlink()
        saved.rename(inference)
        self.assertEqual(snapshot(self.rendered), self.before)

    def test_scope_is_checked_independently_of_object_namespace_field(self):
        entrypoint = self.root / "main.k"
        original = entrypoint.read_text(encoding="utf-8")
        marker = "items = _result.items"
        self.assertIn(marker, original)
        for extra, message in (
            ('{apiVersion: "scheduling.k8s.io/v1", kind: "PriorityClass", metadata: {name: "escape", namespace: "thalamus-inference"}, value: 1}', "incorrectly scoped"),
            ('{apiVersion: "v1", kind: "ConfigMap", metadata: {name: "escape", namespace: "foreign"}, data: {key: "value"}}', "foreign or missing namespace"),
        ):
            with self.subTest(extra=extra):
                entrypoint.write_text(original.replace(marker, marker + " + [" + extra + "]"), encoding="utf-8")
                self.assertIn(message, self.failure("--write").stderr)
                self.assertEqual(snapshot(self.rendered), self.before)


if __name__ == "__main__":
    unittest.main()
