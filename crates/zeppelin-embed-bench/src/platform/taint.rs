//! Shared benchmark-environment taint detection.

use std::fmt;

#[cfg(target_os = "macos")]
const SANDBOX_FILTER_NONE: libc::c_int = 0;

#[cfg(target_os = "macos")]
// SAFETY: This declaration matches sandbox_check(3) from the macOS Sandbox API.
unsafe extern "C" {
    fn sandbox_check(
        pid: libc::pid_t,
        operation: *const libc::c_char,
        filter_type: libc::c_int,
        ...
    ) -> libc::c_int;
}

/// A condition that makes wall-clock benchmark conclusions non-authoritative.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Taint {
    /// The one-minute load average exceeded the operator-selected limit.
    Load { actual: f64, limit: f64 },
    /// The one-minute load average could not be read.
    LoadUnreadable,
    /// The current process is sandboxed.
    Sandbox,
}

impl Taint {
    /// Stable machine-readable label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Load { .. } => "load",
            Self::LoadUnreadable => "load_unreadable",
            Self::Sandbox => "sandbox",
        }
    }
}

impl fmt::Display for Taint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load { actual, limit } => {
                write!(formatter, "load1={actual:.2} exceeds limit={limit:.2}")
            }
            Self::LoadUnreadable => formatter.write_str("load1=unreadable"),
            Self::Sandbox => formatter.write_str("sandbox=detected"),
        }
    }
}

/// Snapshot of the load and sandbox checks performed before measurement.
#[derive(Clone, Debug)]
pub struct TaintCheck {
    /// One-minute load average, if readable.
    pub load1: Option<f64>,
    /// Whether macOS reports the process as sandboxed.
    pub sandboxed: bool,
    /// Stable ordered taint list.
    pub taints: Vec<Taint>,
}

/// Apply the shared WAL-throughput taint policy to already-observed inputs.
pub fn evaluate_taint(load1: Option<f64>, load_limit: f64, sandboxed: bool) -> Vec<Taint> {
    let mut taints = Vec::new();
    match load1 {
        Some(actual) if actual > load_limit => taints.push(Taint::Load {
            actual,
            limit: load_limit,
        }),
        None => taints.push(Taint::LoadUnreadable),
        Some(_) => {}
    }
    if sandboxed {
        taints.push(Taint::Sandbox);
    }
    taints
}

/// Read the process environment and evaluate benchmark taint.
pub fn detect_taint(load_limit: f64) -> TaintCheck {
    let load1 = read_load1();
    let sandboxed = process_is_sandboxed();
    let taints = evaluate_taint(load1, load_limit, sandboxed);
    TaintCheck {
        load1,
        sandboxed,
        taints,
    }
}

/// Print the shared human-readable taint stamp.
pub fn print_taint_status(taint: &TaintCheck, load_limit: f64, subject: &str) {
    if taint.taints.is_empty() {
        println!(
            "taint check: clean (load1={} limit={load_limit:.2} sandbox={})",
            format_load1(taint.load1),
            if taint.sandboxed { "detected" } else { "clear" }
        );
    } else {
        println!(
            "TAINTED — {} — no {subject} conclusion is authoritative",
            taint
                .taints
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

/// Format a load average for machine-readable output.
pub fn format_load1(load1: Option<f64>) -> String {
    load1
        .map(|value| format!("{value:.2}"))
        .unwrap_or_else(|| String::from("NA"))
}

/// Format stable comma-separated taint labels.
pub fn format_taint_labels(taints: &[Taint]) -> String {
    if taints.is_empty() {
        return String::from("none");
    }
    taints
        .iter()
        .map(|taint| taint.label())
        .collect::<Vec<_>>()
        .join(",")
}

fn read_load1() -> Option<f64> {
    let mut load1 = 0.0_f64;
    // SAFETY: load1 points to writable storage for the one requested load average.
    let read = unsafe { libc::getloadavg(&mut load1, 1) };
    (read == 1).then_some(load1)
}

#[cfg(target_os = "macos")]
fn process_is_sandboxed() -> bool {
    // SAFETY: sandbox_check is called with the current PID, the documented null operation,
    // and SANDBOX_FILTER_NONE, which takes no variadic filter arguments.
    unsafe { sandbox_check(libc::getpid(), std::ptr::null(), SANDBOX_FILTER_NONE) > 0 }
}

#[cfg(not(target_os = "macos"))]
const fn process_is_sandboxed() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::{Taint, evaluate_taint};

    #[test]
    fn load_and_sandbox_taints_are_both_retained_in_stable_order() {
        assert_eq!(
            evaluate_taint(Some(2.5), 1.0, true),
            vec![
                Taint::Load {
                    actual: 2.5,
                    limit: 1.0,
                },
                Taint::Sandbox,
            ]
        );
    }

    #[test]
    fn clean_taint_inputs_remain_unlabelled() {
        assert!(evaluate_taint(Some(0.5), 1.0, false).is_empty());
    }

    #[test]
    fn unreadable_load_is_tainted() {
        assert_eq!(
            evaluate_taint(None, 1.0, false),
            vec![Taint::LoadUnreadable]
        );
    }
}
