use serde_json::Value;

/// Unwrap single- or double-encoded JSON string values (e.g. `"\"hyper_pack\""` -> `"hyper_pack"`).
pub fn unquote_json_string(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        if let Ok(unwrapped) = serde_json::from_str::<String>(trimmed) {
            return unwrapped;
        }
        return trimmed.trim_matches('"').to_string();
    }
    trimmed.to_string()
}

pub fn json_arg_str<'a>(args: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| args.get(*k).and_then(Value::as_str))
}

pub fn json_arg_bool(args: &Value, keys: &[&str]) -> bool {
    for k in keys {
        if let Some(v) = args.get(*k) {
            if let Some(b) = v.as_bool() {
                return b;
            }
            if let Some(s) = v.as_str() {
                if unquote_json_string(s).eq_ignore_ascii_case("true") {
                    return true;
                }
            }
        }
    }
    false
}
