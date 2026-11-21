use chrono::{DateTime, Utc};
use clap::ValueEnum;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub type AppResult<T> = Result<T, Error>;

#[derive(Debug)]
pub struct Error {
    pub outcome: Outcome,
    pub message: String,
}

impl Error {
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Failed,
            message: message.into(),
        }
    }

    pub fn blocked(message: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Blocked,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Invalid,
            message: message.into(),
        }
    }

    pub fn busy(message: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Busy,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::failed(error.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::failed(error.to_string())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    #[default]
    Completed,
    Unchanged,
    Skipped,
    Blocked,
    Failed,
    Invalid,
    Waiting,
    Busy,
}

impl Outcome {
    pub fn exit(self) -> u8 {
        match self {
            Self::Completed | Self::Unchanged | Self::Skipped => 0,
            Self::Failed => 1,
            Self::Invalid => 2,
            Self::Blocked => 3,
            Self::Waiting => 4,
            Self::Busy => 5,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Unchanged => "unchanged",
            Self::Skipped => "skipped",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Invalid => "invalid",
            Self::Waiting => "waiting",
            Self::Busy => "busy",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct Report {
    pub outcome: Outcome,
    pub summary: String,
    pub warnings: Vec<String>,
    pub details: Value,
    pub at: DateTime<Utc>,
}

impl Report {
    pub fn new(outcome: Outcome, summary: impl Into<String>) -> Self {
        Self {
            outcome,
            summary: summary.into(),
            warnings: Vec::new(),
            details: Value::Null,
            at: Utc::now(),
        }
    }

    pub fn detail(mut self, details: impl Serialize) -> Self {
        match serde_json::to_value(details) {
            Ok(value) => self.details = value,
            Err(error) => self
                .warnings
                .push(format!("Cannot encode provider details: {error}")),
        }
        self
    }

    pub fn from_result(result: AppResult<Self>) -> Self {
        result.unwrap_or_else(|error| Self::new(error.outcome, error.message))
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Deserialize,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    ValueEnum,
    JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    #[default]
    Cycle,
    Preflight,
    Upgrade,
    Reboot,
    Kernels,
    Workloads,
    Compose,
    Disk,
}

impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cycle => "cycle",
            Self::Preflight => "preflight",
            Self::Upgrade => "upgrade",
            Self::Reboot => "reboot",
            Self::Kernels => "kernels",
            Self::Workloads => "workloads",
            Self::Compose => "compose",
            Self::Disk => "disk",
        }
    }
}

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum RebootMode {
    #[default]
    Auto,
    Never,
}
