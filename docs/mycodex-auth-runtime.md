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

`.github/workflows/auth-core-release.yml` runs only on manual dispatch. A branch
run produces test artifacts; an `auth-core-v<hostVersion>` tag run publishes a
Release after all four platforms pass the native core/GUI process tests and
package verification. Neither branch changes nor tag pushes start this workflow.

Maintainers: version changes, dispatch commands, GitHub publication, GitCode
mirroring, frontend package pins and recovery are documented in
`docs/mycodex-auth-release.md` in the MyCodex-auth-host source repository.
That guide is for maintainers and is not included in runtime archives.

The release contains four native archives, four target metadata files,
`release-manifest.json` and `SHA256SUMS.txt`. Each archive includes this document
as `RUNTIME.md`; keep it alongside the upstream `LICENSE` and dependency report.
Published assets must not be replaced under an existing version.

The embedded upstream/source revisions identify the code used. Checksums detect
changed files, but a checksum downloaded alongside a file is not an independent
trust root; clients pin the expected release identity and hashes.

Cloud build success means the binaries passed tests on their runners. It does
not mean any user machine was deployed or modified. Release signing and target
OS acceptance remain separate from these checks.
