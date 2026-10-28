#!/usr/bin/env python3
"""Prepare the offline validator and its pinned Kubernetes/CRD schema bundle.

Preparation requires Python 3.10+, Go 1.24+, HTTPS access to GitHub and the Go
module proxy/checksum database. It never calls kubectl or contacts a cluster.
From the repository root, once per CI image/cache:

    python3 deploy/prepare-schema-bundle.py /tmp/thalamus-schema

Then, entirely offline, validate any number of rendered YAML/JSON files:

    /tmp/thalamus-schema/validate --bundle /tmp/thalamus-schema/schemas.json \
        /tmp/install.yaml /tmp/service.yaml /tmp/pool.yaml

Only pass rendered Kubernetes resources, not catalog, lock, or install-input
files. Input may contain YAML documents and recursively nested v1/List items.
Exit status is nonzero on malformed/unknown/invalid resources or unsupported
schema semantics. This is offline schema and CEL compilation, NOT evidence of
successful admission, webhook execution, policy behavior or a running cluster.

CRD validation uses Kubernetes structural/OpenAPI validation, pruning checks,
defaulting, list-map/set uniqueness and create-time CEL. Built-ins use the
pinned Kubernetes OpenAPI schema, resolving its local references. This is
stricter than normal pruning for unknown fields, while honoring
x-kubernetes-preserve-unknown-fields and int-or-string. Unsupported schema
keywords, builtin union member discrimination, and ambiguous list-map
uniqueness that depends on an unavailable API-server default fail explicitly.
Update-only oldSelf rules, admission webhooks and handwritten API-server checks
cannot be established without the cluster.
Native v1 ValidatingAdmissionPolicy expressions, ordered variables,
matchConditions, messageExpression and audit annotation expressions compile
with the pinned Kubernetes admission CEL environment and are type checked
against each selected pinned resource kind. Wildcard rules, unknown kinds,
non-status subresources, recursive type references and policies exceeding the
documented limits in schema-validator/policy.go fail closed. This is static
compilation, not policy evaluation or an admission decision.

Source references below are the exact pins in deploy/upstreams.yaml. Release
asset SHA-256s are GitHub's published digest, except Flux helm-controller whose
published checksums file supplies it. Git source files are pinned to the
release commit and verified against the published Git blob object id. Flux
controller versions are taken from flux2 v2.6.4/manifests/crds/kustomization.yaml.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request

HERE = Path(__file__).resolve().parent
GITHUB = "https://github.com"
RAW = "https://raw.githubusercontent.com"

# name, upstream version, URL, digest type, published digest
SOURCES = [
    ("kubernetes-openapi.json", "kubernetes-v1.34.2",
     f"{RAW}/kubernetes/kubernetes/8cc511e399b929453cd98ae65b419c3cc227ec79/api/openapi-spec/swagger.json",
     "git-blob-sha1", "305a8a0ee5dcf8292c9cf58aa3676b3b6b39c8d9"),
    ("gateway.yaml", "gateway-api-v1.6.0",
     f"{GITHUB}/kubernetes-sigs/gateway-api/releases/download/v1.6.0/standard-install.yaml",
     "sha256", "a557172e8348f758479e9ee4000bbbb4b4aa48302a6b73461823ea5349bad56d"),
    ("inference.yaml", "gateway-api-inference-extension-v1.6.2",
     f"{GITHUB}/kubernetes-sigs/gateway-api-inference-extension/releases/download/v1.6.2/manifests.yaml",
     "sha256", "38bfba9c452ab54b1afd1f4df15e75b3238c788d8ab007d2f469e6f7ae16fc9f"),
    ("kserve.tgz", "kserve-v0.21.0",
     f"{GITHUB}/kserve/kserve/releases/download/v0.21.0/helm-chart-kserve-llmisvc-crd-v0.21.0.tgz",
     "sha256", "be27803c432cb8db70f1c9c1f72ee8f6d14eaa74393df572212b41f603db081e"),
    ("flux-source.yaml", "flux-v2.6.4/source-controller-v1.6.2",
     f"{GITHUB}/fluxcd/source-controller/releases/download/v1.6.2/source-controller.crds.yaml",
     "sha256", "12d5996717b8fd737e4a37bae18b1e3666c4e812e370614e7c9f453895862b71"),
    ("flux-kustomize.yaml", "flux-v2.6.4/kustomize-controller-v1.6.1",
     f"{GITHUB}/fluxcd/kustomize-controller/releases/download/v1.6.1/kustomize-controller.crds.yaml",
     "sha256", "222ed2e907294263e3d8ee5d42d46996aacdcb490ab60cf05642a5b9bffd0d03"),
    ("flux-helm.yaml", "flux-v2.6.4/helm-controller-v1.3.0",
     f"{GITHUB}/fluxcd/helm-controller/releases/download/v1.3.0/helm-controller.crds.yaml",
     "sha256", "08ebb2da23258f198bf53c007c85fb30ec060b4dc2989a3751415f0d56c26176"),
    ("kueue.yaml", "kueue-v0.18.0",
     f"{GITHUB}/kubernetes-sigs/kueue/releases/download/v0.18.0/manifests.yaml",
     "sha256", "7b37d9dbba62b88f4691e91efe54607a5d680481f4467290efc5b176537c305d"),
    ("keda.yaml", "keda-v2.20.2",
     f"{GITHUB}/kedacore/keda/releases/download/v2.20.2/keda-2.20.2-crds.yaml",
     "sha256", "2356c9ca2aafe42d3171af49064865298c340385fd866b35ef66fc1c00172fba"),
    ("llmd.yaml", "llm-d-router-v1.0.0",
     f"{GITHUB}/llm-d/llm-d-router/releases/download/v1.0.0/manifests.yaml",
     "sha256", "bae0ae787ec97494ff21933882810fb49bdec88b2cf6fe14539ffeedd2ba21e3"),
]
AG_COMMIT = "26f5aebe99a36a037fe54f8080a608aca5547e15"  # v1.7.0 tag
AG_BASE = f"{RAW}/agentgateway/agentgateway/{AG_COMMIT}/controller/install/helm/agentgateway-crds/templates"
for suffix, blob in [
    ("backends", "17c2f71629c39823c555704af80253908fed8131"),
    ("models", "7db48939d4454bdda0eefb765c8885ae98eda82e"),
    ("parameters", "5f7851987f7f1e298e8dcb69a5a2929403248811"),
    ("policies", "e79e1a391a1bbf68f32e0a105ab811874b13f539"),
]:
    SOURCES.append((f"agentgateway-{suffix}.yaml", "agentgateway-v1.7.0",
                    f"{AG_BASE}/agentgateway.dev_agentgateway{suffix}.yaml", "git-blob-sha1", blob))


def digest(data, method):
    if method == "sha256":
        return hashlib.sha256(data).hexdigest()
    if method == "git-blob-sha1":
        return hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()
    raise ValueError(f"unknown digest method: {method}")

def check_upstream_pins():
    # The install inputs deliberately are not Kubernetes resources. Inspect
    # their simple top-level step/version fields here, not in the validator.
    # A format or version change fails closed and requires updating this bundle.
    expected = {
        "gateway-api-standard-crds": "v1.6.0",
        "gateway-inference-extension-crds": "v1.6.2",
        "kserve-llmisvc-crds": "v0.21.0",
        "agentgateway-crds": "v1.7.0",
        "flux": "v2.6.4",
        "kueue": "v0.18.0",
        "keda": "v2.20.2",
    }
    found = {}
    current = None
    text = (HERE / "upstreams.yaml").read_text()
    for line in text.splitlines():
        if line.startswith("  - name: "):
            current = line.removeprefix("  - name: ").strip()
        elif current in expected and line.startswith("    version: "):
            if current in found:
                raise ValueError(f"duplicate version for upstream step {current}")
            found[current] = line.removeprefix("    version: ").strip()
    if found != expected:
        raise ValueError(f"schema source pins differ from deploy/upstreams.yaml: expected {expected}, got {found}")
    if "  routerRelease: https://github.com/llm-d/llm-d-router/releases/tag/v1.0.0" not in text.splitlines():
        raise ValueError("llm-d-router source pin differs from deploy/upstreams.yaml")



def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("bundle", type=Path, help="output directory for the offline executable and schemas")
    args = parser.parse_args()
    check_upstream_pins()
    bundle = args.bundle.resolve()
    bundle.mkdir(parents=True, exist_ok=True)
    # Work in a temporary directory; do not publish a partially extracted bundle.
    with tempfile.TemporaryDirectory(prefix="thalamus-schema-") as tmp:
        work = Path(tmp)
        sources = work / "sources"
        sources.mkdir()
        lock = []
        for name, version, url, method, expected in SOURCES:
            print(f"fetching {version}: {url}", flush=True)
            with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "thalamus-schema-preparation"}), timeout=120) as response:
                data = response.read()
            actual = digest(data, method)
            if actual != expected:
                raise ValueError(f"{name}: {method} mismatch: expected {expected}, got {actual}")
            (sources / name).write_bytes(data)
            lock.append({"name": name, "version": version, "url": url, method: expected,
                         "sha256": hashlib.sha256(data).hexdigest()})
        # These two complete CRDs are literal templates in the official chart.
        # A third CRD is included through .Files.Get; read that exact file.
        members = {
            "kserve-llmisvc-crd/templates/serving.kserve.io_llminferenceservices.yaml": "kserve-llmisvc.yaml",
            "kserve-llmisvc-crd/templates/serving.kserve.io_llminferenceserviceconfigs.yaml": "kserve-config.yaml",
            "kserve-llmisvc-crd/files/serving.kserve.io_clusterstoragecontainers.yaml": "kserve-storage.yaml",
        }
        with tarfile.open(sources / "kserve.tgz", "r:gz") as archive:
            for member, name in members.items():
                entry = archive.getmember(member)
                if not entry.isfile():
                    raise ValueError(f"chart member is not a regular file: {member}")
                data = archive.extractfile(entry).read()
                if b"{{" in data or b"}}" in data:
                    raise ValueError(f"chart CRD requires unsupported Helm rendering: {member}")
                (sources / name).write_bytes(data)
        # Build in temporary workspace, so go.mod/go.sum and module caches never
        # need network access again during validation. Go verifies module sums.
        for file in ("go.mod", "main.go", "policy.go"):
            shutil.copyfile(HERE / "schema-validator" / file, work / file)
        env = os.environ.copy()
        env["GOWORK"] = "off"
        env["GOTOOLCHAIN"] = "local"
        subprocess.run(["go", "mod", "tidy"], cwd=work, env=env, check=True)
        binary = work / "validate"
        subprocess.run(["go", "build", "-trimpath", "-o", str(binary), "."], cwd=work, env=env, check=True)
        output = work / "schemas.json"
        subprocess.run([str(binary), "--prepare", str(sources), "--bundle", str(output)], check=True)
        (work / "sources.json").write_text(json.dumps(lock, indent=2) + "\n")
        shutil.copyfile(binary, bundle / "validate")
        os.chmod(bundle / "validate", 0o755)
        shutil.copyfile(output, bundle / "schemas.json")
        shutil.copyfile(work / "sources.json", bundle / "sources.json")
        shutil.copyfile(work / "go.sum", bundle / "go.sum")
        print(f"prepared {bundle}; schemas.json sha256={hashlib.sha256(output.read_bytes()).hexdigest()}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError, tarfile.TarError) as error:
        raise SystemExit(f"schema preparation failed: {error}")
