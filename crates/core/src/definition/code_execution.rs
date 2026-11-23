use super::types::DefinitionError;
use serde::{Deserialize, Serialize};

/// Minimum allowed wall-clock timeout for one `run_code` invocation, in seconds.
pub const MIN_CODE_EXECUTION_TIMEOUT_SECS: u64 = 1;
/// Maximum allowed wall-clock timeout for one `run_code` invocation, in seconds.
pub const MAX_CODE_EXECUTION_TIMEOUT_SECS: u64 = 120;

/// Minimum allowed QuickJS heap memory limit, in bytes (4 MiB).
pub const MIN_CODE_EXECUTION_MEMORY_BYTES: usize = 4 * 1024 * 1024;
/// Maximum allowed QuickJS heap memory limit, in bytes (64 MiB).
pub const MAX_CODE_EXECUTION_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// Minimum allowed QuickJS call stack limit, in bytes (64 KiB).
pub const MIN_CODE_EXECUTION_STACK_BYTES: usize = 64 * 1024;
/// Maximum allowed QuickJS call stack limit, in bytes (1 MiB).
pub const MAX_CODE_EXECUTION_STACK_BYTES: usize = 1024 * 1024;

/// Minimum allowed tool subcalls per `run_code` invocation.
pub const MIN_CODE_EXECUTION_SUBCALLS: usize = 1;
/// Maximum allowed tool subcalls per `run_code` invocation.
pub const MAX_CODE_EXECUTION_SUBCALLS: usize = 512;

/// Minimum allowed concurrent tool subcalls inside one `run_code` invocation.
pub const MIN_CODE_EXECUTION_CONCURRENCY: usize = 1;
/// Maximum allowed concurrent tool subcalls inside one `run_code` invocation.
pub const MAX_CODE_EXECUTION_CONCURRENCY: usize = 16;

/// Minimum allowed serialized output size for one `run_code` invocation, in bytes.
pub const MIN_CODE_EXECUTION_OUTPUT_BYTES: usize = 1024;
/// Maximum allowed serialized output size for one `run_code` invocation, in bytes.
pub const MAX_CODE_EXECUTION_OUTPUT_BYTES: usize = 64 * 1024;

/// Frozen resource limits for the built-in read-only `run_code` sandbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeExecutionSettings {
    /// Per-call wall-clock timeout in seconds (`1..=120`, default `30`).
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// QuickJS runtime memory ceiling in bytes (`4 MiB..=64 MiB`, default `32 MiB`).
    #[serde(default = "default_memory_limit_bytes")]
    pub memory_limit_bytes: usize,
    /// QuickJS runtime stack ceiling in bytes (`64 KiB..=1 MiB`, default `512 KiB`).
    #[serde(default = "default_stack_limit_bytes")]
    pub stack_limit_bytes: usize,
    /// Maximum tool subcalls permitted in one `run_code` invocation (`1..=512`, default `256`).
    #[serde(default = "default_max_subcalls")]
    pub max_subcalls: usize,
    /// Maximum in-flight subcalls permitted in one `run_code` invocation (`1..=16`, default `8`).
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: usize,
    /// Maximum serialized output bytes returned by one `run_code` invocation (`1024..=64 KiB`, default `32 KiB`).
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
}

impl Default for CodeExecutionSettings {
    fn default() -> Self {
        Self {
            timeout_secs: default_timeout_secs(),
            memory_limit_bytes: default_memory_limit_bytes(),
            stack_limit_bytes: default_stack_limit_bytes(),
            max_subcalls: default_max_subcalls(),
            max_concurrency: default_max_concurrency(),
            max_output_bytes: default_max_output_bytes(),
        }
    }
}

impl CodeExecutionSettings {
    /// Reject settings outside the supported safety floors and hard ceilings.
    pub fn validate(&self) -> Result<(), DefinitionError> {
        if !(MIN_CODE_EXECUTION_TIMEOUT_SECS..=MAX_CODE_EXECUTION_TIMEOUT_SECS)
            .contains(&self.timeout_secs)
            || !(MIN_CODE_EXECUTION_MEMORY_BYTES..=MAX_CODE_EXECUTION_MEMORY_BYTES)
                .contains(&self.memory_limit_bytes)
            || !(MIN_CODE_EXECUTION_STACK_BYTES..=MAX_CODE_EXECUTION_STACK_BYTES)
                .contains(&self.stack_limit_bytes)
            || !(MIN_CODE_EXECUTION_SUBCALLS..=MAX_CODE_EXECUTION_SUBCALLS)
                .contains(&self.max_subcalls)
            || !(MIN_CODE_EXECUTION_CONCURRENCY..=MAX_CODE_EXECUTION_CONCURRENCY)
                .contains(&self.max_concurrency)
            || !(MIN_CODE_EXECUTION_OUTPUT_BYTES..=MAX_CODE_EXECUTION_OUTPUT_BYTES)
                .contains(&self.max_output_bytes)
        {
            return Err(DefinitionError::InvalidCodeExecutionSettings);
        }
        Ok(())
    }
}

const fn default_timeout_secs() -> u64 {
    30
}

const fn default_memory_limit_bytes() -> usize {
    32 * 1024 * 1024
}

const fn default_stack_limit_bytes() -> usize {
    512 * 1024
}

const fn default_max_subcalls() -> usize {
    256
}

const fn default_max_concurrency() -> usize {
    8
}

const fn default_max_output_bytes() -> usize {
    32 * 1024
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_bounds_are_stable() {
        let settings = CodeExecutionSettings::default();
        assert_eq!(settings.timeout_secs, 30);
        assert_eq!(settings.memory_limit_bytes, 32 * 1024 * 1024);
        assert_eq!(settings.stack_limit_bytes, 512 * 1024);
        assert_eq!(settings.max_subcalls, 256);
        assert_eq!(settings.max_concurrency, 8);
        assert_eq!(settings.max_output_bytes, 32 * 1024);
        assert!(settings.validate().is_ok());

        let min_bound = CodeExecutionSettings {
            timeout_secs: MIN_CODE_EXECUTION_TIMEOUT_SECS,
            memory_limit_bytes: MIN_CODE_EXECUTION_MEMORY_BYTES,
            stack_limit_bytes: MIN_CODE_EXECUTION_STACK_BYTES,
            max_subcalls: MIN_CODE_EXECUTION_SUBCALLS,
            max_concurrency: MIN_CODE_EXECUTION_CONCURRENCY,
            max_output_bytes: MIN_CODE_EXECUTION_OUTPUT_BYTES,
        };
        assert!(min_bound.validate().is_ok());

        let max_bound = CodeExecutionSettings {
            timeout_secs: MAX_CODE_EXECUTION_TIMEOUT_SECS,
            memory_limit_bytes: MAX_CODE_EXECUTION_MEMORY_BYTES,
            stack_limit_bytes: MAX_CODE_EXECUTION_STACK_BYTES,
            max_subcalls: MAX_CODE_EXECUTION_SUBCALLS,
            max_concurrency: MAX_CODE_EXECUTION_CONCURRENCY,
            max_output_bytes: MAX_CODE_EXECUTION_OUTPUT_BYTES,
        };
        assert!(max_bound.validate().is_ok());
    }

    #[test]
    fn out_of_range_and_zero_settings_are_rejected() {
        let invalid_cases = [
            CodeExecutionSettings {
                timeout_secs: 0,
                ..Default::default()
            },
            CodeExecutionSettings {
                timeout_secs: 121,
                ..Default::default()
            },
            CodeExecutionSettings {
                memory_limit_bytes: 4 * 1024 * 1024 - 1,
                ..Default::default()
            },
            CodeExecutionSettings {
                memory_limit_bytes: 64 * 1024 * 1024 + 1,
                ..Default::default()
            },
            CodeExecutionSettings {
                stack_limit_bytes: 64 * 1024 - 1,
                ..Default::default()
            },
            CodeExecutionSettings {
                stack_limit_bytes: 1024 * 1024 + 1,
                ..Default::default()
            },
            CodeExecutionSettings {
                max_subcalls: 0,
                ..Default::default()
            },
            CodeExecutionSettings {
                max_subcalls: 513,
                ..Default::default()
            },
            CodeExecutionSettings {
                max_concurrency: 0,
                ..Default::default()
            },
            CodeExecutionSettings {
                max_concurrency: 17,
                ..Default::default()
            },
            CodeExecutionSettings {
                max_output_bytes: 1023,
                ..Default::default()
            },
            CodeExecutionSettings {
                max_output_bytes: 64 * 1024 + 1,
                ..Default::default()
            },
        ];
        for settings in invalid_cases {
            assert_eq!(
                settings.validate(),
                Err(DefinitionError::InvalidCodeExecutionSettings)
            );
        }
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let json = serde_json::json!({
            "timeout_secs": 30,
            "extra_field": true
        });
        assert!(serde_json::from_value::<CodeExecutionSettings>(json).is_err());
    }
}
