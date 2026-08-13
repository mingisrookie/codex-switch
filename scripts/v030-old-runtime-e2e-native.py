#!/usr/bin/env python3
"""Run the Rust-owned native Codex downgrade verifier and emit bounded JSON evidence."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


REPO_ROOT = Path(__file__).resolve().parent.parent
PACKAGE_PREFIX = "codex-switch-downgrade-0-2-"


def absolute_file(value: str, label: str) -> Path:
    path = Path(value)
    if not path.is_absolute() or not path.is_file():
        raise argparse.ArgumentTypeError(f"{label} must be an existing absolute file")
    return path.resolve()


def absolute_directory(value: str, label: str) -> Path:
    path = Path(value)
    if not path.is_absolute() or not path.is_dir():
        raise argparse.ArgumentTypeError(f"{label} must be an existing absolute directory")
    return path.resolve()


def inside(root: Path, candidate: Path) -> bool:
    try:
        candidate.resolve().relative_to(root.resolve())
        return True
    except ValueError:
        return False


def validate_package_containment(root: Path) -> None:
    index_path = root / "fixture-index.json"
    if not index_path.is_file():
        raise RuntimeError("fixture-index.json is missing")
    index = json.loads(index_path.read_text(encoding="utf-8"))
    packages = index.get("packages")
    if not isinstance(packages, list) or len(packages) != 8:
        raise RuntimeError("fixture index must contain exactly eight packages")
    expected = {f"v0.2.{patch}" for patch in range(8)}
    observed: set[str] = set()
    for entry in packages:
        if not isinstance(entry, dict):
            raise RuntimeError("fixture package entry is invalid")
        version = entry.get("version")
        relative = entry.get("relativePackagePath")
        if version not in expected or not isinstance(relative, str):
            raise RuntimeError("fixture package identity is invalid")
        package = root / Path(relative)
        if not package.is_dir() or not inside(root, package):
            raise RuntimeError("fixture package escaped the final root")
        observed.add(version)
    if observed != expected:
        raise RuntimeError("fixture package versions are incomplete")


def discover_codex(explicit: str | None) -> Path:
    if explicit:
        return absolute_file(explicit, "codex executable")
    located = shutil.which("codex.exe")
    if not located:
        raise RuntimeError("native codex.exe was not found")
    return Path(located).resolve()


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Validate package containment, run the Rust native Codex list/resume/continue "
            "gate, and write bounded evidence."
        )
    )
    parser.add_argument("--root", required=True, help="Existing absolute fixture root")
    parser.add_argument("--output", required=True, help="New evidence JSON path inside root")
    parser.add_argument("--codex-exe", help="Exact native codex.exe")
    return parser.parse_args(argv)


def main() -> None:
    args = parse_args(sys.argv[1:])
    root = absolute_directory(args.root, "fixture root")
    output = Path(args.output)
    if not output.is_absolute() or not inside(root, output):
        raise RuntimeError("output must be an absolute path inside the fixture root")
    if output.exists():
        raise RuntimeError("refusing to overwrite native runtime evidence")
    validate_package_containment(root)
    executable = discover_codex(args.codex_exe)

    environment = os.environ.copy()
    environment["CODEX_SWITCH_DOWNGRADE_FIXTURE_ROOT"] = str(root)
    environment["CODEX_SWITCH_CODEX_RUNTIME_EXE"] = str(executable)
    environment.pop("CODEX_SWITCH_DOWNGRADE_FIXTURE_PATCH", None)
    subprocess.run(
        [
            "cargo",
            "test",
            "--locked",
            "--manifest-path",
            "src-tauri/Cargo.toml",
            "--test",
            "downgrade_old_runtime_contract",
            "verifies_every_supported_v02_fixture_with_native_codex",
            "--",
            "--ignored",
            "--exact",
            "--nocapture",
        ],
        cwd=REPO_ROOT,
        env=environment,
        check=True,
        timeout=20 * 60,
    )

    source = root / "native-runtime-verification.json"
    result = json.loads(source.read_text(encoding="utf-8"))
    versions = result.get("packages")
    if result.get("schemaVersion") != 1 or not isinstance(versions, list) or len(versions) != 8:
        raise RuntimeError("Rust native verification evidence is incomplete")
    envelope = {
        "schemaVersion": 1,
        "verdict": "PASS",
        "runtime": result.get("runtime"),
        "versions": versions,
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(
        json.dumps(envelope, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
        newline="",
    )
    print(json.dumps({"verdict": "PASS", "versions": len(versions)}))


if __name__ == "__main__":
    main()
