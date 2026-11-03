#!/usr/bin/env python3
"""Bounded adapter for the pinned inference-perf 0.7.0 image.

Only the stage reports are interpreted; the benchmark itself is upstream's runner.
The same entry point supports a loopback-only local mode without a result upload.
"""
import argparse
import json
import math
import os
import re
import select
import signal
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path

UPSTREAM_IMAGE = "quay.io/inference-perf/inference-perf@sha256:d247e6ad725dc5bb56df71d115abc8ca455bba9f0ccd4110e14bbb56504250ec"
UPLOAD_FILE = Path("/var/run/thalamus/result-upload/url")
MAX_CONFIG = 64 * 1024
MAX_REQUEST = 1024 * 1024
MAX_REPORT = 8 * 1024 * 1024
MAX_RESULT = 2 * 1024 * 1024
# Bound upstream's eager shared-prefix materialization independently of request count.
MAX_PREGENERATED_TOKENS = 4_000_000
# Upstream retains lifecycle data for every request until reports are generated.
MAX_STAGE_TOKENS = 16_000_000


class AdapterError(Exception):
    pass


def fail(message):
    raise AdapterError(message)


def fields(value, keys, label):
    if not isinstance(value, dict) or set(value) != set(keys):
        fail("invalid " + label)
    return value


def integer(value, lower, upper, label):
    if type(value) is not int or not lower <= value <= upper:
        fail("invalid " + label)
    return value


def text(value, maximum, label):
    if not isinstance(value, str) or not value.strip() or len(value.encode("utf-8")) > maximum or any(ord(c) < 32 or ord(c) == 127 for c in value):
        fail("invalid " + label)
    return value


def finite(value, label, *, positive=False):
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0 or (positive and value == 0):
        fail("invalid " + label)
    return value


def json_object(raw, max_bytes, label):
    if not isinstance(raw, str) or len(raw.encode("utf-8")) > max_bytes:
        fail("invalid " + label)
    try:
        def reject_constant(_):
            fail("invalid " + label)

        def unique(pairs):
            obj = {}
            for key, value in pairs:
                if key in obj:
                    fail("invalid " + label)
                obj[key] = value
            return obj

        result = json.loads(raw, parse_constant=reject_constant, object_pairs_hook=unique)
    except (ValueError, TypeError, UnicodeError, RecursionError):
        fail("invalid " + label)
    if not isinstance(result, dict):
        fail("invalid " + label)
    return result


def repository_id(value):
    text(value, 256, "tokenizer repository")
    pieces = value.split("/")
    if not 1 <= len(pieces) <= 2:
        fail("invalid tokenizer repository")
    for part in pieces:
        if not re.fullmatch(r"[A-Za-z0-9_.-]+", part) or part[0] in ".-" or part[-1] in ".-" or ".." in part or "--" in part:
            fail("invalid tokenizer repository")
    return value


def runner_image(value):
    text(value, 1024, "runner image")
    image, separator, digest = value.rpartition("@sha256:")
    if not separator or not image or "@" in image or not re.fullmatch(r"[0-9a-f]{64}", digest):
        fail("invalid runner image")
    return value


def target_url(value, local):
    text(value, 1024, "base URL")
    try:
        url = urllib.parse.urlsplit(value)
        _ = url.port
    except ValueError:
        fail("invalid base URL")
    if url.scheme not in ("http", "https") or not url.hostname or url.username is not None or url.password is not None or url.query or url.fragment or not url.path.endswith("/v1") or any(c.isspace() for c in value):
        fail("invalid base URL")
    if local and url.hostname not in ("127.0.0.1", "localhost", "::1"):
        fail("local mode requires a loopback target")
    return value[:-3]


def validate_spec(spec):
    fields(spec, ("load", "data", "goodput"), "workload specification")
    load = fields(spec["load"], ("type", "stages"), "load")
    if load["type"] not in ("constant", "concurrent") or not isinstance(load["stages"], list) or not 1 <= len(load["stages"]) <= 16:
        fail("invalid load stages")
    for stage in load["stages"]:
        if load["type"] == "constant":
            fields(stage, ("rate", "duration"), "constant stage")
            integer(stage["rate"], 1, 256, "rate")
            integer(stage["duration"], 1, 3600, "duration")
        else:
            fields(stage, ("concurrency_level", "num_requests"), "concurrent stage")
            integer(stage["concurrency_level"], 1, 256, "concurrency")
            integer(stage["num_requests"], 1, 1_000_000, "request count")
    data = spec["data"]
    if not isinstance(data, dict):
        fail("invalid data")
    if data.get("type") == "random":
        fields(data, ("type", "input_len", "output_len"), "random data")
        integer(data["input_len"], 1, 65536, "input length")
    elif data.get("type") == "shared_prefix":
        fields(data, ("type", "num_groups", "num_prompts_per_group", "system_prompt_len", "question_len", "output_len", "enable_multi_turn_chat"), "shared prefix data")
        integer(data["num_groups"], 1, 128, "groups")
        integer(data["num_prompts_per_group"], 1, 128, "prompts per group")
        integer(data["system_prompt_len"], 1, 65536, "system prompt length")
        integer(data["question_len"], 1, 65536, "question length")
        if type(data["enable_multi_turn_chat"]) is not bool:
            fail("invalid multi-turn flag")
        generated = data["num_groups"] * data["num_prompts_per_group"] * (data["system_prompt_len"] + data["question_len"])
        if generated > MAX_PREGENERATED_TOKENS:
            fail("shared prefix materialization exceeds limit")
    else:
        fail("invalid data type")
    integer(data["output_len"], 1, 16384, "output length")
    goodput = fields(spec["goodput"], ("ttft", "itl"), "goodput")
    for key in ("ttft", "itl"):
        if finite(goodput[key], "goodput", positive=True) > 3600:
            fail("invalid goodput")
    return spec


def validate_inputs(config, request, run_id, local=False):
    try:
        parsed = uuid.UUID(run_id)
    except (ValueError, TypeError, AttributeError):
        fail("invalid run id")
    if str(parsed) != run_id:
        fail("invalid run id")
    fields(config, ("base_url", "model", "benchmark"), "benchmark config")
    target_url(config["base_url"], local)
    text(config["model"], 256, "model")
    benchmark = fields(config["benchmark"], ("tokenizer", "workloads"), "benchmark")
    tokenizer = fields(benchmark["tokenizer"], ("repository", "revision"), "tokenizer")
    repository_id(tokenizer["repository"])
    if not isinstance(tokenizer["revision"], str) or not re.fullmatch(r"[0-9a-f]{40}", tokenizer["revision"]):
        fail("invalid tokenizer revision")
    workloads = benchmark["workloads"]
    if not isinstance(workloads, dict) or not 1 <= len(workloads) <= 128:
        fail("invalid workloads")
    for name, spec in workloads.items():
        text(name, 128, "workload name")
        validate_spec(spec)
    if not isinstance(request, dict) or request.get("kind") != "benchmark":
        fail("invalid benchmark request")
    workload = request.get("workload")
    fields(workload, ("name", "seed", "concurrency", "duration_seconds", "warmup_seconds", "max_requests", "model_budget_tokens", "tool_budget_calls"), "request workload")
    name = text(workload["name"], 128, "workload name")
    integer(workload["seed"], 0, (1 << 64) - 1, "seed")
    concurrency = integer(workload["concurrency"], 1, 256, "concurrency")
    duration = integer(workload["duration_seconds"], 1, 3600, "duration")
    warmup = integer(workload["warmup_seconds"], 0, duration, "warmup")
    max_requests = integer(workload["max_requests"], 1, 1_000_000, "request budget")
    token_budget = integer(workload["model_budget_tokens"], 1, 1_000_000, "model token budget")
    integer(workload["tool_budget_calls"], 0, 1000, "tool budget")
    if name not in workloads:
        fail("workload is not configured")
    spec = workloads[name]
    load = spec["load"]
    if load["type"] == "constant":
        counts = [stage["rate"] * stage["duration"] for stage in load["stages"]]
        if warmup + sum(stage["duration"] for stage in load["stages"]) > duration:
            fail("stage durations exceed workload budget")
    else:
        counts = [stage["num_requests"] for stage in load["stages"]]
        if any(stage["concurrency_level"] > concurrency for stage in load["stages"]):
            fail("stage concurrency exceeds workload budget")
    count = warmup + sum(counts)
    if count > max_requests or count * spec["data"]["output_len"] > token_budget:
        fail("stages exceed request or output token budget")
    data = spec["data"]
    for stage_count in ([warmup] if warmup else []) + counts:
        if data["type"] == "random":
            per_request = data["input_len"] + data["output_len"]
        else:
            per_request = 2 * data["system_prompt_len"] + data["question_len"] + data["output_len"]
            if data["enable_multi_turn_chat"]:
                sessions = data["num_groups"] * data["num_prompts_per_group"]
                previous_turns = (stage_count - 1) // sessions
                per_request += previous_turns * (data["system_prompt_len"] + data["question_len"] + data["output_len"])
        if stage_count * per_request > MAX_STAGE_TOKENS:
            fail("stage token memory estimate exceeds limit")
    return spec, counts


def distribution(length):
    return {"type": "fixed", "min": length, "max": length, "mean": length, "std_dev": 0}


def upstream_config(config, request, spec, stage, load_type, tokenizer_path, output_path):
    data = spec["data"]
    seed = request["workload"]["seed"]
    if data["type"] == "random":
        upstream_data = {"type": "random", "input_distribution": distribution(data["input_len"]), "output_distribution": distribution(data["output_len"])}
    else:
        upstream_data = {"type": "shared_prefix", "shared_prefix": {**{k: v for k, v in data.items() if k != "type"}, "seed": seed}}
    return {
        "api": {"type": "completion", "streaming": True},
        "server": {"type": "vllm", "base_url": target_url(config["base_url"], False), "model_name": config["model"], "ignore_eos": True},
        "tokenizer": {"pretrained_model_name_or_path": str(tokenizer_path), "trust_remote_code": False, "load_timeout": 120},
        "data": upstream_data,
        "load": {"type": load_type, "stages": [stage], "num_workers": 1, "worker_max_concurrency": request["workload"]["concurrency"], "worker_max_tcp_connections": request["workload"]["concurrency"], "request_timeout": request["workload"]["duration_seconds"], "request_retries": 0, "stage_teardown_grace_seconds": 0, "base_seed": seed, "interval": 0},
        # Prefer generated-token usage; upstream falls back to client counts when usage is absent.
        "report": {"request_lifecycle": {"summary": False, "per_stage": True, "per_request": False, "per_adapter": False, "per_adapter_stage": False, "percentiles": [50, 95], "use_server_output_tokens": True, "max_error_messages": 0}, "goodput": {"constraints": spec["goodput"]}, "prometheus": None, "session_lifecycle": {"summary": False, "per_stage": False, "per_session": False}},
        "storage": {"local_storage": {"path": str(output_path)}},
    }


def optional_number(obj, key, label):
    if key not in obj or obj[key] is None:
        return None
    return finite(obj[key], label)


def percentile(latency, key):
    if key not in latency:
        fail("missing latency report")
    value = latency[key]
    if value is None:
        return None
    if not isinstance(value, dict) or "median" not in value or "p95" not in value:
        fail("incomplete latency report")
    p50 = finite(value["median"], "latency")
    p95 = finite(value["p95"], "latency")
    if p95 < p50:
        fail("invalid latency percentiles")
    return {"p50": p50, "p95": p95}


def parse_report(raw, stage, load_type, expected_count):
    report = json_object(raw, MAX_REPORT, "stage report")
    successes = report.get("successes")
    failures = report.get("failures")
    summary = report.get("load_summary")
    if not isinstance(successes, dict) or not isinstance(failures, dict) or not isinstance(summary, dict):
        fail("missing stage report fields")
    good = integer(successes.get("count"), 0, expected_count, "success count")
    bad = integer(failures.get("count"), 0, expected_count, "failure count")
    if good + bad != expected_count or integer(summary.get("count"), 0, expected_count, "load count") != expected_count:
        fail("incomplete stage report")
    expected_rate = stage["rate"] if load_type == "constant" else stage["num_requests"]
    if finite(summary.get("requested_rate"), "requested rate") != expected_rate:
        fail("stage report does not match configured load")
    if load_type == "concurrent" and integer(summary.get("concurrency"), 1, 256, "reported concurrency") != stage["concurrency_level"]:
        fail("stage report does not match configured load")
    latency = successes.get("latency")
    throughput = successes.get("throughput")
    if not isinstance(latency, dict) or not isinstance(throughput, dict) or "requests_per_sec" not in throughput:
        fail("missing stage measurements")
    requests_per_second = finite(throughput["requests_per_sec"], "request throughput")
    output_tokens = optional_number(throughput, "output_tokens_per_sec", "token throughput")
    if good == 0 or successes.get("output_len") is None:
        # Upstream computes zero token throughput even when no token was observed.
        output_tokens = None
    measured = successes.get("goodput_metrics")
    if measured is None:
        if good:
            fail("missing goodput report")
        goodput = None
    elif isinstance(measured, dict) and "request_goodput" in measured:
        goodput = {"requests_per_second": finite(measured["request_goodput"], "goodput")}
    else:
        fail("invalid goodput report")
    send_duration = optional_number(summary, "send_duration", "send duration")
    achieved = optional_number(summary, "achieved_rate", "achieved rate")
    if send_duration is None or send_duration == 0:
        achieved = None
    return {"stage": {"type": load_type, **stage}, "successes": good, "failures": bad,
            "ttft_seconds": percentile(latency, "time_to_first_token"),
            "itl_seconds": percentile(latency, "inter_token_latency"),
            "throughput": {"requests_per_second": requests_per_second, "output_tokens_per_second": output_tokens},
            "goodput": goodput, "achieved_request_rate": achieved}


def minimal_env(workspace):
    return {"PATH": os.environ.get("PATH", "/usr/local/bin:/usr/bin:/bin"), "HOME": str(workspace), "TMPDIR": str(workspace), "HF_HOME": str(workspace / "hf"), "HF_HUB_DISABLE_IMPLICIT_TOKEN": "1", "HF_HUB_DISABLE_TELEMETRY": "1", "HF_HUB_DISABLE_PROGRESS_BARS": "1", "HF_HUB_DISABLE_XET": "1", "PYTHONPATH": os.environ.get("PYTHONPATH", ""), "PYTHONUNBUFFERED": "1"}


def kill_process(process):
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()


def run_bounded(args, env, deadline):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        fail("benchmark deadline exceeded")
    process = subprocess.Popen(args, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    try:
        code = process.wait(timeout=remaining)
    except subprocess.TimeoutExpired:
        kill_process(process)
        fail("benchmark deadline exceeded")
    except BaseException:
        kill_process(process)
        raise
    if code:
        fail("upstream preparation failed")


def run_stage(config_path, workspace, api_key, active_remaining, deadline):
    read_fd, write_fd = os.pipe()
    env = minimal_env(workspace)
    env.update({"THALAMUS_API_KEY": api_key, "HF_HUB_OFFLINE": "1", "TRANSFORMERS_OFFLINE": "1", "THALAMUS_STAGE_SIGNAL_FD": str(write_fd)})
    process = None
    try:
        process = subprocess.Popen([sys.executable, str(Path(__file__).with_name("upstream_entry.py")), str(config_path)], env=env, pass_fds=(write_fd,), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        os.close(write_fd)
        write_fd = -1
        started = None
        ended = None
        pending = b""
        while True:
            now = time.monotonic()
            limit = min(deadline, started + active_remaining if started is not None and ended is None else deadline)
            if now >= limit:
                fail("benchmark deadline exceeded")
            ready, _, _ = select.select([read_fd], [], [], min(0.1, limit - now))
            if ready:
                data = os.read(read_fd, 128)
                if data:
                    pending += data
                    while b"\n" in pending:
                        line, pending = pending.split(b"\n", 1)
                        match = re.fullmatch(rb"(start|end) ([0-9]{1,24})", line)
                        if match is None:
                            fail("invalid upstream stage status")
                        timestamp = int(match.group(2)) / 1_000_000_000
                        if match.group(1) == b"start" and started is None:
                            started = timestamp
                        elif match.group(1) == b"end" and started is not None and ended is None and timestamp >= started:
                            ended = timestamp
                        else:
                            fail("invalid upstream stage status")
            if process.poll() is not None:
                if process.returncode or started is None or ended is None:
                    fail("upstream stage failed")
                if ended - started > active_remaining:
                    fail("benchmark deadline exceeded")
                return active_remaining - (ended - started)
    finally:
        if write_fd >= 0:
            os.close(write_fd)
        os.close(read_fd)
        if process is not None and process.poll() is None:
            kill_process(process)


def read_limited(path, maximum, label):
    try:
        with open(path, "rb") as source:
            data = source.read(maximum + 1)
        if len(data) > maximum:
            fail("oversized " + label)
        return data.decode("utf-8")
    except (OSError, UnicodeError):
        fail("cannot read " + label)


def validate_upload_url(value, run_id):
    try:
        url = urllib.parse.urlsplit(value)
        _ = url.port
    except ValueError:
        fail("invalid result upload URL")
    if url.scheme != "https" or not url.hostname or url.username is not None or url.password is not None or url.fragment or not url.query or not url.path.endswith(f"/runs/{run_id}/result/output.json") or any(c.isspace() for c in value):
        fail("invalid result upload URL")
    return value


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def upload_result(result, run_id):
    url = validate_upload_url(read_limited(UPLOAD_FILE, 16 * 1024, "result upload URL"), run_id)
    request = urllib.request.Request(url, data=result, method="PUT", headers={"Content-Type": "application/json", "If-None-Match": "*"})
    try:
        context = ssl.create_default_context()
        ca_file = os.environ.get("THALAMUS_S3_CA_FILE")
        if ca_file:
            try:
                os.stat(ca_file)
            except FileNotFoundError:
                pass  # The projected Secret's CA key is optional.
            else:
                context.load_verify_locations(cafile=ca_file)
        https = urllib.request.HTTPSHandler(context=context)
        with urllib.request.build_opener(NoRedirect(), urllib.request.ProxyHandler({}), https).open(request, timeout=60) as response:
            if not 200 <= response.status < 300:
                fail("result upload failed")
    except (OSError, ValueError, urllib.error.URLError):
        fail("result upload failed")


def execute(config, request, run_id, api_key, image, local=False):
    text(api_key, 8192, "API key")
    runner_image(image)
    spec, counts = validate_inputs(config, request, run_id, local)
    w = request["workload"]
    # Includes snapshot acquisition, upstream startup, reporting, and load execution.
    deadline = time.monotonic() + w["duration_seconds"] + 240
    with tempfile.TemporaryDirectory(prefix="thalamus-inference-perf-", dir=os.environ.get("TMPDIR", "/tmp")) as tmp:
        workspace = Path(tmp)
        os.chmod(workspace, 0o700)
        tokenizer = config["benchmark"]["tokenizer"]
        run_bounded([sys.executable, str(Path(__file__).with_name("snapshot.py")), tokenizer["repository"], tokenizer["revision"], str(workspace)], minimal_env(workspace), deadline)
        tokenizer_path = workspace / "tokenizer"
        if not tokenizer_path.is_dir():
            fail("tokenizer snapshot unavailable")
        active_remaining = float(w["duration_seconds"])
        points = []
        stages = []
        if w["warmup_seconds"]:
            stages.append(("constant", {"rate": 1, "duration": w["warmup_seconds"]}, w["warmup_seconds"], True))
        stages += [(spec["load"]["type"], stage, count, False) for stage, count in zip(spec["load"]["stages"], counts)]
        for index, (kind, stage, count, warmup) in enumerate(stages):
            report_path = workspace / f"stage-{index}"
            report_path.mkdir()
            cfg = upstream_config(config, request, spec, stage, kind, tokenizer_path, report_path)
            cfg_path = workspace / f"config-{index}.json"
            # JSON is a YAML subset. No API key is present in this file.
            cfg_path.write_text(json.dumps(cfg, allow_nan=False), encoding="utf-8")
            active_remaining = run_stage(cfg_path, workspace, api_key, active_remaining, deadline)
            report = read_limited(report_path / "stage_0_lifecycle_metrics.json", MAX_REPORT, "stage report")
            point = parse_report(report, stage, kind, count)
            if not warmup:
                points.append(point)
        result = {"schema_version": 1, "run_id": run_id, "request": request,
                  "runner": {"name": "inference-perf", "version": "0.7.0", "adapter_version": "1", "upstream_image": UPSTREAM_IMAGE, "image": image},
                  "tokenizer": tokenizer, "definition": {"name": w["name"], "seed": w["seed"], "warmup_seconds": w["warmup_seconds"], "spec": spec}, "points": points}
        encoded = json.dumps(result, ensure_ascii=True, allow_nan=False, separators=(",", ":")).encode("utf-8")
        if len(encoded) > MAX_RESULT:
            fail("result exceeds size limit")
        return encoded


def main():
    parser = argparse.ArgumentParser(description="Run the pinned inference-perf benchmark adapter")
    parser.add_argument("--local", action="store_true", help="allow a loopback HTTP simulator and write a local result instead of uploading")
    parser.add_argument("--output", help="local result file (required with --local)")
    args = parser.parse_args()
    if args.local != bool(args.output):
        parser.error("--local and --output must be used together")
    def terminated(_signum, _frame):
        raise AdapterError("benchmark interrupted")
    signal.signal(signal.SIGTERM, terminated)
    try:
        config = json_object(os.environ.get("THALAMUS_BENCHMARK_CONFIG", ""), MAX_CONFIG, "benchmark config")
        request = json_object(os.environ.get("THALAMUS_RUN_REQUEST", ""), MAX_REQUEST, "run request")
        run_id = os.environ.get("THALAMUS_RUN_ID", "")
        result = execute(config, request, run_id, os.environ.get("THALAMUS_API_KEY", ""), os.environ.get("THALAMUS_RUNNER_IMAGE", ""), args.local)
        if args.local:
            # Exclusive create keeps local results immutable, like the object-store PUT.
            with open(args.output, "xb") as target:
                target.write(result)
        else:
            upload_result(result, run_id)
        print("inference-perf benchmark result written")
        return 0
    except (AdapterError, OSError, ValueError, OverflowError) as exc:
        # Only controlled messages are emitted; library exceptions can contain secrets.
        print("inference-perf adapter: " + (str(exc) if isinstance(exc, AdapterError) else "operation failed"), file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
