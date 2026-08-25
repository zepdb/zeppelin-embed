//! Across-process aggregation for DRAM-regime measurements.

use std::fmt;

/// Odd-sized process observations summarized by their cross-process median.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessMedian {
    values: Vec<f64>,
    median: f64,
    minimum: f64,
    maximum: f64,
}

impl ProcessMedian {
    /// Validates, sorts, and summarizes at least three independent processes.
    pub fn new(mut values: Vec<f64>) -> Result<Self, ProcessMedianError> {
        if values.len() < 3 || values.len().is_multiple_of(2) {
            return Err(ProcessMedianError::ProcessCount(values.len()));
        }
        if values
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0)
        {
            return Err(ProcessMedianError::InvalidObservation);
        }
        values.sort_by(f64::total_cmp);
        let median = values[values.len() / 2];
        let minimum = values[0];
        let maximum = values[values.len() - 1];
        Ok(Self {
            values,
            median,
            minimum,
            maximum,
        })
    }

    /// Sorted per-process observations.
    #[must_use]
    pub fn values(&self) -> &[f64] {
        &self.values
    }

    /// Median across independent processes.
    #[must_use]
    pub const fn median(&self) -> f64 {
        self.median
    }

    /// Minimum process observation.
    #[must_use]
    pub const fn minimum(&self) -> f64 {
        self.minimum
    }

    /// Maximum process observation.
    #[must_use]
    pub const fn maximum(&self) -> f64 {
        self.maximum
    }

    /// Full cross-process range as a percentage of the median.
    #[must_use]
    pub fn spread_percent(&self) -> f64 {
        (self.maximum - self.minimum) / self.median * 100.0
    }
}

/// Invalid across-process input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessMedianError {
    /// The process count was even or below three.
    ProcessCount(usize),
    /// An observation was zero, negative, NaN, or infinite.
    InvalidObservation,
}

impl fmt::Display for ProcessMedianError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProcessCount(count) => write!(
                formatter,
                "across-process median requires an odd count of at least three, got {count}"
            ),
            Self::InvalidObservation => {
                formatter.write_str("process observations must be positive and finite")
            }
        }
    }
}

impl std::error::Error for ProcessMedianError {}
