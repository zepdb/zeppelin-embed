use std::collections::BTreeMap;

use zeppelin_embed::quant::QuantScheme;
use zeppelin_embed_bench::recall::datasets::{SyntheticKind, load_bvecs, load_fvecs, synthetic};
use zeppelin_embed_bench::recall::run_recall;

#[test]
fn synthetic_families_expose_required_jaggedness() {
    const ROWS: usize = 256;
    const QUERIES: usize = 8;
    const DIMENSION: usize = 64;
    let uniform =
        synthetic(SyntheticKind::Uniform, ROWS, QUERIES, DIMENSION, 11).expect("uniform fixture");
    let anisotropic = synthetic(SyntheticKind::Anisotropic, ROWS, QUERIES, DIMENSION, 13)
        .expect("anisotropic fixture");
    let clustered = synthetic(SyntheticKind::Clustered, ROWS, QUERIES, DIMENSION, 17)
        .expect("clustered fixture");
    let heavy = synthetic(SyntheticKind::HeavyTailed, ROWS, QUERIES, DIMENSION, 19)
        .expect("heavy-tail fixture");
    let correlated = synthetic(SyntheticKind::Correlated, ROWS, QUERIES, DIMENSION, 23)
        .expect("correlated fixture");

    let uniform_variances = dimension_variances(&uniform.vectors.values, DIMENSION);
    let anisotropic_variances = dimension_variances(&anisotropic.vectors.values, DIMENSION);
    assert!(
        variance_ratio(&anisotropic_variances) > variance_ratio(&uniform_variances) * 20.0,
        "anisotropic variance spread was not materially wider"
    );

    let labels = clustered
        .cluster_labels
        .as_ref()
        .expect("clustered data carries validation labels");
    let mut counts = BTreeMap::<usize, usize>::new();
    for &label in labels {
        *counts.entry(label).or_default() += 1;
    }
    let least = counts.values().copied().min().expect("non-empty labels");
    let most = counts.values().copied().max().expect("non-empty labels");
    assert!(
        most >= least * 3,
        "cluster densities did not vary: {counts:?}"
    );

    let mut norms = heavy
        .vectors
        .values
        .chunks_exact(DIMENSION)
        .map(norm)
        .collect::<Vec<_>>();
    norms.sort_by(f64::total_cmp);
    assert!(
        norms[ROWS * 9 / 10] / norms[ROWS / 2] > 2.5,
        "heavy-tail p90/p50 norm ratio was too small"
    );

    let uniform_lag = mean_adjacent_correlation(&uniform.vectors.values, DIMENSION);
    let correlated_lag = mean_adjacent_correlation(&correlated.vectors.values, DIMENSION);
    assert!(
        correlated_lag > uniform_lag + 0.45,
        "correlated lag signal {correlated_lag:?} was not distinct from uniform {uniform_lag:?}"
    );
}

#[test]
fn recall_table_counts_bytes_for_every_scheme_and_oversample() {
    let dataset = synthetic(SyntheticKind::Uniform, 64, 4, 64, 29).expect("uniform fixture");

    let report = run_recall(&dataset, &[1, 2], 10).expect("recall report");

    assert_eq!(report.points.len(), 4);
    for scheme in [QuantScheme::Bit4, QuantScheme::Int8] {
        assert_eq!(
            report
                .points
                .iter()
                .filter(|point| point.scheme == scheme)
                .count(),
            2
        );
    }
    for point in report.points {
        assert!((0.0..=1.0).contains(&point.recall_at_10));
        assert!(point.bytes_per_query.coarse > 0);
        assert!(point.bytes_per_query.rescore > 0);
        assert_eq!(
            point.bytes_per_query.total(),
            point.bytes_per_query.coarse + point.bytes_per_query.rescore
        );
    }
}

#[test]
fn seeded_rescore_recall_at_oversample_four_is_at_least_95_percent() {
    let dataset =
        synthetic(SyntheticKind::Clustered, 128, 16, 128, 31).expect("clustered recall fixture");

    let report = run_recall(&dataset, &[4], 10).expect("recall report");

    for point in report.points {
        assert!(
            point.recall_at_10 >= 0.95,
            "{:?} recall {:?} fell below 0.95",
            point.scheme,
            point.recall_at_10
        );
    }
}

#[test]
fn fvecs_and_bvecs_loaders_read_standard_records() {
    let base = std::env::temp_dir().join(format!("zeppelin-recall-loaders-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("temporary loader directory");
    let fvecs_path = base.join("fixture.fvecs");
    let bvecs_path = base.join("fixture.bvecs");
    let mut fvecs = Vec::new();
    let mut bvecs = Vec::new();
    for row in [[1.0_f32, -2.5, 3.25], [4.5, 5.0, -6.0]] {
        fvecs.extend_from_slice(&3_i32.to_le_bytes());
        for value in row {
            fvecs.extend_from_slice(&value.to_le_bytes());
        }
    }
    for row in [[1_u8, 2, 255], [0, 17, 99]] {
        bvecs.extend_from_slice(&3_i32.to_le_bytes());
        bvecs.extend_from_slice(&row);
    }
    std::fs::write(&fvecs_path, fvecs).expect("write fvecs fixture");
    std::fs::write(&bvecs_path, bvecs).expect("write bvecs fixture");

    let loaded_fvecs = load_fvecs(&fvecs_path).expect("load fvecs fixture");
    let loaded_bvecs = load_bvecs(&bvecs_path).expect("load bvecs fixture");

    assert_eq!(loaded_fvecs.dimension, 3);
    assert_eq!(loaded_fvecs.rows(), 2);
    assert_eq!(loaded_fvecs.values, [1.0, -2.5, 3.25, 4.5, 5.0, -6.0]);
    assert_eq!(loaded_bvecs.dimension, 3);
    assert_eq!(loaded_bvecs.rows(), 2);
    assert_eq!(loaded_bvecs.values, [1.0, 2.0, 255.0, 0.0, 17.0, 99.0]);

    std::fs::remove_dir_all(base).expect("remove loader fixtures");
}

fn dimension_variances(values: &[f32], dimension: usize) -> Vec<f64> {
    let rows = values.len() / dimension;
    (0..dimension)
        .map(|coordinate| {
            let column = values
                .chunks_exact(dimension)
                .map(|row| f64::from(row[coordinate]))
                .collect::<Vec<_>>();
            let mean = column.iter().sum::<f64>() / rows as f64;
            column
                .iter()
                .map(|value| (value - mean) * (value - mean))
                .sum::<f64>()
                / rows as f64
        })
        .collect()
}

fn variance_ratio(variances: &[f64]) -> f64 {
    let minimum = variances.iter().copied().fold(f64::INFINITY, f64::min);
    let maximum = variances.iter().copied().fold(0.0_f64, f64::max);
    maximum / minimum
}

fn norm(row: &[f32]) -> f64 {
    row.iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt()
}

fn mean_adjacent_correlation(values: &[f32], dimension: usize) -> f64 {
    let (cross, left, right) = values.chunks_exact(dimension).fold(
        (0.0_f64, 0.0_f64, 0.0_f64),
        |(cross, left, right), row| {
            row.windows(2)
                .fold((cross, left, right), |(cross, left, right), pair| {
                    let first = f64::from(pair[0]);
                    let second = f64::from(pair[1]);
                    (
                        cross + first * second,
                        left + first * first,
                        right + second * second,
                    )
                })
        },
    );
    cross / (left * right).sqrt()
}
