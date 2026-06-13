//! Terminal output helpers: JSON passthrough and compact tables.

use comfy_table::{ContentArrangement, Table};
use serde_json::Value;

/// Print a value as pretty JSON (used for `--json` and for passthrough shapes).
pub fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    );
}

/// A bare table with sensible defaults (dynamic width, no heavy borders).
pub fn table(headers: &[&str]) -> Table {
    let mut t = Table::new();
    t.load_preset(comfy_table::presets::UTF8_HORIZONTAL_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(headers.iter().map(|h| h.to_string()).collect::<Vec<_>>());
    t
}

/// Human-readable byte size (e.g. 1.5 MB).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes == 0 {
        return "0 B".into();
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Truncate a string to `max` chars with an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// First present string field among `keys` in a JSON object (as owned String).
pub fn first_str(obj: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = obj.get(*k).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    None
}

/// Best-effort extraction of an array of records from a passthrough response,
/// trying common envelope shapes (`data`, `data.processes`, `processes`, …).
pub fn extract_array(value: &Value) -> Vec<Value> {
    if let Some(arr) = value.as_array() {
        return arr.clone();
    }
    for ptr in [
        "/data/processes",
        "/data/pipelines",
        "/data/items",
        "/data/results",
        "/data",
        "/pipelines",
        "/processes",
        "/items",
        "/results",
    ] {
        if let Some(arr) = value.pointer(ptr).and_then(Value::as_array) {
            return arr.clone();
        }
    }
    Vec::new()
}
