use zeppelin_embed_bench::process_median::ProcessMedian;

#[test]
fn across_process_summary_exposes_the_outlier_hidden_by_within_process_rsd() {
    let summary = ProcessMedian::new(vec![8.710, 8.822, 12.505])
        .expect("three process observations are valid");

    assert_eq!(summary.median(), 8.822);
    assert_eq!(summary.minimum(), 8.710);
    assert_eq!(summary.maximum(), 12.505);
    assert!((summary.spread_percent() - 43.017_46).abs() < 0.000_01);
}

#[test]
fn across_process_summary_refuses_even_or_invalid_process_sets() {
    assert!(ProcessMedian::new(vec![1.0, 2.0]).is_err());
    assert!(ProcessMedian::new(vec![1.0, f64::NAN, 3.0]).is_err());
}
