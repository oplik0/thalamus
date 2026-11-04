#!/usr/bin/env python3
"""Run the adapter against the real llm-d-inference-sim without Kubernetes.

Uses the upstream v1.0.0 simulator image and a locally available adapter image.
For a local image tag, runner.image uses its actual image ID under a synthetic
local/ name, not a claim of a pushed repository digest. Measurements are of
the simulator, not production hardware.
"""
import argparse
import json
import os
import re
import signal
import socket
import subprocess
import sys
import tempfile
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

SIMULATOR_IMAGE = "ghcr.io/llm-d/llm-d-inference-sim:v1.0.0"
TOKENIZER = {"repository": "HuggingFaceTB/SmolLM2-135M-Instruct", "revision": "12fd25f77366fa6b3b4b768ec3050bf629380bac"}

def observed_time():
    # chrono serializes a whole-second UTC DateTime in this canonical form.
    # The benchmark result parser requires an exact canonical RunRequest.
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")



def local_image_identity(image):
    # An image ID is a content digest, but is not a registry manifest digest.
    # Label the constructed local/ reference explicitly rather than pretending
    # it is a pushed image. Registry digest inputs retain their exact identity.
    if re.fullmatch(r"[^@\s]+@sha256:[0-9a-f]{64}", image):
        return image
    try:
        image_id = subprocess.check_output(["docker", "image", "inspect", "--format={{.Id}}", image], stderr=subprocess.DEVNULL, text=True).strip()
    except (OSError, subprocess.CalledProcessError):
        raise RuntimeError("adapter image is not available locally") from None
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", image_id):
        raise RuntimeError("invalid local adapter image ID")
    return "local/inference-perf-adapter@" + image_id


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, help="new file for the measured result")
    parser.add_argument("--adapter-image", required=True, help="locally available adapter image (tag or digest)")
    parser.add_argument("--scenario", choices=("random", "shared-prefix", "multi-turn"), default="random")
    parser.add_argument("--load", choices=("constant", "concurrent"), default="constant")
    args = parser.parse_args()
    def terminate(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminate)
    image = local_image_identity(args.adapter_image)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    model = "thalamus-sim"
    sim_args = ["--port", "8000", "--model", model, "--max-waiting-queue-length", "1000"]
    sim_name = "thalamus-sim-" + uuid.uuid4().hex
    sim_command = ["docker", "run", "--rm", "--name", sim_name, "--publish", f"127.0.0.1:{port}:8000", SIMULATOR_IMAGE, *sim_args]
    simulator = subprocess.Popen(sim_command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 180
        while True:
            if simulator.poll() is not None:
                raise RuntimeError("simulator exited before it became ready")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=1):
                    break
            except OSError:
                if time.monotonic() >= deadline:
                    raise RuntimeError("simulator did not become ready")
                time.sleep(0.2)
        if args.scenario == "random":
            data = {"type": "random", "input_len": 32, "output_len": 8}
        else:
            data = {"type": "shared_prefix", "num_groups": 1, "num_prompts_per_group": 4 if args.scenario == "multi-turn" else 1,
                    "system_prompt_len": 16, "question_len": 16, "output_len": 8,
                    "enable_multi_turn_chat": args.scenario == "multi-turn"}
        if args.load == "constant":
            load = {"type": "constant", "stages": [{"rate": 1, "duration": 3}, {"rate": 2, "duration": 3}]}
        else:
            load = {"type": "concurrent", "stages": [{"concurrency_level": 2, "num_requests": 6}, {"concurrency_level": 4, "num_requests": 6}]}
        spec = {"load": load, "data": data, "goodput": {"ttft": 5, "itl": 1}}
        config = {"base_url": f"http://127.0.0.1:{port}/v1", "model": model,
                  "benchmark": {"tokenizer": TOKENIZER, "workloads": {"simulator": spec}}}
        run_id = str(uuid.uuid4())
        request = {"project_id": str(uuid.uuid4()), "kind": "benchmark", "service": "local-simulator",
                   "deployment": "local-simulator", "runner": "inference-perf",
                   "workload": {"name": "simulator", "seed": 42, "concurrency": 4,
                                "duration_seconds": 90, "warmup_seconds": 1, "max_requests": 32,
                                "model_budget_tokens": 1024, "tool_budget_calls": 0},
                   "evidence": {"desired_configuration": "local-simulator", "observed_runtime": "local-simulator",
                                "test_definition": "local-simulator", "conditions": {"hardware": "simulator",
                                "nodes": ["localhost"], "replicas": 1, "cache_state": "unknown",
                                "competing_traffic": "none", "observed_at": observed_time()}}}
        env = dict(os.environ)
        env.update({"THALAMUS_BENCHMARK_CONFIG": json.dumps(config), "THALAMUS_RUN_REQUEST": json.dumps(request),
                    "THALAMUS_RUN_ID": run_id, "THALAMUS_API_KEY": "local-simulator-only", "THALAMUS_RUNNER_IMAGE": image})
        # The adapter image runs as UID 65532. A private scratch directory with
        # a writable mount lets it write only the non-secret result.
        with tempfile.TemporaryDirectory(prefix="thalamus-sim-result-") as scratch:
            os.chmod(scratch, 0o777)
            keys = ("THALAMUS_BENCHMARK_CONFIG", "THALAMUS_RUN_REQUEST", "THALAMUS_RUN_ID", "THALAMUS_API_KEY", "THALAMUS_RUNNER_IMAGE")
            env_args = [part for key in keys for part in ("--env", key)]
            command = ["docker", "run", "--rm", "--network", "host", "--read-only", "--tmpfs", "/tmp:rw,size=512m,mode=1777",
                       "--mount", f"type=bind,src={scratch},dst=/result", *env_args, args.adapter_image,
                       "--local", "--output", "/result/result.json"]
            completed = subprocess.run(command, env=env, check=False)
            if completed.returncode:
                return completed.returncode
            result = (Path(scratch) / "result.json").read_bytes()
            with open(args.output, "xb") as target:
                target.write(result)
            return 0
    finally:
        # Stop the named container even if the attached Docker CLI exits early.
        subprocess.run(["docker", "rm", "-f", sim_name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
        if simulator.poll() is None:
            simulator.terminate()
            try:
                simulator.wait(timeout=5)
            except subprocess.TimeoutExpired:
                simulator.kill()
                simulator.wait()


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        print("local simulator: interrupted", file=sys.stderr)
        sys.exit(130)
    except (OSError, RuntimeError) as error:
        print(f"local simulator: {error}", file=sys.stderr)
        sys.exit(1)
