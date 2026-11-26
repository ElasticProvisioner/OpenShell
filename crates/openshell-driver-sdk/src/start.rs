// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

/// `EX_UNAVAILABLE`: the backend is not reachable with these settings.
pub const EXIT_UNAVAILABLE: i32 = 69;
/// `EX_CONFIG`: the driver table is invalid.
pub const EXIT_CONFIG: i32 = 78;

/// Why a driver stopped before it began serving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartKind {
    Unavailable,
    InvalidConfig,
    Error,
}

impl StartKind {
    /// The process exit code for this result.
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Unavailable => EXIT_UNAVAILABLE,
            Self::InvalidConfig => EXIT_CONFIG,
            Self::Error => 1,
        }
    }

    /// The result for a driver's exit code. Any code other than the two
    /// defined ones is an internal failure.
    #[must_use]
    pub const fn from_exit_code(code: i32) -> Self {
        match code {
            EXIT_UNAVAILABLE => Self::Unavailable,
            EXIT_CONFIG => Self::InvalidConfig,
            _ => Self::Error,
        }
    }
}

/// The one JSON line a driver writes to stdout before exiting. Stderr stays
/// for tracing, panics and native library output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartReport {
    #[serde(rename = "result")]
    pub kind: StartKind,
    pub reason: String,
}

impl StartReport {
    /// The report in the last line of `stdout` that parses as one.
    #[must_use]
    pub fn from_output(stdout: &str) -> Option<Self> {
        stdout
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str(line.trim()).ok())
    }
}

/// A start-up failure, reported once and mapped to an exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartResult {
    pub kind: StartKind,
    pub reason: String,
}

impl StartResult {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::new(StartKind::Unavailable, reason)
    }

    pub fn invalid_config(reason: impl Into<String>) -> Self {
        Self::new(StartKind::InvalidConfig, reason)
    }

    pub fn error(reason: impl Into<String>) -> Self {
        Self::new(StartKind::Error, reason)
    }

    fn new(kind: StartKind, reason: impl Into<String>) -> Self {
        Self {
            kind,
            reason: reason.into(),
        }
    }

    /// The JSON line, without a trailing newline.
    #[must_use]
    pub fn json_line(&self) -> String {
        serde_json::to_string(&StartReport {
            kind: self.kind,
            reason: self.reason.clone(),
        })
        .expect("a report of strings serializes")
    }

    /// Write the JSON line to stdout and return the exit code.
    #[must_use]
    pub fn report(&self) -> i32 {
        println!("{}", self.json_line());
        self.kind.exit_code()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_map_to_the_documented_exit_codes() {
        assert_eq!(StartKind::Unavailable.exit_code(), 69);
        assert_eq!(StartKind::InvalidConfig.exit_code(), 78);
        assert_eq!(StartKind::Error.exit_code(), 1);
        for kind in [
            StartKind::Unavailable,
            StartKind::InvalidConfig,
            StartKind::Error,
        ] {
            assert_eq!(StartKind::from_exit_code(kind.exit_code()), kind);
        }
        assert_eq!(StartKind::from_exit_code(2), StartKind::Error);
    }

    #[test]
    fn the_report_line_has_the_documented_shape() {
        let line = StartResult::invalid_config("unknown key `x`").json_line();
        assert_eq!(
            line,
            r#"{"result":"invalid_config","reason":"unknown key `x`"}"#
        );
        assert_eq!(
            StartReport::from_output(&format!("log noise\n{line}\n")),
            Some(StartReport {
                kind: StartKind::InvalidConfig,
                reason: "unknown key `x`".to_string()
            })
        );
        assert_eq!(StartReport::from_output("no report here\n"), None);
    }
}
