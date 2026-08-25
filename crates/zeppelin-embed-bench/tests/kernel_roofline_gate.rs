use zeppelin_embed_bench::kernel_gate::{
    GateFailure, KernelMeasurements, evaluate_kernel_measurements,
};

#[test]
fn roofline_and_ratio_contracts_reject_a_tenfold_float_regression() {
    let failure = evaluate_kernel_measurements(KernelMeasurements {
        i8_ns: 10.004_013,
        u1_ns: 2.919_274,
        f16_ns: 443.487_37,
        f32_ns: 43.288_878,
    })
    .expect_err("a tenfold f16 slowdown must trip the gate");

    assert!(
        failure
            .iter()
            .any(|failure| matches!(failure, GateFailure::RooflineFloor { kernel: "f16", .. }))
    );
    assert!(failure.iter().any(|failure| matches!(
        failure,
        GateFailure::RatioCeiling {
            numerator: "f16",
            denominator: "i8",
            ..
        }
    )));
}

#[test]
fn measured_task03_rows_clear_generous_fivefold_headroom() {
    let report = evaluate_kernel_measurements(KernelMeasurements {
        i8_ns: 10.004_013,
        u1_ns: 2.919_274,
        f16_ns: 44.348_737,
        f32_ns: 43.288_878,
    })
    .expect("recorded task-03 medians must clear the derived gates");

    assert!(report.f32_over_i8 < 8.0);
    assert!(report.f16_over_i8 < 6.0);
}
