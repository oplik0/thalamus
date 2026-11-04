#!/usr/bin/env python3
"""Generate catalog/service.schema.json from KCL's Service schema.

Run from the repository root with Python 3 and KCL 0.13.2 on PATH:
    python3 scripts/generate-service-schema.py
    python3 scripts/generate-service-schema.py --check  # compare without writing

KCL's GenerateDoc API returns a map of named schemas with bare-name references.
This exporter roots that map at Service and retains its complete transitive
reference graph in $defs. It does not invent fields or evaluate KCL expressions;
KCL's normal render and checks remain authoritative for service values.
"""

import argparse
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


ROOT = Path(__file__).resolve().parent.parent
SOURCE = "templates/serving/kserve.k"
OUTPUT = ROOT / "catalog" / "service.schema.json"
KCL_VERSION = "0.13.2"


class ExportError(Exception):
    pass


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ExportError(f"duplicate JSON key {key!r} in KCL output")
        result[key] = value
    return result


def parse_json(text, context):
    def invalid_constant(value):
        raise ExportError(f"invalid JSON constant {value!r} in {context}")

    try:
        return json.loads(text, object_pairs_hook=unique_object, parse_constant=invalid_constant)
    except json.JSONDecodeError as exc:
        raise ExportError(f"invalid JSON in {context}: {exc}") from exc


def check_kcl_version():
    try:
        result = subprocess.run(
            ["kcl", "version"], cwd=ROOT, capture_output=True, text=True, timeout=30
        )
    except FileNotFoundError as exc:
        raise ExportError(f"KCL {KCL_VERSION} is required on PATH") from exc
    except subprocess.TimeoutExpired as exc:
        raise ExportError("timed out checking the KCL version") from exc
    version = result.stdout.strip()
    version_pattern = rf"{re.escape(KCL_VERSION)}(?:-(?:linux|darwin|windows)-[A-Za-z0-9_]+)?"
    if result.returncode != 0 or not re.fullmatch(version_pattern, version):
        detail = result.stderr.strip() or version or "no version reported"
        raise ExportError(f"KCL {KCL_VERSION} is required; got {detail}")


def stop_server(process):
    # KCL may spawn workers. A separate process group prevents orphaned workers
    # even when an error or interrupt occurs while requesting documentation.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()


def generate_native_map():
    check_kcl_version()
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]

    with tempfile.TemporaryFile(mode="w+t", encoding="utf-8") as log:
        try:
            process = subprocess.Popen(
                ["kcl", "server", "--http", f"127.0.0.1:{port}", "--proc", "1"],
                cwd=ROOT,
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        except OSError as exc:
            raise ExportError(f"could not start KCL server: {exc}") from exc
        try:
            deadline = time.monotonic() + 30
            while True:
                if process.poll() is not None:
                    log.seek(0)
                    raise ExportError(f"KCL server exited during startup: {log.read().strip()}")
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=0.25):
                        break
                except OSError:
                    if time.monotonic() >= deadline:
                        log.seek(0)
                        raise ExportError(f"timed out starting KCL server: {log.read().strip()}")
                    time.sleep(0.1)

            request = urllib.request.Request(
                f"http://127.0.0.1:{port}/api:protorpc/KclService.GenerateDoc",
                data=json.dumps({"parse_args": {"paths": [SOURCE]}, "format": "json-schema"}).encode(),
                headers={"Content-Type": "application/json", "Accept": "application/json"},
                method="POST",
            )
            try:
                with urllib.request.urlopen(request, timeout=120) as response:
                    body = response.read().decode("utf-8")
            except urllib.error.HTTPError as exc:
                detail = exc.read().decode("utf-8", errors="replace")
                raise ExportError(f"KCL GenerateDoc returned HTTP {exc.code}: {detail}") from exc
            except (urllib.error.URLError, TimeoutError, UnicodeDecodeError) as exc:
                raise ExportError(f"KCL GenerateDoc request failed: {exc}") from exc

            envelope = parse_json(body, "KCL GenerateDoc response")
            if not isinstance(envelope, dict):
                raise ExportError("KCL GenerateDoc returned a non-object response")
            if envelope.get("error"):
                raise ExportError(f"KCL GenerateDoc failed: {envelope['error']}")
            result = envelope.get("result")
            if not isinstance(result, dict) or not isinstance(result.get("content"), str):
                raise ExportError("KCL GenerateDoc did not return schema content")
            schemas = parse_json(result["content"], "KCL schema map")
            if not isinstance(schemas, dict) or not schemas:
                raise ExportError("KCL GenerateDoc returned an empty or non-object schema map")
            return schemas
        finally:
            stop_server(process)


def pointer_token(name):
    if "%" in name:
        raise ExportError(f"schema name contains an unsupported percent character: {name!r}")
    return name.replace("~", "~0").replace("/", "~1")


def local_target(schema, reference, owner):
    """Resolve a native local pointer before relocating its containing schema."""
    if reference == "#":
        return schema
    if not reference.startswith("#/") or "%" in reference:
        raise ExportError(f"unsupported local reference {reference!r} in {owner}")
    target = schema
    for token in reference[2:].split("/"):
        if re.search(r"~(?![01])", token):
            raise ExportError(f"malformed JSON Pointer {reference!r} in {owner}")
        token = token.replace("~1", "/").replace("~0", "~")
        if isinstance(target, dict) and token in target:
            target = target[token]
        elif isinstance(target, list) and re.fullmatch(r"0|[1-9][0-9]*", token) and int(token) < len(target):
            target = target[int(token)]
        else:
            raise ExportError(f"dangling local reference {reference!r} in {owner}")
    if not isinstance(target, (dict, bool)):
        raise ExportError(f"reference {reference!r} in {owner} does not point to a schema")
    return target


def root_schema(schemas):
    candidates = [name for name in ("Service___main__", "Service") if name in schemas]
    if len(candidates) != 1:
        raise ExportError(f"expected one root Service schema, found {candidates}")
    root_name = candidates[0]
    root = schemas[root_name]
    if not isinstance(root, dict) or root.get("type") != "object" or not isinstance(root.get("properties"), dict) or not root["properties"]:
        raise ExportError(f"KCL {root_name} is not a Service object with properties")

    # The public root is named Service; all other names stay exactly as KCL
    # emitted them so package disambiguation cannot silently collapse types.
    def output_name(name):
        return "Service" if name == root_name else name

    pending = [root_name]
    included = {}
    visited_nodes = set()
    map_keywords = ("properties", "patternProperties", "$defs", "definitions", "dependentSchemas")
    single_keywords = (
        "additionalProperties", "additionalItems", "unevaluatedProperties", "unevaluatedItems",
        "propertyNames", "contains", "contentSchema", "not", "if", "then", "else",
    )
    array_keywords = ("allOf", "anyOf", "oneOf", "prefixItems")

    def visit(node, owner, location):
        if isinstance(node, bool):
            return
        if not isinstance(node, dict):
            raise ExportError(f"expected a schema object or boolean at {location}")
        identity = id(node)
        if identity in visited_nodes:
            return
        visited_nodes.add(identity)
        if any(key in node for key in ("$id", "$dynamicRef", "$recursiveRef")):
            raise ExportError(f"unsupported nested schema identity or dynamic reference at {location}")
        if "$ref" in node:
            reference = node["$ref"]
            if not isinstance(reference, str):
                raise ExportError(f"non-string reference at {location}")
            if reference in schemas:
                pending.append(reference)
                node["$ref"] = "#/$defs/" + pointer_token(output_name(reference))
            elif reference.startswith("#"):
                target = local_target(schemas[owner], reference, owner)
                node["$ref"] = "#/$defs/" + pointer_token(output_name(owner)) + reference[1:]
                visit(target, owner, location + "/$ref")
            else:
                raise ExportError(f"dangling or nonlocal reference {reference!r} at {location}")

        for key in map_keywords:
            if key in node:
                children = node[key]
                if not isinstance(children, dict):
                    raise ExportError(f"expected schema map at {location}/{key}")
                for name, child in children.items():
                    visit(child, owner, f"{location}/{key}/{name}")
        for key in single_keywords:
            if key in node:
                visit(node[key], owner, f"{location}/{key}")
        if "items" in node:
            items = node["items"]
            if isinstance(items, list):
                for index, child in enumerate(items):
                    visit(child, owner, f"{location}/items/{index}")
            else:
                visit(items, owner, f"{location}/items")
        for key in array_keywords:
            if key in node:
                children = node[key]
                if not isinstance(children, list) or not children:
                    raise ExportError(f"expected nonempty schema array at {location}/{key}")
                for index, child in enumerate(children):
                    visit(child, owner, f"{location}/{key}/{index}")
        if "dependencies" in node:
            dependencies = node["dependencies"]
            if not isinstance(dependencies, dict):
                raise ExportError(f"expected dependencies map at {location}/dependencies")
            for name, child in dependencies.items():
                if not isinstance(child, list):
                    visit(child, owner, f"{location}/dependencies/{name}")

    while pending:
        name = pending.pop()
        key = output_name(name)
        if key in included:
            if included[key] != name:
                raise ExportError(f"schema name collision: {included[key]!r} and {name!r} map to {key!r}")
            continue
        included[key] = name
        visit(schemas[name], name, name)

    return {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": "#/$defs/Service",
        "$defs": {key: schemas[included[key]] for key in sorted(included)},
    }


def write_schema(data):
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="wb", dir=OUTPUT.parent, prefix=".service.schema.", delete=False
        ) as file:
            temporary = Path(file.name)
            file.write(data)
        os.chmod(temporary, 0o644)
        os.replace(temporary, OUTPUT)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail if the committed schema is missing or stale")
    args = parser.parse_args()
    try:
        schema = root_schema(generate_native_map())
        data = (json.dumps(schema, ensure_ascii=False, indent=2, sort_keys=True, allow_nan=False) + "\n").encode("utf-8")
        if args.check:
            try:
                committed = OUTPUT.read_bytes()
            except FileNotFoundError:
                committed = None
            if committed != data:
                raise ExportError(
                    f"{OUTPUT.relative_to(ROOT)} is missing or stale; "
                    "run python3 scripts/generate-service-schema.py"
                )
            print(f"Committed {OUTPUT.relative_to(ROOT)} matches the KCL {KCL_VERSION} schema")
        else:
            write_schema(data)
            print(f"Generated {OUTPUT.relative_to(ROOT)} from {SOURCE} with KCL {KCL_VERSION}")
    except (ExportError, OSError) as exc:
        print(f"service schema export failed: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
