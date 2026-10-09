#!/usr/bin/env python3
"""Prove a packaged macOS app reaches native appReady and quits its owned PID."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import time

EXPECTED_ID = "local.codexswitch.desktop"
BUNDLE_VERSION = "0.5.0"

QUIT_SOURCE = r"""
import AppKit
import CoreGraphics
import Foundation

guard (CommandLine.arguments.count == 4 || CommandLine.arguments.count == 5),
      let pid = Int32(CommandLine.arguments[1]), pid > 1,
      let app = NSRunningApplication(processIdentifier: pid),
      app.processIdentifier == pid,
      app.bundleIdentifier == "local.codexswitch.desktop",
      let bundle = app.bundleURL,
      let executable = app.executableURL else { exit(2) }
let expectedBundle = URL(fileURLWithPath: CommandLine.arguments[2]).resolvingSymlinksInPath()
let expectedExecutable = URL(fileURLWithPath: CommandLine.arguments[3]).resolvingSymlinksInPath()
guard bundle.resolvingSymlinksInPath() == expectedBundle,
      executable.resolvingSymlinksInPath() == expectedExecutable else { exit(3) }
if CommandLine.arguments.count == 5 {
    guard CommandLine.arguments[4] == "--window",
          CGPreflightScreenCaptureAccess() else { exit(10) }
    guard let windows = CGWindowListCopyWindowInfo(
        [.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID
    ) as? [[String: Any]] else { exit(11) }
    let ownWindows = windows.filter {
        ($0[kCGWindowOwnerPID as String] as? NSNumber)?.int32Value == pid
        && ($0[kCGWindowLayer as String] as? NSNumber)?.intValue == 0
        && ($0[kCGWindowName as String] as? String) == "ChatGPT Switch"
    }
    guard ownWindows.count == 1,
          let windowID = ownWindows[0][kCGWindowNumber as String] as? NSNumber else { exit(11) }
    print(windowID.uint32Value)
    exit(0)
}
guard !app.isTerminated, app.terminate() else { exit(4) }
"""


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def events_under(appdata):
    events = []
    for path in sorted((appdata / "codex-switch/logs/diagnostics").glob("events-*.jsonl")):
        if path.is_symlink() or path.stat().st_size > 10 * 1024 * 1024:
            raise RuntimeError("invalid smoke diagnostic segment")
        raw = path.read_bytes()
        # A recorder may currently be finishing its final line.
        for line in raw.splitlines(keepends=True):
            if line.endswith(b"\n") and line.strip():
                events.append(json.loads(line))
    return events


def session_events(appdata, pid):
    events = events_under(appdata)
    starts = [
        event for event in events
        if event.get("eventKind") == "sessionStarted"
        and event.get("safeContext", {}).get("processId") == pid
    ]
    if not starts:
        return []
    if len(starts) != 1:
        raise RuntimeError("smoke observed more than one session for its owned PID")
    selected = [
        event for event in events if event.get("sessionId") == starts[0]["sessionId"]
    ]
    if any(event.get("eventKind") in ("startupFailure", "panic") for event in selected):
        raise RuntimeError("packaged app recorded a startup failure or panic")
    return selected


def codex_state_digest(codex):
    records = []
    for item in sorted(codex.rglob("*")):
        relative = item.relative_to(codex).as_posix()
        if item.is_symlink():
            raise RuntimeError("updater rejection fixture contains an unexpected symbolic link")
        if item.is_file():
            records.append([relative, "file", digest(item)])
        elif item.is_dir():
            records.append([relative, "directory"])
        else:
            raise RuntimeError("updater rejection fixture contains an unexpected special file")
    return hashlib.sha256(json.dumps(records, separators=(",", ":")).encode()).hexdigest()


def reject_windows_updater(executable, root, home, codex, appdata, env):
    fixture = codex / "updater-rejection-fixture"
    fixture.mkdir()
    (fixture / "preserved.bin").write_bytes(b"owned macOS updater rejection fixture\x00\xff\n")
    before = codex_state_digest(codex)
    manifest = root / "missing-update-manifest.json"
    if manifest.exists():
        raise RuntimeError("updater rejection manifest must be absent")
    started = time.monotonic()
    helper = subprocess.Popen(
        [str(executable), "--codex-switch-apply-update", str(manifest)], cwd=home, env=env,
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        exit_code = helper.wait(timeout=15)
        elapsed_ms = int((time.monotonic() - started) * 1000)
        if exit_code != 1 or elapsed_ms >= 15000:
            raise RuntimeError("macOS Windows-updater helper was not promptly rejected with exit 1")
        # main initializes diagnostics before rejecting this flag. Those files belong
        # to isolated appdata; they are not mutations of the seeded CODEX_HOME.
        events = session_events(appdata, helper.pid)
        kinds = [event.get("eventKind") for event in events]
        if kinds != ["sessionStarted", "sessionEnded"]:
            raise RuntimeError("Windows-updater rejection did not stay in the pre-GUI helper branch")
        ended = events[-1].get("safeContext", {})
        if ended.get("reason") != "updateStartupHelper" or ended.get("exitCode") != 1:
            raise RuntimeError("Windows-updater rejection did not record the expected early exit")
        after = codex_state_digest(codex)
        if before != after:
            raise RuntimeError("Windows-updater rejection changed the seeded Codex user state")
        return {
            "argument": "--codex-switch-apply-update", "exitCode": exit_code,
            "deadlineSeconds": 15, "elapsedMilliseconds": elapsed_ms,
            "lifecycle": kinds, "endReason": "updateStartupHelper",
            "appReadyObserved": False, "codexHomeUnchanged": True,
            "codexHomeBeforeSha256": before, "codexHomeAfterSha256": after,
        }
    finally:
        if helper.poll() is None:
            helper.terminate()
            try:
                helper.wait(timeout=5)
            except subprocess.TimeoutExpired:
                helper.kill()
                helper.wait(timeout=5)


def capture_owned_window(helper, child, app, executable, architecture):
    output = Path("macos-ui-evidence")
    output.mkdir(parents=True, exist_ok=True)
    screenshot = output / f"window-{architecture}.png"
    report = {
        "schemaVersion": 1, "architecture": architecture,
        "status": "unavailable", "ownedProcessOnly": True,
    }
    try:
        selected = subprocess.run(
            [str(helper), str(child.pid), str(app), str(executable), "--window"],
            capture_output=True, text=True, timeout=15,
        )
        if selected.returncode == 10:
            report["reason"] = "runner screen-capture permission is unavailable; no prompt was requested"
        elif selected.returncode != 0 or not selected.stdout.strip().isdigit():
            report["reason"] = "the owned ChatGPT Switch window could not be identified"
        else:
            captured = subprocess.run(
                ["/usr/sbin/screencapture", "-x", "-l", selected.stdout.strip(), str(screenshot)],
                capture_output=True, timeout=15,
            )
            if captured.returncode == 0 and screenshot.is_file():
                with screenshot.open("rb") as stream:
                    signature = stream.read(8)
                if signature == b"\x89PNG\r\n\x1a\n" and screenshot.stat().st_size > 1024:
                    report.update(status="captured", image=screenshot.name, sha256=digest(screenshot))
                else:
                    report["reason"] = "runner did not return a usable PNG"
            else:
                report["reason"] = "runner could not capture the identified owned window"
    except (OSError, subprocess.TimeoutExpired):
        report["reason"] = "runner window capture helper was unavailable or timed out"
    if report["status"] != "captured":
        screenshot.unlink(missing_ok=True)
    (output / f"window-{architecture}.json").write_text(
        json.dumps(report, indent=2) + "\n", encoding="utf-8"
    )
    return report["status"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--architecture", choices=["aarch64", "x64"], required=True)
    args = parser.parse_args()
    if sys.platform != "darwin" or os.environ.get("GITHUB_ACTIONS") != "true":
        parser.error("startup smoke is restricted to a disposable macOS Actions runner")
    app = args.app.resolve(strict=True)
    with (app / "Contents/Info.plist").open("rb") as stream:
        info = plistlib.load(stream)
    if info.get("CFBundleIdentifier") != EXPECTED_ID:
        raise RuntimeError("unexpected smoke application bundle identity")
    if info.get("CodexSwitchReleaseVersion") != args.version:
        raise RuntimeError("unexpected smoke application release version")
    if (info.get("CFBundleShortVersionString") != BUNDLE_VERSION
            or info.get("CFBundleVersion") != BUNDLE_VERSION):
        raise RuntimeError("unexpected smoke application Apple bundle version")
    if info.get("CFBundleExecutable") != "codex-switch":
        raise RuntimeError("unexpected packaged executable")
    executable = app / "Contents/MacOS/codex-switch"
    if executable.is_symlink() or not executable.is_file():
        raise RuntimeError("invalid packaged smoke executable")
    executable = executable.resolve(strict=True)
    child = None
    report = None
    isolated_root = Path(os.environ["CODEX_SWITCH_CI_ROOT"]).resolve(strict=True)
    expected_keychain = os.environ["CODEX_SWITCH_CI_KEYCHAIN_PATH"]
    home, codex, appdata, tmp = [
        Path(os.environ[name]).resolve(strict=True)
        for name in ("HOME", "CODEX_HOME", "CODEX_SWITCH_DATA_HOME", "TMPDIR")
    ]
    for directory in (home, codex, appdata, tmp):
        if not directory.is_dir() or not directory.is_relative_to(isolated_root):
            raise RuntimeError("smoke home is outside the wrapper's isolated CI root")
    if any(codex.iterdir()) or any(appdata.iterdir()):
        raise RuntimeError("packaged app smoke requires fresh isolated app state")
    with tempfile.TemporaryDirectory(
        prefix="codex-switch-startup-", dir=os.environ.get("RUNNER_TEMP")
    ) as temporary:
        root = Path(temporary).resolve(strict=True)
        quit_source = root / "quit-owned-app.swift"
        quit_binary = root / "quit-owned-app"
        quit_source.write_text(QUIT_SOURCE, encoding="utf-8")
        built = subprocess.run(
            ["/usr/bin/xcrun", "swiftc", "-O", "-framework", "AppKit", "-framework", "CoreGraphics",
             str(quit_source), "-o", str(quit_binary)],
            capture_output=True, timeout=180,
        )
        if built.returncode:
            raise RuntimeError("could not build the PID-scoped AppKit quit helper")
        # Do not pass repository credentials, API keys or runner configuration to the app.
        env = {
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            "HOME": str(home),
            "CFFIXED_USER_HOME": str(home),
            "CODEX_HOME": str(codex),
            "CODEX_SWITCH_DATA_HOME": str(appdata),
            "TMPDIR": str(tmp) + "/",
            "LANG": "en_US.UTF-8",
            "GITHUB_ACTIONS": "true",
            "CODEX_SWITCH_CI_ROOT": str(isolated_root),
            "CODEX_SWITCH_CI_KEYCHAIN_PATH": expected_keychain,
        }
        verified = subprocess.run(
            [sys.executable, str(Path(__file__).with_name("macos-ci-keychain.py").resolve()),
             "--verify-default", expected_keychain],
            env=env, capture_output=True, timeout=30,
        )
        if verified.returncode:
            raise RuntimeError("final app environment did not resolve the isolated default Keychain")
        try:
            child = subprocess.Popen(
                [str(executable)], cwd=home, env=env,
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            )
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                if child.poll() is not None:
                    raise RuntimeError("packaged app exited before native appReady")
                events = session_events(appdata, child.pid)
                if any(event.get("eventKind") == "appReady" for event in events):
                    break
                time.sleep(0.2)
            else:
                raise RuntimeError("packaged app did not record appReady within 60 seconds")
            time.sleep(3)
            session_events(appdata, child.pid)
            if child.poll() is not None:
                raise RuntimeError("packaged app exited during the startup observation")
            screenshot = capture_owned_window(
                quit_binary, child, app, executable, args.architecture
            )
            if child.poll() is not None:
                raise RuntimeError("packaged app exited before normal quit")
            # While this helper runs, do not reap child: its PID cannot be reused.
            requested = subprocess.run(
                [str(quit_binary), str(child.pid), str(app), str(executable)],
                capture_output=True, timeout=15,
            )
            if requested.returncode:
                raise RuntimeError("PID-scoped normal AppKit quit was refused")
            if child.wait(timeout=20) != 0:
                raise RuntimeError("packaged app did not exit successfully after normal quit")
            events = session_events(appdata, child.pid)
            kinds = [event.get("eventKind") for event in events]
            required = ["sessionStarted", "appReady", "exitRequested", "sessionEnded"]
            cursor = -1
            for kind in required:
                cursor = kinds.index(kind, cursor + 1)
            if any(codex.iterdir()):
                raise RuntimeError("fresh-home smoke unexpectedly changed Codex user state")
            updater_rejection = reject_windows_updater(
                executable, root, home, codex, appdata, env
            )
            report = {
                "schemaVersion": 1,
                "releaseVersion": info["CodexSwitchReleaseVersion"],
                "bundleVersion": info["CFBundleVersion"],
                "bundleShortVersion": info["CFBundleShortVersionString"],
                "architecture": args.architecture,
                "bundleIdentifier": EXPECTED_ID,
                "executableSha256": digest(executable),
                "lifecycle": required,
                "normalQuit": True,
                "exitCode": 0,
                "codexHomeUnchanged": True,
                "isolatedHome": True,
                "realClientStarted": False,
                "isolatedKeychainVerified": True,
                "windowScreenshot": screenshot,
                "windowsUpdaterRejected": updater_rejection,
                "scope": "native app startup, normal exit and Windows updater rejection; no real-client switch",
            }
        finally:
            if child is not None and child.poll() is None:
                # Cleanup of the subprocess created above is never counted as a passing quit.
                child.terminate()
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("x", encoding="utf-8") as stream:
        json.dump(report, stream, indent=2)
        stream.write("\n")
    print("MACOS_STARTUP_OK lifecycle=sessionStarted,appReady,exitRequested,sessionEnded")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, KeyError, ValueError, subprocess.TimeoutExpired) as error:
        print(f"MACOS_STARTUP_FAILED: {error}", file=sys.stderr)
        sys.exit(1)
