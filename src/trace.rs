//! Append-only JSONL trace of tool calls.
//!
//! Every call is recorded with its arguments, duration, outcome, and - for
//! tools that can change the screen - a before/after frame hash.  The file is
//! deliberately outside the session runtime directory so it survives
//! `display_stop` and can be inspected afterwards.

use crate::env;
use crate::state;
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn traces_dir() -> PathBuf {
    env::shared_runtime_dir().join("traces")
}

pub fn session_path(session_id: &str) -> PathBuf {
    traces_dir().join(format!("{session_id}.jsonl"))
}

/// The trace file for this server process.  It collects calls made before a
/// session exists, plus lifecycle events that are not tied to a session.
pub fn server_path() -> PathBuf {
    traces_dir().join(format!("server-{}.jsonl", std::process::id()))
}

/// Resolve the trace file for a call.  Prefer the active session's id so all
/// calls from one desktop land in one file, even before/after start.
pub fn path_for(session_id: Option<&str>) -> PathBuf {
    match session_id {
        Some(id) if !id.is_empty() => session_path(id),
        _ => server_path(),
    }
}

/// Append one trace record.  Tracing must never break a tool call, so all
/// errors are swallowed by callers; this function only reports them.
pub fn append(path: &Path, entry: Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        env::create_private_dir(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    serde_json::to_writer(&mut file, &entry)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

/// Trim arguments before they reach the trace: long strings (typed text,
/// clipboard contents) are truncated and image payloads never appear here.
pub fn sanitize_args(args: &Value) -> Value {
    fn limit(value: &Value, depth: usize) -> Value {
        if depth > 4 {
            return json!("<deep>");
        }
        match value {
            Value::String(text) => {
                if text.chars().count() > 512 {
                    let head: String = text.chars().take(512).collect();
                    json!(format!("{head}… ({} chars)", text.chars().count()))
                } else {
                    Value::String(text.clone())
                }
            }
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| limit(item, depth + 1)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .filter(|(key, _)| !matches!(key.as_str(), "data" | "image" | "png"))
                    .map(|(key, value)| (key.clone(), limit(value, depth + 1)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    limit(args, 0)
}

/// Build the standard trace entry.
#[allow(clippy::too_many_arguments)]
pub fn entry(
    tool: &str,
    args: &Value,
    session_id: Option<&str>,
    display: Option<&str>,
    duration_ms: u128,
    ok: bool,
    error: Option<&str>,
    before_hash: Option<&str>,
    after_hash: Option<&str>,
) -> Value {
    let mut record = json!({
        "ts": state::now(),
        "tool": tool,
        "args": sanitize_args(args),
        "session_id": session_id,
        "display": display,
        "duration_ms": duration_ms,
        "ok": ok,
        "server_pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
    });
    if let Some(error) = error {
        record["error"] = json!(error.chars().take(2048).collect::<String>());
        // A stable category lets `trace_stats` group failures without parsing
        // prose.  The full message stays in `error` for humans.
        record["error_code"] = json!(error_code(error));
    }
    if let Some(hash) = before_hash {
        record["before_hash"] = json!(hash);
    }
    if let Some(hash) = after_hash {
        record["after_hash"] = json!(hash);
        if let Some(before) = before_hash {
            record["changed"] = json!(before != hash);
        }
    }
    record
}

/// Build a lifecycle record (server start, session stop, panic).  Events are
/// deliberately not tool calls so failure statistics stay meaningful.
pub fn event(name: &str, fields: Value) -> Value {
    let mut record = json!({
        "ts": state::now(),
        "event": name,
        "server_pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
    });
    if let Value::Object(map) = fields {
        for (key, value) in map {
            record[key] = value;
        }
    }
    record
}

/// Append an event to this server's trace.  Like [`append`], failures are the
/// caller's to ignore: telemetry must never change the outcome of a call.
pub fn record_event(name: &str, fields: Value) {
    let _ = append(&server_path(), event(name, fields));
}

/// Stable error categories used by the trace and `trace_stats`.
///
/// The mapping is intentionally prefix-based: callers see the original
/// message, while dashboards can count `no_display` or
/// `display_already_running` without depending on exact wording.
pub fn error_code(message: &str) -> &'static str {
    let text = message.trim().to_ascii_lowercase();
    let starts = |prefix: &str| text.starts_with(prefix);
    let contains = |needle: &str| text.contains(needle);
    if starts("no nested display is running") || starts("no display is running") {
        "no_display"
    } else if starts("a nested display is already running")
        || starts("a display is already running")
    {
        "display_already_running"
    } else if starts("unknown argument") {
        "unknown_argument"
    } else if starts("missing ") || contains(" is required") {
        "missing_argument"
    } else if starts("invalid")
        || starts("backend must")
        || starts("width and height")
        || contains("must be ")
        || contains("must have ")
    {
        "invalid_argument"
    } else if contains("was not found; install") || starts("xdotool was not found") {
        "missing_tool"
    } else if contains("human control") || contains("control is owned") {
        "human_control"
    } else if contains("was not started by launch") {
        "not_launched"
    } else if contains("timed out") || contains("timeout") {
        "timeout"
    } else if contains("not supported") || contains("not available") {
        "unsupported"
    } else if contains("no such") || contains("does not exist") {
        "not_found"
    } else {
        "internal"
    }
}

/// Aggregate every trace into the counters an operator actually needs: call
/// volume, failure rate, error categories, latency, and lifecycle stops.
///
/// `session_id` restricts the scan to one session file; otherwise the newest
/// `max_files` JSONL traces are read.  Aggregation is bounded and ignores
/// malformed lines so a partially written trace cannot break diagnostics.
pub fn summary(session_id: Option<&str>, max_files: usize, recent: usize) -> Value {
    summary_in(&traces_dir(), session_id, max_files, recent)
}

pub fn summary_in(dir: &Path, session_id: Option<&str>, max_files: usize, recent: usize) -> Value {
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct ToolStat {
        calls: u64,
        failures: u64,
        durations: Vec<u64>,
        codes: BTreeMap<String, u64>,
        last_error: Option<String>,
        last_error_ts: u64,
    }

    let max_files = max_files.clamp(1, 1000);
    let recent = recent.clamp(1, 200);
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();

    if let Some(id) = session_id {
        let path = dir.join(format!("{id}.jsonl"));
        if let Ok(modified) = std::fs::metadata(&path).and_then(|meta| meta.modified()) {
            files.push((modified, path));
        }
    } else if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
                continue;
            };
            files.push((modified, path));
        }
    }
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    files.truncate(max_files);

    let mut tools: BTreeMap<String, ToolStat> = BTreeMap::new();
    let mut events: BTreeMap<String, u64> = BTreeMap::new();
    let mut stop_reasons: BTreeMap<String, u64> = BTreeMap::new();
    let mut error_codes: BTreeMap<String, u64> = BTreeMap::new();
    let mut recent_failures: Vec<Value> = Vec::new();
    let mut calls = 0u64;
    let mut failures = 0u64;
    let mut scanned = 0u64;
    let mut since: Option<u64> = None;
    let mut until: Option<u64> = None;

    for (_, path) in &files {
        let Ok(file) = std::fs::File::open(path) else {
            continue;
        };
        scanned += 1;
        for line in BufReader::new(file).lines().take(100_000) {
            let Ok(line) = line else {
                break;
            };
            if line.len() > 1024 * 1024 {
                continue;
            }
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let ts = record.get("ts").and_then(Value::as_u64).unwrap_or(0);
            if ts > 0 {
                since = Some(since.map_or(ts, |value| value.min(ts)));
                until = Some(until.map_or(ts, |value| value.max(ts)));
            }

            if let Some(name) = record.get("event").and_then(Value::as_str) {
                *events.entry(name.to_string()).or_insert(0) += 1;
                if name == "session_stop" {
                    if let Some(reason) = record.get("reason").and_then(Value::as_str) {
                        *stop_reasons.entry(reason.to_string()).or_insert(0) += 1;
                    }
                }
                continue;
            }

            let Some(tool) = record.get("tool").and_then(Value::as_str) else {
                continue;
            };
            // Older builds recorded lifecycle stops as tool rows.  Keep those
            // out of call/failure statistics and fold them into stop reasons.
            if tool == "watchdog" || tool == "viewer_stop" {
                *events.entry("session_stop".to_string()).or_insert(0) += 1;
                let reason = if tool == "viewer_stop" {
                    "viewer_request".to_string()
                } else {
                    record
                        .get("args")
                        .and_then(|args| args.get("reason"))
                        .and_then(Value::as_str)
                        .unwrap_or("watchdog")
                        .replace(' ', "_")
                };
                *stop_reasons.entry(reason).or_insert(0) += 1;
                continue;
            }
            let ok = record.get("ok").and_then(Value::as_bool).unwrap_or(true);
            calls += 1;
            let stat = tools.entry(tool.to_string()).or_default();
            stat.calls += 1;
            if let Some(duration) = record.get("duration_ms").and_then(Value::as_u64) {
                stat.durations.push(duration);
            }
            if ok {
                continue;
            }
            failures += 1;
            stat.failures += 1;
            let code = record
                .get("error_code")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    record
                        .get("error")
                        .and_then(Value::as_str)
                        .map(error_code)
                        .unwrap_or("unclassified")
                        .to_string()
                });
            *stat.codes.entry(code.clone()).or_insert(0) += 1;
            *error_codes.entry(code.clone()).or_insert(0) += 1;
            let message = record
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("tool error");
            if ts >= stat.last_error_ts {
                stat.last_error_ts = ts;
                stat.last_error = Some(message.chars().take(512).collect());
            }
            recent_failures.push(json!({
                "ts": ts,
                "tool": tool,
                "error_code": code,
                "error": message.chars().take(512).collect::<String>(),
                "session_id": record.get("session_id"),
                "display": record.get("display"),
            }));
        }
    }

    let mut tool_stats: Vec<Value> = Vec::new();
    for (name, mut stat) in tools {
        stat.durations.sort_unstable();
        let percentile = |percent: f64| -> u64 {
            if stat.durations.is_empty() {
                return 0;
            }
            let rank = ((percent / 100.0) * stat.durations.len() as f64).ceil() as usize;
            let index = rank.saturating_sub(1).min(stat.durations.len() - 1);
            stat.durations[index]
        };
        tool_stats.push(json!({
            "tool": name,
            "calls": stat.calls,
            "failures": stat.failures,
            "failure_rate": ratio(stat.failures, stat.calls),
            "p50_ms": percentile(50.0),
            "p95_ms": percentile(95.0),
            "max_ms": stat.durations.last().copied().unwrap_or(0),
            "error_codes": stat.codes,
            "last_error": stat.last_error,
            "last_error_ts": (stat.last_error_ts > 0).then_some(stat.last_error_ts),
        }));
    }
    tool_stats.sort_by_key(|item| {
        std::cmp::Reverse(item.get("calls").and_then(Value::as_u64).unwrap_or(0))
    });

    recent_failures
        .sort_by_key(|item| std::cmp::Reverse(item.get("ts").and_then(Value::as_u64).unwrap_or(0)));
    recent_failures.truncate(recent);

    json!({
        "trace_dir": dir.display().to_string(),
        "session_id": session_id,
        "files": scanned,
        "since": since,
        "until": until,
        "calls": calls,
        "failures": failures,
        "failure_rate": ratio(failures, calls),
        "events": events,
        "stop_reasons": stop_reasons,
        "error_codes": error_codes,
        "tools": tool_stats,
        "recent_failures": recent_failures,
    })
}

fn ratio(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (part as f64 / total as f64 * 10_000.0).round() / 10_000.0
    }
}

/// Tools whose call can plausibly change the frame.  Only these pay the cost
/// of two extra captures per call.
pub const HASHED_TOOLS: &[&str] = &[
    "click",
    "move",
    "drag",
    "scroll",
    "type",
    "key",
    "mouse_down",
    "mouse_up",
    "key_down",
    "key_up",
    "launch",
    "focus_window",
    "move_resize_window",
    "close_window",
    "window_state",
    "resize_display",
    "a11y_click",
    "a11y_set_text",
    "a11y_focus",
    "clipboard_set",
];

pub fn should_hash(tool: &str) -> bool {
    HASHED_TOOLS.contains(&tool)
}

/// Read a bounded trace tail.  The path is constrained to the trace directory
/// so a caller cannot use this to read arbitrary files.
pub fn read_tail(session_id: Option<&str>, lines: usize) -> Result<(PathBuf, String)> {
    let path = match session_id {
        Some(id) => {
            crate::display::validate_session_id(id)?;
            session_path(id)
        }
        None => newest_trace().ok_or_else(|| anyhow!("no trace files exist yet"))?,
    };
    let canonical_dir = traces_dir().canonicalize().unwrap_or_else(|_| traces_dir());
    let canonical = path
        .canonicalize()
        .with_context(|| format!("trace file {} does not exist", path.display()))?;
    if !canonical.starts_with(&canonical_dir) {
        return Err(anyhow!("trace path is outside the trace directory"));
    }
    let text = crate::proc::tail_bounded(&canonical, lines.clamp(1, 2000), 512 * 1024)?;
    Ok((canonical, text))
}

fn newest_trace() -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(traces_dir()).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        if best
            .as_ref()
            .map(|(time, _)| modified > *time)
            .unwrap_or(true)
        {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

/// Per-call trace screenshots are opt-in: each screen-changing call would
/// otherwise pay for two full captures plus PNG encoding.
pub fn screenshots_enabled() -> bool {
    match std::env::var("XEPHYR_MCP_TRACE_SCREENSHOTS") {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

/// Keep the newest N step images per session so a long run cannot fill the
/// runtime filesystem.
pub const MAX_STEPS: usize = 400;

fn safe_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub fn steps_dir(session_id: Option<&str>) -> PathBuf {
    let name = match session_id {
        Some(id) if !id.is_empty() => safe_component(id),
        _ => format!("server-{}", std::process::id()),
    };
    traces_dir().join("steps").join(name)
}

/// Write one before/after frame and prune older images.  Returns the path so
/// it can be recorded in the trace entry.
pub fn write_step(dir: &Path, tool: &str, phase: &str, png: &[u8]) -> Result<PathBuf> {
    env::create_private_dir(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!(
        "{stamp:013}-{}-{}.png",
        safe_component(tool),
        safe_component(phase)
    ));
    std::fs::write(&path, png).with_context(|| format!("cannot write {}", path.display()))?;
    prune_steps(dir, MAX_STEPS);
    Ok(path)
}

fn prune_steps(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut images: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("png"))
        .collect();
    if images.len() <= keep {
        return;
    }
    images.sort();
    let excess = images.len() - keep;
    for path in images.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitize_truncates_long_strings() {
        let text = "x".repeat(900);
        let value = sanitize_args(&json!({"text": text, "n": 3}));
        let kept = value["text"].as_str().unwrap();
        assert!(kept.contains("900 chars"));
        assert!(kept.chars().count() < 600);
        assert_eq!(value["n"], json!(3));
    }

    #[test]
    fn sanitize_drops_payload_fields() {
        let value = sanitize_args(&json!({"data": "AAAA", "image": {"png": "x"}, "x": 1}));
        assert!(value.get("data").is_none());
        assert!(value.get("image").is_none());
        assert_eq!(value["x"], json!(1));
    }

    #[test]
    fn hash_tool_list_is_stable() {
        assert!(should_hash("click"));
        assert!(should_hash("type"));
        assert!(!should_hash("screenshot"));
        assert!(!should_hash("diagnose"));
    }

    #[test]
    fn error_codes_group_known_failures() {
        assert_eq!(
            error_code("no nested display is running; call display_start first"),
            "no_display"
        );
        assert_eq!(
            error_code(
                "a nested display is already running at :100 (1280x800); call display_stop first"
            ),
            "display_already_running"
        );
        assert_eq!(
            error_code("unknown argument \"idle_timeout_secs\""),
            "unknown_argument"
        );
        assert_eq!(
            error_code("xdotool was not found; install it to use input tools"),
            "missing_tool"
        );
        assert_eq!(
            error_code("pid 42 was not started by launch in this session"),
            "not_launched"
        );
        assert_eq!(error_code("something exploded"), "internal");
    }

    #[test]
    fn summary_counts_calls_failures_and_events() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("xephyr-mcp-trace-{}-{stamp}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("abc.jsonl");

        append(
            &path,
            entry(
                "screenshot",
                &json!({}),
                Some("abc"),
                Some(":100"),
                5,
                false,
                Some("no nested display is running; call display_start first"),
                None,
                None,
            ),
        )
        .unwrap();
        append(
            &path,
            entry(
                "screenshot",
                &json!({}),
                Some("abc"),
                Some(":100"),
                7,
                true,
                None,
                None,
                None,
            ),
        )
        .unwrap();
        append(
            &path,
            event("session_stop", json!({"reason": "idle_timeout"})),
        )
        .unwrap();

        let summary = summary_in(&dir, None, 10, 5);
        assert_eq!(summary["calls"], 2);
        assert_eq!(summary["failures"], 1);
        assert_eq!(summary["error_codes"]["no_display"], 1);
        assert_eq!(summary["events"]["session_stop"], 1);
        assert_eq!(summary["stop_reasons"]["idle_timeout"], 1);
        assert_eq!(summary["tools"][0]["tool"], "screenshot");
        assert_eq!(summary["tools"][0]["calls"], 2);
        assert_eq!(summary["tools"][0]["failures"], 1);
        assert_eq!(summary["tools"][0]["p95_ms"], 7);
        assert_eq!(summary["recent_failures"][0]["error_code"], "no_display");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
