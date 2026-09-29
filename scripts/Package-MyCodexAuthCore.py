#!/usr/bin/env python3
"""Package, verify or install a native Unix core using only Python's stdlib."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tempfile

NAME = "mycodex-auth-host"
MANIFEST = NAME + ".manifest.json"
UPSTREAM = "e0f70019b2758f5b6b9a04dd60e4689481a0c0ac"
FIELDS = ("hostVersion", "protocolVersion", "upstreamRevision", "sourceRevision",
          "sourceDirty", "target", "sha256")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(65536), b""):
            value.update(block)
    return value.hexdigest()


def native_target():
    arch = {"x86_64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}.get(platform.machine())
    system = {"Linux": "unknown-linux-gnu", "Darwin": "apple-darwin"}.get(platform.system())
    require(arch and system, "Unsupported Unix platform.")
    return arch + "-" + system


def validate(identity):
    require(identity.get("protocolVersion") == 2 and identity.get("hostVersion") == "0.2.3"
            and identity.get("upstreamRevision") == UPSTREAM
            and re.fullmatch(r"[0-9a-f]{40}", identity.get("sourceRevision", ""))
            and type(identity.get("sourceDirty")) is bool
            and re.fullmatch(r"(x86_64|aarch64)-(unknown-linux-gnu|apple-darwin)", identity.get("target", ""))
            and re.fullmatch(r"[0-9a-f]{64}", identity.get("sha256", "")), "Invalid core identity.")


def verify(folder):
    for name in (NAME, MANIFEST, "LICENSE"):
        item = folder / name
        require(item.is_file() and not item.is_symlink(), "Missing or redirected package file.")
    require((folder / MANIFEST).stat().st_size <= 65536, "Manifest is too large.")
    identity = json.loads((folder / MANIFEST).read_text(encoding="utf-8"))
    validate(identity)
    require(identity.get("schemaVersion") == 1 and identity.get("executable") == NAME,
            "Invalid package manifest.")
    require(digest(folder / NAME) == identity["sha256"], "Executable hash mismatch.")
    return identity


def executable_identity(binary):
    result = subprocess.run([str(binary), "--version-json"], capture_output=True, timeout=30)
    require(result.returncode == 0, "Cannot read native executable identity; check runtime dependencies.")
    identity = json.loads(result.stdout)
    validate(identity)
    require(identity["target"] == native_target() and identity["sha256"] == digest(binary),
            "Executable identity mismatch.")
    return identity


def package(binary, output):
    require(not output.exists() or (output.is_dir() and not any(output.iterdir())),
            "Output directory must be new or empty.")
    require(not output.is_symlink(), "Output directory must not be a symlink.")
    identity = executable_identity(binary)
    license_file = Path(__file__).resolve().parent.parent / "LICENSE"
    require(license_file.is_file(), "Missing upstream LICENSE.")
    output.mkdir(parents=True, exist_ok=True, mode=0o700)
    shutil.copyfile(binary, output / NAME)
    (output / NAME).chmod(0o700)
    shutil.copyfile(license_file, output / "LICENSE")
    identity.update(schemaVersion=1, executable=NAME)
    (output / MANIFEST).write_text(json.dumps(identity, indent=2) + "\n", encoding="utf-8")
    verify(output)


def install(folder):
    identity = verify(folder)
    require(identity["target"] == native_target(), "Package is for another platform.")
    actual = executable_identity(folder / NAME)
    require(all(identity[key] == actual[key] for key in FIELDS), "Manifest identity mismatch.")
    home = Path.home().resolve(strict=True)
    destination = home / ".local/share/MyCodex/auth-core/bin"
    # Do not follow installation redirects into another product or user's files.
    path = home
    for part in destination.relative_to(home).parts:
        path /= part
        require(not path.is_symlink(), "Installation path contains a symlink.")
        path.mkdir(exist_ok=True, mode=0o700)
        require(path.is_dir() and path.stat().st_uid == os.geteuid(), "Installation path owner mismatch.")
        require(path.stat().st_mode & 0o022 == 0, "Installation path is writable by another user.")
    if any(destination.iterdir()):
        verify(destination)
    with tempfile.TemporaryDirectory(prefix=".update-", dir=destination) as temporary:
        stage = Path(temporary)
        for name in (NAME, MANIFEST, "LICENSE"):
            shutil.copyfile(folder / name, stage / name)
            (stage / name).chmod(0o700 if name == NAME else 0o600)
        verify(stage)
        # Replacing the binary is atomic; a partially replaced manifest fails closed.
        # Do not terminate existing hosts or change their target configuration.
        for name in (NAME, "LICENSE", MANIFEST):
            os.replace(stage / name, destination / name)
    verify(destination)
    print(destination / NAME)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    build = commands.add_parser("package")
    build.add_argument("--binary", type=Path, required=True)
    build.add_argument("--output", type=Path, required=True)
    for name in ("verify", "install"):
        commands.add_parser(name).add_argument("--package", type=Path, required=True)
    args = parser.parse_args()
    os.umask(0o077)
    if args.command == "package":
        package(args.binary.resolve(strict=True), args.output.absolute())
    elif args.command == "verify":
        verify(args.package)
    else:
        install(args.package)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        raise SystemExit(str(error)) from None
