//! Compiles and runs a native MSVC C consumer against both distribution forms.
//!
//! Rust ABI tests prove what Rust believes about the layout. This proves what
//! the C compiler believes, which is the thing a real consumer depends on:
//! `cl.exe` parses the generated header, computes every `abi_size` itself, and
//! the resulting executable is linked once against the static implementation
//! archive and once against the DLL's import library.
//!
//! Both links matter. A static consumer needs the real archive plus the system
//! libraries rustc reports; a dynamic consumer needs the import library and
//! must find the DLL at run time. Linking the import library by mistake where
//! the implementation archive was intended produces an executable that cannot
//! run, which is exactly the confusion this guards against.
//!
//! Ignored by default: it shells out to a release build and to the MSVC
//! toolchain, which is qualification work rather than an ordinary unit test.

#![cfg(all(windows, target_env = "msvc"))]
#![allow(clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;

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

/// Finds a tool in the latest installed MSVC x64 toolset.
fn msvc_tool(name: &str) -> Option<PathBuf> {
    let program_files = std::env::var("ProgramFiles(x86)").ok()?;
    let vswhere =
        PathBuf::from(program_files).join("Microsoft Visual Studio/Installer/vswhere.exe");
    let output = Command::new(vswhere)
        .args(["-products", "*", "-latest", "-property", "installationPath"])
        .output()
        .ok()?;
    let root = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    let mut versions = std::fs::read_dir(root.join("VC/Tools/MSVC"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    versions.sort();
    versions
        .into_iter()
        .rev()
        .map(|version| version.join("bin/Hostx64/x64").join(name))
        .find(|candidate| candidate.is_file())
}

/// The include and library search paths a bare `cl.exe` needs.
///
/// A Developer PowerShell sets these through `vcvars64.bat`; running the tool
/// directly means supplying them explicitly, which also pins exactly which SDK
/// the fixture was compiled against.
struct MsvcEnvironment {
    includes: Vec<PathBuf>,
    libraries: Vec<PathBuf>,
}

fn msvc_environment() -> Option<MsvcEnvironment> {
    let program_files = std::env::var("ProgramFiles(x86)").ok()?;
    let vswhere =
        PathBuf::from(&program_files).join("Microsoft Visual Studio/Installer/vswhere.exe");
    let output = Command::new(vswhere)
        .args(["-products", "*", "-latest", "-property", "installationPath"])
        .output()
        .ok()?;
    let vs_root = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    let mut toolsets = std::fs::read_dir(vs_root.join("VC/Tools/MSVC"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    toolsets.sort();
    let toolset = toolsets.pop()?;

    let kits = PathBuf::from(&program_files).join("Windows Kits/10");
    let mut sdks = std::fs::read_dir(kits.join("Include"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    sdks.sort();
    let sdk = sdks.pop()?;
    let sdk_version = sdk.file_name()?.to_owned();

    Some(MsvcEnvironment {
        includes: vec![
            toolset.join("include"),
            sdk.join("ucrt"),
            sdk.join("um"),
            sdk.join("shared"),
        ],
        libraries: vec![
            toolset.join("lib/x64"),
            kits.join("Lib").join(&sdk_version).join("ucrt/x64"),
            kits.join("Lib").join(&sdk_version).join("um/x64"),
        ],
    })
}

/// Builds the release FFI artifacts with default (non-`text`) features.
fn build_release_artifacts(target_dir: &Path) -> PathBuf {
    let workspace = workspace_root();
    let mut command = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()));
    command
        .current_dir(&workspace)
        .env("CARGO_TARGET_DIR", target_dir)
        .args([
            "build",
            "-p",
            "zeppelin-embed-ffi",
            "--release",
            "--no-default-features",
            "--target",
            "x86_64-pc-windows-msvc",
        ]);
    for key in [
        "LLVM_PROFILE_FILE",
        "RUSTC_WRAPPER",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
    ] {
        command.env_remove(key);
    }
    assert!(
        command.status().expect("build FFI artifacts").success(),
        "release FFI build failed"
    );
    target_dir.join("x86_64-pc-windows-msvc/release")
}

/// Compiles the C fixture and links it against `link_library`.
///
/// `/TC` compiles as C (not C++), `/std:c11` pins the dialect and `/MD` selects
/// the dynamic CRT, matching the CRT rustc links so one process never holds two
/// C runtimes.
fn compile_consumer(
    environment: &MsvcEnvironment,
    cl: &Path,
    output_dir: &Path,
    executable: &str,
    link_library: &Path,
) -> PathBuf {
    let source = crate_dir().join("tests/fixtures/windows_c_consumer.c");
    let exe = output_dir.join(executable);
    let mut command = Command::new(cl);
    command
        .current_dir(output_dir)
        .arg("/nologo")
        .arg("/TC")
        .arg("/std:c11")
        .arg("/MD")
        .arg("/W3")
        .arg(format!("/I{}", crate_dir().join("include").display()));
    for include in &environment.includes {
        command.arg(format!("/I{}", include.display()));
    }
    command.arg(&source).arg(format!("/Fe:{}", exe.display()));
    command.arg("/link");
    for library in &environment.libraries {
        command.arg(format!("/LIBPATH:{}", library.display()));
    }
    command.arg(link_library);
    // The system libraries rustc reports for a static link. Discovered from
    // `--print native-static-libs` rather than guessed.
    for system in [
        "kernel32.lib",
        "ntdll.lib",
        "userenv.lib",
        "ws2_32.lib",
        "dbghelp.lib",
        "advapi32.lib",
        "bcrypt.lib",
    ] {
        command.arg(system);
    }

    let output = command.output().expect("run cl.exe");
    assert!(
        output.status.success(),
        "cl.exe failed for {executable}:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(exe.is_file(), "cl.exe produced no {}", exe.display());
    exe
}

/// Runs a built consumer against a fresh store directory and asserts it passed.
fn run_consumer(executable: &Path, label: &str) {
    let store = tempfile::tempdir().expect("consumer store directory");
    let output = Command::new(executable)
        .arg(store.path())
        .output()
        .unwrap_or_else(|error| panic!("run the {label} consumer: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("--- {label} consumer ---\n{stdout}");
    assert!(
        output.status.success(),
        "the {label} consumer failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("ALL CHECKS PASSED"),
        "the {label} consumer did not report success:\n{stdout}"
    );
}

/// Both distribution forms must compile against the generated header with a
/// real C compiler and execute the same lifecycle.
#[test]
#[ignore = "manual full-build qualification; invokes cargo and the MSVC toolchain"]
fn a_native_c_consumer_links_and_runs_against_both_distribution_forms() {
    let Some(cl) = msvc_tool("cl.exe") else {
        panic!("cl.exe was not found; this gate requires the MSVC C++ build tools");
    };
    let Some(environment) = msvc_environment() else {
        panic!("could not locate the MSVC toolset and Windows SDK include/lib paths");
    };

    let staging = tempfile::tempdir().expect("consumer staging directory");
    let target_dir = workspace_root().join("target/ffi-c-consumer-windows");
    let release = build_release_artifacts(&target_dir);

    let static_archive = release.join("zeppelin_embed_ffi.lib");
    let import_library = release.join("zeppelin_embed_ffi.dll.lib");
    let dll = release.join("zeppelin_embed_ffi.dll");
    assert!(
        static_archive.is_file(),
        "missing {}",
        static_archive.display()
    );
    assert!(
        import_library.is_file(),
        "missing {}",
        import_library.display()
    );
    assert!(dll.is_file(), "missing {}", dll.display());

    // Form 1: linked directly against the static implementation archive. The
    // resulting executable needs no Zeppelin DLL at run time.
    let static_exe = compile_consumer(
        &environment,
        &cl,
        staging.path(),
        "consumer_static.exe",
        &static_archive,
    );
    run_consumer(&static_exe, "statically linked");

    // Form 2: linked against the DLL's import library. The DLL must be beside
    // the executable, and it is deliberately copied rather than found on PATH
    // so the run proves the packaged layout works.
    let dynamic_dir = staging.path().join("dynamic");
    std::fs::create_dir_all(&dynamic_dir).expect("dynamic staging directory");
    std::fs::copy(&dll, dynamic_dir.join("zeppelin_embed_ffi.dll")).expect("stage the DLL");
    let dynamic_exe = compile_consumer(
        &environment,
        &cl,
        &dynamic_dir,
        "consumer_dynamic.exe",
        &import_library,
    );
    run_consumer(&dynamic_exe, "dynamically linked");

    // The two forms must genuinely differ, or the dynamic case would pass even
    // if it had accidentally been linked statically.
    //
    // This is decided from the import table rather than by deleting the DLL and
    // watching the process fail. A missing-DLL launch raises a modal Windows
    // error dialog, which is a poor thing for a test suite to do to whoever is
    // at the keyboard, and it infers linkage from a crash instead of reading
    // it. `dumpbin /dependents` states it directly.
    let dependents = |executable: &Path| -> String {
        let dumpbin = msvc_tool("dumpbin.exe").expect("dumpbin.exe");
        let output = Command::new(dumpbin)
            .arg("/dependents")
            .arg(executable)
            .output()
            .expect("run dumpbin /dependents");
        assert!(
            output.status.success(),
            "dumpbin /dependents failed for {}: {}",
            executable.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    let dynamic_imports = dependents(&dynamic_exe);
    assert!(
        dynamic_imports.contains("zeppelin_embed_ffi.dll"),
        "the dynamic consumer does not import the DLL, so it was not really \
         dynamically linked:\n{dynamic_imports}"
    );

    let static_imports = dependents(&static_exe);
    assert!(
        !static_imports.contains("zeppelin_embed_ffi.dll"),
        "the static consumer imports the DLL, so the implementation archive and \
         the import library were swapped:\n{static_imports}"
    );

    // The static form must need no Zeppelin DLL beside it at all, which is the
    // property the Node addon depends on.
    assert!(
        !static_exe
            .parent()
            .expect("static consumer directory")
            .join("zeppelin_embed_ffi.dll")
            .exists(),
        "the static consumer was run with a DLL beside it, so its independence \
         from one was not actually demonstrated"
    );
}
