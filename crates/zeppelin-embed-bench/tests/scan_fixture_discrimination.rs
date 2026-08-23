#[path = "../../zeppelin-embed/benches/scan.rs"]
#[allow(dead_code)]
mod scan_bench;

#[test]
fn random_int8_fixture_is_discriminating_and_default() {
    let (distinct, is_default) =
        scan_bench::random_int8_fixture_distinct_rows(128, 768).expect("random Int8 fixture");
    assert_eq!(distinct, 128);
    assert!(is_default);
}
