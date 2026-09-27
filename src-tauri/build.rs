fn main() {
    mycodex_build_identity();
    tauri_build::build();

    // Windows: Embed Common Controls v6 manifest for test binaries
    //
    // When running `cargo test`, the generated test executables don't include
    // the standard Tauri application manifest. Without Common Controls v6,
    // `tauri::test` calls fail with STATUS_ENTRYPOINT_NOT_FOUND.
    //
    // This workaround:
    // 1. Embeds the manifest into test binaries via /MANIFEST:EMBED
    // 2. Uses /MANIFEST:NO for the main binary to avoid duplicate resources
    //    (Tauri already handles manifest embedding for the app binary)
    #[cfg(target_os = "windows")]
    {
        let manifest_path = std::path::PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").expect("missing CARGO_MANIFEST_DIR"),
        )
        .join("common-controls.manifest");
        let manifest_arg = format!("/MANIFESTINPUT:{}", manifest_path.display());

        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg={}", manifest_arg);
        // Avoid duplicate manifest resources in binary builds.
        println!("cargo:rustc-link-arg-bins=/MANIFEST:NO");
        println!("cargo:rerun-if-changed={}", manifest_path.display());
    }
}

fn mycodex_build_identity() {
    use std::path::PathBuf;
    use std::process::Command;

    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let git = |args: &[&str]| -> Option<String> {
        let result = Command::new("git")
            .current_dir(&root)
            .args(args)
            .output()
            .ok()?;
        if !result.status.success() {
            return None;
        }
        String::from_utf8(result.stdout).ok()
    };
    let revision = git(&["rev-parse", "HEAD"])
        .map(|v| v.trim().to_string())
        .filter(|v| v.len() == 40 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = revision == "unknown"
        || git(&["status", "--porcelain", "--untracked-files=normal"])
            .is_none_or(|status| !status.trim().is_empty());
    // ponytail: refresh on every build to include untracked worktree changes;
    // replace with complete Git-aware dependency tracking only if build cost matters.
    println!(
        "cargo:rerun-if-changed={}/mycodex-identity-refresh",
        std::env::var("OUT_DIR").unwrap()
    );
    println!("cargo:rustc-env=MYCODEX_CORE_SOURCE_REVISION={revision}");
    println!("cargo:rustc-env=MYCODEX_CORE_SOURCE_DIRTY={dirty}");
    println!(
        "cargo:rustc-env=MYCODEX_CORE_TARGET={}",
        std::env::var("TARGET").unwrap()
    );
}
