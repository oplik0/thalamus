"""Boundary tests for the pinned upstream report and restricted runner contract.

Run with: python -m unittest discover -s scripts/inference-perf -p 'test_*.py'
The successful report fields below are copied from the v0.7.0 source tree's
``demos/kubecon-na-2025/vllm/results/stage_0_lifecycle_metrics.json``. The
added goodput block uses the shape produced by v0.7.0 reportgen/base.py.
"""
import copy
import importlib.util
import json
import os
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).parent
spec = importlib.util.spec_from_file_location("thalamus_inference_adapter", HERE / "adapter.py")
adapter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adapter)
simulator_spec = importlib.util.spec_from_file_location("thalamus_inference_simulator", HERE / "simulator.py")
simulator = importlib.util.module_from_spec(simulator_spec)
simulator_spec.loader.exec_module(simulator)

RUN_ID = "00000000-0000-4000-8000-000000000001"
STAGE = {"rate": 1, "duration": 60}
# Metrics except the illustrative goodput block are from the upstream stage report.
REPORT = {
    "load_summary": {"count": 60, "send_duration": 58.51001774193719,
                     "requested_rate": 1.0, "achieved_rate": 1.0254654214024423},
    "successes": {"count": 60,
                  "latency": {"time_to_first_token": {"median": 0.031026913959067315, "p95": 0.03623536097584292},
                              "inter_token_latency": {"median": 3.718049265444279e-05, "p95": 0.007165764196543023}},
                  "throughput": {"requests_per_sec": 1.0171575756131475, "output_tokens_per_sec": 64.23350089997027},
                  "output_len": {"mean": 63.15}, "goodput_metrics": {"request_goodput": 0.75, "good_requests": 45}},
    "failures": {"count": 0},
}


def inputs():
    workload = {"load": {"type": "constant", "stages": [{"rate": 2, "duration": 4}]},
                "data": {"type": "random", "input_len": 32, "output_len": 8},
                "goodput": {"ttft": 0.25, "itl": 0.05}}
    config = {"base_url": "https://example.test/candidates/candidate/v1", "model": "the-model",
              "benchmark": {"tokenizer": {"repository": "org/tokenizer", "revision": "a" * 40},
                            "workloads": {"test": workload}}}
    request = {"project_id": "00000000-0000-4000-8000-000000000002", "kind": "benchmark", "service": "service",
               "deployment": "candidate", "runner": "benchmark", "workload": {"name": "test", "seed": 42,
               "concurrency": 4, "duration_seconds": 20, "warmup_seconds": 1, "max_requests": 9,
               "model_budget_tokens": 72, "tool_budget_calls": 0},
               "evidence": {"desired_configuration": "desired", "observed_runtime": "observed", "test_definition": "test",
                            "conditions": {"hardware": "simulator", "nodes": ["node"], "replicas": 1,
                                           "cache_state": "cold", "competing_traffic": "none", "observed_at": "2026-01-01T00:00:00Z"}}}
    return config, request


def parse(report):
    return adapter.parse_report(json.dumps(report), STAGE, "constant", 60)


class SimulatorEnvelopeTests(unittest.TestCase):
    def test_observation_time_uses_chrono_canonical_utc(self):
        self.assertRegex(
            simulator.observed_time(),
            r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$",
        )


class ReportTests(unittest.TestCase):
    def test_real_upstream_report_mapping(self):
        point = parse(REPORT)
        self.assertEqual(point["stage"], {"type": "constant", **STAGE})
        self.assertEqual((point["successes"], point["failures"]), (60, 0))
        self.assertEqual(point["ttft_seconds"], {"p50": 0.031026913959067315, "p95": 0.03623536097584292})
        self.assertEqual(point["itl_seconds"], {"p50": 3.718049265444279e-05, "p95": 0.007165764196543023})
        self.assertEqual(point["throughput"], {"requests_per_second": 1.0171575756131475, "output_tokens_per_second": 64.23350089997027})
        self.assertEqual(point["achieved_request_rate"], 1.0254654214024423)
        self.assertEqual(point["goodput"], {"requests_per_second": 0.75})

    def test_concurrent_stage_must_match_upstream_report(self):
        report = copy.deepcopy(REPORT)
        report["load_summary"].update(requested_rate=60, concurrency=4)
        stage = {"concurrency_level": 4, "num_requests": 60}
        point = adapter.parse_report(json.dumps(report), stage, "concurrent", 60)
        self.assertEqual(point["stage"], {"type": "concurrent", **stage})
        for field in ("requested_rate", "concurrency"):
            altered = copy.deepcopy(report)
            altered["load_summary"][field] += 1
            with self.subTest(field=field), self.assertRaises(adapter.AdapterError):
                adapter.parse_report(json.dumps(altered), stage, "concurrent", 60)

    def test_goodput_and_missing_optional_measurements(self):
        report = copy.deepcopy(REPORT)
        report["successes"]["goodput_metrics"] = {"request_goodput": 0.75, "good_requests": 45}
        report["successes"]["latency"]["inter_token_latency"] = None
        report["successes"]["latency"]["time_to_first_token"] = None
        del report["successes"]["throughput"]["output_tokens_per_sec"]
        del report["load_summary"]["achieved_rate"]
        point = parse(report)
        self.assertEqual(point["goodput"], {"requests_per_second": 0.75})
        self.assertIsNone(point["ttft_seconds"])
        self.assertIsNone(point["itl_seconds"])
        self.assertIsNone(point["throughput"]["output_tokens_per_second"])
        self.assertIsNone(point["achieved_request_rate"])

    def test_upstream_single_send_rate_and_no_token_measurement_are_not_zero_invented(self):
        report = copy.deepcopy(REPORT)
        report["load_summary"]["send_duration"] = 0
        report["load_summary"]["achieved_rate"] = 0
        report["successes"]["output_len"] = None
        self.assertIsNone(parse(report)["achieved_request_rate"])
        self.assertIsNone(parse(report)["throughput"]["output_tokens_per_second"])

    def test_all_failure_report_retains_actual_failure_counts(self):
        report = copy.deepcopy(REPORT)
        report["successes"].update({"count": 0, "latency": {"time_to_first_token": None, "inter_token_latency": None},
                                    "throughput": {"requests_per_sec": 0.0, "output_tokens_per_sec": 0.0}, "output_len": None})
        del report["successes"]["goodput_metrics"]
        report["failures"]["count"] = 60
        point = parse(report)
        self.assertEqual((point["successes"], point["failures"]), (0, 60))
        self.assertEqual(point["throughput"]["requests_per_second"], 0.0)
        self.assertIsNone(point["throughput"]["output_tokens_per_second"])
        self.assertIsNone(point["goodput"])

    def test_malformed_empty_duplicate_and_nonfinite_json(self):
        for raw in ("", "{}", "[]", "{", '{"successes":NaN}', '{"a":1,"a":2}', 'null', '"no"'):
            with self.subTest(raw=raw), self.assertRaises(adapter.AdapterError):
                adapter.parse_report(raw, STAGE, "constant", 60)
        report = copy.deepcopy(REPORT)
        report["successes"]["latency"]["time_to_first_token"]["p95"] = float("inf")
        with self.assertRaises(adapter.AdapterError):
            parse(report)

    def test_missing_or_inconsistent_required_data(self):
        for section in ("successes", "failures", "load_summary"):
            report = copy.deepcopy(REPORT)
            del report[section]
            with self.subTest(section=section), self.assertRaises(adapter.AdapterError):
                parse(report)
        for change in (lambda r: r["successes"].pop("count"),
                       lambda r: r["failures"].update(count=2),
                       lambda r: r["load_summary"].update(count=True),
                       lambda r: r["successes"]["throughput"].pop("requests_per_sec"),
                       lambda r: r["successes"]["throughput"].update(requests_per_sec=-1),
                       lambda r: r["successes"]["latency"]["time_to_first_token"].pop("p95"),
                       lambda r: r["successes"]["latency"].pop("time_to_first_token"),
                       lambda r: r["successes"]["latency"].pop("inter_token_latency"),
                       lambda r: r["successes"]["latency"]["inter_token_latency"].clear(),
                       lambda r: r["successes"]["latency"]["time_to_first_token"].update(median=None),
                       lambda r: r["successes"].pop("goodput_metrics"),
                       lambda r: r["successes"].update(goodput_metrics=None)):
            report = copy.deepcopy(REPORT)
            change(report)
            with self.assertRaises(adapter.AdapterError):
                parse(report)

    def test_error_body_not_copied_into_result(self):
        report = copy.deepcopy(REPORT)
        report["successes"]["count"] = 59
        report["failures"] = {"count": 1, "by_label": {"other": {"messages": [{"message": "secret-value"}]}}}
        self.assertNotIn("secret-value", json.dumps(parse(report)))


class OrchestrationTests(unittest.TestCase):
    def test_distinct_warmup_and_measured_processes_and_clean_workspace(self):
        config, request = inputs()
        calls = []
        workspaces = []
        secret = "super-secret"

        def snapshot(args, env, deadline):
            workspace = Path(args[-1])
            workspaces.append(workspace)
            self.assertNotIn(secret, json.dumps(env))
            (workspace / "tokenizer").mkdir()

        def stage(config_path, workspace, api_key, remaining, deadline):
            self.assertEqual(api_key, secret)
            raw_config = config_path.read_text()
            self.assertNotIn(secret, raw_config)
            config_data = json.loads(raw_config)
            stages = config_data["load"]["stages"]
            self.assertEqual(len(stages), 1)
            current = stages[0]
            count = current["rate"] * current["duration"]
            calls.append(current)
            report = copy.deepcopy(REPORT)
            report["load_summary"].update(count=count, requested_rate=current["rate"])
            report["successes"]["count"] = count
            report["failures"]["by_label"] = {"other": {"messages": [{"message": secret}]}}
            (Path(config_data["storage"]["local_storage"]["path"]) / "stage_0_lifecycle_metrics.json").write_text(json.dumps(report))
            return remaining - 1

        image = "example/adapter@sha256:" + "b" * 64
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(os.environ, {"TMPDIR": tmp}), \
             mock.patch.object(adapter, "run_bounded", side_effect=snapshot), mock.patch.object(adapter, "run_stage", side_effect=stage):
            result = adapter.execute(config, request, RUN_ID, secret, image)
            self.assertEqual(calls, [{"rate": 1, "duration": 1}, {"rate": 2, "duration": 4}])
            self.assertTrue(all(not path.exists() for path in workspaces))
        artifact = json.loads(result)
        self.assertEqual(artifact["request"], request)
        self.assertEqual(artifact["runner"]["image"], image)
        self.assertEqual(artifact["runner"]["upstream_image"], adapter.UPSTREAM_IMAGE)
        self.assertEqual(artifact["definition"]["spec"], config["benchmark"]["workloads"]["test"])
        self.assertEqual([p["stage"] for p in artifact["points"]], [{"type": "constant", "rate": 2, "duration": 4}])
        self.assertNotIn(secret, result.decode())


class BudgetTests(unittest.TestCase):
    def test_exact_request_and_token_budgets_include_warmup(self):
        config, request = inputs()
        _, counts = adapter.validate_inputs(config, request, RUN_ID)
        self.assertEqual(counts, [8])
        for field, value in (("max_requests", 8), ("model_budget_tokens", 71), ("duration_seconds", 4)):
            changed = copy.deepcopy(request)
            changed["workload"][field] = value
            with self.subTest(field=field), self.assertRaises(adapter.AdapterError):
                adapter.validate_inputs(config, changed, RUN_ID)

    def test_concurrency_and_multi_stage_budget(self):
        config, request = inputs()
        config["benchmark"]["workloads"]["test"]["load"] = {"type": "concurrent", "stages": [
            {"concurrency_level": 2, "num_requests": 3}, {"concurrency_level": 4, "num_requests": 5}]}
        adapter.validate_inputs(config, request, RUN_ID)
        config["benchmark"]["workloads"]["test"]["load"]["stages"][1]["concurrency_level"] = 5
        with self.assertRaises(adapter.AdapterError):
            adapter.validate_inputs(config, request, RUN_ID)

    def test_multi_turn_history_has_memory_bound(self):
        config, request = inputs()
        config["benchmark"]["workloads"]["test"]["data"] = {
            "type": "shared_prefix", "num_groups": 1, "num_prompts_per_group": 1,
            "system_prompt_len": 1000, "question_len": 1000, "output_len": 1, "enable_multi_turn_chat": True}
        config["benchmark"]["workloads"]["test"]["load"] = {
            "type": "concurrent", "stages": [{"concurrency_level": 1, "num_requests": 100}]}
        request["workload"].update(max_requests=101, model_budget_tokens=101)
        with self.assertRaisesRegex(adapter.AdapterError, "token memory"):
            adapter.validate_inputs(config, request, RUN_ID)
        config["benchmark"]["workloads"]["test"]["data"]["enable_multi_turn_chat"] = False
        adapter.validate_inputs(config, request, RUN_ID)

    def test_pregeneration_and_random_memory_bounds(self):
        config, request = inputs()
        config["benchmark"]["workloads"]["test"]["data"] = {
            "type": "shared_prefix", "num_groups": 128, "num_prompts_per_group": 128,
            "system_prompt_len": 1000, "question_len": 1000, "output_len": 1, "enable_multi_turn_chat": False}
        with self.assertRaisesRegex(adapter.AdapterError, "materialization"):
            adapter.validate_inputs(config, request, RUN_ID)
        config["benchmark"]["workloads"]["test"]["data"] = {"type": "random", "input_len": 65536, "output_len": 1}
        config["benchmark"]["workloads"]["test"]["load"] = {"type": "concurrent", "stages": [{"concurrency_level": 1, "num_requests": 300}]}
        request["workload"].update(max_requests=301, model_budget_tokens=301)
        with self.assertRaisesRegex(adapter.AdapterError, "token memory"):
            adapter.validate_inputs(config, request, RUN_ID)

    def test_schema_seed_and_tokenizer_are_bounded(self):
        config, request = inputs()
        for revision in ("main", "A" * 40, "a" * 39):
            altered = copy.deepcopy(config)
            altered["benchmark"]["tokenizer"]["revision"] = revision
            with self.subTest(revision=revision), self.assertRaises(adapter.AdapterError):
                adapter.validate_inputs(altered, request, RUN_ID)
        for repo in ("https://hub/repo", "org/../x", "foo--bar", "org/repo.git/extra", "org/.hidden"):
            with self.subTest(repo=repo), self.assertRaises(adapter.AdapterError):
                adapter.repository_id(repo)
        for bad_id in (RUN_ID.upper(), "{00000000-0000-4000-8000-000000000001}", "bad"):
            if bad_id == RUN_ID:  # this particular UUID contains no letters
                continue
            with self.assertRaises(adapter.AdapterError):
                adapter.validate_inputs(config, request, bad_id)
        altered = copy.deepcopy(config)
        altered["benchmark"]["workloads"]["test"]["data"]["unknown"] = "unsafe"
        with self.assertRaises(adapter.AdapterError):
            adapter.validate_inputs(altered, request, RUN_ID)

    def test_generated_configs_are_restricted_and_seeded(self):
        config, request = inputs()
        spec, _ = adapter.validate_inputs(config, request, RUN_ID)
        generated = adapter.upstream_config(config, request, spec, {"rate": 2, "duration": 4}, "constant", "/tmp/tokenizer", "/tmp/report")
        self.assertEqual(generated["server"]["base_url"], "https://example.test/candidates/candidate")
        self.assertEqual(generated["api"], {"type": "completion", "streaming": True})
        self.assertEqual(generated["load"]["base_seed"], 42)
        self.assertEqual(generated["load"]["num_workers"], 1)
        self.assertEqual(generated["load"]["worker_max_concurrency"], 4)
        self.assertEqual(generated["load"]["request_retries"], 0)
        self.assertEqual(generated["data"]["input_distribution"]["mean"], 32)
        self.assertNotIn("api_key", generated["server"])
        self.assertFalse(generated["report"]["request_lifecycle"]["per_request"])
        self.assertEqual(generated["report"]["request_lifecycle"]["max_error_messages"], 0)
        self.assertFalse(generated["tokenizer"]["trust_remote_code"])
        self.assertEqual(generated["report"]["goodput"]["constraints"], spec["goodput"])
        data = {"type": "shared_prefix", "num_groups": 1, "num_prompts_per_group": 1,
                "system_prompt_len": 8, "question_len": 4, "output_len": 2, "enable_multi_turn_chat": True}
        spec["data"] = data
        generated = adapter.upstream_config(config, request, spec, {"concurrency_level": 2, "num_requests": 4}, "concurrent", "/tmp/tokenizer", "/tmp/report")
        self.assertEqual(generated["data"]["shared_prefix"]["seed"], 42)
        self.assertTrue(generated["data"]["shared_prefix"]["enable_multi_turn_chat"])
        self.assertEqual(generated["load"]["stages"], [{"concurrency_level": 2, "num_requests": 4}])


class SecurityTests(unittest.TestCase):
    def test_result_url_ownership_https_and_no_secret_in_errors(self):
        good = f"https://storage.example/bucket/runs/{RUN_ID}/result/output.json?signature=secret"
        self.assertEqual(adapter.validate_upload_url(good, RUN_ID), good)
        bad_urls = [good.replace("https:", "http:"), good.replace("storage.example", "user:password@storage.example"),
                    good.replace(RUN_ID, "00000000-0000-4000-8000-000000000003"), good.replace("output.json?", "other.json?"),
                    good + "#fragment", good.split("?")[0], good + "\n"]
        for value in bad_urls:
            with self.subTest(value=value), self.assertRaises(adapter.AdapterError) as ctx:
                adapter.validate_upload_url(value, RUN_ID)
            self.assertNotIn("secret", str(ctx.exception))

    def test_upload_conditional_and_no_redirect(self):
        url = f"https://storage.example/runs/{RUN_ID}/result/output.json?signature=secret"
        response = mock.MagicMock()
        response.__enter__.return_value.status = 200
        opener = mock.Mock()
        opener.open.return_value = response
        with mock.patch.dict(os.environ, {"THALAMUS_S3_CA_FILE": ""}), \
             mock.patch.object(adapter, "read_limited", return_value=url), \
             mock.patch.object(adapter.urllib.request, "build_opener", return_value=opener):
            adapter.upload_result(b"{}", RUN_ID)
        req = opener.open.call_args.args[0]
        self.assertEqual(req.method, "PUT")
        self.assertEqual(req.get_header("If-none-match"), "*")
        self.assertEqual(req.data, b"{}")
        self.assertEqual(opener.open.call_args.kwargs["timeout"], 60)
        self.assertIsNone(adapter.NoRedirect().redirect_request(None, None, 302, None, None, "https://other"))

    def test_optional_object_store_ca_augments_system_roots(self):
        url = f"https://storage.example/runs/{RUN_ID}/result/output.json?signature=secret"
        response = mock.MagicMock()
        response.__enter__.return_value.status = 200
        opener = mock.Mock()
        opener.open.return_value = response
        with tempfile.TemporaryDirectory() as tmp:
            ca = Path(tmp) / "ca.pem"
            ca.write_text("placeholder certificate for mock context")
            context = mock.Mock()
            with mock.patch.dict(os.environ, {"THALAMUS_S3_CA_FILE": str(ca)}), \
                 mock.patch.object(adapter.ssl, "create_default_context", return_value=context) as default_context, \
                 mock.patch.object(adapter, "read_limited", return_value=url), \
                 mock.patch.object(adapter.urllib.request, "build_opener", return_value=opener) as build_opener:
                adapter.upload_result(b"{}", RUN_ID)
            default_context.assert_called_once_with()
            context.load_verify_locations.assert_called_once_with(cafile=str(ca))
            handler = build_opener.call_args.args[-1]
            self.assertIs(handler._context, context)

            context.reset_mock()
            with mock.patch.dict(os.environ, {"THALAMUS_S3_CA_FILE": str(ca) + ".missing"}), \
                 mock.patch.object(adapter.ssl, "create_default_context", return_value=context), \
                 mock.patch.object(adapter, "read_limited", return_value=url), \
                 mock.patch.object(adapter.urllib.request, "build_opener", return_value=opener):
                adapter.upload_result(b"{}", RUN_ID)
            context.load_verify_locations.assert_not_called()

            context.reset_mock()
            context.load_verify_locations.side_effect = OSError("secret")
            with mock.patch.dict(os.environ, {"THALAMUS_S3_CA_FILE": str(ca)}), \
                 mock.patch.object(adapter.ssl, "create_default_context", return_value=context), \
                 mock.patch.object(adapter, "read_limited", return_value=url), \
                 self.assertRaises(adapter.AdapterError) as error:
                adapter.upload_result(b"{}", RUN_ID)
            self.assertEqual(str(error.exception), "result upload failed")

    def test_invalid_runner_image_is_rejected(self):
        valid = "registry.test/team/adapter@sha256:" + "a" * 64
        self.assertEqual(adapter.runner_image(valid), valid)
        for image in ("registry.test/team/adapter:latest", "@sha256:" + "a" * 64,
                      "registry.test/adapter@sha256:" + "A" * 64, "registry.test/a@x@sha256:" + "a" * 64):
            with self.subTest(image=image), self.assertRaises(adapter.AdapterError):
                adapter.runner_image(image)

    def test_key_is_injected_only_at_client_construction(self):
        seen = {}
        runner = type("Runner", (), {"run": lambda self: seen.update(ran=True)})
        fake = types.ModuleType("inference_perf.main")
        fake.InferencePerfRunner = runner
        fake.vLLMModelServerClient = lambda *args, **kwargs: seen.update(kwargs)
        def fake_cli():
            fake.vLLMModelServerClient(api_key=None)
            fake.InferencePerfRunner().run()
        fake.main_cli = fake_cli
        package = types.ModuleType("inference_perf")
        package.main = fake
        read_fd, write_fd = os.pipe()
        try:
            module_spec = importlib.util.spec_from_file_location("thalamus_upstream_entry", HERE / "upstream_entry.py")
            module = importlib.util.module_from_spec(module_spec)
            with mock.patch.dict(sys.modules, {"inference_perf": package, "inference_perf.main": fake}), \
                 mock.patch.dict(os.environ, {"THALAMUS_API_KEY": "super-secret", "THALAMUS_STAGE_SIGNAL_FD": str(write_fd)}), \
                 mock.patch.object(sys, "argv", ["upstream_entry.py", "/tmp/config.json"]):
                module_spec.loader.exec_module(module)
                module.main()
            self.assertEqual(seen["api_key"], "super-secret")
            self.assertTrue(seen["ran"])
            start, end = os.read(read_fd, 100).decode("ascii").splitlines()
            self.assertEqual(start.split()[0], "start")
            self.assertEqual(end.split()[0], "end")
            self.assertGreaterEqual(int(end.split()[1]), int(start.split()[1]))
        finally:
            os.close(read_fd)
            os.close(write_fd)

    def test_environment_and_target_do_not_include_secrets(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(os.environ, {"THALAMUS_API_KEY": "secret", "HF_TOKEN": "secret", "AWS_SECRET_ACCESS_KEY": "secret"}):
            env = adapter.minimal_env(Path(tmp))
            self.assertNotIn("THALAMUS_API_KEY", env)
            self.assertNotIn("HF_TOKEN", env)
            self.assertNotIn("AWS_SECRET_ACCESS_KEY", env)
        for value in ("https://user:secret@example.test/v1", "https://example.test/v1?token=secret", "https://example.test/v1#secret", "file:///v1"):
            with self.assertRaises(adapter.AdapterError) as ctx:
                adapter.target_url(value, False)
            self.assertNotIn("secret", str(ctx.exception))
        self.assertEqual(adapter.target_url("http://localhost:8000/v1", True), "http://localhost:8000")


if __name__ == "__main__":
    unittest.main()
