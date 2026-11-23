use super::host::{SubcallError, SubcallHost, SubcallSession, SubcallStats};
use crate::compact::history::{ARTIFACT_ID_PREFIX, is_canonical_uuid, parse_artifact_id};
use crate::definition::CodeExecutionSettings;
use causa::kernel::{CallControl, ControlError};
use rquickjs::convert::Coerced;
use rquickjs::function::Rest;
use rquickjs::prelude::Async;
use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, CaughtError, Ctx, Exception, FromJs, Function,
    Object, Promise, Value as JsValue,
};
use serde_json::{Map, Value, json};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::runtime::Handle;

/// Execution result of one stateless `run_code` sandbox invocation.
#[derive(Clone, Debug)]
pub(crate) struct SandboxOutcome {
    pub(crate) ok: bool,
    pub(crate) output: String,
    pub(crate) truncated: bool,
    pub(crate) error: Option<String>,
    pub(crate) stats: SubcallStats,
    pub(crate) cancelled: bool,
    pub(crate) timed_out: bool,
}

fn js_val_to_json<'js>(ctx: &Ctx<'js>, val: &JsValue<'js>) -> Result<Value, String> {
    if val.is_undefined() || val.is_null() {
        return Ok(Value::Null);
    }
    match ctx.json_stringify(val.clone()) {
        Ok(Some(s)) => {
            let rust_str = s.to_string().map_err(|e| e.to_string())?;
            serde_json::from_str(&rust_str).map_err(|e| e.to_string())
        }
        Ok(None) => Ok(Value::Null),
        Err(e) => Err(format!("JSON stringify failed: {e}")),
    }
}

fn json_to_js<'js>(ctx: &Ctx<'js>, val: &Value) -> rquickjs::Result<JsValue<'js>> {
    let s = serde_json::to_string(val)
        .map_err(|e| Exception::throw_message(ctx, &format!("json encode error: {e}")))?;
    ctx.json_parse(s)
}

fn decode_js_args<'js>(
    ctx: &Ctx<'js>,
    tool_name: &str,
    rest: Rest<JsValue<'js>>,
) -> Result<Value, String> {
    let args: Vec<JsValue<'js>> = rest.0;
    if args.is_empty() {
        return Err(format!("{tool_name} requires arguments"));
    }
    if args.len() == 1 && args[0].is_object() && !args[0].is_array() && !args[0].is_null() {
        return js_val_to_json(ctx, &args[0]);
    }
    if tool_name == "recall" {
        if args.len() > 1 {
            return Err(
                "recall positional call accepts a single id or query string; pass an options object for additional parameters".to_owned(),
            );
        }
        let jv = js_val_to_json(ctx, &args[0])?;
        let Some(text) = jv.as_str() else {
            return Err("recall requires an options object or string argument".to_owned());
        };
        let key = if text.starts_with(ARTIFACT_ID_PREFIX)
            || parse_artifact_id(text).is_ok()
            || is_canonical_uuid(text)
        {
            "id"
        } else {
            "query"
        };
        let mut map = Map::new();
        map.insert(key.to_owned(), Value::String(text.to_owned()));
        return Ok(Value::Object(map));
    }
    let param_names: &[&str] = match tool_name {
        "list_directory" => &["path", "limit"],
        "read_file" => &["path", "start_line", "max_lines"],
        "search_text" => &[
            "path",
            "query",
            "max_matches",
            "regex",
            "ignore_case",
            "glob",
            "context_lines",
            "include_ignored",
        ],
        _ => return Err(format!("unknown tool '{tool_name}'")),
    };
    if args.len() > param_names.len() {
        return Err(format!(
            "{tool_name} takes at most {} arguments ({} given)",
            param_names.len(),
            args.len()
        ));
    }
    let mut map = Map::new();
    for (idx, val) in args.iter().enumerate() {
        let jv = js_val_to_json(ctx, val)?;
        if !jv.is_null() {
            map.insert(param_names[idx].to_owned(), jv);
        }
    }
    Ok(Value::Object(map))
}

fn format_caught_error<'js>(ctx: &Ctx<'js>, err: CaughtError<'js>) -> String {
    match err {
        CaughtError::Exception(exc) => {
            let msg = exc.message().unwrap_or_else(|| "JS Exception".to_owned());
            match exc.stack() {
                Some(stack) if !stack.trim().is_empty() => format!("{msg}\n{stack}"),
                _ => msg,
            }
        }
        CaughtError::Value(val) => {
            if let Ok(s) = Coerced::<String>::from_js(ctx, val) {
                s.0
            } else {
                "JS thrown value".to_owned()
            }
        }
        CaughtError::Error(e) => e.to_string(),
    }
}

fn format_rquickjs_err<'js>(ctx: &Ctx<'js>, err: rquickjs::Error) -> String {
    if matches!(err, rquickjs::Error::Exception) {
        let caught = ctx.catch();
        if let Some(obj) = caught.clone().into_object()
            && let Some(exc) = Exception::from_object(obj)
        {
            let msg = exc.message().unwrap_or_else(|| "JS Exception".to_owned());
            return match exc.stack() {
                Some(stack) if !stack.trim().is_empty() => format!("{msg}\n{stack}"),
                _ => msg,
            };
        }
        if let Ok(s) = Coerced::<String>::from_js(ctx, caught) {
            return s.0;
        }
    }
    err.to_string()
}

fn append_bounded_log(buf: &Arc<Mutex<String>>, line: &str, max_bytes: usize) {
    let mut guard = buf.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.len() >= max_bytes {
        return;
    }
    if !guard.is_empty() {
        guard.push('\n');
    }
    let rem = max_bytes.saturating_sub(guard.len());
    if line.len() <= rem {
        guard.push_str(line);
    } else {
        let mut cut = rem;
        while cut > 0 && !line.is_char_boundary(cut) {
            cut -= 1;
        }
        guard.push_str(&line[..cut]);
    }
}

fn register_sandbox_globals<'js>(
    ctx: &Ctx<'js>,
    session: &Arc<SubcallSession>,
    control: &CallControl,
    outer_handle: &Handle,
    stdout_buf: &Arc<Mutex<String>>,
    max_stdout_bytes: usize,
) -> Result<(), String> {
    let globals = ctx.globals();

    let console_obj = Object::new(ctx.clone()).map_err(|e| e.to_string())?;
    let log_buf = Arc::clone(stdout_buf);
    let log_fn = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, rest: Rest<JsValue<'js>>| {
            let mut parts = Vec::with_capacity(rest.0.len());
            for item in rest.0 {
                if let Some(s) = item.as_string() {
                    parts.push(s.to_string().unwrap_or_default());
                } else if let Ok(Some(json_s)) = ctx.json_stringify(item.clone()) {
                    parts.push(json_s.to_string().unwrap_or_default());
                } else if let Ok(Coerced(s)) = Coerced::<String>::from_js(&ctx, item) {
                    parts.push(s);
                }
            }
            let line = parts.join(" ");
            append_bounded_log(&log_buf, &line, max_stdout_bytes);
            Ok::<(), rquickjs::Error>(())
        },
    )
    .map_err(|e| e.to_string())?;
    console_obj
        .set("log", log_fn.clone())
        .map_err(|e| e.to_string())?;
    console_obj
        .set("warn", log_fn.clone())
        .map_err(|e| e.to_string())?;
    console_obj
        .set("error", log_fn)
        .map_err(|e| e.to_string())?;
    globals
        .set("console", console_obj)
        .map_err(|e| e.to_string())?;

    let tools_obj = Object::new(ctx.clone()).map_err(|e| e.to_string())?;
    for tool_name in session.bound_names() {
        let sess = Arc::clone(session);
        let ctrl = control.clone();
        let handle = outer_handle.clone();
        let name_owned = tool_name.to_owned();

        let js_fn = Function::new(
            ctx.clone(),
            Async(move |ctx: Ctx<'js>, rest: Rest<JsValue<'js>>| {
                let sess = Arc::clone(&sess);
                let ctrl = ctrl.clone();
                let handle = handle.clone();
                let name_owned = name_owned.clone();
                let decoded = decode_js_args(&ctx, &name_owned, rest);

                async move {
                    let args_json = match decoded {
                        Ok(v) => v,
                        Err(msg) => {
                            return Err(Exception::throw_message(&ctx, &msg));
                        }
                    };
                    let joined = handle
                        .spawn(async move { sess.call_tool(&name_owned, args_json, &ctrl).await })
                        .await;
                    match joined {
                        Ok(Ok(val)) => json_to_js(&ctx, &val),
                        Ok(Err(SubcallError::Fatal(msg))) => {
                            Err(Exception::throw_message(&ctx, &msg))
                        }
                        Ok(Err(err)) => Err(Exception::throw_message(&ctx, &err.to_string())),
                        Err(join_err) => Err(Exception::throw_message(
                            &ctx,
                            &format!("subcall task failed: {join_err}"),
                        )),
                    }
                }
            }),
        )
        .map_err(|e| e.to_string())?;

        globals
            .set(tool_name, js_fn.clone())
            .map_err(|e| e.to_string())?;
        tools_obj.set(tool_name, js_fn).map_err(|e| e.to_string())?;
    }
    globals.set("tools", tools_obj).map_err(|e| e.to_string())?;
    Ok(())
}

fn floor_char_boundary(text: &str, max_bytes: usize) -> usize {
    let mut end = text.len().min(max_bytes);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

pub(crate) fn bound_output_payload(
    raw_output: String,
    stats: &SubcallStats,
    max_output_bytes: usize,
) -> (String, bool) {
    let stats_json = serde_json::to_value(stats).unwrap_or(Value::Null);
    let full = json!({
        "output": raw_output,
        "truncated": false,
        "stats": stats_json,
    });
    if serde_json::to_vec(&full).is_ok_and(|bytes| bytes.len() <= max_output_bytes) {
        return (raw_output, false);
    }

    let empty_shell = json!({
        "output": "",
        "truncated": true,
        "stats": stats_json,
    });
    let shell_bytes = serde_json::to_vec(&empty_shell).map_or(max_output_bytes, |b| b.len());
    let available = max_output_bytes.saturating_sub(shell_bytes);
    if available == 0 {
        return (String::new(), true);
    }

    // Account for JSON escaping by shrinking to the largest UTF-8 prefix whose
    // serialized JSON string fits inside `available`.
    let mut end = floor_char_boundary(&raw_output, available);
    loop {
        let candidate = &raw_output[..end];
        let json_len =
            serde_json::to_vec(candidate).map_or(usize::MAX, |bytes| bytes.len().saturating_sub(2));
        if json_len <= available || end == 0 {
            return (candidate.to_owned(), true);
        }
        let excess = json_len.saturating_sub(available).max(1);
        end = floor_char_boundary(&raw_output, end.saturating_sub(excess));
    }
}

/// Run the QuickJS-NG sandbox synchronously on the current blocking worker thread.
pub(crate) fn run_quickjs_blocking(
    host: Arc<SubcallHost>,
    code: String,
    settings: CodeExecutionSettings,
    control: CallControl,
    outer_handle: Handle,
) -> SandboxOutcome {
    let session = host.new_run_session();
    let max_duration = Duration::from_secs(settings.timeout_secs);
    let deadline = match control.deadline() {
        Some(existing) => existing.min(Instant::now() + max_duration),
        None => Instant::now() + max_duration,
    };
    let local_control = CallControl::new(control.cancellation_token().clone(), Some(deadline));
    let fatal_flag = session.fatal_flag();
    let stdout_buf = Arc::new(Mutex::new(String::new()));
    let max_stdout_bytes = settings.max_output_bytes.saturating_mul(2).max(64 * 1024);

    let rt_builder = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            return SandboxOutcome {
                ok: false,
                output: String::new(),
                truncated: false,
                error: Some(format!("failed to create tokio runtime: {e}")),
                stats: session.stats(),
                cancelled: false,
                timed_out: false,
            };
        }
    };

    let local = tokio::task::LocalSet::new();
    local.block_on(&rt_builder, async move {
        let rt = match AsyncRuntime::new() {
            Ok(rt) => rt,
            Err(e) => {
                return SandboxOutcome {
                    ok: false,
                    output: String::new(),
                    truncated: false,
                    error: Some(format!("AsyncRuntime::new failed: {e}")),
                    stats: session.stats(),
                    cancelled: false,
                    timed_out: false,
                };
            }
        };

        rt.set_memory_limit(settings.memory_limit_bytes).await;
        rt.set_max_stack_size(settings.stack_limit_bytes).await;

        let ctrl_interrupt = local_control.clone();
        let fatal_interrupt = Arc::clone(&fatal_flag);
        rt.set_interrupt_handler(Some(Box::new(move || {
            Instant::now() >= deadline
                || ctrl_interrupt.is_cancelled()
                || fatal_interrupt.load(Ordering::Acquire)
        })))
        .await;

        let ctx = match AsyncContext::full(&rt).await {
            Ok(ctx) => ctx,
            Err(e) => {
                return SandboxOutcome {
                    ok: false,
                    output: String::new(),
                    truncated: false,
                    error: Some(format!("AsyncContext::full failed: {e}")),
                    stats: session.stats(),
                    cancelled: false,
                    timed_out: false,
                };
            }
        };

        let eval_res: Result<(Value, bool), String> = ctx
            .async_with(async |ctx| {
                register_sandbox_globals(
                    &ctx,
                    &session,
                    &local_control,
                    &outer_handle,
                    &stdout_buf,
                    max_stdout_bytes,
                )?;

                let wrapped = format!("(async () => {{\n{code}\n}})()");
                let promise: Promise = ctx
                    .eval(wrapped)
                    .catch(&ctx)
                    .map_err(|e| format_caught_error(&ctx, e))?;

                let fut = promise.into_future::<JsValue>();
                let remaining = deadline.saturating_duration_since(Instant::now());
                let res = tokio::select! {
                    biased;
                    _ = local_control.cancellation_token().cancelled() => {
                        session.record_fatal(ControlError::Cancelled.to_string());
                        return Err(ControlError::Cancelled.to_string());
                    }
                    _ = tokio::time::sleep(remaining) => {
                        session.record_fatal(ControlError::TimedOut.to_string());
                        return Err(ControlError::TimedOut.to_string());
                    }
                    r = fut => r,
                };

                match res {
                    Ok(val) => {
                        let was_undefined = val.is_undefined();
                        js_val_to_json(&ctx, &val).map(|v| (v, was_undefined))
                    }
                    Err(err) => Err(format_rquickjs_err(&ctx, err)),
                }
            })
            .await;

        rt.idle().await;

        let stdout = stdout_buf
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let fatal = session.fatal_error();
        let stats = session.stats();
        let cancelled = local_control.is_cancelled();
        let timed_out = !cancelled && Instant::now() >= deadline;

        match (fatal, eval_res) {
            (Some(msg), _) => {
                let (bounded_out, truncated) =
                    bound_output_payload(stdout, &stats, settings.max_output_bytes);
                SandboxOutcome {
                    ok: false,
                    output: bounded_out,
                    truncated,
                    error: Some(msg),
                    stats,
                    cancelled,
                    timed_out,
                }
            }
            (None, Ok((val, was_undefined))) => {
                let raw_output = if was_undefined {
                    stdout
                } else if let Value::String(s) = val {
                    s
                } else {
                    serde_json::to_string(&val).unwrap_or_else(|_| val.to_string())
                };
                let (bounded_out, truncated) =
                    bound_output_payload(raw_output, &stats, settings.max_output_bytes);
                SandboxOutcome {
                    ok: true,
                    output: bounded_out,
                    truncated,
                    error: None,
                    stats,
                    cancelled: false,
                    timed_out: false,
                }
            }
            (None, Err(err)) => {
                let (bounded_out, truncated) =
                    bound_output_payload(stdout, &stats, settings.max_output_bytes);
                SandboxOutcome {
                    ok: false,
                    output: bounded_out,
                    truncated,
                    error: Some(err),
                    stats,
                    cancelled,
                    timed_out,
                }
            }
        }
    })
}
