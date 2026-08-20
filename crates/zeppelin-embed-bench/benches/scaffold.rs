use criterion::{Criterion, black_box, criterion_group, criterion_main};

fn scaffold(criterion: &mut Criterion) {
    criterion.bench_function("scaffold/version", |bencher| {
        bencher.iter(|| black_box(zeppelin_embed::VERSION));
    });
}

criterion_group!(benches, scaffold);
criterion_main!(benches);
