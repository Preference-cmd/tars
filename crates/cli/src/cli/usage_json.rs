//! Serialization of usage statistics into standard JSON shapes.

use serde_json::{Value, json};
use tars_core::usage::{LastCall, PurposeUsage, SessionUsage, UsageFigures, WorkUsage};

/// The token fields shared by every usage block; `null` where unreported.
fn figures_fields(figures: &UsageFigures) -> serde_json::Map<String, Value> {
    let Value::Object(fields) = json!({
        "input_tokens": figures.input_tokens,
        "cache_read_tokens": figures.cache_read_tokens,
        "cache_write_tokens": figures.cache_write_tokens,
        "output_tokens": figures.output_tokens,
        "reasoning_tokens": figures.reasoning_tokens,
        "prompt_tokens": figures.prompt_tokens,
        "total_tokens": figures.total_tokens,
        "cache_hit_rate": figures.cache_hit_rate,
    }) else {
        unreachable!("a JSON object literal")
    };
    fields
}

/// Convert a [`PurposeUsage`] into a standard usage JSON block.
pub fn purpose_usage_value(usage: &PurposeUsage) -> Value {
    let mut block = figures_fields(&usage.figures());
    block.insert(
        "calls".into(),
        json!({
            "succeeded": usage.calls.succeeded,
            "failed": usage.calls.failed,
            "usage_unreported": usage.calls.unreported,
        }),
    );
    Value::Object(block)
}

/// Convert a [`LastCall`] into its JSON representation.
pub fn last_call_value(last_call: &LastCall) -> Value {
    let mut call = figures_fields(&last_call.figures());
    call.insert("purpose".into(), json!(last_call.purpose.as_str()));
    call.insert("at_unix_ms".into(), json!(last_call.at_unix_ms()));
    Value::Object(call)
}

/// Convert a [`WorkUsage`] into the work statistics JSON shape.
pub fn work_usage_value(work_usage: &WorkUsage) -> Value {
    let combined = work_usage.combined();
    let mut val = purpose_usage_value(&combined);
    let obj = val.as_object_mut().expect("purpose_usage_value is object");
    obj.insert("model_rounds".to_string(), json!(work_usage.model_rounds));
    obj.insert(
        "conversation".to_string(),
        purpose_usage_value(&work_usage.conversation),
    );
    obj.insert(
        "compaction".to_string(),
        purpose_usage_value(&work_usage.compaction),
    );
    obj.insert(
        "last_call".to_string(),
        work_usage
            .last_call
            .as_ref()
            .map_or(json!(null), last_call_value),
    );
    val
}

/// Convert a [`SessionUsage`] into the session statistics JSON shape for `session show`.
pub fn session_usage_value(session_usage: &SessionUsage) -> Value {
    json!({
        "input_rounds": session_usage.input_rounds,
        "total": purpose_usage_value(&session_usage.combined()),
        "conversation": purpose_usage_value(&session_usage.conversation),
        "compaction": purpose_usage_value(&session_usage.compaction),
        "last_call": session_usage
            .last_call
            .as_ref()
            .map_or(json!(null), last_call_value),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_core::usage::{CallCounts, CallUsage, UsageTotals};

    #[test]
    fn us01_display_json_blocks() {
        // fixture usage input 100, read Some(11), write Some(3), output 20
        let call = CallUsage {
            input_tokens: 100,
            cache_read_tokens: Some(11),
            cache_write_tokens: Some(3),
            output_tokens: 20,
            reasoning_tokens: None,
        };
        let mut totals = UsageTotals::default();
        totals.record_call(&call);

        let usage = PurposeUsage {
            calls: CallCounts {
                succeeded: 1,
                failed: 0,
                unreported: 0,
            },
            tokens: totals,
        };

        let block = purpose_usage_value(&usage);
        assert_eq!(block["calls"]["succeeded"], 1);
        assert_eq!(block["calls"]["failed"], 0);
        assert_eq!(block["calls"]["usage_unreported"], 0);
        assert_eq!(block["input_tokens"], 100);
        assert_eq!(block["cache_read_tokens"], 11);
        assert_eq!(block["cache_write_tokens"], 3);
        assert_eq!(block["output_tokens"], 20);
        assert!(block["reasoning_tokens"].is_null());
        assert_eq!(block["prompt_tokens"], 114);
        assert_eq!(block["total_tokens"], 134);
        let hit_rate = block["cache_hit_rate"].as_f64().unwrap();
        assert!((hit_rate - 0.09649).abs() < 0.001);

        // read/write None -> JSON null
        let call_no_cache = CallUsage {
            input_tokens: 100,
            cache_read_tokens: None,
            cache_write_tokens: None,
            output_tokens: 20,
            reasoning_tokens: None,
        };
        let mut totals_no_cache = UsageTotals::default();
        totals_no_cache.record_call(&call_no_cache);
        let usage_no_cache = PurposeUsage {
            calls: CallCounts {
                succeeded: 1,
                failed: 0,
                unreported: 0,
            },
            tokens: totals_no_cache,
        };
        let block_no_cache = purpose_usage_value(&usage_no_cache);
        assert!(block_no_cache["cache_read_tokens"].is_null());
        assert!(block_no_cache["cache_write_tokens"].is_null());
        assert!(block_no_cache["cache_hit_rate"].is_null());

        // A call with no usage at all -> usage_unreported 1 and token fields null when it is the only call
        let usage_unreported = PurposeUsage {
            calls: CallCounts {
                succeeded: 1,
                failed: 0,
                unreported: 1,
            },
            tokens: UsageTotals::default(),
        };
        let block_unreported = purpose_usage_value(&usage_unreported);
        assert_eq!(block_unreported["calls"]["usage_unreported"], 1);
        assert!(block_unreported["input_tokens"].is_null());
        assert!(block_unreported["cache_read_tokens"].is_null());
        assert!(block_unreported["cache_write_tokens"].is_null());
        assert!(block_unreported["output_tokens"].is_null());
        assert!(block_unreported["reasoning_tokens"].is_null());
        assert!(block_unreported["prompt_tokens"].is_null());
        assert!(block_unreported["total_tokens"].is_null());
        assert!(block_unreported["cache_hit_rate"].is_null());
    }
}
