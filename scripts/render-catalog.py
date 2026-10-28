#!/usr/bin/env python3
"""Render the reviewed catalog into the two Flux-owned generated trees.

Run from the repository root with Python 3 and KCL 0.13.2 on PATH:
    python3 scripts/render-catalog.py --write
    python3 scripts/render-catalog.py --check

The production KCL entrypoints remain authoritative for catalog schemas and
resource construction. JSON resource files and the JSON-formatted kustomization
are also valid YAML; no Python YAML package is needed.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile


# The executable may be installed outside the catalog checkout; its working
# directory is always the reviewed catalog worktree selected by the caller.
ROOT = Path.cwd()
KCL_VERSION = "0.13.2"
DEFAULT_NAMESPACE = "thalamus-inference"
SOURCE_DIRECTORIES = ("services", "limits", "pools")
OUTPUT_ROOTS = ("inference", "pools")

# These are the scopes of the reviewed production KCL recipes, not an attempt
# to infer Kubernetes scope from metadata.namespace. A newly emitted kind or API
# version must be reviewed here before it can enter either Flux inventory.
NAMESPACED_KINDS = {
    ("v1", "ConfigMap"),
    ("v1", "Service"),
    ("apps/v1", "Deployment"),
    ("networking.k8s.io/v1", "NetworkPolicy"),
    ("serving.kserve.io/v1alpha2", "LLMInferenceService"),
    ("agentgateway.dev/v1alpha1", "AgentgatewayBackend"),
    ("agentgateway.dev/v1alpha1", "AgentgatewayPolicy"),
    ("gateway.networking.k8s.io/v1", "HTTPRoute"),
    ("inference.networking.k8s.io/v1", "InferencePool"),
    ("keda.sh/v1alpha1", "ScaledObject"),
    ("kueue.x-k8s.io/v1beta2", "LocalQueue"),
    ("llm-d.ai/v1", "InferenceObjective"),
}
CLUSTER_KINDS = {
    ("kueue.x-k8s.io/v1beta2", "ResourceFlavor"),
    ("kueue.x-k8s.io/v1beta2", "ClusterQueue"),
    ("scheduling.k8s.io/v1", "PriorityClass"),
}
DNS_LABEL = re.compile(r"[a-z0-9](?:[-a-z0-9]*[a-z0-9])?\Z")


class RenderError(Exception):
    pass


def valid_dns_name(value):
    return (
        isinstance(value, str)
        and len(value) <= 253
        and all(len(part) <= 63 and DNS_LABEL.fullmatch(part) for part in value.split("."))
    )


def require_directory(path, *, missing_ok=False):
    if path.is_symlink():
        raise RenderError(f"symlink is not allowed: {path.relative_to(ROOT)}")
    if not path.exists() and missing_ok:
        return False
    if not path.is_dir():
        raise RenderError(f"expected directory: {path.relative_to(ROOT)}")
    return True


def require_file(path):
    if path.is_symlink() or not path.is_file():
        raise RenderError(f"expected regular, non-symlink file: {path.relative_to(ROOT)}")


def source_files():
    """Enumerate all catalog YAML, without following links or unsafe paths."""
    catalog = ROOT / "catalog"
    require_directory(catalog)
    for name in ("artifacts.yaml", "hardware.yaml", "budget-caps.json"):
        require_file(catalog / name)
    for name in ("main.k", "limits.k", "pool.k", "kcl.mod"):
        require_file(ROOT / name)
    templates = ROOT / "templates"
    require_directory(templates)

    def check_templates(directory):
        for entry in sorted(os.scandir(directory), key=lambda item: item.name):
            path = Path(entry.path)
            if entry.is_symlink():
                raise RenderError(f"symlink is not allowed: {path.relative_to(ROOT)}")
            if entry.is_dir(follow_symlinks=False):
                check_templates(path)
            elif not entry.is_file(follow_symlinks=False):
                raise RenderError(f"unsupported template entry: {path.relative_to(ROOT)}")

    check_templates(templates)

    result = {}
    for category in SOURCE_DIRECTORIES:
        directory = catalog / category
        files = []
        if require_directory(directory, missing_ok=True):
            def visit(current):
                for entry in sorted(os.scandir(current), key=lambda item: item.name):
                    path = Path(entry.path)
                    if entry.is_symlink():
                        raise RenderError(f"symlink is not allowed: {path.relative_to(ROOT)}")
                    if entry.is_dir(follow_symlinks=False):
                        visit(path)
                    elif not entry.is_file(follow_symlinks=False):
                        raise RenderError(f"unsupported catalog entry: {path.relative_to(ROOT)}")
                    elif path.suffix in (".yaml", ".yml"):
                        files.append(path.relative_to(ROOT).as_posix())

            visit(directory)
        result[category] = files
    return result


def run_kcl(arguments, context, env):
    try:
        result = subprocess.run(
            ["kcl", *arguments], cwd=ROOT, env=env, capture_output=True, text=True, timeout=120
        )
    except FileNotFoundError as exc:
        raise RenderError(f"KCL {KCL_VERSION} is required on PATH") from exc
    except subprocess.TimeoutExpired as exc:
        raise RenderError(f"KCL timed out while {context}") from exc
    if result.returncode:
        detail = result.stderr.strip() or result.stdout.strip() or "no diagnostic"
        raise RenderError(f"KCL failed while {context}:\n{detail}")
    if result.stderr:
        sys.stderr.write(result.stderr)
    return result.stdout


def check_kcl_version(env):
    version = run_kcl(["version"], "checking its version", env).strip()
    pattern = rf"{re.escape(KCL_VERSION)}(?:-(?:linux|darwin|windows)-[A-Za-z0-9_]+)?"
    if not re.fullmatch(pattern, version):
        raise RenderError(f"KCL {KCL_VERSION} is required; got {version or 'no version'}")


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise RenderError(f"duplicate JSON key in KCL output: {key!r}")
        result[key] = value
    return result


def parse_kcl_json(output, context):
    def invalid_constant(value):
        raise RenderError(f"invalid JSON constant in {context}: {value}")

    try:
        value = json.loads(output, object_pairs_hook=unique_object, parse_constant=invalid_constant)
    except json.JSONDecodeError as exc:
        raise RenderError(f"invalid JSON from {context}: {exc}") from exc
    # KCL's JSON formatter can wrap the single entrypoint document in an array.
    if isinstance(value, list) and len(value) == 1:
        value = value[0]
    return value


def parse_list(output, context):
    value = parse_kcl_json(output, context)
    if not isinstance(value, dict) or value.get("apiVersion") != "v1" or value.get("kind") != "List" or not isinstance(value.get("items"), list):
        raise RenderError(f"{context} did not produce a Kubernetes v1 List")
    return value["items"]


def identity(item, destination, namespace, source):
    if not isinstance(item, dict) or not isinstance(item.get("metadata"), dict):
        raise RenderError(f"invalid Kubernetes object from {source}")
    api_version = item.get("apiVersion")
    kind = item.get("kind")
    if not isinstance(api_version, str) or not isinstance(kind, str):
        raise RenderError(f"missing Kubernetes type from {source}")
    allowed = NAMESPACED_KINDS if destination == "inference" else CLUSTER_KINDS
    if (api_version, kind) not in allowed:
        raise RenderError(f"unreviewed or incorrectly scoped Kubernetes type {api_version}/{kind} from {source}")
    metadata = item["metadata"]
    name = metadata.get("name")
    if not valid_dns_name(name):
        raise RenderError(f"invalid Kubernetes object name {name!r} from {source}")
    actual_namespace = metadata.get("namespace")
    if destination == "inference":
        if actual_namespace != namespace:
            raise RenderError(f"foreign or missing namespace {actual_namespace!r} in {kind}/{name} from {source}")
    elif "namespace" in metadata:
        raise RenderError(f"cluster resource {kind}/{name} unexpectedly has a namespace from {source}")
    # Kubernetes identity is group/kind/namespace/name, not version/kind/name.
    group = api_version.split("/", 1)[0] if "/" in api_version else ""
    return group, kind, actual_namespace or "", name


def render_catalog(namespace, env, temporary_root):
    sources = source_files()
    check_kcl_version(env)
    # main.k builds default pool/profile filenames from service fields. Decode
    # with the same KCL YAML parser and reject unsafe references *before* the
    # production entrypoint can read either file. The production Service schema
    # and main.k remain authoritative for all other catalog validation.
    if sources["services"]:
        preflight = temporary_root / "reference-preflight"
        preflight.mkdir()
        (preflight / "kcl.mod").write_text(
            '[package]\nname = "thalamus"\nedition = "v0.11.0"\nversion = "0.1.0"\n[dependencies]\n',
            encoding="utf-8",
        )
        entrypoint = preflight / "references.k"
        entrypoint.write_text(
            'import file\n'
            'import yaml\n'
            '_document = yaml.decode(file.read(option("service_file", required=True)))\n'
            'hasPool = "pool" in _document\n'
            'pool = _document.pool if hasPool else ""\n'
            'rateLimit = _document.gateway.rateLimit\n',
            encoding="utf-8",
        )
        local_service = preflight / "service.yaml"
        for source in sources["services"]:
            # KCL confines file.read to the preflight module. Copy only the
            # already-enumerated source, and do not follow a substituted link.
            descriptor = os.open(ROOT / source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            with os.fdopen(descriptor, "rb") as input_file:
                if not stat.S_ISREG(os.fstat(input_file.fileno()).st_mode):
                    raise RenderError(f"expected regular service file: {source}")
                with local_service.open("wb") as output_file:
                    shutil.copyfileobj(input_file, output_file)
            output = run_kcl(
                ["run", str(entrypoint), "--format", "json", "-D", f"service_file={local_service}"],
                f"validating {source}",
                env,
            )
            references = parse_kcl_json(output, source)
            if not isinstance(references, dict):
                raise RenderError(f"invalid service references while validating {source}")
            has_pool = references.get("hasPool")
            pool = references.get("pool")
            limit = references.get("rateLimit")
            if not isinstance(has_pool, bool):
                raise RenderError(f"invalid pool reference while validating {source}")
            if has_pool and (
                not isinstance(pool, str) or len(pool) > 56 or not DNS_LABEL.fullmatch(pool)
            ):
                raise RenderError(f"unsafe pool reference while validating {source}")
            if not isinstance(limit, str) or len(limit) > 63 or not DNS_LABEL.fullmatch(limit):
                raise RenderError(f"unsafe rate limit reference while validating {source}")

    objects = {root: [] for root in OUTPUT_ROOTS}
    seen = {}
    entries = (
        ("limits", "limits.k", "limit_file", "inference"),
        ("services", "main.k", "service_file", "inference"),
        ("pools", "pool.k", "pool_file", "pools"),
    )
    for category, entrypoint, option, destination in entries:
        for source in sources[category]:
            output = run_kcl(
                ["run", entrypoint, "--format", "json", "-D", f"{option}={source}", "-D", f"workload_namespace={namespace}"],
                f"rendering {source} with {entrypoint}",
                env,
            )
            for item in parse_list(output, source):
                key = identity(item, destination, namespace, source)
                if key in seen:
                    raise RenderError(f"duplicate Kubernetes object {key}: {seen[key]} and {source}")
                seen[key] = source
                objects[destination].append((key, item))
    for value in objects.values():
        value.sort(key=lambda pair: pair[0])
    return objects


def json_bytes(value):
    return (json.dumps(value, ensure_ascii=False, sort_keys=True, indent=2, allow_nan=False) + "\n").encode("utf-8")


def expected_trees(objects):
    trees = {}
    for root in OUTPUT_ROOTS:
        files = {}
        for key, item in objects[root]:
            digest = hashlib.sha256(json_bytes(key)).hexdigest()
            filename = f"{item['kind'].lower()}-{item['metadata']['name'][:40]}-{digest}.json"
            if filename in files:
                raise RenderError(f"generated resource filename collision: {filename}")
            files[filename] = json_bytes(item)
        files["kustomization.yaml"] = json_bytes({
            "apiVersion": "kustomize.config.k8s.io/v1beta1",
            "kind": "Kustomization",
            "resources": sorted(files),
        })
        trees[root] = files
    return trees


def existing_files(directory):
    """Read an owned tree without following symlinks, including stale entries."""
    if not require_directory(directory, missing_ok=True):
        return None
    files = {}

    def visit(current):
        for entry in sorted(os.scandir(current), key=lambda item: item.name):
            path = Path(entry.path)
            relative = path.relative_to(directory).as_posix()
            if entry.is_symlink():
                raise RenderError(f"symlink is not allowed: {path.relative_to(ROOT)}")
            if entry.is_dir(follow_symlinks=False):
                # Directories are not generated; include even an empty one so
                # --check reports it and --write prunes it.
                files[relative + "/"] = None
                visit(path)
            elif entry.is_file(follow_symlinks=False):
                files[relative] = (path.read_bytes(), bool(entry.stat(follow_symlinks=False).st_mode & 0o111))
            else:
                raise RenderError(f"unsupported generated entry: {path.relative_to(ROOT)}")

    visit(directory)
    return files


def current_trees():
    rendered = ROOT / "rendered"
    if not require_directory(rendered, missing_ok=True):
        return {root: None for root in OUTPUT_ROOTS}
    return {root: existing_files(rendered / root) for root in OUTPUT_ROOTS}


def check_trees(expected, current):
    differences = []
    for root in OUTPUT_ROOTS:
        actual = current[root]
        if actual is None:
            differences.append(f"missing rendered/{root}/")
            continue
        for name in sorted(expected[root].keys() - actual.keys()):
            differences.append(f"missing rendered/{root}/{name}")
        for name in sorted(actual.keys() - expected[root].keys()):
            differences.append(f"unexpected rendered/{root}/{name}")
        for name in sorted(expected[root].keys() & actual.keys()):
            if actual[name] != (expected[root][name], False):
                differences.append(f"changed rendered/{root}/{name}")
    if differences:
        raise RenderError("generated catalog is out of date:\n" + "\n".join(differences))


def write_trees(expected, current):
    rendered = ROOT / "rendered"
    rendered.mkdir(exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=".render-catalog-", dir=rendered))
    backups = set()
    installed = set()
    preserve_staging = False
    try:
        # Prepare both trees completely before changing either owned root.
        for root in OUTPUT_ROOTS:
            stage = staging / root
            stage.mkdir()
            for name, contents in expected[root].items():
                (stage / name).write_bytes(contents)
        try:
            for root in OUTPUT_ROOTS:
                target = rendered / root
                if current[root] is not None:
                    os.replace(target, staging / f"old-{root}")
                    backups.add(root)
                os.replace(staging / root, target)
                installed.add(root)
        except OSError as exc:
            failures = []
            for root in reversed(OUTPUT_ROOTS):
                try:
                    if root in installed:
                        os.replace(rendered / root, staging / f"failed-{root}")
                    if root in backups:
                        os.replace(staging / f"old-{root}", rendered / root)
                except OSError as restore_error:
                    failures.append(f"{root}: {restore_error}")
            if failures:
                preserve_staging = True
                raise RenderError(
                    f"could not publish generated catalog: {exc}; rollback failed ({'; '.join(failures)}); "
                    f"previous trees remain in {staging}"
                ) from exc
            raise RenderError(f"could not publish generated catalog: {exc}; previous trees restored") from exc
    finally:
        if not preserve_staging:
            shutil.rmtree(staging)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    operation = parser.add_mutually_exclusive_group(required=True)
    operation.add_argument("--write", action="store_true", help="replace the owned generated trees")
    operation.add_argument("--check", action="store_true", help="compare generated trees without writing")
    parser.add_argument("--workload-namespace", default=DEFAULT_NAMESPACE)
    args = parser.parse_args()
    if len(args.workload_namespace) > 63 or not DNS_LABEL.fullmatch(args.workload_namespace):
        raise RenderError("workload namespace must be a Kubernetes DNS label of at most 63 characters")

    # Isolate compiler and package-manager caches/locks outside the checkout;
    # concurrent renders must not put lock diagnostics into the JSON output.
    with tempfile.TemporaryDirectory(prefix="thalamus-kcl-") as cache:
        compiler_cache = Path(cache) / "compiler"
        package_cache = Path(cache) / "packages"
        compiler_cache.mkdir()
        package_cache.mkdir()
        env = os.environ.copy()
        env["KCL_CACHE_PATH"] = str(compiler_cache)
        env["KCL_PKG_PATH"] = str(package_cache)
        objects = render_catalog(args.workload_namespace, env, Path(cache))
    expected = expected_trees(objects)
    current = current_trees()
    if args.check:
        check_trees(expected, current)
    else:
        write_trees(expected, current)
        combined = [item for root in OUTPUT_ROOTS for _, item in objects[root]]
        sys.stdout.buffer.write(json_bytes({"apiVersion": "v1", "kind": "List", "items": combined}))


if __name__ == "__main__":
    try:
        main()
    except (RenderError, OSError) as exc:
        print(f"render-catalog: {exc}", file=sys.stderr)
        sys.exit(1)
