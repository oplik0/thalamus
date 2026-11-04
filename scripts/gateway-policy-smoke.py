#!/usr/bin/env python3
"""Opt-in standalone gateway behavior check; run with python3 scripts/gateway-policy-smoke.py.

Requires KCL, Python 3, a writable checkout and /tmp, and a local Linux Docker
daemon with host networking. Starts isolated agentgateway, Envoy RLS and Redis
containers (Docker may pull the pinned images); removes owned containers on exit.
The CEL and limiter configuration come from rendered gateway.Policy fixtures.
This manually translates the policy, not the Kubernetes controller's conversion
or ConfigMap selection. The echo backend does not test AI token accounting,
metrics export, EPP queueing/timeouts, route rewrites, or a real deployment.
"""

import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


ROOT = Path(__file__).resolve().parent.parent
IMAGE = "cr.agentgateway.dev/agentgateway:v1.7.0"
RLS_IMAGE = "docker.io/envoyproxy/ratelimit:3e085e5b"
REDIS_IMAGE = "redis:7.4.3"
SERVICE = "chat"
DEPLOYMENT = "model"
CHAT_PATH = "/v1/chat/completions"
COMPLETIONS_PATH = "/v1/completions"
BODY = b'{"model":"chat","messages":[]}'
USER = "11111111-1111-4111-8111-111111111111"
PERSONAL_USER = "22222222-2222-4222-8222-222222222222"
PROJECT = "33333333-3333-4333-8333-333333333333"
TEAM = "44444444-4444-4444-8444-444444444444"
MONTH_TEAM = "77777777-7777-4777-8777-777777777777"
ZERO_TEAM = "88888888-8888-4888-8888-888888888888"
RUN = "55555555-5555-4555-8555-555555555555"
SESSION = "66666666-6666-4666-8666-666666666666"
FUTURE = "2999-01-01T00:00:00Z"
VARIANTS = {
    "public": ("chat", "", False, True),
    "candidate": ("model-candidate", "model", False, True),
    "benchmark": ("chat-benchmark", "", True, True),
    "candidate-benchmark": ("model-benchmark", "model", True, True),
    "no-class": ("chat-no-class", "", False, False),
}


class SmokeError(Exception):
    pass


def command(args, *, timeout, cwd=None):
    try:
        return subprocess.run(
            args, cwd=cwd, capture_output=True, text=True, timeout=timeout, check=False
        )
    except FileNotFoundError as exc:
        raise SmokeError(f"required tool not found: {args[0]}") from exc
    except subprocess.TimeoutExpired as exc:
        raise SmokeError(f"{' '.join(args[:2])} timed out after {timeout}s") from exc
    except OSError as exc:
        raise SmokeError(f"could not start {args[0]}: {exc}") from exc


def render_policies():
    # Render the real template with deterministic inputs that expose all route
    # branches. No authorization or rate-limit CEL is authored by this script.
    variants = []
    for route, candidate, benchmark, classes in VARIANTS.values():
        class_names = '["standard", "batch"]' if classes else "[]"
        variants.append(
            "gateway.Policy { namespace: \"inference\", service: \"chat\", "
            f"route: {json.dumps(route)}, keySelector: _selector, version: \"v1.7.0\", "
            f"rateLimit: _rate, candidateDeployment: {json.dumps(candidate)}, "
            f"benchmark: {'True' if benchmark else 'False'}, "
            f"trafficClasses: {class_names}"
            " }.item"
        )
    source = (
        "import templates.gateway.policy as gateway\n"
        "_profile = gateway.LimitProfile {name: \"smoke\", service: \"rate-limit\", "
        "namespace: \"inference\", port: 8081, domain: \"smoke\"}\n"
        "_caps = gateway.BudgetCaps {teams: ["
        f"gateway.BudgetCap {{team_id: {json.dumps(TEAM)}, max_tokens: 1000, unit: \"tokens\", window: \"day\", allowed_classes: [\"standard\", \"batch\"]}}, "
        f"gateway.BudgetCap {{team_id: {json.dumps(MONTH_TEAM)}, max_tokens: 500, unit: \"tokens\", window: \"month\", allowed_classes: [\"standard\", \"batch\"]}}, "
        f"gateway.BudgetCap {{team_id: {json.dumps(ZERO_TEAM)}, max_tokens: 0, unit: \"tokens\", window: \"day\"}}"
        "]}\n"
        "_rate = gateway.from_catalog(_profile, _caps)\n"
        "_selector = {\"thalamus.dev/gateway-keys\": \"true\"}\n"
        "items = [\n" + ",\n".join(variants) +
        ",\ngateway.limiter_config(\"inference\", _rate)\n]\n"
    )
    # Place the temporary entry point in the module so KCL resolves imports
    # from this checkout. It is removed even if rendering fails.
    with tempfile.TemporaryDirectory(prefix=".gateway-policy-smoke-", dir=ROOT) as directory:
        entry = Path(directory) / "main.k"
        entry.write_text(source, encoding="utf-8")
        result = command(["kcl", "run", str(entry), "--format", "json"], cwd=ROOT, timeout=120)
    if result.returncode:
        raise SmokeError(f"KCL render failed:\n{result.stderr.strip() or result.stdout.strip()}")
    try:
        rendered = json.loads(result.stdout)
        if isinstance(rendered, list) and len(rendered) == 1:
            rendered = rendered[0]
        items = rendered["items"]
        policies = {}
        for variant, (route, _candidate, benchmark, _classes) in VARIANTS.items():
            matches = [
                item for item in items
                if item.get("kind") == "AgentgatewayPolicy"
                and item["spec"].get("targetRefs") == [
                    {"group": "gateway.networking.k8s.io", "kind": "HTTPRoute", "name": route}
                ]
            ]
            if len(matches) != 1:
                raise SmokeError(f"{variant}: expected one rendered route policy, found {len(matches)}")
            policies[variant] = translate_policy(matches[0]["spec"]["traffic"], benchmark)
        configs = [
            item for item in items
            if item.get("kind") == "ConfigMap" and item["metadata"]["name"] == "smoke-ratelimit-config"
        ]
        if len(configs) != 1 or not configs[0]["data"]["config.yaml"]:
            raise SmokeError("expected one rendered RLS ConfigMap with config.yaml")
        return policies, configs[0]["data"]["config.yaml"]
    except (KeyError, TypeError, ValueError, AttributeError) as exc:
        raise SmokeError(f"cannot translate rendered KCL gateway policy: {exc}") from exc


def translate_policy(traffic, benchmark):
    unsupported = set(traffic) - {"apiKeyAuthentication", "authorization", "transformation", "rateLimit"}
    if unsupported:
        raise SmokeError(f"unsupported rendered traffic policy fields: {sorted(unsupported)}")
    if set(traffic["apiKeyAuthentication"]) != {"mode", "configMapSelector"}:
        raise SmokeError("unsupported rendered API key authentication fields")
    if traffic["apiKeyAuthentication"]["mode"] != "Strict":
        raise SmokeError("fixture must render strict API key authentication")
    authz = traffic["authorization"]
    if set(authz) != {"action", "policy"} or set(authz["policy"]) != {"matchExpressions"}:
        raise SmokeError("unsupported rendered authorization fields")
    if authz["action"] != "Require":
        raise SmokeError("fixture must render Require authorization")
    expressions = authz["policy"]["matchExpressions"]
    if not expressions or not isinstance(expressions, list) or not all(isinstance(e, str) for e in expressions):
        raise SmokeError("rendered matchExpressions must be a nonempty list of CEL strings")

    transformation = traffic["transformation"]
    if set(transformation) != {"request"}:
        raise SmokeError("unsupported rendered transformation phases")
    request = transformation["request"]
    unsupported = set(request) - {"set", "remove", "metadata"}
    if unsupported:
        raise SmokeError(f"unsupported rendered request transformations: {sorted(unsupported)}")
    # Standalone uses a map for Kubernetes' ordered set list. Duplicate names
    # cannot be represented faithfully and must not be silently discarded.
    sets, seen = {}, set()
    for item in request.get("set", []):
        if set(item) != {"name", "value"}:
            raise SmokeError("unsupported rendered header set entry")
        name, expression = item["name"], item["value"]
        if not isinstance(name, str) or not isinstance(expression, str):
            raise SmokeError("rendered header sets must have string names and CEL values")
        if name.lower() in seen:
            raise SmokeError(f"cannot translate duplicate rendered header set: {name}")
        seen.add(name.lower())
        sets[name] = expression
    remove = request.get("remove", [])
    metadata = request.get("metadata", {})
    if not isinstance(remove, list) or not all(isinstance(name, str) for name in remove):
        raise SmokeError("rendered request header removals must be a list of names")
    if not isinstance(metadata, dict) or not all(
        isinstance(name, str) and isinstance(expr, str) for name, expr in metadata.items()
    ) or "thalamus_service" not in metadata:
        raise SmokeError("rendered request metadata must contain the service CEL expression")

    limiter = traffic["rateLimit"]
    if set(limiter) != {"global"}:
        raise SmokeError("unsupported rendered rate-limit policy")
    global_limit = limiter["global"]
    if set(global_limit) != {"backendRef", "domain", "failureMode", "descriptors"}:
        raise SmokeError("unsupported rendered global rate-limit fields")
    if global_limit["domain"] != "smoke" or global_limit["failureMode"] != "FailClosed":
        raise SmokeError("fixture must use its rendered domain and fail-closed limiter")
    if global_limit["backendRef"] != {"name": "rate-limit", "namespace": "inference", "port": 8081}:
        raise SmokeError("unexpected rendered RLS backend reference")
    descriptors = []
    for descriptor in global_limit["descriptors"]:
        if set(descriptor) != {"entries", "unit", "limitOverride"}:
            raise SmokeError("unsupported rendered rate-limit descriptor")
        if descriptor["unit"] not in ("Requests", "Tokens"):
            raise SmokeError("unsupported rendered rate-limit unit")
        entries = []
        for entry in descriptor["entries"]:
            if set(entry) != {"name", "expression"} or not all(
                isinstance(entry[key], str) for key in ("name", "expression")
            ):
                raise SmokeError("unsupported rendered rate-limit entry")
            entries.append({"key": entry["name"], "value": entry["expression"]})
        if not entries or not isinstance(descriptor["limitOverride"], str):
            raise SmokeError("rendered rate-limit descriptor is incomplete")
        descriptors.append({
            "entries": entries,
            "type": descriptor["unit"].lower(),
            "limitOverride": descriptor["limitOverride"],
        })
    expected = [("benchmark_key", "requests")] if benchmark else [
        ("key_id", "tokens"), ("project", "tokens"), ("team", "tokens")
    ]
    actual = [(entry["entries"][0]["key"], entry["type"]) for entry in descriptors]
    if actual != expected or any(len(entry["entries"]) != 1 for entry in descriptors):
        raise SmokeError(f"unexpected rendered limiter dimensions: {actual!r}")
    return {
        "authorization": {"rules": [{"require": expression} for expression in expressions]},
        "transformations": {
            "request": {"set": sets, "remove": remove, "metadata": metadata},
            # Harness-only probe: observe the rendered request metadata in the
            # response; it is not a replacement for a metrics integration test.
            "response": {"set": {"x-smoke-service": "metadata.thalamus_service"}},
        },
        "remoteRateLimit": {
            "domain": global_limit["domain"],
            "failureMode": global_limit["failureMode"],
            "descriptors": descriptors,
        },
    }


def fixtures():
    missing = object()
    base = {
        "user": USER, "creator": USER, "team": TEAM, "project": PROJECT,
        "kind": "project", "allowed_services": [SERVICE], "expiry": None,
        "budget": None, "project_budget": None, "team_budget": 1000,
        "budget_window": "day", "class": "standard", "run_id": None,
        "deployment": None, "run_kind": None, "request_budget": None,
        "debug_session_id": None, "debug_model": None,
    }
    job = {
        "kind": "job", "run_id": RUN, "deployment": DEPLOYMENT,
        "run_kind": "validation", "expiry": FUTURE, "budget": 50,
    }
    benchmark = {**job, "run_kind": "benchmark", "request_budget": 2, "budget": 0}
    debug = {"kind": "debug", "debug_session_id": SESSION, "debug_model": SERVICE,
             "expiry": FUTURE, "budget": 50}
    cases, keys = [], []

    def create(label, route="public", changes=None, status=200, *, path=CHAT_PATH,
               body=BODY, headers=()):
        metadata = {**base, "key_id": str(uuid.uuid4()), **(changes or {})}
        metadata = {name: value for name, value in metadata.items() if value is not missing}
        case = {
            "label": label, "route": route, "metadata": metadata, "status": status,
            "path": path, "body": body, "headers": headers,
            "key": "thl_smoke_" + secrets.token_hex(24),
        }
        keys.append(case)
        return case

    def add(label, route="public", changes=None, status=200, **kwargs):
        cases.append(create(label, route, changes, status, **kwargs))

    add("project credential")
    add("projectless personal credential", changes={"kind": "personal", "user": PERSONAL_USER, "project": None})
    add("personal credential with project", changes={"kind": "personal", "user": PERSONAL_USER})
    add("future projected expiry and positive budgets", changes={"expiry": FUTURE, "budget": 90, "project_budget": 80, "class": "batch"})
    add("different team with monthly window", changes={"team": MONTH_TEAM, "team_budget": 500, "budget_window": "month"})
    add("validation job on public route", changes=job)
    add("public job may target another deployment", changes={**job, "deployment": "other"})
    add("validation job on bound candidate", "candidate", job)
    add("debug session with matching model", changes=debug)
    add("class disabled removes forged scheduling headers", "no-class", {"class": "not-reviewed"})
    add("projectless class-disabled personal", "no-class", {"kind": "personal", "project": None})

    invalid = [
        ("missing key id", {"key_id": missing}),
        ("malformed key id", {"key_id": "not-a-uuid"}),
        ("missing user", {"user": missing}),
        ("null user", {"user": None}),
        ("non-string user", {"user": 42}),
        ("malformed user", {"user": "not-a-uuid"}),
        ("missing creator", {"creator": missing}),
        ("malformed creator", {"creator": "not-a-uuid"}),
        ("missing team", {"team": missing}),
        ("malformed team", {"team": "not-a-uuid"}),
        ("unconfigured team", {"team": "99999999-9999-4999-8999-999999999999"}),
        ("zero-cap team excluded", {"team": ZERO_TEAM, "team_budget": 0}),
        ("missing kind", {"kind": missing}),
        ("non-string kind", {"kind": 42}),
        ("unknown kind", {"kind": "other"}),
        ("missing project", {"project": missing}),
        ("null project for project key", {"project": None}),
        ("non-string project", {"project": 42}),
        ("malformed project", {"project": "not-a-uuid"}),
        ("malformed personal project", {"kind": "personal", "project": "not-a-uuid"}),
        ("missing allowed services", {"allowed_services": missing}),
        ("non-list allowed services", {"allowed_services": SERVICE}),
        ("service not allowed", {"allowed_services": ["other"]}),
        ("expired projection", {"expiry": "2000-01-01T00:00:00Z"}),
        ("malformed expiry", {"expiry": "not-a-time"}),
        ("non-string expiry", {"expiry": 42}),
        ("zero token budget", {"budget": 0}),
        ("negative token budget", {"budget": -1}),
        ("non-integer token budget", {"budget": "1"}),
        ("fractional token budget", {"budget": 1.5}),
        ("zero project budget", {"project_budget": 0}),
        ("negative project budget", {"project_budget": -1}),
        ("non-integer project budget", {"project_budget": "1"}),
        ("projectless personal with project budget", {"kind": "personal", "project": None, "project_budget": 1}),
        ("missing team budget", {"team_budget": missing}),
        ("non-integer team budget", {"team_budget": "1000"}),
        ("wrong team budget", {"team_budget": 999}),
        ("missing budget window", {"budget_window": missing}),
        ("wrong budget window", {"budget_window": "month"}),
        ("monthly team rejects daily projection", {"team": MONTH_TEAM, "team_budget": 500, "budget_window": "day"}),
        ("missing traffic class", {"class": missing}),
        ("non-string traffic class", {"class": 5}),
        ("unreviewed traffic class", {"class": "priority"}),
    ]
    for label, changes in invalid:
        add(label, changes=changes, status=403)
    for label, changes in [
        ("missing job run id", {"run_id": missing}),
        ("invalid job run id", {"run_id": "bad"}),
        ("non-string job run id", {"run_id": 7}),
        ("missing job deployment", {"deployment": missing}),
        ("empty job deployment", {"deployment": ""}),
        ("missing job expiry", {"expiry": None}),
        ("missing job budget", {"budget": None}),
        ("unknown job run kind", {"run_kind": "other"}),
        ("benchmark job on validation route", {"run_kind": "benchmark"}),
        ("projectless job", {"project": None}),
    ]:
        add(label, changes={**job, **changes}, status=403)
    add("candidate rejects wrong deployment", "candidate", {**job, "deployment": "other"}, 403)
    add("candidate rejects project key", "candidate", status=403)
    add("candidate rejects debug key", "candidate", debug, 403)
    for label, changes in [
        ("missing debug session", {"debug_session_id": missing}),
        ("malformed debug session", {"debug_session_id": "bad"}),
        ("missing debug model", {"debug_model": missing}),
        ("different debug model", {"debug_model": "other"}),
        ("missing debug expiry", {"expiry": None}),
        ("missing debug budget", {"budget": None}),
    ]:
        add(label, changes={**debug, **changes}, status=403)
    add("debug request rejects another model", changes=debug, status=403,
        body=b'{"model":"other","messages":[]}')
    add("debug request rejects absent model", changes=debug, status=403, body=b'{"messages":[]}')
    add("debug request rejects malformed body", changes=debug, status=403, body=b'not-json')
    add("debug completion checks body model", changes=debug, path=COMPLETIONS_PATH)
    add("unknown thalamus header is rejected", status=403, headers=(("X-Thalamus-Untrusted", "forged"),))
    add("unknown inference header is rejected", status=403, headers=(("X-LLM-D-Inference-Untrusted", "forged"),))

    add("public benchmark accepts zero token budget", "benchmark", benchmark, path=COMPLETIONS_PATH)
    add("candidate benchmark accepts bound job", "candidate-benchmark", benchmark, path=COMPLETIONS_PATH)
    add("benchmark accepts maximum request budget", "benchmark",
        {**benchmark, "request_budget": 1000000}, path=COMPLETIONS_PATH)
    add("benchmark accepts zero project budget", "benchmark",
        {**benchmark, "project_budget": 0}, path=COMPLETIONS_PATH)
    add("benchmark rejects project key", "benchmark", status=403, path=COMPLETIONS_PATH)
    add("benchmark rejects validation run", "benchmark", job, 403, path=COMPLETIONS_PATH)
    add("candidate benchmark rejects wrong deployment", "candidate-benchmark",
        {**benchmark, "deployment": "other"}, 403, path=COMPLETIONS_PATH)
    for label, value in [
        ("missing benchmark request budget", missing),
        ("null benchmark request budget", None),
        ("zero benchmark request budget", 0),
        ("negative benchmark request budget", -1),
        ("oversized benchmark request budget", 1000001),
        ("non-integer benchmark request budget", "2"),
        ("boolean benchmark request budget", True),
        ("fractional benchmark request budget", 1.5),
    ]:
        add(label, "benchmark", {**benchmark, "request_budget": value}, 403, path=COMPLETIONS_PATH)

    # These keys are reserved for repeated requests, proving a real Requests
    # limit, a distinct key bucket, and fail-closed behavior when RLS is down.
    count_two = create("two-request benchmark", "benchmark", benchmark, path=COMPLETIONS_PATH)
    count_one = create("different key in same team", "candidate-benchmark",
                       {**benchmark, "request_budget": 1}, path=COMPLETIONS_PATH)
    count_month = create("monthly two-request benchmark", "benchmark",
                         {**benchmark, "team": MONTH_TEAM, "team_budget": 500,
                          "budget_window": "month"}, path=COMPLETIONS_PATH)
    outage = create("benchmark during RLS outage", "benchmark", benchmark, path=COMPLETIONS_PATH)
    return cases, keys, count_two, count_one, count_month, outage


class EchoServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), EchoHandler)
        self.proof = secrets.token_hex(16)
        self.records = {}
        self.lock = threading.Lock()


class EchoHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        headers = {}
        for name, value in self.headers.raw_items():
            headers.setdefault(name.lower(), []).append(value)
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        record = {
            "proof": self.server.proof,
            "path": self.path,
            "body": body.decode("utf-8", errors="replace"),
            "headers": headers,
        }
        request_id = self.headers.get("x-smoke-request-id")
        with self.server.lock:
            self.server.records[request_id] = record
        data = json.dumps(record).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, _format, *_args):
        pass


def available_ports(count):
    listeners = []
    try:
        for _ in range(count):
            listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            listeners.append(listener)
            listener.bind(("127.0.0.1", 0))
        return [listener.getsockname()[1] for listener in listeners]
    except OSError as exc:
        raise SmokeError(f"cannot allocate loopback ports: {exc}") from exc
    finally:
        for listener in listeners:
            listener.close()


def route_host(variant):
    return f"{variant}.gateway.smoke.invalid"


def gateway_config(policies, keys, gateway_port, backend_port, rls_port):
    gateway = {
        "bindAddress": "127.0.0.1", "port": gateway_port,
        # All rendered variants are Strict; standalone authentication is at
        # the gateway and receives only local hashes and fixture metadata.
        "apiKey": {
            "mode": "strict",
            "keys": [
                {"keyHash": "sha256:" + hashlib.sha256(case["key"].encode()).hexdigest(),
                 "metadata": case["metadata"]}
                for case in keys
            ],
        },
    }
    routes = []
    for variant, policy in policies.items():
        local_policy = {
            **policy,
            # The only adaptation of backendRef is the address of our local
            # RLS; all descriptors, CEL, domain and failure mode are rendered.
            "remoteRateLimit": {**policy["remoteRateLimit"], "host": f"127.0.0.1:{rls_port}"},
        }
        routes.append({
            "name": variant, "hostnames": [route_host(variant)], "policies": local_policy,
            "backends": [{"host": f"127.0.0.1:{backend_port}"}],
        })
    return {
        "config": {"adminAddr": "off", "statsAddr": "off", "readinessAddr": "off", "enableIpv6": False},
        "gateways": {"default": gateway}, "routes": routes,
    }


def docker_logs(name):
    try:
        result = command(["docker", "logs", name], timeout=10)
        return (result.stdout + result.stderr).strip()
    except SmokeError as exc:
        return str(exc)


def wait_for_listener(port, name):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.25):
                return
        except OSError:
            time.sleep(0.1)
    logs = docker_logs(name)
    raise SmokeError(
        f"{name} did not listen on 127.0.0.1:{port}; check Docker host networking, "
        f"the ephemeral port, and container startup. Docker logs:\n{logs or '(none)'}"
    )


def wait_for_rls_health(port, name):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=1)
        try:
            conn.request("GET", "/healthcheck")
            response = conn.getresponse()
            response.read()
            if response.status == 200:
                return
        except (OSError, http.client.HTTPException):
            pass
        finally:
            conn.close()
        time.sleep(0.1)
    raise SmokeError(f"RLS did not become healthy with configuration loaded:\n{docker_logs(name) or '(none)'}")


def request(port, case):
    request_id = secrets.token_hex(16)
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        conn.putrequest("POST", case["path"], skip_host=True, skip_accept_encoding=True)
        conn.putheader("Host", route_host(case["route"]))
        conn.putheader("Content-Type", "application/json")
        conn.putheader("Content-Length", str(len(case["body"])))
        conn.putheader("X-Smoke-Request-Id", request_id)
        conn.putheader("X-Smoke-Control", "intact")
        # Duplicate mixed-case headers detect overwrites that leave forged values behind.
        for name in ("user", "project", "team", "class", "backend"):
            conn.putheader(f"X-Thalamus-{name}", f"forged-{name}-one")
            conn.putheader(f"x-thalamus-{name}", f"forged-{name}-two")
        for name in ("objective", "fairness-id"):
            conn.putheader(f"X-LLM-D-Inference-{name}", f"forged-{name}-one")
            conn.putheader(f"x-llm-d-inference-{name}", f"forged-{name}-two")
        conn.putheader("X-Api-Key", "forged-api-key")
        for name, value in case["headers"]:
            conn.putheader(name, value)
        if case["key"] is not None:
            conn.putheader("Authorization", f"Bearer {case['key']}")
        conn.endheaders(case["body"])
        response = conn.getresponse()
        status, response_headers, body = response.status, response.getheaders(), response.read()
        return request_id, status, response_headers, body
    except (OSError, http.client.HTTPException) as exc:
        raise SmokeError(f"request to gateway failed on loopback port {port}: {exc}") from exc
    finally:
        conn.close()


def check_forwarded(case, port, backend, *, quota=None):
    label, metadata = case["label"], case["metadata"]
    started = int(time.time())
    request_id, status, response_headers, body = request(port, case)
    finished = int(time.time())
    if status != 200:
        raise SmokeError(f"{label}: expected HTTP 200, got {status}: {body[:300]!r}")
    try:
        observed = json.loads(body)
    except (ValueError, UnicodeDecodeError) as exc:
        raise SmokeError(f"{label}: backend did not return an echo response") from exc
    with backend.lock:
        recorded = backend.records.get(request_id)
    if not isinstance(observed, dict) or observed != recorded or observed.get("proof") != backend.proof:
        raise SmokeError(f"{label}: response did not come from this run's echo backend")
    headers = observed["headers"]
    if observed["path"] != case["path"] or observed["body"] != case["body"].decode():
        raise SmokeError(f"{label}: echo backend received the wrong request")
    if headers.get("x-smoke-request-id") != [request_id] or headers.get("x-smoke-control") != ["intact"]:
        raise SmokeError(f"{label}: backend did not receive the control headers intact")
    expected = {
        "x-thalamus-user": [metadata["user"]],
        "x-thalamus-project": [] if metadata["project"] is None else [metadata["project"]],
        "x-thalamus-team": [metadata["team"]],
    }
    if VARIANTS[case["route"]][3]:
        expected.update({
            "x-thalamus-class": [metadata["class"]],
            "x-llm-d-inference-objective": [f"{SERVICE}-{metadata['class']}"],
            "x-llm-d-inference-fairness-id": [metadata["project"] or metadata["team"]],
        })
    else:
        expected.update({
            "x-thalamus-class": [],
            "x-llm-d-inference-objective": [],
            "x-llm-d-inference-fairness-id": [],
        })
    for name, values in expected.items():
        if headers.get(name, []) != values:
            raise SmokeError(f"{label}: expected {name} {values!r}, received {headers.get(name, [])!r}")
    for name in ("x-thalamus-backend", "x-api-key", "authorization"):
        if name in headers:
            raise SmokeError(f"{label}: unexpected {name} reached the backend: {headers[name]!r}")
    services = [value for name, value in response_headers if name.lower() == "x-smoke-service"]
    if services != [SERVICE]:
        raise SmokeError(f"{label}: rendered request metadata was not observed in response: {services!r}")
    if quota is not None:
        limit, remaining, window_seconds = quota
        for name, value in (("x-ratelimit-limit", limit), ("x-ratelimit-remaining", remaining)):
            values = [v for n, v in response_headers if n.lower() == name]
            if values != [str(value)]:
                raise SmokeError(f"{label}: expected {name}={value}, received {values!r}")
        resets = [v for n, v in response_headers if n.lower() == "x-ratelimit-reset"]
        try:
            reset = int(resets[0]) if len(resets) == 1 else -1
        except ValueError:
            reset = -1
        # Pinned Envoy RLS defines a month as 30 days since Unix epoch.
        expected_resets = (window_seconds - t % window_seconds for t in (started, finished))
        if reset < 1 or not any(abs(reset - expected) <= 2 for expected in expected_resets):
            raise SmokeError(f"{label}: unexpected {window_seconds}s window reset {resets!r}")
    print(f"PASS {label}")


def check_rejected(case, expected_status, port, backend):
    request_id, status, _headers, body = request(port, case)
    with backend.lock:
        reached_backend = request_id in backend.records
    if reached_backend:
        raise SmokeError(f"{case['label']}: rejected request reached the backend")
    if status != expected_status:
        raise SmokeError(f"{case['label']}: expected HTTP {expected_status}, got {status}: {body[:300]!r}")
    print(f"PASS {case['label']}")


def cleanup_container(name, owner):
    # Only remove the uniquely labeled container from this run, including when
    # docker run failed or was interrupted after creating it.
    result = command(
        ["docker", "inspect", "--format", '{{ index .Config.Labels "thalamus.smoke.owner" }}', name],
        timeout=10,
    )
    if result.returncode:
        if "no such object" in result.stderr.lower() or "no such container" in result.stderr.lower():
            return
        raise SmokeError(f"could not inspect temporary container for cleanup: {result.stderr.strip()}")
    if result.stdout.strip() != owner:
        raise SmokeError(f"refusing to remove container {name}: ownership label differs")
    result = command(["docker", "rm", "-f", name], timeout=20)
    if result.returncode and "no such container" not in result.stderr.lower():
        raise SmokeError(f"could not remove temporary container {name}: {result.stderr.strip()}")


def run_smoke():
    if not sys.platform.startswith("linux"):
        raise SmokeError("this smoke requires Linux Docker host networking and a local daemon")
    for tool in ("kcl", "docker"):
        if shutil.which(tool) is None:
            raise SmokeError(f"required tool not found: {tool}")
    policies, rls_config = render_policies()
    cases, keys, count_two, count_one, count_month, outage = fixtures()
    try:
        backend = EchoServer()
    except OSError as exc:
        raise SmokeError(f"cannot bind loopback echo backend: {exc}") from exc
    thread = threading.Thread(target=backend.serve_forever, daemon=True)
    thread.start()
    owner = secrets.token_hex(16)
    prefix = "thalamus-policy-smoke-" + owner
    gateway_name, rls_name, redis_name = (prefix + "-gateway", prefix + "-rls", prefix + "-redis")
    attempted = []
    failure = None

    def start(name, options, image, args):
        # Record before docker run in case it is interrupted after creation.
        attempted.append(name)
        result = command(
            ["docker", "run", "--detach", "--name", name,
             "--label", f"thalamus.smoke.owner={owner}", "--network", "host"]
            + options + [image] + args,
            timeout=180,
        )
        if result.returncode:
            raise SmokeError(f"Docker could not start {image}:\n{result.stderr.strip() or result.stdout.strip()}")

    try:
        # Distinct ports are reserved together and released before the
        # containers bind. An external process can still win that small race.
        gateway_port, rls_port, rls_http_port, rls_debug_port, redis_port = available_ports(5)
        config = gateway_config(policies, keys, gateway_port, backend.server_port, rls_port)
        with tempfile.TemporaryDirectory(prefix="thalamus-gateway-smoke-", dir="/tmp") as directory:
            os.chmod(directory, 0o755)
            config_path = Path(directory) / "gateway.yaml"
            config_path.write_text(json.dumps(config), encoding="utf-8")
            os.chmod(config_path, 0o644)  # Unprivileged image; only key hashes are written.
            rls_directory = Path(directory) / "rls"
            rls_directory.mkdir(mode=0o755)
            (rls_directory / "config.yaml").write_text(rls_config, encoding="utf-8")
            os.chmod(rls_directory / "config.yaml", 0o644)

            start(redis_name, [], REDIS_IMAGE, [
                "redis-server", "--bind", "127.0.0.1", "--port", str(redis_port),
                "--save", "", "--appendonly", "no",
            ])
            wait_for_listener(redis_port, redis_name)
            environment = {
                "USE_STATSD": "false", "LOG_LEVEL": "warn",
                "RUNTIME_ROOT": "/data", "RUNTIME_SUBDIRECTORY": "ratelimit",
                "RUNTIME_IGNOREDOTFILES": "true", "HEALTHY_WITH_AT_LEAST_ONE_CONFIG_LOADED": "true",
                "REDIS_SOCKET_TYPE": "tcp", "REDIS_URL": f"127.0.0.1:{redis_port}",
                "GRPC_HOST": "127.0.0.1", "GRPC_PORT": str(rls_port),
                "HOST": "127.0.0.1", "PORT": str(rls_http_port),
                "DEBUG_HOST": "127.0.0.1", "DEBUG_PORT": str(rls_debug_port),
            }
            env_args = [arg for name, value in environment.items() for arg in ("--env", f"{name}={value}")]
            start(rls_name, env_args + [
                "--mount", f"type=bind,src={rls_directory},dst=/data/ratelimit/config,readonly",
            ], RLS_IMAGE, ["/bin/ratelimit"])
            wait_for_listener(rls_port, rls_name)
            wait_for_rls_health(rls_http_port, rls_name)
            start(gateway_name, [
                "--mount", f"type=bind,src={config_path},dst=/config.yaml,readonly",
            ], IMAGE, ["-f", "/config.yaml"])
            wait_for_listener(gateway_port, gateway_name)

            for case in cases:
                if case["status"] == 200:
                    check_forwarded(case, gateway_port, backend)
                else:
                    check_rejected(case, case["status"], gateway_port, backend)
            for variant in ("public", "candidate-benchmark"):
                auth_case = {**cases[0], "route": variant}
                check_rejected({**auth_case, "label": f"{variant} missing credential", "key": None},
                               401, gateway_port, backend)
                check_rejected({
                    **auth_case, "label": f"{variant} invalid credential",
                    "key": "thl_smoke_unknown_" + secrets.token_hex(16),
                }, 401, gateway_port, backend)

            # A budget of two must override the rendered one-unit RLS fallback.
            # The same key shares its count across public and candidate routes;
            # a different key in the same team/project has its own counter.
            check_forwarded(count_two, gateway_port, backend)
            check_forwarded({**count_two, "route": "candidate-benchmark", "label": "same benchmark key on candidate"},
                            gateway_port, backend)
            check_rejected({**count_two, "label": "benchmark request budget exhausted"}, 429, gateway_port, backend)
            check_forwarded(count_one, gateway_port, backend)
            check_rejected({**count_one, "route": "benchmark", "label": "one-request budget exhausted"},
                           429, gateway_port, backend)
            check_forwarded(count_month, gateway_port, backend, quota=(2, 1, 30 * 86400))
            check_forwarded({**count_month, "label": "second monthly benchmark request"},
                            gateway_port, backend, quota=(2, 0, 30 * 86400))
            check_rejected({**count_month, "label": "monthly benchmark budget exhausted"},
                           429, gateway_port, backend)

            check_forwarded({**outage, "label": "benchmark before RLS outage"}, gateway_port, backend)
            cleanup_container(rls_name, owner)
            check_rejected(outage, 500, gateway_port, backend)
    except (SmokeError, OSError, KeyboardInterrupt) as exc:
        failure = exc
    finally:
        for name in reversed(attempted):
            try:
                cleanup_container(name, owner)
            except SmokeError as exc:
                if failure is None:
                    failure = exc
                else:
                    print(f"cleanup error: {exc}", file=sys.stderr)
        backend.shutdown()
        backend.server_close()
        thread.join(timeout=5)
    if failure is not None:
        if isinstance(failure, KeyboardInterrupt):
            raise failure
        raise SmokeError(str(failure)) from failure
    print("PASS standalone gateway policy and benchmark request limiting "
          "(not Kubernetes conversion, AI token accounting, metrics, or EPP)")


def interrupted(_signum, _frame):
    raise KeyboardInterrupt


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    try:
        run_smoke()
    except KeyboardInterrupt:
        print("gateway smoke interrupted", file=sys.stderr)
        sys.exit(130)
    except SmokeError as exc:
        print(f"gateway smoke failed: {exc}", file=sys.stderr)
        sys.exit(1)
