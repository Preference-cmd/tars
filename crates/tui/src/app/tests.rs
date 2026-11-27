mod flow;
mod http_streaming;
mod input;
mod measurements;
mod support;

#[path = "../../../cli/tests/common/tools.rs"]
#[allow(dead_code)] // Shared HTTP and tool reply fixture for several test modules.
mod wire;

mod navigation;

mod repair;
mod startup;
mod streaming;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod tools;

mod approval;
mod code_execution;
mod command;
mod compaction;
mod ergonomics;
mod history;
mod markdown;
mod media;
mod multi_agent;
mod onboarding_doctor_init;
mod prompt_cache;
mod sessions;
mod workflow;
mod workspace_rules;
