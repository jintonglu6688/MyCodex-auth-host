#!/usr/bin/env python3
"""Offline checks for complete, pinned multi-platform release assembly."""
import importlib.util
import json
from pathlib import Path
import tarfile
import tempfile
import zipfile

spec = importlib.util.spec_from_file_location("release", Path(__file__).with_name("Release-MyCodexAuthCore.py"))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


def rejects(action):
    try:
        action()
    except (ValueError, FileNotFoundError):
        return
    raise AssertionError("Invalid release was accepted")


def main():
    revision = "a" * 40
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        output = root / "release"
        for target in release.TARGETS:
            package = root / target
            package.mkdir()
            name = release.NAME + (".exe" if "windows" in target else "")
            (package / name).write_bytes(b"synthetic executable")
            (package / name).chmod(0o755)
            for filename in ("LICENSE", "RUNTIME.md", "dependencies.txt"):
                (package / filename).write_text("fixture", encoding="utf-8")
            manifest = dict(schemaVersion=1, hostVersion="0.2.0", protocolVersion=2,
                            upstreamRevision=release.UPSTREAM, sourceRevision=revision,
                            sourceDirty=False, target=target, executable=name,
                            sha256=release.digest(package / name))
            release.write_json(package / release.MANIFEST, manifest)
            release.pack(package, output, target, revision)
            archive = output / release.archive_name(manifest)
            if "windows" in target:
                with zipfile.ZipFile(archive) as bundle:
                    assert name in bundle.namelist()
                    assert json.loads(bundle.read(release.MANIFEST)) == manifest
            else:
                with tarfile.open(archive) as bundle:
                    assert bundle.getmember(name).mode & 0o100
                    assert json.load(bundle.extractfile(release.MANIFEST)) == manifest
            rejects(lambda: release.validate(dict(manifest, sourceDirty=True), target, revision))
            rejects(lambda: release.validate(dict(manifest, sourceRevision="b" * 40), target, revision))
        index = release.assemble(output, revision, "auth-core-v0.2.0")
        assert len(index["assets"]) == 4
        assert len((output / "SHA256SUMS.txt").read_text().splitlines()) == 9
        rejects(lambda: release.assemble(output, revision, "auth-core-v9.0.0"))
        asset = index["assets"][0]
        original = (output / asset["archive"]).read_bytes()
        (output / asset["archive"]).write_bytes(b"corrupt")
        rejects(lambda: release.assemble(output, revision))
        (output / asset["archive"]).write_bytes(original)
        (output / (release.TARGETS[-1] + ".json")).unlink()
        rejects(lambda: release.assemble(output, revision))
    print("Authentication release checks passed.")


if __name__ == "__main__":
    main()
