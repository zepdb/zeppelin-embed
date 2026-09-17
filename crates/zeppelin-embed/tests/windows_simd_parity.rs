//! Runtime SIMD selection and cross-arm result parity, observed directly.
//!
//! Running an existing suite with `ZE_KERNEL` set in the environment proves
//! nothing on its own: the kernels suite spawns its own children with its own
//! overrides, so an ambient value can be ignored entirely and every arm still
//! reports "ok". This file therefore does not infer the arm from a passing
//! run. It spawns a child, has that child report the arm it actually selected,
//! and asserts on the report.
//!
//! The parity claim is the important one: the same query over the same fixture
//! must produce bit-identical results under the scalar oracle and under the
//! vectorised arm. Exact integer and identity checks stay exact; nothing here
//! relaxes a comparison to a tolerance.

#![cfg(windows)]
#![allow(clippy::expect_used, clippy::panic)]

use std::process::Command;

use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::kernels::{KernelArm, is_arm_supported};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};

/// Marks the subprocess so the helper does its work instead of returning.
const CHILD: &str = "ZE_SIMD_PARITY_CHILD";

const DIMENSION: usize = 16;
const ROWS: u128 = 96;

fn fixture_vector(seed: u128) -> Vec<f32> {
    (0..DIMENSION)
        .map(|index| ((seed as usize * 13 + index * 7) % 29) as f32)
        .collect()
}

/// Runs the helper in a fresh process under the given `ZE_KERNEL` value and
/// returns everything it printed.
///
/// A fresh process is required: the selected arm is cached on first use, so an
/// in-process override could not change it.
fn child_report(kernel: Option<&str>) -> String {
    let executable = std::env::current_exe().expect("test executable");
    let mut command = Command::new(executable);
    command
        .arg("simd_selection_child_helper")
        .arg("--exact")
        .arg("--ignored")
        .arg("--nocapture")
        .env(CHILD, "1");
    match kernel {
        Some(value) => {
            command.env("ZE_KERNEL", value);
        }
        None => {
            command.env_remove("ZE_KERNEL");
        }
    }
    let output = command.output().expect("run the SIMD child helper");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "child helper failed under ZE_KERNEL={kernel:?}: {stdout}"
    );
    stdout
}

/// Extracts one `KEY=value` line from a child's output.
fn field<'a>(report: &'a str, key: &str) -> &'a str {
    report
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .unwrap_or_else(|| panic!("child report has no {key} line:\n{report}"))
        .trim()
}

/// Subprocess helper. Reports the arm it selected and, when initialization
/// succeeded, the exact result of one deterministic query under that arm.
#[test]
#[ignore = "subprocess-only helper selected by the SIMD parity tests"]
fn simd_selection_child_helper() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    match zeppelin_embed::kernels::initialize() {
        Ok(arm) => {
            println!("ARM={arm:?}");
            println!("RESULT={}", exact_query_signature());
        }
        Err(error) => println!("INITERR={error:?}"),
    }
}

/// Builds a fixture, runs one exact query and renders the outcome as a stable
/// string: `docid:scorebits` per candidate, in returned order.
///
/// Score bits, not a rounded value: a vectorised arm that produced a slightly
/// different exact score would be caught rather than tolerated.
fn exact_query_signature() -> String {
    let directory = tempfile::tempdir().expect("fixture directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open fixture");
    let documents = (0..ROWS)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(row + 1), Revision::new(1)),
                fixture_vector(row),
            )
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents))
        .expect("ingest fixture");
    store.seal().expect("seal fixture");

    let query = fixture_vector(7);
    let outcome = store
        .search(
            SearchRequest::new(&query),
            16,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("exact query");
    let rendered = outcome
        .candidates
        .iter()
        .map(|candidate| {
            let id = candidate
                .document()
                .map(|document| document.doc_id().get())
                .unwrap_or_default();
            format!("{id}:{:08x}", candidate.score().to_bits())
        })
        .collect::<Vec<_>>()
        .join(",");
    store.close().expect("close fixture");
    rendered
}

/// An explicit `scalar` override must select the scalar oracle, on every host.
#[test]
fn an_explicit_scalar_override_selects_the_scalar_arm() {
    let report = child_report(Some("scalar"));
    assert_eq!(field(&report, "ARM="), format!("{:?}", KernelArm::Scalar));
}

/// With no override the engine must select the best arm this CPU supports.
/// Asserting against `is_arm_supported` rather than a hard-coded arm keeps the
/// test honest on a host without AVX2.
#[test]
fn no_override_selects_the_best_supported_arm() {
    let report = child_report(None);
    let expected = if is_arm_supported(KernelArm::Avx2) {
        KernelArm::Avx2
    } else {
        KernelArm::Scalar
    };
    assert_eq!(field(&report, "ARM="), format!("{expected:?}"));
}

/// An arm this CPU can run must be selectable; one it cannot must return the
/// typed unsupported result rather than silently falling back to scalar.
#[test]
fn an_unavailable_arm_returns_the_typed_unsupported_result() {
    // NEON is an AArch64 family and can never run on x86-64, so this is the
    // unavailable case on every host this port targets.
    let neon = child_report(Some("neon"));
    let error = field(&neon, "INITERR=");
    assert!(
        error.contains("UnsupportedArm") && error.contains("Neon"),
        "an unavailable arm must report UnsupportedArm, observed {error}"
    );

    // AVX2 is the available case here, and must be selectable when supported.
    let avx2 = child_report(Some("avx2"));
    if is_arm_supported(KernelArm::Avx2) {
        assert_eq!(field(&avx2, "ARM="), format!("{:?}", KernelArm::Avx2));
    } else {
        let error = field(&avx2, "INITERR=");
        assert!(
            error.contains("UnsupportedArm"),
            "a host without AVX2 must refuse it, observed {error}"
        );
    }
}

/// An unrecognised override is a typed rejection, not a silent fallback.
#[test]
fn an_unknown_override_is_rejected() {
    let report = child_report(Some("definitely-not-an-arm"));
    let error = field(&report, "INITERR=");
    assert!(
        error.contains("UnknownOverride"),
        "an unknown override must be rejected, observed {error}"
    );
}

/// The parity claim: the vectorised arm and the scalar oracle must agree
/// bit-for-bit on the same exact query over the same fixture.
#[test]
fn the_vectorised_arm_agrees_with_the_scalar_oracle_bit_for_bit() {
    let scalar = child_report(Some("scalar"));
    let scalar_result = field(&scalar, "RESULT=");
    assert!(
        !scalar_result.is_empty(),
        "the scalar child produced no result"
    );

    if !is_arm_supported(KernelArm::Avx2) {
        // Nothing to compare against. Say so rather than passing quietly.
        panic!(
            "AVX2 is unavailable on this host, so the cross-arm parity cell \
             cannot execute here; it must be reported unexecuted, not passed"
        );
    }
    let avx2 = child_report(Some("avx2"));
    assert_eq!(
        field(&avx2, "ARM="),
        format!("{:?}", KernelArm::Avx2),
        "the AVX2 child did not actually select AVX2"
    );
    assert_eq!(
        field(&avx2, "RESULT="),
        scalar_result,
        "AVX2 and the scalar oracle disagree on an exact query"
    );
}
