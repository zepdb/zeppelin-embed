use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn unique_temp_dir(prefix: &str) -> PathBuf {
    let invocation = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{invocation}", std::process::id()))
}
