#!/usr/bin/env python3
"""Archive verified native packages and assemble one complete cloud release."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import tarfile
import zipfile

NAME = "mycodex-auth-host"
MANIFEST = NAME + ".manifest.json"
TARGETS = (
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
)
UPSTREAM = "e0f70019b2758f5b6b9a04dd60e4689481a0c0ac"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def read_json(path):
    require(path.is_file() and not path.is_symlink() and path.stat().st_size <= 65536,
            "Missing, redirected or oversized metadata.")
    return json.loads(path.read_text(encoding="utf-8-sig"))


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def validate(manifest, target, revision):
    require(target in TARGETS and manifest.get("target") == target, "Unexpected target.")
    require(re.fullmatch(r"[0-9a-f]{40}", revision) is not None
            and manifest.get("sourceRevision") == revision, "Unexpected source revision.")
    require(manifest.get("sourceDirty") is False, "Cloud releases require clean source.")
    require(manifest.get("schemaVersion") == 1 and manifest.get("protocolVersion") == 2
            and manifest.get("hostVersion") == "0.2.2"
            and manifest.get("upstreamRevision") == UPSTREAM, "Incompatible core identity.")
    executable = NAME + (".exe" if "windows" in target else "")
    require(manifest.get("executable") == executable, "Unexpected executable name.")
    require(re.fullmatch(r"[0-9a-f]{64}", manifest.get("sha256", "")) is not None,
            "Invalid executable hash.")


def archive_name(manifest):
    extension = ".zip" if "windows" in manifest["target"] else ".tar.gz"
    return f"{NAME}-{manifest['hostVersion']}-{manifest['target']}{extension}"


def pack(package, output, target, revision):
    manifest = read_json(package / MANIFEST)
    validate(manifest, target, revision)
    names = [manifest["executable"], MANIFEST, "LICENSE", "RUNTIME.md", "dependencies.txt"]
    for name in names:
        path = package / name
        require(path.is_file() and not path.is_symlink() and path.stat().st_size > 0,
                "Missing or redirected package file: " + name)
    require(digest(package / manifest["executable"]) == manifest["sha256"], "Executable hash mismatch.")
    output.mkdir(parents=True, exist_ok=True)
    archive = output / archive_name(manifest)
    metadata = output / (target + ".json")
    require(not archive.exists() and not metadata.exists(), "Release output already exists.")
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as bundle:
            for name in names:
                bundle.write(package / name, name)
    else:
        def attributes(info):
            info.mode = 0o755 if info.name == manifest["executable"] else 0o644
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            return info
        with tarfile.open(archive, "w:gz") as bundle:
            for name in names:
                bundle.add(package / name, arcname=name, recursive=False, filter=attributes)
    write_json(metadata, {"manifest": manifest, "archive": archive.name,
                          "sha256": digest(archive), "size": archive.stat().st_size})


def assemble(folder, revision, tag=None):
    assets = []
    for target in TARGETS:
        asset = read_json(folder / (target + ".json"))
        manifest = asset["manifest"]
        validate(manifest, target, revision)
        require(asset.get("archive") == archive_name(manifest), "Unexpected archive name.")
        archive = folder / asset["archive"]
        require(archive.is_file() and not archive.is_symlink(), "Missing or redirected archive.")
        require(archive.stat().st_size == asset.get("size") and digest(archive) == asset.get("sha256"),
                "Archive hash or size mismatch.")
        assets.append(asset)
    version = assets[0]["manifest"]["hostVersion"]
    require(tag is None or tag == "auth-core-v" + version, "Release tag does not match host version.")
    index = {"schemaVersion": 1, "hostVersion": version, "protocolVersion": 2,
             "sourceRevision": revision, "upstreamRevision": UPSTREAM, "assets": assets}
    index_path = folder / "release-manifest.json"
    write_json(index_path, index)
    # Include the platform manifests and aggregate index as well as archives.
    files = [folder / a["archive"] for a in assets]
    files += [folder / (target + ".json") for target in TARGETS] + [index_path]
    (folder / "SHA256SUMS.txt").write_text(
        "".join(f"{digest(path)}  {path.name}\n" for path in sorted(files)), encoding="utf-8")
    return index


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    create = commands.add_parser("pack")
    create.add_argument("--package", type=Path, required=True)
    create.add_argument("--output", type=Path, required=True)
    create.add_argument("--target", choices=TARGETS, required=True)
    create.add_argument("--revision", required=True)
    collect = commands.add_parser("assemble")
    collect.add_argument("--folder", type=Path, required=True)
    collect.add_argument("--revision", required=True)
    collect.add_argument("--tag")
    args = parser.parse_args()
    if args.command == "pack":
        pack(args.package, args.output, args.target, args.revision)
    else:
        assemble(args.folder, args.revision, args.tag)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError) as error:
        raise SystemExit(str(error)) from None
