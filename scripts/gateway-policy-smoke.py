#!/usr/bin/env python3
"""Opt-in standalone gateway behavior check; run with python3 scripts/gateway-policy-smoke.py.

Requires KCL, Python 3, and a local Linux Docker daemon with host networking.
This exercises manually adapted policy expressions and transformations, not the
Kubernetes controller's policy conversion, ConfigMap selection, or deployment.
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
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


ROOT = Path(__file__).resolve().parent.parent
IMAGE = "cr.agentgateway.dev/agentgateway:v1.7.0"
PATH = "/v1/chat/completions"
BODY = b'{"messages":[]}'
USER = "11111111-1111-4111-8111-111111111111"
PERSONAL_USER = "22222222-2222-4222-8222-222222222222"
PROJECT = "33333333-3333-4333-8333-333333333333"
TEAM = "44444444-4444-4444-8444-444444444444"


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


def render_policy():
    result = command(
        ["kcl", "run", "main.k", "--format", "json", "-D",
         "service_file=catalog/services/example-service.yaml"],
        cwd=ROOT,
        timeout=120,
    )
    if result.returncode:
        raise SmokeError(f"KCL render failed:\n{result.stderr.strip() or result.stdout.strip()}")
    try:
        rendered = json.loads(result.stdout)
        # Some KCL versions wrap a single output document in a JSON array.
        if isinstance(rendered, list) and len(rendered) == 1:
            rendered = rendered[0]
        items = rendered["items"]
        candidates = [
            item for item in items
            if item.get("kind") == "AgentgatewayPolicy"
            and any(ref.get("kind") == "HTTPRoute" for ref in item["spec"].get("targetRefs", []))
        ]
        if len(candidates) != 1:
            raise SmokeError(f"expected one HTTPRoute gateway policy, found {len(candidates)}")
        traffic = candidates[0]["spec"]["traffic"]
        authn = traffic.get("apiKeyAuthentication")
        authz = traffic.get("authorization")
        request = traffic.get("transformation", {}).get("request", {})
        unsupported = set(request) - {"set", "remove"}
        if unsupported:
            raise SmokeError(f"unsupported rendered request transformations: {sorted(unsupported)}")

        # Standalone represents Kubernetes' ordered set list as a header map.
        # Reject duplicate names instead of silently changing their semantics.
        sets = {}
        seen = set()
        for item in request.get("set", []):
            name, expression = item["name"], item["value"]
            if not isinstance(name, str) or not isinstance(expression, str):
                raise SmokeError("rendered header set entries must have string names and CEL values")
            if name.lower() in seen:
                raise SmokeError(f"cannot translate duplicate rendered header set: {name}")
            seen.add(name.lower())
            sets[name] = expression
        remove = request.get("remove", [])
        if not isinstance(remove, list) or not all(isinstance(name, str) for name in remove):
            raise SmokeError("rendered request header removals must be a list of names")

        standalone_authz = None
        if authz is not None:
            action = authz["action"].lower()
            if action not in ("require", "allow", "deny"):
                raise SmokeError(f"unsupported rendered authorization action: {action}")
            expressions = authz["policy"]["matchExpressions"]
            if not isinstance(expressions, list) or not all(isinstance(e, str) for e in expressions):
                raise SmokeError("rendered matchExpressions must be a list of CEL strings")
            standalone_authz = {"rules": [{action: expression} for expression in expressions]}
        mode = authn.get("mode", "Optional").lower() if authn is not None else None
        if mode not in (None, "strict", "optional", "permissive"):
            raise SmokeError(f"unsupported rendered API key authentication mode: {mode}")
        return mode, standalone_authz, {"set": sets, "remove": remove}
    except (KeyError, TypeError, ValueError, AttributeError) as exc:
        raise SmokeError(f"cannot translate rendered KCL gateway policy: {exc}") from exc


def fixtures():
    project = {"user": USER, "kind": "project", "project": PROJECT, "team": TEAM}
    personal = {"user": PERSONAL_USER, "kind": "personal", "project": None, "team": TEAM}
    cases = [
        ("project credential", project, USER, PROJECT),
        ("projectless personal credential", personal, PERSONAL_USER, None),
        ("job credential", {**project, "kind": "job"}, USER, PROJECT),
        ("personal credential with project", {**personal, "project": PROJECT}, PERSONAL_USER, PROJECT),
        ("missing user", {key: value for key, value in project.items() if key != "user"}, None, None),
        ("null user", {**project, "user": None}, None, None),
        ("non-string user", {**project, "user": 42}, None, None),
        ("malformed user UUID", {**project, "user": "not-a-uuid"}, None, None),
        ("missing kind", {key: value for key, value in project.items() if key != "kind"}, None, None),
        ("unknown kind", {**project, "kind": "other"}, None, None),
        ("non-string kind", {**project, "kind": 42}, None, None),
        ("missing project for project key", {key: value for key, value in project.items() if key != "project"}, None, None),
        ("null project for project key", {**project, "project": None}, None, None),
        ("null project for job key", {**project, "kind": "job", "project": None}, None, None),
        ("malformed project UUID", {**project, "project": "not-a-uuid"}, None, None),
        ("non-string project", {**project, "project": 42}, None, None),
        ("malformed personal project", {**personal, "project": "not-a-uuid"}, None, None),
    ]
    return [(label, "thl_smoke_" + secrets.token_hex(24), metadata, user, project_id)
            for label, metadata, user, project_id in cases]


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


def available_port():
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
            listener.bind(("127.0.0.1", 0))
            return listener.getsockname()[1]
    except OSError as exc:
        raise SmokeError(f"cannot allocate a loopback gateway port: {exc}") from exc


def gateway_config(mode, authorization, transform, keys, gateway_port, backend_port):
    gateway = {"bindAddress": "127.0.0.1", "port": gateway_port}
    if mode is not None:
        gateway["apiKey"] = {
            "mode": mode,
            "keys": [
                {"keyHash": "sha256:" + hashlib.sha256(key.encode()).hexdigest(), "metadata": metadata}
                for _, key, metadata, _, _ in keys
            ],
        }
    policies = {"transformations": {"request": transform}}
    if authorization is not None:
        policies["authorization"] = authorization
    return {
        "config": {"adminAddr": "off", "statsAddr": "off", "readinessAddr": "off", "enableIpv6": False},
        "gateways": {"default": gateway},
        "routes": [{"policies": policies, "backends": [{"host": f"127.0.0.1:{backend_port}"}]}],
    }


def docker_logs(name):
    try:
        result = command(["docker", "logs", name], timeout=10)
        return (result.stdout + result.stderr).strip()
    except SmokeError as exc:
        return str(exc)


def wait_for_listener(port, name):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.25):
                return
        except OSError:
            time.sleep(0.1)
    logs = docker_logs(name)
    raise SmokeError(
        f"gateway did not listen on 127.0.0.1:{port}; check Docker host networking, "
        f"the ephemeral port, and gateway startup. Docker logs:\n{logs or '(none)'}"
    )


def request(port, key):
    request_id = secrets.token_hex(16)
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        conn.putrequest("POST", PATH, skip_host=True, skip_accept_encoding=True)
        conn.putheader("Host", "inference.example.invalid")
        conn.putheader("Content-Type", "application/json")
        conn.putheader("Content-Length", str(len(BODY)))
        conn.putheader("X-Smoke-Request-Id", request_id)
        conn.putheader("X-Smoke-Control", "intact")
        # Duplicate mixed-case headers detect overwrites that leave forged values behind.
        for name in ("user", "project", "team", "class", "backend"):
            conn.putheader(f"X-Thalamus-{name}", f"forged-{name}-one")
            conn.putheader(f"x-thalamus-{name}", f"forged-{name}-two")
        if key is not None:
            conn.putheader("X-Api-Key", "forged-api-key")
            conn.putheader("Authorization", f"Bearer {key}")
        conn.endheaders(BODY)
        response = conn.getresponse()
        return request_id, response.status, response.read()
    except (OSError, http.client.HTTPException) as exc:
        raise SmokeError(f"request to gateway failed on loopback port {port}: {exc}") from exc
    finally:
        conn.close()


def check_forwarded(label, key, expected_user, expected_project, port, backend):
    request_id, status, body = request(port, key)
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
    if observed["path"] != PATH or observed["body"] != BODY.decode():
        raise SmokeError(f"{label}: echo backend received the wrong request")
    if headers.get("x-smoke-request-id") != [request_id] or headers.get("x-smoke-control") != ["intact"]:
        raise SmokeError(f"{label}: backend did not receive the control headers intact")
    if headers.get("x-thalamus-user") != [expected_user]:
        raise SmokeError(f"{label}: backend user is not exactly the verified user: {headers.get('x-thalamus-user')!r}")
    projects = headers.get("x-thalamus-project", [])
    if projects != ([] if expected_project is None else [expected_project]):
        raise SmokeError(f"{label}: backend project is not exactly the verified project: {projects!r}")
    for name in ("x-thalamus-team", "x-thalamus-class", "x-thalamus-backend", "x-api-key", "authorization"):
        if name in headers:
            raise SmokeError(f"{label}: unexpected {name} reached the backend: {headers[name]!r}")
    print(f"PASS {label}")


def check_rejected(label, key, expected_status, port, backend):
    request_id, status, body = request(port, key)
    with backend.lock:
        reached_backend = request_id in backend.records
    if reached_backend:
        raise SmokeError(f"{label}: rejected request reached the backend")
    if status != expected_status:
        raise SmokeError(f"{label}: expected HTTP {expected_status}, got {status}: {body[:300]!r}")
    print(f"PASS {label}")


def cleanup_container(name, owner):
    # Only remove the uniquely labeled container from this run, including when
    # docker run failed or was interrupted after creating it.
    result = command(
        ["docker", "inspect", "--format", '{{ index .Config.Labels "thalamus.smoke.owner" }}', name],
        timeout=10,
    )
    if result.returncode:
        if "No such object" in result.stderr or "No such container" in result.stderr:
            return
        raise SmokeError(f"could not inspect temporary container for cleanup: {result.stderr.strip()}")
    if result.stdout.strip() != owner:
        raise SmokeError(f"refusing to remove container {name}: ownership label differs")
    result = command(["docker", "rm", "-f", name], timeout=20)
    if result.returncode and "No such container" not in result.stderr:
        raise SmokeError(f"could not remove temporary container {name}: {result.stderr.strip()}")


def run_smoke():
    if not sys.platform.startswith("linux"):
        raise SmokeError("this smoke requires Linux Docker host networking and a local daemon")
    for tool in ("kcl", "docker"):
        if shutil.which(tool) is None:
            raise SmokeError(f"required tool not found: {tool}")
    mode, authorization, transform = render_policy()
    cases = fixtures()
    try:
        backend = EchoServer()
    except OSError as exc:
        raise SmokeError(f"cannot bind loopback echo backend: {exc}") from exc
    thread = threading.Thread(target=backend.serve_forever, daemon=True)
    thread.start()
    owner = secrets.token_hex(16)
    name = "thalamus-policy-smoke-" + owner
    attempted_container = False
    failure = None
    try:
        # The port must be released before agentgateway binds it. A competing
        # process can win that small race; startup/echo checks then fail clearly.
        port = available_port()
        config = gateway_config(mode, authorization, transform, cases, port, backend.server_port)
        with tempfile.TemporaryDirectory(prefix="thalamus-gateway-smoke-", dir="/tmp") as directory:
            os.chmod(directory, 0o755)
            config_path = Path(directory) / "config.yaml"
            config_path.write_text(json.dumps(config), encoding="utf-8")
            os.chmod(config_path, 0o644)  # Image runs as an unprivileged user; keys are hashes.
            attempted_container = True
            result = command(
                ["docker", "run", "--detach", "--name", name,
                 "--label", f"thalamus.smoke.owner={owner}", "--network", "host",
                 "--mount", f"type=bind,src={config_path},dst=/config.yaml,readonly",
                 IMAGE, "-f", "/config.yaml"],
                timeout=180,
            )
            if result.returncode:
                raise SmokeError(f"Docker could not start {IMAGE}:\n{result.stderr.strip() or result.stdout.strip()}")
            wait_for_listener(port, name)
            for label, key, _metadata, user, project in cases:
                if user is None:
                    check_rejected(label, key, 403, port, backend)
                else:
                    check_forwarded(label, key, user, project, port, backend)
            check_rejected("missing credential", None, 401, port, backend)
            check_rejected("invalid credential", "thl_smoke_unknown_" + secrets.token_hex(16), 401, port, backend)
    except (SmokeError, OSError, KeyboardInterrupt) as exc:
        failure = exc
    finally:
        if attempted_container:
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
    print("PASS standalone gateway policy behavior (not Kubernetes policy conversion)")


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
