//! Attempt-local deterministic ZE-290 observations; never changes limits.
use super::artifact::ArtifactId;
use std::{cell::RefCell, collections::BTreeMap};
#[derive(Debug, Default)]
pub(crate) struct Report {
    pub phases: Vec<(&'static str, u64)>,
    pub artifacts: BTreeMap<ArtifactId, (u64, usize)>,
    pub origins: BTreeMap<(&'static str, u32), (u64, usize)>,
    pub leaf_entries: usize,
    pub child_pages: usize,
    pub mark_entries: u64,
    pub census_rows: usize,
    pub buffer_refusals: Vec<(&'static str, usize)>,
    pub protected_mark: Vec<(u64, u64, u16, u64)>,
    pub rejected: Option<(u64, u64)>,
    pub query_scratch: Vec<(u64, usize)>,
}
thread_local! {
    static REPORT: RefCell<Option<Report>> = const { RefCell::new(None) };
}
pub(crate) fn start() {
    REPORT.with(|r| *r.borrow_mut() = Some(Report::default()));
}
pub(crate) fn take() -> Report {
    REPORT.with(|r| r.borrow_mut().take().unwrap_or_default())
}
fn update(f: impl FnOnce(&mut Report)) {
    REPORT.with(|r| {
        if let Some(r) = r.borrow_mut().as_mut() {
            f(r);
        }
    });
}
pub(crate) fn phase(name: &'static str, work: u64) {
    update(|r| r.phases.push((name, work)));
}
#[track_caller]
pub(crate) fn artifact(id: ArtifactId, bytes: u64) {
    let caller = std::panic::Location::caller();
    update(|r| {
        let entry = r.artifacts.entry(id).or_default();
        entry.0 = bytes;
        entry.1 += 1;
        let origin = r.origins.entry((caller.file(), caller.line())).or_default();
        origin.0 += 2 * bytes - 8;
        origin.1 += 1;
    });
}
pub(crate) fn leaf(entries: usize) {
    update(|r| r.leaf_entries += entries);
}
pub(crate) fn child() {
    update(|r| r.child_pages += 1);
}
pub(crate) fn rejected(work: u64, units: u64) {
    update(|r| r.rejected = Some((work, units)));
}

pub(crate) fn query_scratch(work: u64, bytes: usize) {
    update(|r| r.query_scratch.push((work, bytes)));
}

pub(crate) fn protected_mark(mark_count: u64, protected_count: u64, height: u16, reads: u64) {
    update(|r| {
        r.protected_mark
            .push((mark_count, protected_count, height, reads))
    });
}

pub(crate) fn mark_entry() {
    update(|r| r.mark_entries += 1);
}
pub(crate) fn mark_entries() -> u64 {
    REPORT.with(|r| r.borrow().as_ref().map_or(0, |r| r.mark_entries))
}

pub(crate) fn census_rows(rows: usize) {
    update(|r| r.census_rows = r.census_rows.max(rows));
}
pub(crate) fn buffer_refusal(kind: &'static str, limit: usize) {
    update(|r| r.buffer_refusals.push((kind, limit)));
}
