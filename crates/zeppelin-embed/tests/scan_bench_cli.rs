#[allow(dead_code)]
#[path = "../benches/scan.rs"]
mod scan_bench;

use scan_bench::scan_repeat;

use std::error::Error;
use std::time::Duration;

#[test]
fn scan_benchmark_repeats_reuse_one_fixture_build() {
    let mut arguments = [String::from("3")].into_iter();
    let repeats = scan_repeat::parse_repeats(&mut arguments).expect("valid --repeats value");
    let mut fixture_builds = 0_usize;
    let mut timed_repeats = 0_usize;
    let mut output = Vec::new();

    scan_repeat::with_single_fixture(
        || {
            fixture_builds += 1;
            Ok::<_, Box<dyn Error>>(())
        },
        |fixture| {
            scan_repeat::write_timed_repeats(&mut output, fixture, repeats, 2, |_| {
                timed_repeats += 1;
                Ok(Duration::from_secs(timed_repeats as u64 * 2))
            })
        },
    )
    .expect("repeat benchmark succeeds");

    let output = String::from_utf8(output).expect("benchmark output is UTF-8");
    assert_eq!(fixture_builds, 1, "fixture builds: {fixture_builds}");
    assert_eq!(
        output.lines().collect::<Vec<_>>(),
        [
            "repeat 1/3: mean wall time per scan: 1.000000 s over 2 iterations",
            "repeat 2/3: mean wall time per scan: 2.000000 s over 2 iterations",
            "repeat 3/3: mean wall time per scan: 3.000000 s over 2 iterations",
            "summary: mean wall time per scan: 2.000000 s across 3 repeats; relative standard deviation: 50.000%",
        ],
        "benchmark output:\n{output}"
    );
    assert_eq!(timed_repeats, 3, "timed repeats: {timed_repeats}");
}

#[test]
fn scan_benchmark_single_repeat_preserves_timing_output() {
    let mut output = Vec::new();

    scan_repeat::write_timed_repeats(&mut output, &(), 1, 2, |_| Ok(Duration::from_secs(2)))
        .expect("single-repeat benchmark succeeds");

    assert_eq!(
        output,
        b"mean wall time per scan: 1.000000 s over 2 iterations\n"
    );
}

#[test]
fn scan_benchmark_repeat_validation_rejects_zero_or_missing_values() {
    assert!(scan_repeat::parse_repeats(&mut std::iter::empty()).is_err());
    assert!(scan_repeat::parse_repeats(&mut [String::from("0")].into_iter()).is_err());
    assert!(scan_repeat::parse_repeats(&mut [String::from("nope")].into_iter()).is_err());
    assert!(
        scan_repeat::write_timed_repeats(&mut Vec::new(), &(), 0, 1, |_| { Ok(Duration::ZERO) })
            .is_err()
    );
    assert!(
        scan_repeat::write_timed_repeats(&mut Vec::new(), &(), 1, 0, |_| { Ok(Duration::ZERO) })
            .is_err()
    );
}

#[test]
fn scan_benchmark_zero_duration_has_zero_relative_standard_deviation() {
    let mut output = Vec::new();

    scan_repeat::write_timed_repeats(&mut output, &(), 2, 1, |_| Ok(Duration::ZERO))
        .expect("zero-duration repeat summary succeeds");

    let output = String::from_utf8(output).expect("benchmark output is UTF-8");
    assert!(
        output.ends_with("relative standard deviation: 0.000%\n"),
        "benchmark output:\n{output}"
    );
}
