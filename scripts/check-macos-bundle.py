#!/usr/bin/env python3
"""Validate native, signed final DMG bytes and smoke the app mounted from that DMG."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile

VERSION = "0.5.0-macos.1"
BUNDLE_VERSION = "0.5.0"
BUNDLE_ID = "local.codexswitch.desktop"
SUBTLE_LICENSE_SHA256 = "cc0332a88c2ea21d5f3c1298f966120f4c95196871c3f6bb4fcf615508b93fa1"
ARCHES = {
    "aarch64": ("aarch64-apple-darwin", "arm64"),
    "x64": ("x86_64-apple-darwin", "x86_64"),
}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def command(*args, timeout=60):
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        # These are our verification helpers; app stdout/stderr are always discarded.
        if args[0] == sys.executable:
            details = " ".join(line for line in result.stderr.splitlines()
                               if line.startswith("MACOS_STARTUP_FAILED:"))
        else:
            details = result.stderr.strip()[-2000:]
        raise RuntimeError(
            f"bundle verification command failed: {Path(args[0]).name} "
            f"(exit {result.returncode}); {details}"
        )
    return result.stdout, result.stderr


def sha256(path):
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def tree_digest(root):
    result = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix()
        if path.is_symlink():
            record = ["link", relative, os.readlink(path)]
        elif path.is_file():
            record = ["file", relative, sha256(path), path.stat().st_mode & 0o777]
        elif path.is_dir():
            record = ["directory", relative]
        else:
            raise RuntimeError("bundle contains an unsupported special file")
        result.update((json.dumps(record, separators=(",", ":")) + "\n").encode())
    return result.hexdigest()


def check_app(app, architecture, version):
    require(app.name == "ChatGPT Switch.app" and app.is_dir() and not app.is_symlink(),
            "unexpected application bundle")
    plist = app / "Contents/Info.plist"
    require(plist.is_file() and not plist.is_symlink() and plist.stat().st_size <= 256 * 1024,
            "invalid bundle Info.plist")
    with plist.open("rb") as stream:
        info = plistlib.load(stream)
    expected = {
        "CFBundleIdentifier": BUNDLE_ID,
        "CFBundleShortVersionString": BUNDLE_VERSION,
        "CFBundleVersion": BUNDLE_VERSION,
        "CodexSwitchReleaseVersion": version,
        "CFBundlePackageType": "APPL",
        "CFBundleExecutable": "codex-switch",
        "LSMinimumSystemVersion": "12.0",
    }
    for key, value in expected.items():
        require(info.get(key) == value, f"bundle {key} does not match its locked value")
    source_license = Path(__file__).resolve().parent.parent / "src-tauri/resources/SUBTLE-LICENSE.txt"
    bundled_license = app / "Contents/Resources/SUBTLE-LICENSE.txt"
    for license_file in (source_license, bundled_license):
        require(license_file.is_file() and not license_file.is_symlink()
                and license_file.stat().st_size == 1581,
                "required subtle copyright and license text is missing or invalid")
        require(sha256(license_file) == SUBTLE_LICENSE_SHA256,
                "subtle copyright and license text differs from its reviewed distribution bytes")
    executable = app / "Contents/MacOS/codex-switch"
    require(executable.is_file() and not executable.is_symlink()
            and bool(executable.stat().st_mode & 0o111), "invalid bundle executable")
    target, native_arch = ARCHES[architecture]
    file_type, _ = command("/usr/bin/file", "-b", str(executable))
    require("Mach-O" in file_type and "executable" in file_type and native_arch in file_type,
            "bundle executable is not the expected native Mach-O")
    found_arches, _ = command("/usr/bin/lipo", "-archs", str(executable))
    require(found_arches.split() == [native_arch], "unexpected bundle Mach-O architectures")
    command("/usr/bin/codesign", "--verify", "--deep", "--strict", str(app))
    details_out, details_err = command("/usr/bin/codesign", "-d", "--verbose=4", str(app))
    details = details_out + details_err
    require(re.search(r"^Signature=adhoc$", details, re.M) is not None,
            "preview bundle must carry an ad-hoc signature")
    require(re.search(r"^Identifier=" + re.escape(BUNDLE_ID) + r"$", details, re.M) is not None,
            "code signature identity does not match the app")
    require("Authority=" not in details, "unexpected Developer ID claim in ad-hoc preview")
    # Check every bundled Mach-O, including future embedded libraries.
    macho_magics = {b"\xfe\xed\xfa\xce", b"\xce\xfa\xed\xfe", b"\xfe\xed\xfa\xcf",
                    b"\xcf\xfa\xed\xfe", b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca",
                    b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca"}
    for path in app.rglob("*"):
        if not path.is_file() or path.is_symlink():
            continue
        with path.open("rb") as stream:
            magic = stream.read(4)
        if magic in macho_magics:
            slices, _ = command("/usr/bin/lipo", "-archs", str(path))
            require(slices.split() == [native_arch], "an embedded Mach-O has the wrong architecture")
    return {
        "target": target,
        "architecture": architecture,
        "bundleIdentifier": BUNDLE_ID,
        "releaseVersion": info["CodexSwitchReleaseVersion"],
        "bundleVersion": info["CFBundleVersion"],
        "bundleShortVersion": info["CFBundleShortVersionString"],
        "minimumSystemVersion": "12.0",
        "signature": "adhoc",
        "notarized": False,
        "bundledLicenses": {"SUBTLE-LICENSE.txt": sha256(bundled_license)},
        "executableSha256": sha256(executable),
        "bundleTreeSha256": tree_digest(app),
        "machoFormat": file_type.strip(),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--architecture", choices=ARCHES, required=True)
    parser.add_argument("--target-root", type=Path, default=Path("src-tauri/target"))
    parser.add_argument("--output", type=Path, default=Path("release"))
    parser.add_argument("--version", default=VERSION)
    parser.add_argument("--commit", required=True)
    args = parser.parse_args()
    require(sys.platform == "darwin", "bundle verification requires a native macOS runner")
    require(args.version == VERSION, "unexpected preview version")
    require(re.fullmatch(r"[0-9a-f]{40}", args.commit) is not None, "invalid source commit")
    target, native_arch = ARCHES[args.architecture]
    require(platform.machine() == native_arch, "cross-compiled bundles cannot satisfy the native gate")
    bundle = args.target_root / target / "release/bundle"
    apps = list((bundle / "macos").glob("*.app"))
    dmgs = list((bundle / "dmg").glob("*.dmg"))
    require(len(apps) == 1 and len(dmgs) == 1, "expected exactly one app and one DMG")
    require(not dmgs[0].is_symlink() and dmgs[0].is_file(), "invalid built DMG")
    built = check_app(apps[0], args.architecture, args.version)
    args.output.mkdir(parents=True, exist_ok=True)
    asset_name = f"codex-switch_{args.version}_{args.architecture}.dmg"
    published_dmg = args.output / asset_name
    require(not published_dmg.exists(), "refusing to reuse an existing release asset")
    shutil.copyfile(dmgs[0], published_dmg)
    require(sha256(published_dmg) == sha256(dmgs[0]), "DMG copy changed the built bytes")
    command("/usr/bin/hdiutil", "verify", str(published_dmg), timeout=180)
    startup_path = args.output / f"{asset_name}.startup.json"
    with tempfile.TemporaryDirectory(
        prefix="codex-switch-dmg-", dir=os.environ.get("RUNNER_TEMP")
    ) as temporary:
        mount = Path(temporary) / "mounted"
        mount.mkdir()
        attached = False
        try:
            command("/usr/bin/hdiutil", "attach", "-readonly", "-nobrowse", "-noautoopen",
                    "-mountpoint", str(mount), str(published_dmg.resolve()), timeout=120)
            attached = True
            mounted_apps = list(mount.glob("*.app"))
            require(len(mounted_apps) == 1, "final DMG must contain exactly one application")
            final = check_app(mounted_apps[0], args.architecture, args.version)
            require(final == built, "the application in the DMG differs from the verified build")
            command(
                sys.executable, str(Path(__file__).with_name("check-macos-startup.py")),
                "--app", str(mounted_apps[0]), "--output", str(startup_path.resolve()),
                "--version", args.version, "--architecture", args.architecture, timeout=300,
            )
        finally:
            if attached:
                command("/usr/bin/hdiutil", "detach", str(mount), timeout=60)
    digest = sha256(published_dmg)
    with (args.output / f"{asset_name}.sha256").open("x", encoding="ascii") as stream:
        stream.write(f"{digest}  {asset_name}\n")
    report = {
        "schemaVersion": 1,
        "releaseVersion": args.version,
        "tag": "v" + args.version,
        "commit": args.commit,
        **built,
        "runnerMacOSVersion": platform.mac_ver()[0],
        "dmg": {"name": asset_name, "bytes": published_dmg.stat().st_size, "sha256": digest},
        "checks": {
            "file": True, "lipo": True, "plist": True, "codesign": True,
            "hdiutilVerify": True, "mountedBundleMatches": True, "nativeStartup": True,
            "licenseResources": True,
        },
    }
    with (args.output / f"{asset_name}.verification.json").open("x", encoding="utf-8") as stream:
        json.dump(report, stream, indent=2)
        stream.write("\n")
    print(f"MACOS_BUNDLE_OK architecture={args.architecture} bytes={published_dmg.stat().st_size} sha256={digest}")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError, subprocess.TimeoutExpired) as error:
        print(f"MACOS_BUNDLE_FAILED: {error}", file=sys.stderr)
        sys.exit(1)
