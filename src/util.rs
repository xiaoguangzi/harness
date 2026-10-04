//! Small helpers shared by the config loaders.

use serde_json::Value;

/// Held by tests that change the process-wide working directory, and by
/// tests whose child processes would inherit it.
#[cfg(test)]
pub static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Takes [`CWD_LOCK`], even if a test panicked while holding it.
#[cfg(test)]
pub fn lock_cwd() -> std::sync::MutexGuard<'static, ()> {
    CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Cuts `s` to at most `max_chars` characters, marking a cut with "…".
pub fn ellipsize(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// `text` on one line (runs of whitespace, and `\n` written out, as one
/// space), cut to at most `max_chars` characters: a description in a list.
pub fn one_line(text: &str, max_chars: usize) -> String {
    let text = text.replace("\\n", " ");
    ellipsize(&text.split_whitespace().collect::<Vec<_>>().join(" "), max_chars)
}

/// Expands `${VAR}` and `${VAR:-default}`; unset variables become "".
pub fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let expr = &after[..end];
        let (var, default) = match expr.split_once(":-") {
            Some((v, d)) => (v, d),
            None => (expr, ""),
        };
        match std::env::var(var) {
            Ok(v) if !v.is_empty() => out.push_str(&v),
            _ => out.push_str(default),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Deep-merges `patch` into `base`: objects merge key by key, a `null` in
/// `patch` removes the key, anything else replaces it.
pub fn merge_json(base: &mut Value, patch: &Value) {
    let (Value::Object(base), Value::Object(patch)) = (base, patch) else {
        return;
    };
    for (key, value) in patch {
        match value {
            Value::Null => {
                base.remove(key);
            }
            Value::Object(_) if base.get(key).is_some_and(Value::is_object) => {
                merge_json(base.get_mut(key).unwrap(), value);
            }
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn expands_env_vars() {
        unsafe { std::env::set_var("HARNESS_TEST_TOKEN", "abc") };
        assert_eq!(expand_env("Bearer ${HARNESS_TEST_TOKEN}"), "Bearer abc");
        assert_eq!(expand_env("${HARNESS_TEST_UNSET_VAR:-dflt}/x"), "dflt/x");
        assert_eq!(expand_env("${HARNESS_TEST_UNSET_VAR}"), "");
        assert_eq!(expand_env("no vars, ${unclosed"), "no vars, ${unclosed");
    }

    #[test]
    fn merges_json() {
        let mut base = json!({"a": 1, "thinking": {"type": "adaptive"}, "nested": {"x": 1, "y": 2}});
        merge_json(&mut base, &json!({"thinking": null, "nested": {"y": 3, "z": 4}, "b": [1]}));
        assert_eq!(base, json!({"a": 1, "nested": {"x": 1, "y": 3, "z": 4}, "b": [1]}));
    }
}
