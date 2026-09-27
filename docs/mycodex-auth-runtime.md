# MyCodex authentication core release

These archives contain the Codex-only `mycodex-auth-host` entry point, its
identity manifest, upstream MIT license and a build-time dependency report.
They contain no accounts, tokens, databases or `.codex` configuration.

## Platforms

| Artifact target | Build baseline | Runtime requirements |
| --- | --- | --- |
| `x86_64-pc-windows-msvc` | Windows Server 2022 runner | Windows x64; validate the intended Windows version before distribution |
| `x86_64-unknown-linux-gnu` | Ubuntu 22.04 x64 | Compatible glibc and the shared libraries listed in `dependencies.txt` |
| `aarch64-unknown-linux-gnu` | Ubuntu 22.04 ARM64 | Compatible glibc and the shared libraries listed in `dependencies.txt` |
| `aarch64-apple-darwin` | macOS 14 Apple Silicon | Built with macOS 12 deployment target; CI execution covers macOS 14 only |

Intel Macs and Windows ARM64 are not built by this workflow.

The Linux entry point runs without opening a desktop window, but the current
upstream library still links GTK/WebKitGTK. It is **not a standalone static
binary** and is not intended for Alpine/musl. On Ubuntu 22.04 the corresponding
runtime packages include `libwebkit2gtk-4.1-0`, `libgtk-3-0`,
`libayatana-appindicator3-1` and `libssl3`. The attached `ldd` and symbol-version
report is authoritative for each build; another distribution needs matching
libraries. Deployment must detect these requirements before replacing a host.
The workflow installs build dependencies on its runner, not on user targets.

macOS binaries are not Developer ID signed or notarized by this workflow.
Native CI tests do not establish Gatekeeper acceptance or test every supported
OS version. Packaging, remote installation and OS compatibility acceptance
remain separate steps.

## Build and publication

`.github/workflows/auth-core-release.yml` builds four native targets only when
started manually. Select `feature/authentication-core-upstream` as the ref to
build test artifacts without publishing. To publish, first push an
`auth-core-v<hostVersion>` tag on a commit containing this workflow, then run
the workflow manually with that tag as the ref. The release is published after
**all** platforms have built and passed the isolated core/GUI protocol process
tests. The version must match the executable's embedded `hostVersion`
(currently `0.2.0`). Neither branch changes nor tag pushes start this workflow.
The authentication tag prefix does not trigger the upstream desktop `v*`
release workflow.

```text
gh workflow run auth-core-release.yml --repo jintonglu6688/MyCodex-auth-host --ref feature/authentication-core-upstream
gh workflow run auth-core-release.yml --repo jintonglu6688/MyCodex-auth-host --ref auth-core-v<hostVersion>
```

GitHub requires a workflow with `workflow_dispatch` on the default branch to
offer manual runs. The default branch contains a small entry point; select the
authentication-core branch or a matching tag to run the full build workflow.

The workflow reuses `Package-MyCodexAuthCore.ps1` on Windows and
`Package-MyCodexAuthCore.py` on Unix. `Release-MyCodexAuthCore.py` rejects dirty
source, incorrect revisions/targets, missing platforms and changed archives.
It emits one archive and metadata file per target, `release-manifest.json` and
`SHA256SUMS.txt`. The full set is uploaded to a draft before it is published.
A failed publication may leave a draft; resolve that draft deliberately before
retrying. Published assets must not be replaced under an existing version.

The embedded upstream/source revisions identify the code used; checksums detect
changed files, but a checksum downloaded alongside a file is not an independent
trust root. Consumers should pin the expected release metadata/hash. GitCode
mirroring, frontend download/cache integration, automatic deployment and release
signing policy are outside this workflow's scope.

Run the offline packaging checks with:

```text
python scripts/Test-MyCodexAuthRelease.py
```

Run native tests/builds with the repository's pinned Rust toolchain:

```text
cargo test --locked --release --manifest-path src-tauri/Cargo.toml --test mycodex_host_core --test mycodex_gui
```

The integration tests compile and run the native executable. The workflow packages
that tested executable without a second Cargo build. For a standalone local build,
use `cargo build --locked --release --manifest-path src-tauri/Cargo.toml --bin mycodex-auth-host`.

Cloud build success means the published native binaries passed these checks on
their runners. It does not mean any user machine was deployed or modified.
