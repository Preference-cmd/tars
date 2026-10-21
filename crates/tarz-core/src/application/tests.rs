mod claims;
mod lifecycle;
mod opening;
mod progress;
mod repair;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod repair_http;
mod repair_originals;
mod saving;
mod support;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "../../tests/common/workspace_http.rs"]
pub(crate) mod workspace_http;
