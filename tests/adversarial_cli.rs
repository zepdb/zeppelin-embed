#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::{Command, Output};

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace-tests package has a repository parent")
        .join("scripts/adversarial.sh")
}

fn macos_supervisor() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace-tests package has a repository parent")
        .join("scripts/adversarial-macos.sh")
}

fn run(arguments: &[&str], environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new("/bin/bash");
    command.arg(script()).args(arguments).env_clear();
    command.env("ZE_ADV_CLI_TEST", "1");
    for (key, value) in environment {
        command.env(key, value);
    }
    command.output().expect("run adversarial CLI")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("CLI stdout is UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("CLI stderr is UTF-8")
}

#[test]
fn list_prints_the_stable_campaign_catalog() {
    let output = run(&["list"], &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).lines().collect::<Vec<_>>(),
        vec![
            "overall",
            "storage-durability",
            "ingest-retention",
            "vector-execution",
            "vamana-graph",
            "metadata-filter-planner",
            "fts",
            "hybrid-fusion",
            "tiering-maintenance",
            "lifecycle-accounting",
            "diagnostics-health",
            "ffi-bindings",
        ]
    );
}

#[test]
fn cli_values_override_environment_and_environment_overrides_defaults() {
    let output = run(
        &[
            "episode",
            "--campaign",
            "vector-execution",
            "--seed",
            "9",
            "--profile",
            "none",
            "--qualification",
            "exploratory",
        ],
        &[
            ("ZE_ADV_CAMPAIGN", "fts"),
            ("ZE_ADV_SEED", "4"),
            ("ZE_ADV_PROFILE", "content"),
            ("ZE_ADV_QUALIFICATION", "release"),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains(
            "subcommand=episode campaign=vector-execution seed=9 profile=none qualification=exploratory"
        ),
        "{}",
        stdout(&output)
    );

    let environment_only = run(
        &["episode"],
        &[
            ("ZE_ADV_CAMPAIGN", "fts"),
            ("ZE_ADV_SEED", "4"),
            ("ZE_ADV_PROFILE", "content"),
        ],
    );
    assert!(
        environment_only.status.success(),
        "{}",
        stderr(&environment_only)
    );
    assert!(
        stdout(&environment_only).contains(
            "subcommand=episode campaign=fts seed=4 profile=content qualification=exploratory"
        ),
        "{}",
        stdout(&environment_only)
    );
}

#[test]
fn invalid_campaign_duplicates_conflicts_and_numeric_values_fail_before_cargo() {
    for arguments in [
        vec!["episode", "--campaign", "unknown"],
        vec!["episode", "--campaign", "fts", "--campaign", "overall"],
        vec!["episode", "--seed", "-1"],
        vec!["episode", "--min-seconds", "nope"],
        vec!["episode", "--replay-dir", "artifact"],
        vec!["replay"],
    ] {
        let output = run(&arguments, &[]);
        assert!(!output.status.success(), "accepted {arguments:?}");
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
    }
    let unknown = run(&["episode", "--campaign", "unknown"], &[]);
    assert!(stderr(&unknown).contains("valid campaigns:"));
    assert!(stderr(&unknown).contains("ffi-bindings"));
    assert!(!stderr(&unknown).contains("epoch-alias-transitions"));
    assert!(!stderr(&unknown).contains("embedding-delegate"));
}

#[test]
fn release_thresholds_and_subcommand_specific_arguments_are_validated() {
    let too_short = run(
        &[
            "campaign",
            "--qualification",
            "release",
            "--min-seconds",
            "28799",
            "--min-episodes",
            "10000",
        ],
        &[],
    );
    assert_eq!(too_short.status.code(), Some(2));
    assert!(stderr(&too_short).contains("at least 28800 seconds"));

    let exploratory = run(
        &[
            "campaign",
            "--campaign",
            "fts",
            "--qualification",
            "exploratory",
            "--min-seconds",
            "0",
            "--min-episodes",
            "0",
        ],
        &[],
    );
    assert!(exploratory.status.success(), "{}", stderr(&exploratory));
}

#[test]
fn comma_separated_feature_campaigns_are_validated_and_preserve_order() {
    let output = run(
        &[
            "campaign",
            "--campaign",
            "fts,hybrid-fusion,diagnostics-health",
            "--qualification",
            "exploratory",
            "--min-seconds",
            "0",
            "--min-episodes",
            "500",
        ],
        &[],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("campaigns=fts,hybrid-fusion,diagnostics-health"));

    for invalid in [
        "fts,unknown",
        "fts,,hybrid-fusion",
        ",fts",
        "fts,",
        "fts,fts",
    ] {
        let rejected = run(&["campaign", "--campaign", invalid], &[]);
        assert!(!rejected.status.success(), "accepted {invalid:?}");
        assert_eq!(rejected.status.code(), Some(2));
    }

    let episode = run(&["episode", "--campaign", "fts,hybrid-fusion"], &[]);
    assert!(!episode.status.success());
    assert!(stderr(&episode).contains("exactly one campaign"));
}

#[test]
fn macos_supervisor_runs_overall_concurrently_and_reports_it_separately() {
    let source = std::fs::read_to_string(macos_supervisor()).expect("read macOS supervisor");
    for required in [
        "--campaign overall",
        "overall_pid=$!",
        "wait \"$overall_pid\"",
        "\"feature_episodes\": total_feature_episodes",
        "\"overall_episodes\": overall_episodes",
        "feature_episodes=11000 overall_episodes=1000",
    ] {
        assert!(source.contains(required), "supervisor omitted {required}");
    }
    assert!(
        source.contains("--artifacts \"$overall_artifacts\""),
        "overall evidence is not isolated in its own artifact root"
    );
}
