use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn strip_comments(header: &str) -> String {
    let mut stripped = String::new();
    let mut remainder = header;
    while let Some(start) = remainder.find("/*") {
        stripped.push_str(&remainder[..start]);
        let after_start = &remainder[start + 2..];
        let end = after_start.find("*/").expect("terminated C comment");
        remainder = &after_start[end + 2..];
    }
    stripped.push_str(remainder);
    stripped
}

fn declared_functions(header: &str) -> BTreeSet<String> {
    strip_comments(header)
        .split(';')
        .filter_map(|declaration| {
            let open = declaration.find('(')?;
            let name = declaration[..open]
                .split_whitespace()
                .last()?
                .trim_start_matches('*');
            name.starts_with("ze_").then(|| name.to_owned())
        })
        .collect()
}

fn rust_llvm_nm() -> Option<PathBuf> {
    let verbose = Command::new("rustc").arg("-vV").output().ok()?;
    let text = String::from_utf8(verbose.stdout).ok()?;
    let release = text
        .lines()
        .find_map(|line| line.strip_prefix("release: "))?;
    let host = text.lines().find_map(|line| line.strip_prefix("host: "))?;
    let sysroot = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    let sysroot = PathBuf::from(String::from_utf8(sysroot.stdout).ok()?.trim());
    let toolchains = sysroot.parent()?;
    let candidate = toolchains
        .join(format!("{release}-{host}"))
        .join("lib/rustlib")
        .join(host)
        .join("bin/llvm-nm");
    candidate.is_file().then_some(candidate)
}

fn run_nm(object: &Path) -> Option<Output> {
    let mut command = Command::new("nm");
    if cfg!(target_os = "macos") {
        command.arg("-gU");
    } else {
        command.args(["-gD", "--defined-only"]);
    }
    match command.arg(object).output() {
        Ok(output) if output.status.success() => Some(output),
        Ok(output)
            if cfg!(target_os = "macos")
                && String::from_utf8_lossy(&output.stderr).contains("Unknown attribute kind") =>
        {
            let compatible = rust_llvm_nm().expect("Rust-compatible llvm-nm for LLVM 21 objects");
            Some(
                Command::new(compatible)
                    .args(["--extern-only", "--defined-only"])
                    .arg(object)
                    .output()
                    .expect("run Rust-compatible llvm-nm"),
            )
        }
        Ok(output) => panic!(
            "nm failed for {}: {}",
            object.display(),
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("SKIP[nm-missing]: nm is not installed; symbol-table contract not executed");
            None
        }
        Err(error) => panic!("failed to execute nm: {error}"),
    }
}

#[test]
fn the_committed_header_matches_the_exported_symbol_table_and_the_allowlist() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = crate_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let archive = workspace.join("target/release/libzeppelin_embed_ffi.a");
    // Always rebuild. Reusing an existing archive measures whatever bytes a
    // previous run happened to leave behind, so a newly exported symbol is
    // invisible to this gate on any machine with a warm `target/release` --
    // the drift gate then reports green about source it never read. The
    // rebuild is incremental and is a no-op when nothing changed.
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
        .current_dir(workspace)
        .args(["build", "-p", "zeppelin-embed-ffi", "--release"])
        .status()
        .expect("build FFI staticlib");
    assert!(status.success(), "release staticlib build failed");
    assert!(archive.is_file(), "release staticlib missing after build");

    let header = std::fs::read_to_string(crate_dir.join("include/zeppelin_embed.h"))
        .expect("committed header");
    let declared = declared_functions(&header);
    let allowlist = std::fs::read_to_string(crate_dir.join("symbols.allowlist"))
        .expect("symbol allowlist")
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();

    let members = Command::new("ar")
        .args(["-t", archive.to_str().expect("UTF-8 archive path")])
        .output()
        .expect("list staticlib members");
    assert!(members.status.success(), "ar -t failed");
    let own_members = String::from_utf8(members.stdout)
        .expect("UTF-8 archive member list")
        .lines()
        // The first repeated crate name is Cargo's source codegen unit. The
        // single-name sibling is an allocator shim emitted on the crate's
        // behalf, and dependency/std objects have different prefixes. Scoping
        // to these source units avoids claiming std's archive-wide exports as
        // this crate's collision surface.
        .filter(|member| member.starts_with("zeppelin_embed_ffi.zeppelin_embed_ffi"))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(
        !own_members.is_empty(),
        "no FFI source object found in staticlib"
    );

    let extracted = tempfile::tempdir().expect("staticlib extraction directory");
    let status = Command::new("ar")
        .current_dir(extracted.path())
        .args(["-x", archive.to_str().expect("UTF-8 archive path")])
        .status()
        .expect("extract staticlib");
    assert!(status.success(), "ar -x failed");

    let mut exported = BTreeSet::new();
    let mut measured = 0_usize;
    for member in own_members {
        let object = extracted.path().join(member);
        let Some(output) = run_nm(&object) else {
            return;
        };
        assert!(output.status.success(), "compatible nm failed");
        measured += 1;
        for line in String::from_utf8(output.stdout)
            .expect("UTF-8 nm output")
            .lines()
        {
            let Some(raw) = line.split_whitespace().last() else {
                continue;
            };
            let symbol = raw.strip_prefix('_').unwrap_or(raw);
            if symbol.starts_with("ze_") {
                exported.insert(symbol.to_owned());
            } else if symbol.starts_with("_ZN") || symbol.starts_with("_R") {
                // Rust-mangled implementation symbols cannot collide with the
                // intentionally unmangled C namespace and are not C exports.
            } else if !symbol.ends_with(':') {
                panic!("crate source object exported non-ze_ C symbol: {symbol}");
            }
        }
    }
    assert!(measured > 0, "zero crate source objects measured");
    assert_eq!(declared, allowlist, "header and allowlist differ");
    assert_eq!(exported, allowlist, "staticlib and allowlist differ");

    let source = std::fs::read_to_string(crate_dir.join("src/lib.rs")).expect("FFI source");
    for function in &allowlist {
        let marker = format!("fn {function}");
        let start = source.find(&marker).expect("exported function definition");
        let remainder = &source[start..];
        let end = remainder
            .find("#[unsafe(no_mangle)]")
            .unwrap_or(remainder.len());
        assert!(
            remainder[..end].contains("ffi_entry!("),
            "{function} omitted the sole catch_unwind wrapper macro"
        );
    }
}
