use serde_json::{Map, Value};

const SENSITIVE_EXACT_KEYS: &[&str] = &[
    "access_token",
    "refresh_token",
    "id_token",
    "api_key",
    "openai_api_key",
    "authorization",
    "password",
    "secret",
    "cookie",
    "session_cookie",
];

pub fn is_sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase();
    SENSITIVE_EXACT_KEYS
        .iter()
        .any(|sensitive| normalized == *sensitive)
        || normalized.ends_with("_token")
        || normalized.ends_with("_secret")
        || normalized.ends_with("_api_key")
}

pub fn redact_sensitive_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(redact_object(map)),
        Value::Array(values) => Value::Array(values.iter().map(redact_sensitive_json).collect()),
        other => other.clone(),
    }
}

fn redact_object(map: &Map<String, Value>) -> Map<String, Value> {
    map.iter()
        .map(|(key, value)| {
            let redacted_value = if is_sensitive_key(key) {
                Value::String("[REDACTED]".to_string())
            } else {
                redact_sensitive_json(value)
            };
            (key.clone(), redacted_value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{is_sensitive_key, redact_sensitive_json};

    #[test]
    fn redacts_nested_token_values_without_redacting_safe_text() {
        let value = json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": "fake-access",
                "refresh_token": "fake-refresh",
                "nested": [{ "api_key": "fake-key" }]
            },
            "monkey": "safe"
        });

        let redacted = redact_sensitive_json(&value);

        assert_eq!(redacted["auth_mode"], "chatgpt");
        assert_eq!(redacted["tokens"]["access_token"], "[REDACTED]");
        assert_eq!(redacted["tokens"]["refresh_token"], "[REDACTED]");
        assert_eq!(redacted["tokens"]["nested"][0]["api_key"], "[REDACTED]");
        assert_eq!(redacted["monkey"], "safe");
    }

    #[test]
    fn sensitive_key_detection_avoids_substring_false_positives() {
        assert!(is_sensitive_key("OPENAI_API_KEY"));
        assert!(is_sensitive_key("refresh_token"));
        assert!(is_sensitive_key("authorization"));
        assert!(!is_sensitive_key("monkey"));
        assert!(!is_sensitive_key("keynote"));
    }
}
