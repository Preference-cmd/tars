use crate::compact::RecallTool;
use crate::compact::recall::recall_tool_definition;
use crate::definition::{
    BuiltinTool, CodeExecutionSettings, CommandSettings, WorkspaceTool, WorkspaceToolLimits,
    WriteToolLimits,
};
use crate::tools::{WorkspaceTools, workspace_tool_definition};
use async_trait::async_trait;
use causa::kernel::{CallControl, ToolDefinition, ToolResultStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Semaphore;

/// Frozen built-in read-only bindings exposed to `run_code` in this session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BuiltinBindings {
    pub(crate) list_directory: bool,
    pub(crate) read_file: bool,
    pub(crate) search_text: bool,
    pub(crate) recall: bool,
}

impl BuiltinBindings {
    /// Construct frozen bindings from the session's selected workspace tools and recall availability.
    pub(crate) fn from_session(workspace_tools: &[WorkspaceTool], has_recall: bool) -> Self {
        Self {
            list_directory: workspace_tools.contains(&WorkspaceTool::ListDirectory),
            read_file: workspace_tools.contains(&WorkspaceTool::ReadFile),
            search_text: workspace_tools.contains(&WorkspaceTool::SearchText),
            recall: has_recall,
        }
    }

    /// Whether at least one built-in read-only tool is available.
    pub(crate) fn any(&self) -> bool {
        self.list_directory || self.read_file || self.search_text || self.recall
    }

    /// Ordered list of bound built-in read-only tool names.
    pub(crate) fn bound_names(&self) -> Vec<&'static str> {
        let mut names = Vec::with_capacity(4);
        if self.list_directory {
            names.push(WorkspaceTool::ListDirectory.name());
        }
        if self.read_file {
            names.push(WorkspaceTool::ReadFile.name());
        }
        if self.search_text {
            names.push(WorkspaceTool::SearchText.name());
        }
        if self.recall {
            names.push(BuiltinTool::Recall.name());
        }
        names
    }
}

/// Execution telemetry returned alongside every `run_code` result.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SubcallStats {
    /// Total number of subcalls admitted in this `run_code` execution.
    pub(crate) subcalls: usize,
    /// Per-tool admitted subcall counts.
    pub(crate) by_tool: BTreeMap<String, usize>,
    /// Peak concurrent in-flight subcalls observed during this execution.
    pub(crate) max_in_flight: usize,
    /// Wall-clock execution duration in milliseconds.
    pub(crate) duration_ms: u64,
}

/// Summary of a read-only tool returned by `tools.list()`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SandboxToolSummary {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) source: String,
}

/// Full descriptor for a read-only tool exposed inside the sandbox (`tools.describe()`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SandboxToolDescriptor {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) parameters: Value,
    pub(crate) source: String,
}

/// Extension point for dynamic read-only tool discovery and invocation (e.g. Phase 1b MCP).
#[async_trait]
pub(crate) trait ReadOnlySubcallProvider: Send + Sync {
    /// List currently discoverable read-only tools.
    async fn list_tools(&self) -> Vec<SandboxToolDescriptor>;
    /// Describe one tool by name if currently discoverable and read-only.
    async fn describe_tool(&self, name: &str) -> Option<SandboxToolDescriptor>;
    /// Execute one read-only subcall.
    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        control: &CallControl,
    ) -> Result<Value, SubcallError>;
}

/// Error produced by an inner read-only subcall.
#[derive(Debug, Clone)]
pub(crate) enum SubcallError {
    /// Uncatchable sandbox termination (subcall budget exhausted, cancelled, or timed out).
    Fatal(String),
    /// Catchable tool parameter, path, or validation refusal.
    Rejected(String),
    /// Catchable runtime tool failure.
    Failed(String),
}

impl std::fmt::Display for SubcallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fatal(msg) | Self::Rejected(msg) | Self::Failed(msg) => f.write_str(msg),
        }
    }
}

/// Shared host resources bound to a session work for `run_code` subcalls.
pub(crate) struct SubcallHost {
    settings: CodeExecutionSettings,
    bindings: BuiltinBindings,
    workspace_tools: Option<Arc<WorkspaceTools>>,
    recall: Option<Arc<RecallTool>>,
    extra_provider: Option<Arc<dyn ReadOnlySubcallProvider>>,
}

impl SubcallHost {
    /// Create a host backed by the session's bound `WorkspaceTools` and `RecallTool`.
    pub(crate) fn new(
        settings: CodeExecutionSettings,
        bindings: BuiltinBindings,
        workspace_tools: Option<Arc<WorkspaceTools>>,
        recall: Option<Arc<RecallTool>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            settings,
            bindings,
            workspace_tools,
            recall,
            extra_provider: None,
        })
    }

    /// Attach an optional dynamic read-only subcall provider (extension point for Phase 1b).
    pub(crate) fn with_extra_provider(
        mut self: Arc<Self>,
        provider: Arc<dyn ReadOnlySubcallProvider>,
    ) -> Arc<Self> {
        Arc::get_mut(&mut self)
            .expect("SubcallHost is uniquely owned during assembly")
            .extra_provider = Some(provider);
        self
    }

    pub(crate) fn bindings(&self) -> BuiltinBindings {
        self.bindings
    }

    fn builtin_descriptor(&self, name: &str) -> Option<SandboxToolDescriptor> {
        let is_bound = match name {
            "list_directory" => self.bindings.list_directory,
            "read_file" => self.bindings.read_file,
            "search_text" => self.bindings.search_text,
            "recall" => self.bindings.recall,
            _ => false,
        };
        if !is_bound {
            return None;
        }
        let def: ToolDefinition = if name == BuiltinTool::Recall.name() {
            recall_tool_definition()
        } else {
            let kind = WorkspaceTool::from_name(name)?;
            let default_limits = WorkspaceToolLimits::default();
            let default_write = WriteToolLimits::default();
            let (limits, write_limits, command_settings) = match &self.workspace_tools {
                Some(wt) => (wt.limits(), wt.write_limits(), wt.command_settings()),
                None => (&default_limits, &default_write, CommandSettings::default()),
            };
            workspace_tool_definition(kind, limits, write_limits, command_settings)
        };
        Some(SandboxToolDescriptor {
            name: def.name,
            description: def.description,
            parameters: def.parameters,
            source: "builtin".to_owned(),
        })
    }

    pub(crate) fn new_run_session(self: &Arc<Self>) -> Arc<SubcallSession> {
        Arc::new(SubcallSession {
            host: Arc::clone(self),
            subcalls: AtomicUsize::new(0),
            by_tool: Mutex::new(BTreeMap::new()),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            concurrency: Arc::new(Semaphore::new(self.settings.max_concurrency)),
            fatal_flag: Arc::new(AtomicBool::new(false)),
            fatal_error: Arc::new(Mutex::new(None)),
            started_at: Instant::now(),
        })
    }
}

/// Per-invocation subcall budget, concurrency gate, and fatal-abort state.
pub(crate) struct SubcallSession {
    host: Arc<SubcallHost>,
    subcalls: AtomicUsize,
    by_tool: Mutex<BTreeMap<String, usize>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    concurrency: Arc<Semaphore>,
    fatal_flag: Arc<AtomicBool>,
    fatal_error: Arc<Mutex<Option<String>>>,
    started_at: Instant,
}

struct InFlightGuard<'a> {
    in_flight: &'a AtomicUsize,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

impl SubcallSession {
    pub(crate) fn bound_names(&self) -> Vec<&'static str> {
        self.host.bindings.bound_names()
    }

    pub(crate) fn fatal_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.fatal_flag)
    }

    pub(crate) fn record_fatal(&self, message: String) {
        let mut guard = self
            .fatal_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_none() {
            *guard = Some(message);
        }
        self.fatal_flag.store(true, Ordering::Release);
    }

    pub(crate) fn fatal_error(&self) -> Option<String> {
        self.fatal_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn stats(&self) -> SubcallStats {
        let by_tool = self
            .by_tool
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        SubcallStats {
            subcalls: self.subcalls.load(Ordering::Acquire),
            by_tool,
            max_in_flight: self.max_in_flight.load(Ordering::Acquire),
            duration_ms: u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        }
    }

    fn check_active(&self, control: &CallControl) -> Result<(), SubcallError> {
        if self.fatal_flag.load(Ordering::Acquire) {
            return Err(SubcallError::Fatal(
                self.fatal_error()
                    .unwrap_or_else(|| "code execution aborted".to_owned()),
            ));
        }
        if let Err(err) = control.check() {
            let msg = err.to_string();
            self.record_fatal(msg.clone());
            return Err(SubcallError::Fatal(msg));
        }
        Ok(())
    }

    fn reserve_subcall(&self, name: &str) -> Result<(), SubcallError> {
        let max_subcalls = self.host.settings.max_subcalls;
        let updated = self
            .subcalls
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < max_subcalls).then_some(current + 1)
            });
        if updated.is_err() {
            let msg = format!("subcall limit ({max_subcalls}) exceeded");
            self.record_fatal(msg.clone());
            return Err(SubcallError::Fatal(msg));
        }
        let mut counts = self
            .by_tool
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *counts.entry(name.to_owned()).or_insert(0) += 1;
        Ok(())
    }

    pub(crate) async fn list_tools(
        &self,
        control: &CallControl,
    ) -> Result<Vec<SandboxToolSummary>, SubcallError> {
        self.check_active(control)?;
        let mut out: Vec<SandboxToolSummary> = self
            .host
            .bindings
            .bound_names()
            .into_iter()
            .filter_map(|name| self.host.builtin_descriptor(name))
            .map(|desc| SandboxToolSummary {
                name: desc.name,
                description: desc.description,
                source: desc.source,
            })
            .collect();
        if let Some(provider) = &self.host.extra_provider {
            let dynamic = provider.list_tools().await;
            self.check_active(control)?;
            for desc in dynamic {
                out.push(SandboxToolSummary {
                    name: desc.name,
                    description: desc.description,
                    source: desc.source,
                });
            }
        }
        Ok(out)
    }

    pub(crate) async fn describe_tool(
        &self,
        name: &str,
        control: &CallControl,
    ) -> Result<SandboxToolDescriptor, SubcallError> {
        self.check_active(control)?;
        if let Some(desc) = self.host.builtin_descriptor(name) {
            return Ok(desc);
        }
        if let Some(provider) = &self.host.extra_provider {
            let maybe_desc = provider.describe_tool(name).await;
            self.check_active(control)?;
            if let Some(desc) = maybe_desc {
                return Ok(desc);
            }
        }
        Err(SubcallError::Rejected(format!(
            "tool '{name}' is not a read-only tool available inside run_code"
        )))
    }

    pub(crate) async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        control: &CallControl,
    ) -> Result<Value, SubcallError> {
        self.check_active(control)?;

        let bound = match name {
            "list_directory" => self.host.bindings.list_directory,
            "read_file" => self.host.bindings.read_file,
            "search_text" => self.host.bindings.search_text,
            "recall" => self.host.bindings.recall,
            _ => false,
        };
        if !bound && self.host.extra_provider.is_none() {
            return Err(SubcallError::Rejected(format!(
                "tool '{name}' is not available inside run_code"
            )));
        }

        if !bound
            && let Some(provider) = &self.host.extra_provider
            && provider.describe_tool(name).await.is_none()
        {
            return Err(SubcallError::Rejected(format!(
                "tool '{name}' is not available inside run_code"
            )));
        }

        self.reserve_subcall(name)?;

        let deadline = async {
            match control.deadline() {
                Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                None => std::future::pending::<()>().await,
            }
        };
        let _permit = tokio::select! {
            biased;
            _ = control.cancellation_token().cancelled() => {
                let msg = "code execution cancelled".to_owned();
                self.record_fatal(msg.clone());
                return Err(SubcallError::Fatal(msg));
            }
            _ = deadline => {
                let msg = "code execution timed out".to_owned();
                self.record_fatal(msg.clone());
                return Err(SubcallError::Fatal(msg));
            }
            permit = self.concurrency.clone().acquire_owned() => {
                match permit {
                    Ok(permit) => permit,
                    Err(_) => {
                        let msg = "code execution concurrency gate closed".to_owned();
                        self.record_fatal(msg.clone());
                        return Err(SubcallError::Fatal(msg));
                    }
                }
            }
        };

        self.check_active(control)?;

        let current = self.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
        self.max_in_flight.fetch_max(current, Ordering::AcqRel);
        let _in_flight_guard = InFlightGuard {
            in_flight: &self.in_flight,
        };

        let result = match name {
            "list_directory" | "read_file" | "search_text" => {
                let Some(wt) = self.host.workspace_tools.as_ref() else {
                    return Err(SubcallError::Rejected(
                        "workspace tools are not available".to_owned(),
                    ));
                };
                let kind = WorkspaceTool::from_name(name).expect("matched readonly workspace tool");
                let (status, content) = wt.execute_readonly_subcall(kind, arguments, control).await;
                map_subcall_payload(status, content, control, self)
            }
            "recall" => {
                let Some(recall) = self.host.recall.as_ref() else {
                    return Err(SubcallError::Rejected(
                        "recall tool is not available".to_owned(),
                    ));
                };
                match recall.execute_subcall(arguments, control).await {
                    Ok(text) => Ok(Value::String(text)),
                    Err((ToolResultStatus::Cancelled | ToolResultStatus::TimedOut, msg)) => {
                        if control.check().is_err() || self.fatal_flag.load(Ordering::Acquire) {
                            let fatal_msg = self.fatal_error().unwrap_or(msg);
                            self.record_fatal(fatal_msg.clone());
                            Err(SubcallError::Fatal(fatal_msg))
                        } else {
                            Err(SubcallError::Failed(msg))
                        }
                    }
                    Err((ToolResultStatus::Rejected, msg)) => Err(SubcallError::Rejected(msg)),
                    Err((_, msg)) => Err(SubcallError::Failed(msg)),
                }
            }
            _ => {
                let Some(provider) = &self.host.extra_provider else {
                    return Err(SubcallError::Rejected(format!(
                        "tool '{name}' is not available inside run_code"
                    )));
                };
                match provider.call_tool(name, arguments, control).await {
                    Ok(val) => Ok(val),
                    Err(SubcallError::Fatal(msg)) => {
                        self.record_fatal(msg.clone());
                        Err(SubcallError::Fatal(msg))
                    }
                    Err(err) => Err(err),
                }
            }
        };

        if self.fatal_flag.load(Ordering::Acquire) {
            return Err(SubcallError::Fatal(
                self.fatal_error()
                    .unwrap_or_else(|| "code execution aborted".to_owned()),
            ));
        }

        result
    }
}

fn extract_error(content: &Value) -> String {
    content
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| content.to_string())
}

fn map_subcall_payload(
    status: ToolResultStatus,
    content: Value,
    control: &CallControl,
    session: &SubcallSession,
) -> Result<Value, SubcallError> {
    match status {
        ToolResultStatus::Succeeded => Ok(content),
        ToolResultStatus::Cancelled | ToolResultStatus::TimedOut => {
            if control.check().is_err() || session.fatal_flag.load(Ordering::Acquire) {
                let msg = session
                    .fatal_error()
                    .unwrap_or_else(|| extract_error(&content));
                session.record_fatal(msg.clone());
                Err(SubcallError::Fatal(msg))
            } else {
                Err(SubcallError::Failed(extract_error(&content)))
            }
        }
        ToolResultStatus::Rejected => Err(SubcallError::Rejected(extract_error(&content))),
        ToolResultStatus::Failed | ToolResultStatus::UnknownOutcome => {
            Err(SubcallError::Failed(extract_error(&content)))
        }
    }
}
