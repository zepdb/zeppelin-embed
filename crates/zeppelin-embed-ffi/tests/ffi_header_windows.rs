//! The C ABI header and symbol gates, executed natively on Windows.
//!
//! `ffi_header.rs` performs these checks on macOS using `ar`, `nm` and Mach-O
//! conventions. Rather than loosen that file's guard and risk the macOS gate
//! that currently protects the shipped artifact, the same checks are performed
//! here with the COFF toolchain: `dumpbin /exports` over the DLL and `llvm-nm`
//! over the static archive.
//!
//! This exists because a Windows run of `ffi_header` reports **zero tests**,
//! and zero tests is not ABI evidence.
//!
//! What is checked:
//!
//! * the committed header is byte-for-byte what cbindgen generates (this part
//!   is platform-neutral and is the drift gate the repository relies on);
//! * every `ze_*` the header declares is actually exported by the DLL;
//! * the DLL exports nothing beginning `ze_` that the header does not declare;
//! * the feature-gated `ze_text_*` surface stays out of a default build;
//! * the static archive is the implementation library and not an import stub.

#![cfg(windows)]
#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The header gate is pinned to one cbindgen line, as on macOS.
const CBINDGEN_VERSION_PREFIX: &str = "cbindgen 0.29.";

fn crate_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    crate_dir()
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

/// Strips C comments so a commented-out declaration is never counted.
fn strip_comments(header: &str) -> String {
    let mut stripped = String::new();
    let mut remainder = header;
    while let Some(start) = remainder.find("/*") {
        stripped.push_str(remainder.get(..start).expect("comment prefix"));
        let after_start = remainder.get(start + 2..).expect("after comment open");
        let end = after_start.find("*/").expect("terminated C comment");
        remainder = after_start.get(end + 2..).expect("after comment close");
    }
    stripped.push_str(remainder);
    stripped
}

/// Every `ze_*` function the committed header declares.
fn declared_functions(header: &str) -> BTreeSet<String> {
    strip_comments(header)
        .split(';')
        .filter_map(|declaration| {
            let open = declaration.find('(')?;
            let name = declaration
                .get(..open)?
                .split_whitespace()
                .last()?
                .trim_start_matches('*');
            name.starts_with("ze_").then(|| name.to_owned())
        })
        .collect()
}

fn committed_header() -> String {
    std::fs::read_to_string(crate_dir().join("include/zeppelin_embed.h")).expect("committed header")
}

// ---------------------------------------------------------------------------
// Header drift.
// ---------------------------------------------------------------------------

/// The committed header must be exactly what cbindgen emits.
///
/// This is platform-neutral, and it is the gate that makes every C-visible
/// change regenerated and committed rather than hand-edited.
#[test]
fn the_committed_header_is_the_exact_cbindgen_output() {
    let version = match Command::new("cbindgen").arg("--version").output() {
        Ok(output) => String::from_utf8(output.stdout).expect("UTF-8 cbindgen version"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => panic!(
            "cbindgen is not installed; the header drift gate requires it: \
             cargo install cbindgen --version 0.29.4 --locked"
        ),
        Err(error) => panic!("failed to execute cbindgen: {error}"),
    };
    assert!(
        version.trim().starts_with(CBINDGEN_VERSION_PREFIX),
        "header drift gate is pinned to {CBINDGEN_VERSION_PREFIX}x, found {}",
        version.trim()
    );

    let output = Command::new("cbindgen")
        .current_dir(crate_dir())
        .args(["--config", "cbindgen.toml", "--crate", "zeppelin-embed-ffi"])
        .output()
        .expect("run cbindgen");
    assert!(
        output.status.success(),
        "cbindgen failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let generated = String::from_utf8(output.stdout).expect("UTF-8 cbindgen output");
    let committed = committed_header();

    // Compare line by line. cbindgen writes LF; a Windows checkout without the
    // repository's `.gitattributes` would hold CRLF, and comparing whole
    // strings would then fail for a line-ending reason rather than an ABI one.
    // The line comparison states the real contract and reports the real drift.
    let generated_lines = generated.lines().collect::<Vec<_>>();
    let committed_lines = committed.lines().collect::<Vec<_>>();
    if generated_lines != committed_lines {
        let drift = generated_lines
            .iter()
            .zip(committed_lines.iter())
            .enumerate()
            .filter(|(_, (expected, actual))| expected != actual)
            .take(5)
            .map(|(line, (expected, actual))| {
                format!(
                    "line {}:\n  generated: {expected}\n  committed: {actual}",
                    line + 1
                )
            })
            .collect::<Vec<_>>();
        panic!(
            "include/zeppelin_embed.h drifted from the cbindgen output; regenerate with \
             `cbindgen --config cbindgen.toml --crate zeppelin-embed-ffi \
             --output include/zeppelin_embed.h` inside crates/zeppelin-embed-ffi\n\
             generated {} lines, committed {}\n{}",
            generated_lines.len(),
            committed_lines.len(),
            drift.join("\n")
        );
    }
}

// ---------------------------------------------------------------------------
// Export table.
// ---------------------------------------------------------------------------

/// Locates `dumpbin.exe` from the installed Visual Studio toolset.
fn dumpbin() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("ZE_DUMPBIN") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    // `vswhere` is installed at a fixed location by every VS installer.
    let program_files = std::env::var("ProgramFiles(x86)").ok()?;
    let vswhere =
        PathBuf::from(program_files).join("Microsoft Visual Studio/Installer/vswhere.exe");
    let output = Command::new(vswhere)
        .args(["-products", "*", "-latest", "-property", "installationPath"])
        .output()
        .ok()?;
    let root = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    let tools = root.join("VC/Tools/MSVC");
    let mut versions = std::fs::read_dir(tools)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    versions.sort();
    versions
        .into_iter()
        .rev()
        .map(|version| version.join("bin/Hostx64/x64/dumpbin.exe"))
        .find(|candidate| candidate.is_file())
}

/// Builds the release FFI artifacts with the default (non-`text`) features and
/// returns `(static archive, dll, import library)`.
fn build_release_artifacts() -> (PathBuf, PathBuf, PathBuf) {
    let workspace = workspace_root();
    let target_dir = workspace.join("target/ffi-header-gate-windows");
    let mut command = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()));
    command
        .current_dir(&workspace)
        .env("CARGO_TARGET_DIR", &target_dir)
        .args([
            "build",
            "-p",
            "zeppelin-embed-ffi",
            "--release",
            "--no-default-features",
            "--target",
            "x86_64-pc-windows-msvc",
        ]);
    // A coverage-instrumented parent must not poison this build.
    for key in [
        "LLVM_PROFILE_FILE",
        "RUSTC_WRAPPER",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
    ] {
        command.env_remove(key);
    }
    let status = command.status().expect("build release FFI artifacts");
    assert!(status.success(), "release FFI build failed");

    let release = target_dir.join("x86_64-pc-windows-msvc/release");
    (
        release.join("zeppelin_embed_ffi.lib"),
        release.join("zeppelin_embed_ffi.dll"),
        release.join("zeppelin_embed_ffi.dll.lib"),
    )
}

/// Every symbol the DLL exports, from `dumpbin /exports`.
fn dll_exports(dumpbin: &Path, dll: &Path) -> BTreeSet<String> {
    let output = Command::new(dumpbin)
        .arg("/exports")
        .arg(dll)
        .output()
        .expect("run dumpbin /exports");
    assert!(
        output.status.success(),
        "dumpbin /exports failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|line| {
            // Export rows are `ordinal hint RVA name`; the name is last.
            let name = line.split_whitespace().last()?;
            name.starts_with("ze_").then(|| name.to_owned())
        })
        .collect()
}

/// The DLL's export table must match the committed header exactly, in both
/// directions, and must not carry the feature-gated text surface.
#[test]
#[ignore = "manual full-build qualification; builds the release DLL and inspects it"]
fn the_committed_header_matches_the_dll_export_table() {
    let Some(dumpbin) = dumpbin() else {
        panic!(
            "dumpbin.exe was not found; this gate needs the MSVC toolset. \
             Set ZE_DUMPBIN to its path, or run from a Developer PowerShell."
        );
    };
    let (archive, dll, import_library) = build_release_artifacts();
    assert!(
        archive.is_file(),
        "missing static archive {}",
        archive.display()
    );
    assert!(dll.is_file(), "missing DLL {}", dll.display());
    assert!(
        import_library.is_file(),
        "missing import library {}",
        import_library.display()
    );

    // The static implementation archive and the DLL's import library are
    // different artifacts with confusable names. The implementation archive is
    // far larger; mistaking one for the other is the failure this guards.
    let archive_len = std::fs::metadata(&archive).expect("archive metadata").len();
    let import_len = std::fs::metadata(&import_library)
        .expect("import library metadata")
        .len();
    assert!(
        archive_len > import_len * 4,
        "the static archive ({archive_len} bytes) is not plausibly the implementation \
         library next to the import library ({import_len} bytes); they may have been swapped"
    );

    let declared = declared_functions(&committed_header());
    let exported = dll_exports(&dumpbin, &dll);
    assert!(!exported.is_empty(), "dumpbin reported no ze_* exports");

    let declared_default = declared
        .iter()
        .filter(|name| !name.starts_with("ze_text_"))
        .cloned()
        .collect::<BTreeSet<_>>();

    let missing = declared_default
        .difference(&exported)
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "the header declares symbols the DLL does not export: {missing:?}"
    );

    let undeclared = exported.difference(&declared).cloned().collect::<Vec<_>>();
    assert!(
        undeclared.is_empty(),
        "the DLL exports ze_* symbols the header does not declare: {undeclared:?}"
    );

    let text_symbols = exported
        .iter()
        .filter(|name| name.starts_with("ze_text_"))
        .collect::<Vec<_>>();
    assert!(
        text_symbols.is_empty(),
        "a default build must not export the feature-gated text surface: {text_symbols:?}"
    );

    // The allowlist and the header must agree, so neither can drift alone.
    let allowlist = std::fs::read_to_string(crate_dir().join("symbols.allowlist"))
        .expect("symbol allowlist")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let unlisted = exported.difference(&allowlist).cloned().collect::<Vec<_>>();
    assert!(
        unlisted.is_empty(),
        "the DLL exports symbols absent from symbols.allowlist: {unlisted:?}"
    );
}
