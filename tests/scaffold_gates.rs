use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace-tests crate must be directly under the repository root")
        .to_path_buf()
}

fn fixture_dir(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = std::env::temp_dir().join(format!(
        "zeppelin-embed-{name}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn run_script<I, S>(name: &str, args: I, envs: &[(&str, &str)]) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let script = repo_root().join("scripts").join(name);
    let mut command = Command::new(&script);
    command.args(args).current_dir(repo_root());
    for key in [
        "LLVM_PROFILE_FILE",
        "RUSTC_WRAPPER",
        "CARGO_LLVM_COV",
        "CARGO_LLVM_COV_SHOW_ENV",
        "CARGO_LLVM_COV_TARGET_DIR",
        "CARGO_LLVM_COV_BUILD_DIR",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER_RUSTFLAGS",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER_COVERAGE_TARGET",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER_HOST",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER_PRE_EXISTING",
    ] {
        command.env_remove(key);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    command
        .output()
        .unwrap_or_else(|error| panic!("failed to run {}: {error}", script.display()))
}

fn combined_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn coverage_script_fails_below_threshold() -> Result<(), Box<dyn Error>> {
    let fixture = fixture_dir("coverage")?;
    fs::create_dir_all(fixture.join("src"))?;
    fs::write(
        fixture.join("Cargo.toml"),
        "[package]\nname = \"low-coverage-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )?;
    fs::write(
        fixture.join("src/lib.rs"),
        "pub fn covered() -> u8 { 1 }\n\npub fn uncovered(value: bool) -> u8 {\n    if value { 2 } else { 3 }\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn covers_one_line() {\n        assert_eq!(super::covered(), 1);\n    }\n}\n",
    )?;

    let manifest = fixture.join("Cargo.toml");
    let target = fixture.join("target");
    let output = run_script(
        "coverage.sh",
        ["--manifest-path".as_ref(), manifest.as_os_str()],
        &[("CARGO_TARGET_DIR", target.to_str().ok_or("non-UTF-8 path")?)],
    );
    let text = combined_output(&output);
    assert_eq!(
        output.status.code(),
        Some(1),
        "coverage gate did not reject the known-low-coverage crate via its threshold:\n{text}"
    );
    fs::remove_dir_all(fixture)?;
    Ok(())
}

#[test]
#[ignore = "shells out to cargo-deny; CI runs this test explicitly"]
fn deny_blacklist_rejects_banned_dep() -> Result<(), Box<dyn Error>> {
    let fixture = fixture_dir("deny")?;
    fs::create_dir_all(fixture.join("src"))?;
    fs::create_dir_all(fixture.join("tokio/src"))?;
    fs::write(
        fixture.join("Cargo.toml"),
        "[package]\nname = \"banned-dependency-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n\n[dependencies]\ntokio = { path = \"tokio\" }\n",
    )?;
    fs::write(fixture.join("src/lib.rs"), "pub fn fixture() {}\n")?;
    fs::write(
        fixture.join("tokio/Cargo.toml"),
        "[package]\nname = \"tokio\"\nversion = \"1.0.0\"\nedition = \"2024\"\n",
    )?;
    fs::write(fixture.join("tokio/src/lib.rs"), "pub fn runtime() {}\n")?;
    fs::copy(repo_root().join("deny.toml"), fixture.join("deny.toml"))?;

    let output = Command::new("cargo")
        .args([
            "deny",
            "--manifest-path",
            fixture
                .join("Cargo.toml")
                .to_str()
                .ok_or("non-UTF-8 path")?,
            "check",
            "bans",
        ])
        .current_dir(repo_root())
        .output()?;
    let text = combined_output(&output);
    assert!(
        !output.status.success(),
        "dependency blacklist accepted tokio:\n{text}"
    );
    assert!(
        text.contains("tokio") && text.contains("banned"),
        "cargo-deny failed for the wrong reason; expected a tokio ban:\n{text}"
    );
    fs::remove_dir_all(fixture)?;
    Ok(())
}

#[test]
fn size_budget_fails_on_inflated_binary() {
    let output = run_script(
        "size-budget.sh",
        std::iter::empty::<&str>(),
        &[("ZE_SIZE_BUDGET_KB", "0")],
    );
    let text = combined_output(&output);
    assert!(
        !output.status.success(),
        "size gate accepted an artifact under a zero-KB budget:\n{text}"
    );
    let rejected_for_configured_budget = text.lines().any(|line| {
        line.starts_with("error: core stripped staticlib linked size ")
            && line.ends_with(" KB exceeds budget 0 KB")
    });
    assert!(
        rejected_for_configured_budget,
        "size failure did not emit the exact configured-budget rejection line:\n{text}"
    );
}
