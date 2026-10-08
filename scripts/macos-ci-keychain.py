#!/usr/bin/env python3
"""Run a CI command with a native disposable Keychain in one isolated HOME."""

import argparse
import ctypes
import os
from pathlib import Path
import secrets
import signal
import stat
import subprocess
import sys
import tempfile

ROOT_ENV = "CODEX_SWITCH_CI_ROOT"
KEYCHAIN_ENV = "CODEX_SWITCH_CI_KEYCHAIN_PATH"
OWNER_MARKER = b"codex-switch-isolated-keychain-v1\n"
NO_DEFAULT_KEYCHAIN = -25307
NO_SUCH_KEYCHAIN = -25294


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def checked_profile(env):
    """Validate the owned profile before loading any Apple framework."""
    raw_root = env.get(ROOT_ENV, "")
    root = Path(raw_root)
    require(raw_root and root.is_absolute() and root.resolve(strict=True) == root
            and root.name.startswith("codex-switch-keychain-") and root.is_dir(),
            "isolated Keychain root is not a physical fixture directory")
    metadata = root.stat()
    require(metadata.st_uid == os.getuid() and stat.S_IMODE(metadata.st_mode) == 0o700,
            "isolated Keychain root is not private to the runner user")
    marker = root / "owner"
    require(not marker.is_symlink() and marker.is_file() and marker.read_bytes() == OWNER_MARKER,
            "isolated Keychain owner marker is missing")
    for name, leaf in [
        ("HOME", "home"), ("CFFIXED_USER_HOME", "home"), ("CODEX_HOME", "codex"),
        ("CODEX_SWITCH_DATA_HOME", "appdata"), ("TMPDIR", "tmp"),
    ]:
        path = Path(env.get(name, ""))
        require(path == root / leaf and path.resolve(strict=True) == path and path.is_dir(),
                "isolated application paths do not match the Keychain profile")
    expected = root / "home/Library/Keychains/test.keychain-db"
    require(env.get(KEYCHAIN_ENV) == str(expected)
            and expected.parent.resolve(strict=True) == expected.parent,
            "isolated Keychain path does not match the disposable HOME")
    return root, expected


class KeychainSettings(ctypes.Structure):
    _fields_ = [
        ("version", ctypes.c_uint32), ("lockOnSleep", ctypes.c_uint8),
        ("useLockInterval", ctypes.c_uint8), ("lockInterval", ctypes.c_uint32),
    ]


class NativeKeychain:
    """Keychain C APIs; passwords are passed only in native memory buffers."""

    def __init__(self):
        # Only a newly exec'd worker/verifier constructs this object. Its HOME
        # and CFFIXED_USER_HOME were set before Python or Apple frameworks load.
        self.core = ctypes.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
        self.security = ctypes.CDLL("/System/Library/Frameworks/Security.framework/Security")
        pointer = ctypes.c_void_p
        pointer_out = ctypes.POINTER(pointer)
        self.bind(self.core, "CFRelease", [pointer], None)
        self.bind(self.core, "CFCopyHomeDirectoryURL", [], pointer)
        self.bind(self.core, "CFURLGetFileSystemRepresentation",
                  [pointer, ctypes.c_uint8, pointer, ctypes.c_long], ctypes.c_uint8)
        self.bind(self.core, "CFArrayCreate", [pointer, pointer_out, ctypes.c_long, pointer], pointer)
        self.bind(self.security, "SecKeychainCreate",
                  [ctypes.c_char_p, ctypes.c_uint32, pointer, ctypes.c_uint8, pointer, pointer_out])
        self.bind(self.security, "SecKeychainUnlock",
                  [pointer, ctypes.c_uint32, pointer, ctypes.c_uint8])
        self.bind(self.security, "SecKeychainSetSettings",
                  [pointer, ctypes.POINTER(KeychainSettings)])
        self.bind(self.security, "SecKeychainCopyDefault", [pointer_out])
        self.bind(self.security, "SecKeychainGetPath",
                  [pointer, ctypes.POINTER(ctypes.c_uint32), pointer])
        self.bind(self.security, "SecKeychainGetStatus",
                  [pointer, ctypes.POINTER(ctypes.c_uint32)])
        for name in ("SecKeychainSetDefault", "SecKeychainSetSearchList", "SecKeychainDelete"):
            self.bind(self.security, name, [pointer])

    @staticmethod
    def bind(library, name, arguments, result=ctypes.c_int32):
        function = getattr(library, name)
        function.argtypes = arguments
        function.restype = result

    @staticmethod
    def status(code, operation):
        require(code == 0, f"native temporary Keychain {operation} failed (status {code})")

    def check_home(self, home):
        url = self.core.CFCopyHomeDirectoryURL()
        require(url, "native CoreFoundation home is unavailable")
        try:
            buffer = ctypes.create_string_buffer(32768)
            require(self.core.CFURLGetFileSystemRepresentation(url, 1, buffer, len(buffer)),
                    "native CoreFoundation home could not be resolved")
            require(Path(os.fsdecode(buffer.value)).resolve(strict=True) == home,
                    "native CoreFoundation resolved a different HOME")
        finally:
            self.core.CFRelease(url)

    def path(self, keychain):
        buffer = ctypes.create_string_buffer(32768)
        length = ctypes.c_uint32(len(buffer))
        self.status(self.security.SecKeychainGetPath(keychain, ctypes.byref(length), buffer),
                    "path readback")
        require(length.value < len(buffer), "native Keychain path exceeded its size bound")
        return Path(os.fsdecode(buffer.raw[:length.value].rstrip(b"\0"))).resolve(strict=False)

    def default(self, allow_missing=False):
        keychain = ctypes.c_void_p()
        code = self.security.SecKeychainCopyDefault(ctypes.byref(keychain))
        if allow_missing and code in (NO_DEFAULT_KEYCHAIN, NO_SUCH_KEYCHAIN):
            return None
        self.status(code, "default readback")
        require(keychain.value, "native default Keychain reference is empty")
        try:
            return self.path(keychain)
        finally:
            self.core.CFRelease(keychain)

    def verify_default(self, expected):
        self.check_home(expected.parents[2])
        require(not expected.is_symlink() and expected.is_file(),
                "disposable Keychain file is missing or linked")
        require(self.default() == expected, "native default Keychain does not match the CI fixture")

    def create(self, expected):
        home = expected.parents[2]
        self.check_home(home)
        before = self.default(allow_missing=True)
        # Stop before mutation if Security falls back to the runner's real HOME.
        require(before is None or before.is_relative_to(home),
                "native Keychain preferences escaped the isolated HOME")
        require(not expected.exists() and not expected.is_symlink(),
                "refusing to replace an existing Keychain")
        keychain = ctypes.c_void_p()
        password = secrets.token_urlsafe(32).encode("ascii")
        secret = ctypes.create_string_buffer(password, len(password))
        del password
        try:
            self.status(self.security.SecKeychainCreate(
                os.fsencode(expected), len(secret), secret, 0, None, ctypes.byref(keychain)), "creation")
            settings = KeychainSettings(1, 0, 1, 7200)
            self.status(self.security.SecKeychainSetSettings(keychain, ctypes.byref(settings)), "settings")
            self.status(self.security.SecKeychainUnlock(keychain, len(secret), secret, 1), "unlock")
            flags = ctypes.c_uint32()
            self.status(self.security.SecKeychainGetStatus(keychain, ctypes.byref(flags)), "unlock readback")
            require(flags.value & 1, "disposable Keychain is not unlocked")
            entries = (ctypes.c_void_p * 1)(keychain.value)
            # NULL callbacks suffice while our owned Keychain reference is alive.
            array = self.core.CFArrayCreate(None, entries, 1, None)
            require(array, "native Keychain search list could not be allocated")
            try:
                self.status(self.security.SecKeychainSetSearchList(array), "search-list selection")
            finally:
                self.core.CFRelease(array)
            self.status(self.security.SecKeychainSetDefault(keychain), "default selection")
            self.verify_default(expected)
            return keychain
        except BaseException:
            if keychain.value:
                self.security.SecKeychainDelete(keychain)
                self.core.CFRelease(keychain)
            raise
        finally:
            ctypes.memset(secret, 0, len(secret))

    def delete(self, keychain, expected):
        try:
            require(self.path(keychain) == expected, "temporary Keychain identity changed before cleanup")
            self.status(self.security.SecKeychainDelete(keychain), "deletion")
            require(not expected.exists(), "temporary Keychain file remained after native deletion")
        finally:
            self.core.CFRelease(keychain)


def cancelled(_signum, _frame):
    raise KeyboardInterrupt("CI command cancelled")


def stop_owned(child):
    if child is None or child.poll() is not None:
        return
    # Every child below owns a new session. Do not reap its leader before
    # signalling this exact owned process group, so its PID cannot be reused.
    try:
        os.killpg(child.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        child.wait(timeout=10)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait(timeout=10)


def isolated_worker(command):
    require(sys.platform == "darwin" and os.environ.get("GITHUB_ACTIONS") == "true",
            "Keychain creation is restricted to a disposable macOS Actions runner")
    _, expected = checked_profile(os.environ)
    native = NativeKeychain()
    keychain = native.create(expected)
    child = None
    try:
        # A fresh interpreter proves the selected Keychain is visible in the
        # exact environment inherited by the test executable or packaged app.
        verification = subprocess.run(
            [sys.executable, str(Path(__file__).resolve()), "--verify-default", str(expected)],
            stdin=subprocess.DEVNULL, capture_output=True, timeout=30,
        )
        require(verification.returncode == 0, "isolated child could not verify its native default Keychain")
        child = subprocess.Popen(command, stdin=subprocess.DEVNULL, start_new_session=True)
        result = child.wait(timeout=2400)
        native.verify_default(expected)
        return result if 0 <= result <= 255 else 1
    finally:
        stop_owned(child)
        native.delete(keychain, expected)


def run_isolated(command):
    require(sys.platform == "darwin" and os.environ.get("GITHUB_ACTIONS") == "true",
            "this helper is restricted to disposable macOS Actions runners")
    require(os.environ.get("RUNNER_TEMP"), "the runner temporary directory is unavailable")
    runner_temp = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
    require(runner_temp.is_absolute() and runner_temp.is_dir(), "invalid runner temporary directory")
    # Preserve compiler installations. The outer process never loads Apple
    # frameworks or changes its own HOME; isolation begins at exec.
    env = os.environ.copy()
    env["CARGO_HOME"] = env.get("CARGO_HOME", str(Path.home() / ".cargo"))
    env["RUSTUP_HOME"] = env.get("RUSTUP_HOME", str(Path.home() / ".rustup"))
    with tempfile.TemporaryDirectory(prefix="codex-switch-keychain-", dir=runner_temp) as temporary:
        root = Path(temporary).resolve(strict=True)
        root.chmod(0o700)
        marker = root / "owner"
        marker.write_bytes(OWNER_MARKER)
        marker.chmod(0o600)
        for name, leaf in [
            ("HOME", "home"), ("CODEX_HOME", "codex"),
            ("CODEX_SWITCH_DATA_HOME", "appdata"), ("TMPDIR", "tmp"),
        ]:
            directory = root / leaf
            directory.mkdir(mode=0o700)
            env[name] = str(directory) + ("/" if name == "TMPDIR" else "")
        env["CFFIXED_USER_HOME"] = env["HOME"]
        for directory in [
            root / "home/Library", root / "home/Library/Preferences", root / "home/Library/Keychains",
        ]:
            directory.mkdir(mode=0o700)
        env[ROOT_ENV] = str(root)
        env[KEYCHAIN_ENV] = str(root / "home/Library/Keychains/test.keychain-db")
        checked_profile(env)
        worker = None
        try:
            worker = subprocess.Popen(
                [sys.executable, str(Path(__file__).resolve()), "--run-isolated", "--", *command],
                env=env, stdin=subprocess.DEVNULL, start_new_session=True,
            )
            result = worker.wait(timeout=2460)
            return result if 0 <= result <= 255 else 1
        finally:
            stop_owned(worker)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify-default", type=Path)
    parser.add_argument("--run-isolated", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    signal.signal(signal.SIGTERM, cancelled)
    if args.verify_default is not None:
        require(sys.platform == "darwin" and not command and not args.run_isolated,
                "native default verification requires macOS and no command")
        _, expected = checked_profile(os.environ)
        require(args.verify_default == expected, "unexpected Keychain verification path")
        NativeKeychain().verify_default(expected)
        print("MACOS_KEYCHAIN_DEFAULT_VERIFIED")
        return 0
    require(command, "a CI command is required after --")
    return isolated_worker(command) if args.run_isolated else run_isolated(command)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (RuntimeError, OSError, ValueError, subprocess.TimeoutExpired, KeyboardInterrupt) as error:
        print(f"MACOS_KEYCHAIN_FAILED: {error}", file=sys.stderr)
        sys.exit(1)
