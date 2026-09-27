#!/usr/bin/env python3
"""Offline native-process checks; all configuration and installs use temporary homes."""
import hashlib
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile

binary = Path(sys.argv[1]).resolve(strict=True)
packager = Path(__file__).with_name("Package-MyCodexAuthCore.py")


with tempfile.TemporaryDirectory(prefix="mycodex-core-unix-") as temporary:
    root = Path(temporary).resolve()
    home = root / "home"
    codex = root / "codex"
    other = root / "other"
    for path in (home, codex, other):
        path.mkdir()
    env = dict(os.environ, HOME=str(home), CODEX_HOME=str(codex), CC_SWITCH_TEST_HOME=str(home))

    def run(args, input=None, ok=True):
        result = subprocess.run([str(binary), *args], input=input, capture_output=True,
                                env=env, timeout=30)
        assert (result.returncode == 0) == ok, result.stderr.decode()
        return json.loads(result.stdout) if ok else result.stderr.decode().strip()

    paths = run(["--paths-json"])
    data = Path(paths["dataDir"])
    assert paths["codexHome"] == str(codex)
    assert data == home / ".local/share/MyCodex/auth-core/targets" / hashlib.sha256(os.fsencode(codex)).hexdigest()
    assert not data.exists(), "Discovery must be read-only"
    alias = root / "alias"
    alias.symlink_to(codex, target_is_directory=True)
    assert run(["--paths-json", "--codex-home", str(alias)]) == paths
    assert run(["--paths-json", "--codex-home", str(other)])["dataDir"] != str(data)
    assert run(["--paths-json", "--codex-home", "relative"], ok=False) == "absolute_directory_required"
    assert run(["--paths-json", "--data-dir", str(codex / "nested")], ok=False) == "overlapping_directories"
    assert run(["--paths-json", "--data-dir", str(root / "missing" / ".." / "unsafe")], ok=False) == "invalid_data_dir"
    data.mkdir(parents=True)
    args = ["--codex-home", str(codex), "--data-dir", str(data)]
    endpoint = Path("/tmp") / ("mycodex-auth-core-%d-%s" % (os.geteuid(), hashlib.sha256(os.fsencode(codex)).hexdigest()[:32]))
    server = subprocess.Popen([str(binary), "serve", *args], stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, env=env)
    try:
        assert select.select([server.stdout], [], [], 30)[0], "No ready message"
        line = server.stdout.readline()
        assert line, server.stderr.read().decode()
        ready = json.loads(line)
        assert ready["event"] == "ready" and ready["protocolVersion"] == 2
        assert ready["codexHome"] == str(codex) and ready["dataDir"] == str(data)
        assert data.stat().st_mode & 0o777 == 0o700
        assert endpoint.stat().st_mode & 0o777 == 0o700
        sock = endpoint / "management.sock"
        assert sock.stat().st_mode & 0o777 == 0o600
        for item in data.rglob("*"):
            assert item.stat().st_mode & 0o077 == 0, item

        def rpc(method, extra=None):
            request = json.dumps(dict(jsonrpc="2.0", id=1, method=method, params={})).encode()
            return run(["rpc", *(extra or args)], input=request)

        status = rpc("status")["result"]
        assert status["sha256"] == ready["sha256"]
        assert status["protocolVersion"] == 2 and "guiMcpManagement" in status["capabilities"]
        assert run(["serve", *args], ok=False) == "backend_already_running"
        wrong = root / "wrong-store"
        wrong.mkdir()
        request = b'{"jsonrpc":"2.0","id":1,"method":"status","params":{}}'
        assert run(["rpc", "--codex-home", str(codex), "--data-dir", str(wrong)],
                   input=request, ok=False) == "target_mismatch"
        sock.chmod(0o644)
        assert run(["rpc", *args], input=request, ok=False) == "backend_identity_mismatch"
        sock.chmod(0o600)
        assert rpc("backend/shutdown")["result"]["status"] == "shutting_down"
        assert server.wait(timeout=10) == 0
    finally:
        if server.poll() is None:
            server.kill()
            server.wait(timeout=10)
        for name in ("management.sock", "management.lock"):
            (endpoint / name).unlink(missing_ok=True)
        endpoint.rmdir()

    assert list(codex.iterdir()) == [], "Read-only IPC smoke must not modify global config"
    package = root / "package"
    def package_command(*args, success=True):
        result = subprocess.run([sys.executable, str(packager), *args], capture_output=True,
                                env=env, timeout=60)
        assert (result.returncode == 0) == success, result.stderr.decode()
    package_command("package", "--binary", str(binary), "--output", str(package))
    package_command("verify", "--package", str(package))
    (home / ".local").chmod(0o777)
    package_command("install", "--package", str(package), success=False)
    assert not Path(paths["executablePath"]).exists()
    (home / ".local").chmod(0o700)
    package_command("install", "--package", str(package))
    installed = Path(paths["executablePath"])
    assert installed.is_file() and installed.stat().st_mode & 0o777 == 0o700
    package_command("install", "--package", str(package))
    assert not (home / ".local/share/MyCodex/auth-center").exists()
    with (package / "mycodex-auth-host").open("ab") as stream:
        stream.write(b"tampered")
    package_command("verify", "--package", str(package), success=False)
    package_command("install", "--package", str(package), success=False)
    assert hashlib.sha256(installed.read_bytes()).hexdigest() == ready["sha256"]
    print("Unix paths, identity, private IPC, target isolation, package and isolated install checks passed.")
