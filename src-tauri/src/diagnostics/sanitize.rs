use std::{env, path::PathBuf};

use serde_json::{Map, Value};

use super::event::DiagnosticEvent;

pub const MAX_SAFE_MESSAGE_BYTES: usize = 4 * 1024;
pub const MAX_SAFE_STRING_BYTES: usize = 2 * 1024;
pub const MAX_CONTEXT_FIELDS: usize = 32;
const MAX_JSON_DEPTH: usize = 4;
const MAX_ARRAY_ITEMS: usize = 32;
const MAX_RAW_TEXT_BYTES: usize = 16 * 1024;
const MAX_AUTH_SEPARATOR_BYTES: usize = 64;
const REDACTED_SECRET: &str = "[REDACTED_SECRET]";
const REDACTED_ID: &str = "[REDACTED_ID]";
const REDACTED_IDENTITY: &str = "[REDACTED_IDENTITY]";
const REDACTED_PATH: &str = "[REDACTED_PATH]";
const REDACTED_URL: &str = "[REDACTED_URL]";
const TRUNCATED: &str = "[TRUNCATED]";

const ASSIGNMENT_MARKERS: [&str; 12] = [
    "authorization",
    "client_secret",
    "access_token",
    "refresh_token",
    "api_key",
    "api-key",
    "apikey",
    "password",
    "credential",
    "cookie",
    "token",
    "secret",
];

const TOKEN_PREFIXES: [&str; 12] = [
    "github_pat_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "sk-",
    "xoxb-",
    "xoxp-",
    "sess-",
    "bearer.",
    "basic.",
];

const PRIVATE_KEY_MARKERS: [&str; 3] = [
    "-----begin private key-----",
    "-----begin rsa private key-----",
    "-----begin openssh private key-----",
];

#[derive(Debug, Clone, Default)]
pub struct SanitizerRoots {
    pub user_profile: Option<PathBuf>,
    pub appdata: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
}

impl SanitizerRoots {
    pub fn from_environment() -> Self {
        Self {
            user_profile: env::var_os("USERPROFILE").map(PathBuf::from),
            appdata: env::var_os("APPDATA").map(PathBuf::from),
            codex_home: env::var_os("CODEX_HOME").map(PathBuf::from),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiagnosticSanitizer {
    path_replacements: Vec<(String, &'static str)>,
    identity_replacements: Vec<String>,
}

impl DiagnosticSanitizer {
    pub fn new(roots: SanitizerRoots) -> Self {
        let mut path_replacements = Vec::new();
        push_root(
            &mut path_replacements,
            roots.codex_home.clone(),
            "%CODEX_HOME%",
        );
        push_root(&mut path_replacements, roots.appdata.clone(), "%APPDATA%");
        push_root(
            &mut path_replacements,
            roots.user_profile.clone(),
            "%USERPROFILE%",
        );
        path_replacements.sort_by_key(|right| std::cmp::Reverse(right.0.len()));
        path_replacements.dedup_by(|left, right| left.0.eq_ignore_ascii_case(&right.0));

        let mut identity_replacements = Vec::new();
        if let Some(user_profile) = roots.user_profile {
            if let Some(username) = user_profile.file_name().and_then(|value| value.to_str()) {
                push_identity(&mut identity_replacements, username);
            }
        }
        for key in ["USERNAME", "COMPUTERNAME"] {
            if let Ok(value) = env::var(key) {
                push_identity(&mut identity_replacements, &value);
            }
        }
        identity_replacements.sort_by_key(|right| std::cmp::Reverse(right.len()));
        identity_replacements.dedup_by(|left, right| left.eq_ignore_ascii_case(right));

        Self {
            path_replacements,
            identity_replacements,
        }
    }

    pub fn from_environment_with_appdata(appdata: PathBuf) -> Self {
        let mut roots = SanitizerRoots::from_environment();
        roots.appdata = Some(appdata);
        Self::new(roots)
    }

    pub fn sanitize_event(&self, event: &DiagnosticEvent) -> DiagnosticEvent {
        DiagnosticEvent {
            schema_version: event.schema_version,
            event_id: sanitize_identifier(&event.event_id, 160),
            session_id: sanitize_identifier(&event.session_id, 128),
            sequence: event.sequence,
            timestamp: event.timestamp,
            level: event.level,
            component: sanitize_identifier(&event.component, 96),
            event_kind: event.event_kind,
            attempt_id: event
                .attempt_id
                .as_deref()
                .map(|value| sanitize_identifier(value, 160)),
            operation_id: event
                .operation_id
                .as_deref()
                .map(|value| sanitize_identifier(value, 160)),
            action: event
                .action
                .as_deref()
                .map(|value| sanitize_identifier(value, 96)),
            phase: event
                .phase
                .as_deref()
                .map(|value| sanitize_identifier(value, 96)),
            terminal_status: event.terminal_status,
            error_code: event
                .error_code
                .as_deref()
                .map(|value| sanitize_identifier(value, 128)),
            safe_message: event
                .safe_message
                .as_deref()
                .map(|value| self.sanitize_text_with_limit(value, MAX_SAFE_MESSAGE_BYTES)),
            safe_context: event
                .safe_context
                .iter()
                .take(MAX_CONTEXT_FIELDS)
                .map(|(key, value)| {
                    let sensitive = sensitive_key(key, value);
                    let key = sanitize_identifier(key, 96);
                    let value = if sensitive {
                        Value::String(REDACTED_SECRET.to_string())
                    } else {
                        self.sanitize_json_value(value, 0)
                    };
                    (key, value)
                })
                .collect(),
        }
    }

    pub fn sanitize_for_export(&self, event: &DiagnosticEvent) -> DiagnosticEvent {
        self.sanitize_event(event)
    }

    pub fn sanitize_text(&self, value: &str) -> String {
        self.sanitize_text_with_limit(value, MAX_SAFE_STRING_BYTES)
    }

    fn sanitize_text_with_limit(&self, value: &str, limit: usize) -> String {
        let raw_limit = limit.saturating_mul(4).min(MAX_RAW_TEXT_BYTES);
        let (bounded, was_truncated) = bounded_prefix(value, raw_limit);
        let mut output = bounded.replace(['\r', '\n', '\t'], " ");
        if was_truncated {
            output.push_str(" [TRUNCATED]");
        }
        for (root, token) in &self.path_replacements {
            output = replace_known_path_root(&output, root, token);
            let alternate = root.replace('\\', "/");
            if alternate != *root {
                output = replace_known_path_root(&output, &alternate, token);
            }
        }
        output = redact_urls(output);
        output = redact_unknown_paths(output);
        for identity in &self.identity_replacements {
            output = replace_identity_literal(&output, identity);
        }
        output = redact_free_form_ids(&output);
        output = redact_assignments(output);
        output = redact_embedded_secrets(output);
        truncate_utf8(&output, limit)
    }

    fn sanitize_json_value(&self, value: &Value, depth: usize) -> Value {
        if depth >= MAX_JSON_DEPTH {
            return Value::String(TRUNCATED.to_string());
        }
        match value {
            Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
            Value::String(value) => Value::String(self.sanitize_text(value)),
            Value::Array(values) => Value::Array(
                values
                    .iter()
                    .take(MAX_ARRAY_ITEMS)
                    .map(|value| self.sanitize_json_value(value, depth + 1))
                    .collect(),
            ),
            Value::Object(values) => {
                let mut sanitized = Map::new();
                for (key, value) in values.iter().take(MAX_CONTEXT_FIELDS) {
                    let sensitive = sensitive_key(key, value);
                    let key = sanitize_identifier(key, 96);
                    let value = if sensitive {
                        Value::String(REDACTED_SECRET.to_string())
                    } else {
                        self.sanitize_json_value(value, depth + 1)
                    };
                    sanitized.insert(key, value);
                }
                Value::Object(sanitized)
            }
        }
    }
}

fn push_root(
    replacements: &mut Vec<(String, &'static str)>,
    root: Option<PathBuf>,
    token: &'static str,
) {
    let Some(root) = root else {
        return;
    };
    let root = root.to_string_lossy();
    let root = root.trim_end_matches(['\\', '/']);
    if !root.is_empty() {
        replacements.push((root.to_string(), token));
    }
}

fn push_identity(identities: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if !value.is_empty() && value.len() <= 256 {
        identities.push(value.to_string());
    }
}

fn sensitive_key(key: &str, value: &Value) -> bool {
    let normalized = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if matches!(value, Value::Bool(_))
        && matches!(
            normalized.as_str(),
            "credentialconfigured" | "tokenconfigured" | "authenticated"
        )
    {
        return false;
    }
    [
        "apikey",
        "authorization",
        "bearer",
        "password",
        "token",
        "credential",
        "secret",
        "cookie",
        "privatekey",
        "requestbody",
        "responsebody",
        "prompt",
        "completion",
        "chatcontent",
        "messagecontent",
        "sessionjsonl",
        "authjson",
        "configtoml",
        "username",
        "machinename",
        "devicename",
        "accountname",
        "threadid",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

fn sanitize_identifier(value: &str, limit: usize) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':') {
                character
            } else {
                '_'
            }
        })
        .take(limit)
        .collect()
}

fn bounded_prefix(value: &str, limit: usize) -> (&str, bool) {
    if value.len() <= limit {
        return (value, false);
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    (&value[..end], true)
}

pub(super) fn replace_known_path_root(value: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() || value.len() < needle.len() {
        return value.to_string();
    }
    let source = value.as_bytes();
    let target = needle.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    let mut index = 0;
    while index + target.len() <= source.len() {
        let end = index + target.len();
        if source[index..end].eq_ignore_ascii_case(target)
            && is_path_root_start_boundary(source.get(index.wrapping_sub(1)).copied())
            && is_path_root_boundary(source.get(end).copied())
        {
            output.push_str(&value[cursor..index]);
            output.push_str(replacement);
            index = end;
            cursor = index;
        } else {
            index += 1;
        }
    }
    output.push_str(&value[cursor..]);
    output
}

fn is_path_root_start_boundary(byte: Option<u8>) -> bool {
    byte.is_none_or(|byte| {
        byte.is_ascii_whitespace()
            || matches!(
                byte,
                b'"' | b'\'' | b'(' | b'[' | b'{' | b',' | b';' | b'=' | b':'
            )
    })
}

fn is_path_root_boundary(byte: Option<u8>) -> bool {
    byte.is_none_or(|byte| matches!(byte, b'\\' | b'/'))
}

fn replace_identity_literal(value: &str, identity: &str) -> String {
    if identity.is_empty() || value.len() < identity.len() {
        return value.to_string();
    }
    let source = value.as_bytes();
    let target = identity.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    let mut index = 0;
    while index + target.len() <= source.len() {
        if let Some(length) = protected_placeholder_len(value, index) {
            index += length;
            continue;
        }
        let end = index + target.len();
        if source[index..end].eq_ignore_ascii_case(target)
            && is_identity_boundary(source.get(index.wrapping_sub(1)).copied())
            && is_identity_boundary(source.get(end).copied())
        {
            output.push_str(&value[cursor..index]);
            output.push_str(REDACTED_IDENTITY);
            cursor = end;
            index = end;
        } else {
            index += 1;
        }
    }
    output.push_str(&value[cursor..]);
    output
}

fn protected_placeholder_len(value: &str, index: usize) -> Option<usize> {
    [
        REDACTED_SECRET,
        REDACTED_ID,
        REDACTED_IDENTITY,
        REDACTED_PATH,
        REDACTED_URL,
        TRUNCATED,
        "%CODEX_HOME%",
        "%APPDATA%",
        "%USERPROFILE%",
    ]
    .into_iter()
    .find_map(|placeholder| {
        value
            .as_bytes()
            .get(index..index.saturating_add(placeholder.len()))
            .is_some_and(|candidate| candidate == placeholder.as_bytes())
            .then_some(placeholder.len())
    })
}

fn is_identity_boundary(byte: Option<u8>) -> bool {
    byte.is_none_or(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'-'))
}

pub(super) fn redact_free_form_ids(value: &str) -> String {
    let (bounded, was_truncated) = bounded_prefix(value, MAX_RAW_TEXT_BYTES);
    let mut output = String::with_capacity(bounded.len());
    let mut cursor = 0;
    let mut index = 0;
    while index < bounded.len() {
        if let Some(end) = uuid_like_id_end_at(bounded.as_bytes(), index) {
            output.push_str(&bounded[cursor..index]);
            output.push_str(REDACTED_ID);
            cursor = end;
            index = end;
            continue;
        }
        index += bounded[index..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
    }
    output.push_str(&bounded[cursor..]);
    if was_truncated {
        output.push_str(" [TRUNCATED]");
    }
    output
}

fn uuid_like_id_end_at(bytes: &[u8], index: usize) -> Option<usize> {
    const UUID_BYTES: usize = 36;
    let candidate = bytes.get(index..index.checked_add(UUID_BYTES)?)?;
    for (offset, byte) in candidate.iter().enumerate() {
        let valid = if matches!(offset, 8 | 13 | 18 | 23) {
            *byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        };
        if !valid {
            return None;
        }
    }
    Some(index + UUID_BYTES)
}

pub(super) fn contains_uuid_like_id(value: &str) -> bool {
    if value.len() > MAX_RAW_TEXT_BYTES {
        return true;
    }
    value
        .char_indices()
        .any(|(index, _)| uuid_like_id_end_at(value.as_bytes(), index).is_some())
}

fn redact_assignments(value: String) -> String {
    replace_secret_spans(value, assignment_value_span_at)
}

fn find_unescaped_quote(value: &str, start: usize, quote: char) -> Option<usize> {
    let mut escaped = false;
    for (relative, character) in value[start..].char_indices() {
        if character == quote && !escaped {
            return Some(start + relative);
        }
        escaped = character == '\\' && !escaped;
        if character != '\\' {
            escaped = false;
        }
    }
    None
}

fn replace_secret_spans(
    value: String,
    finder: fn(&str, usize) -> Option<(usize, usize)>,
) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    let mut index = 0;
    while index < value.len() {
        if let Some((start, end)) = finder(&value, index) {
            debug_assert!(start >= cursor && start < end && end <= value.len());
            output.push_str(&value[cursor..start]);
            output.push_str(REDACTED_SECRET);
            cursor = end;
            index = end;
            continue;
        }
        index += value[index..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
    }
    output.push_str(&value[cursor..]);
    output
}

fn assignment_value_span_at(value: &str, index: usize) -> Option<(usize, usize)> {
    for marker in ASSIGNMENT_MARKERS {
        if !starts_with_ascii_case_insensitive(value.as_bytes(), index, marker.as_bytes()) {
            continue;
        }
        let mut cursor = index + marker.len();
        if value[cursor..]
            .chars()
            .next()
            .is_some_and(|character| matches!(character, '"' | '\''))
        {
            cursor += 1;
        }
        cursor = skip_whitespace(value, cursor);
        if !value[cursor..]
            .chars()
            .next()
            .is_some_and(|character| matches!(character, ':' | '='))
        {
            continue;
        }
        cursor += 1;
        cursor = skip_whitespace(value, cursor);
        if value[cursor..].starts_with('>') {
            cursor += 1;
            cursor = skip_whitespace(value, cursor);
        }

        let quote = value[cursor..]
            .chars()
            .next()
            .filter(|character| matches!(character, '"' | '\''));
        let start = cursor + quote.map(char::len_utf8).unwrap_or_default();
        if start >= value.len() || starts_with_safe_redaction(value, start) {
            continue;
        }
        let end = if let Some(quote) = quote {
            find_unescaped_quote(value, start, quote).unwrap_or(value.len())
        } else {
            value[start..]
                .char_indices()
                .find(|(_, character)| {
                    matches!(
                        character,
                        ',' | ';' | '\r' | '\n' | '"' | '\'' | ')' | ']' | '}'
                    )
                })
                .map(|(relative, _)| start + relative)
                .unwrap_or(value.len())
        };
        if end > start {
            return Some((start, end));
        }
    }
    None
}

fn skip_whitespace(value: &str, mut index: usize) -> usize {
    while let Some(character) = value[index..].chars().next() {
        if !character.is_whitespace() {
            break;
        }
        index += character.len_utf8();
    }
    index
}

fn starts_with_safe_redaction(value: &str, index: usize) -> bool {
    value[index..].starts_with(REDACTED_SECRET) || value[index..].starts_with("[REDACTED]")
}

fn starts_with_ascii_case_insensitive(source: &[u8], index: usize, target: &[u8]) -> bool {
    source
        .get(index..index.saturating_add(target.len()))
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(target))
}

fn embedded_secret_span_at(value: &str, index: usize) -> Option<(usize, usize)> {
    let bytes = value.as_bytes();
    for prefix in TOKEN_PREFIXES {
        if starts_with_ascii_case_insensitive(bytes, index, prefix.as_bytes()) {
            let end = consume_ascii_credential(bytes, index + prefix.len());
            return Some((index, end.max(index + prefix.len())));
        }
    }
    if let Some(end) = jwt_end_at(bytes, index) {
        return Some((index, end));
    }
    auth_span_at(value, index)
}

fn consume_ascii_credential(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(byte, b'_' | b'-' | b'.' | b'~' | b'+' | b'/' | b'=')
    }) {
        index += 1;
    }
    index
}

fn jwt_end_at(bytes: &[u8], index: usize) -> Option<usize> {
    if bytes.get(index..index + 3)? != b"eyJ" {
        return None;
    }
    let mut cursor = index;
    let mut dots = 0;
    let mut part_start = index;
    while let Some(byte) = bytes.get(cursor) {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            cursor += 1;
        } else if *byte == b'.' && dots < 2 && cursor > part_start {
            dots += 1;
            cursor += 1;
            part_start = cursor;
        } else {
            break;
        }
    }
    (dots == 2 && cursor > part_start && cursor.saturating_sub(index) >= 24).then_some(cursor)
}

fn auth_span_at(value: &str, index: usize) -> Option<(usize, usize)> {
    for scheme in ["authorization", "bearer", "basic"] {
        if !starts_with_ascii_case_insensitive(value.as_bytes(), index, scheme.as_bytes()) {
            continue;
        }
        let mut cursor = index + scheme.len();
        let separator_start = cursor;
        cursor = skip_auth_separators(value, cursor)?;
        if cursor == separator_start {
            continue;
        }

        if scheme == "authorization" {
            for nested in ["bearer", "basic"] {
                if starts_with_ascii_case_insensitive(value.as_bytes(), cursor, nested.as_bytes()) {
                    let nested_end = cursor + nested.len();
                    let credential_start = skip_auth_separators(value, nested_end)?;
                    if credential_start == nested_end {
                        continue;
                    }
                    cursor = credential_start;
                    break;
                }
            }
        }
        if starts_with_safe_redaction(value, cursor) {
            continue;
        }
        let end = consume_ascii_credential(value.as_bytes(), cursor);
        if end > cursor {
            return Some((index, end));
        }
    }
    None
}

fn skip_auth_separators(value: &str, mut cursor: usize) -> Option<usize> {
    let start = cursor;
    while cursor < value.len() && cursor.saturating_sub(start) <= MAX_AUTH_SEPARATOR_BYTES {
        if starts_with_safe_redaction(value, cursor) {
            return None;
        }
        let byte = value.as_bytes()[cursor];
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'~' | b'+' | b'/') {
            break;
        }
        cursor += value[cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
    }
    (cursor.saturating_sub(start) <= MAX_AUTH_SEPARATOR_BYTES).then_some(cursor)
}

fn contains_private_key_marker(value: &str) -> bool {
    PRIVATE_KEY_MARKERS.iter().any(|marker| {
        (0..value.len()).any(|index| {
            starts_with_ascii_case_insensitive(value.as_bytes(), index, marker.as_bytes())
        })
    })
}

fn contains_credential_url(value: &str) -> bool {
    let bytes = value.as_bytes();
    for index in 0..value.len() {
        let Some(scheme) = ["https://", "http://", "wss://", "ws://"]
            .into_iter()
            .find(|scheme| starts_with_ascii_case_insensitive(bytes, index, scheme.as_bytes()))
        else {
            continue;
        };
        let authority_start = index + scheme.len();
        let authority_end = value[authority_start..]
            .char_indices()
            .find(|(_, character)| matches!(character, '/' | '?' | '#' | ' ' | '\t' | '\r' | '\n'))
            .map(|(relative, _)| authority_start + relative)
            .unwrap_or(value.len());
        if value[authority_start..authority_end].contains('@') {
            return true;
        }
    }
    false
}

pub(super) fn contains_secret_shape(value: &str) -> bool {
    if value.len() > MAX_RAW_TEXT_BYTES {
        return true;
    }
    if contains_private_key_marker(value) || contains_credential_url(value) {
        return true;
    }
    let mut index = 0;
    while index < value.len() {
        if assignment_value_span_at(value, index).is_some()
            || embedded_secret_span_at(value, index).is_some()
        {
            return true;
        }
        index += value[index..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
    }
    false
}

fn redact_embedded_secrets(value: String) -> String {
    if contains_private_key_marker(&value) {
        return REDACTED_SECRET.to_string();
    }
    replace_secret_spans(value, embedded_secret_span_at)
}

fn redact_urls(mut value: String) -> String {
    loop {
        let lower = value.to_ascii_lowercase();
        let start = [
            lower.find("https://"),
            lower.find("http://"),
            lower.find("wss://"),
            lower.find("ws://"),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(start) = start else {
            return value;
        };
        let end = value[start..]
            .char_indices()
            .find(|(_, character)| {
                character.is_ascii_whitespace()
                    || matches!(character, '"' | '\'' | ')' | ']' | '}' | ',' | ';')
            })
            .map(|(index, _)| start + index)
            .unwrap_or(value.len());
        value.replace_range(start..end, REDACTED_URL);
    }
}

fn redact_unknown_paths(value: String) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    let mut index = 0;
    while index < value.len() {
        if let Some(end) = absolute_path_end_at(&value, index) {
            output.push_str(&value[cursor..index]);
            output.push_str(REDACTED_PATH);
            cursor = end;
            index = end;
            continue;
        }
        index += value[index..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
    }
    output.push_str(&value[cursor..]);
    output
}

fn absolute_path_end_at(value: &str, index: usize) -> Option<usize> {
    if is_inside_web_url(value, index) || is_part_of_web_url_scheme(value, index) {
        return None;
    }
    let bytes = value.as_bytes();
    let windows_drive = index + 2 < bytes.len()
        && bytes[index].is_ascii_alphabetic()
        && bytes[index + 1] == b':'
        && matches!(bytes[index + 2], b'\\' | b'/');
    let unc = index + 1 < bytes.len()
        && matches!(
            (bytes[index], bytes[index + 1]),
            (b'\\', b'\\') | (b'/', b'/')
        );
    let unix_home = value[index..].starts_with("/Users/")
        || value[index..].starts_with("/home/")
        || value[index..].starts_with("/tmp/");
    if !(windows_drive || unc || unix_home) {
        return None;
    }
    if let Some(quote) = value[..index]
        .chars()
        .next_back()
        .filter(|character| matches!(character, '"' | '\''))
    {
        return Some(find_unescaped_quote(value, index, quote).unwrap_or(value.len()));
    }
    Some(
        value[index..]
            .char_indices()
            .skip(1)
            .find(|(_, character)| matches!(character, '\r' | '\n' | '\t' | '"' | '<' | '>' | '|'))
            .map(|(relative, _)| index + relative)
            .unwrap_or(value.len()),
    )
}

fn is_part_of_web_url_scheme(value: &str, index: usize) -> bool {
    let bytes = value.as_bytes();
    ["https://", "http://", "wss://", "ws://"]
        .iter()
        .any(|scheme| {
            let earliest = index.saturating_sub(scheme.len());
            (earliest..=index).any(|start| {
                starts_with_ascii_case_insensitive(bytes, start, scheme.as_bytes())
                    && index < start + scheme.len()
            })
        })
}

fn is_inside_web_url(value: &str, index: usize) -> bool {
    let token_start = value[..index]
        .char_indices()
        .rev()
        .find(|(_, character)| {
            character.is_whitespace()
                || matches!(
                    character,
                    '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
                )
        })
        .map(|(offset, character)| offset + character.len_utf8())
        .unwrap_or(0);
    let prefix = &value[token_start..index];
    ["https://", "http://", "wss://", "ws://"]
        .iter()
        .any(|scheme| {
            prefix
                .as_bytes()
                .windows(scheme.len())
                .any(|candidate| candidate.eq_ignore_ascii_case(scheme.as_bytes()))
        })
}

pub(super) fn contains_absolute_path(value: &str) -> bool {
    if value.len() > MAX_RAW_TEXT_BYTES {
        return true;
    }
    value
        .char_indices()
        .any(|(index, _)| absolute_path_end_at(value, index).is_some())
}

fn truncate_utf8(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }
    let suffix = "...[TRUNCATED]";
    let mut end = limit.saturating_sub(suffix.len()).min(value.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{suffix}", &value[..end])
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use serde_json::{json, Value};

    use crate::diagnostics::event::{
        DiagnosticEvent, DiagnosticEventKind, DiagnosticLevel, DIAGNOSTIC_SCHEMA_VERSION,
    };

    use super::{
        contains_absolute_path, contains_secret_shape, contains_uuid_like_id, DiagnosticSanitizer,
        SanitizerRoots, MAX_CONTEXT_FIELDS, MAX_RAW_TEXT_BYTES,
    };

    fn sanitizer() -> DiagnosticSanitizer {
        DiagnosticSanitizer::new(SanitizerRoots {
            user_profile: Some(PathBuf::from(r"C:\Users\Alice")),
            appdata: Some(PathBuf::from(r"C:\Users\Alice\AppData\Roaming")),
            codex_home: Some(PathBuf::from(r"D:\Codex Home")),
        })
    }

    fn event() -> DiagnosticEvent {
        DiagnosticEvent {
            schema_version: DIAGNOSTIC_SCHEMA_VERSION,
            event_id: "event-1".to_string(),
            session_id: "session-1".to_string(),
            sequence: 1,
            timestamp: 1,
            level: DiagnosticLevel::Error,
            component: "runtimeSwitch".to_string(),
            event_kind: DiagnosticEventKind::OperationTerminal,
            attempt_id: Some("attempt-1".to_string()),
            operation_id: None,
            action: Some("switchRuntime".to_string()),
            phase: Some("preflight".to_string()),
            terminal_status: None,
            error_code: Some("runtime.preflight".to_string()),
            safe_message: None,
            safe_context: BTreeMap::new(),
        }
    }

    #[test]
    fn redacts_credentials_urls_and_unknown_paths_before_persistence() {
        let sanitizer = sanitizer();
        let output = sanitizer.sanitize_text(
            r"api_key=sk-secret; Authorization: Bearer abcdef; https://user:pass@example.test/v1 C:\OtherUser\private.txt | D:\Codex Home\state.db",
        );

        assert!(!output.contains("sk-secret"), "{output}");
        assert!(!output.contains("abcdef"), "{output}");
        assert!(!output.contains("example.test"), "{output}");
        assert!(!output.contains("OtherUser"), "{output}");
        assert!(output.contains("%CODEX_HOME%"), "{output}");
    }

    #[test]
    fn sensitive_context_keys_are_redacted_and_context_is_bounded() {
        let mut raw = event();
        raw.safe_context
            .insert("apiKey".to_string(), Value::String("secret".to_string()));
        raw.safe_context
            .insert("credentialConfigured".to_string(), Value::Bool(true));
        raw.safe_context.insert(
            "db_password".to_string(),
            Value::String("db-pass".to_string()),
        );
        raw.safe_context.insert(
            "auth_token".to_string(),
            Value::String("ordinary-token-value".to_string()),
        );
        raw.safe_context.insert(
            "privateKey".to_string(),
            Value::String("ordinary-private-value".to_string()),
        );
        for index in 0..(MAX_CONTEXT_FIELDS + 10) {
            raw.safe_context
                .insert(format!("zfield{index:02}"), json!(index));
        }

        let safe = sanitizer().sanitize_event(&raw);

        assert_eq!(safe.safe_context["apiKey"], json!("[REDACTED_SECRET]"));
        assert_eq!(safe.safe_context["credentialConfigured"], json!(true));
        assert_eq!(safe.safe_context["db_password"], json!("[REDACTED_SECRET]"));
        assert_eq!(safe.safe_context["auth_token"], json!("[REDACTED_SECRET]"));
        assert_eq!(safe.safe_context["privateKey"], json!("[REDACTED_SECRET]"));
        assert!(safe.safe_context.len() <= MAX_CONTEXT_FIELDS);
    }

    #[test]
    fn sanitizer_is_idempotent_for_export_rechecks() {
        let mut raw = event();
        raw.safe_message = Some(r"failed at C:\Users\Alice\secret with token=abc".to_string());
        let sanitizer = sanitizer();

        let once = sanitizer.sanitize_event(&raw);
        let twice = sanitizer.sanitize_for_export(&once);

        assert_eq!(once, twice);
    }

    #[test]
    fn unicode_text_does_not_panic_while_redacting_unknown_paths() {
        let output = sanitizer().sanitize_text(r"切换失败：请查看 C:\OtherUser\private.txt");

        assert!(output.contains("切换失败"));
        assert!(!output.contains("OtherUser"));
        assert!(output.contains("[REDACTED_PATH]"));
    }

    #[test]
    fn quoted_and_json_assignments_are_redacted() {
        let output = sanitizer().sanitize_text(
            r#"token="secret-one" {"api_key":"secret-two"} Authorization Bearer secret-three"#,
        );

        assert!(!output.contains("secret-one"), "{output}");
        assert!(!output.contains("secret-two"), "{output}");
        assert!(!output.contains("secret-three"), "{output}");
    }

    #[test]
    fn embedded_github_and_openai_tokens_are_redacted_at_punctuation_boundaries() {
        let github_classic = format!("{}{}", "ghp_", "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789");
        let github_fine_grained = format!(
            "{}{}",
            "github_pat_", "abcdefghijklmnopqrstuvwxyz_0123456789"
        );
        let github_oauth = format!("{}{}", "gho_", "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789");
        let openai = format!("{}{}", "sk-proj-", "abcdefghijklmnopqrstuvwxyz012345");
        for (left, right) in [
            (":", ")"),
            ("=", "]"),
            ("(", "}"),
            ("[", ","),
            ("{", ";"),
            ("\"", "\""),
            ("'", "'"),
        ] {
            for secret in [
                &github_classic,
                &github_fine_grained,
                &github_oauth,
                &openai,
            ] {
                let input = format!("safe-before{left}{secret}{right}safe-after");
                let output = sanitizer().sanitize_text(&input);
                assert!(!output.contains(secret), "{output}");
                assert!(output.contains("safe-before"), "{output}");
                assert!(output.contains("safe-after"), "{output}");
            }
        }
    }

    #[test]
    fn embedded_jwt_is_redacted_without_whitespace_token_boundaries() {
        let jwt = concat!(
            "eyJhbGciOiJIUzI1NiJ9.",
            "eyJzdWIiOiIxMjM0NTY3ODkwIn0.",
            "c2lnbmF0dXJlYWJjZGVm"
        );
        for input in [
            format!("safe:cause:{jwt};after"),
            format!("safe=(cause:{jwt}) after"),
            serde_json::json!({"detail": format!("cause:{jwt}")}).to_string(),
        ] {
            let output = sanitizer().sanitize_text(&input);
            assert!(!output.contains(jwt), "{output}");
            assert!(output.contains("[REDACTED_SECRET]"), "{output}");
        }
    }

    #[test]
    fn basic_bearer_and_authorization_credentials_are_redacted_when_embedded() {
        let basic = "QWxhZGRpbjpvcGVuU2VzYW1l";
        let bearer = "abcdefghijklmnop0123456789";
        for input in [
            format!("auth failed:Basic {basic}; retry-safe"),
            format!("Authorization:Basic {basic}"),
            format!("authorization = basic {basic}"),
            format!("prefix(Bearer:{bearer})suffix"),
            format!("prefix Authorization Bearer {bearer}; suffix"),
        ] {
            let output = sanitizer().sanitize_text(&input);
            assert!(!output.contains(basic), "{output}");
            assert!(!output.contains(bearer), "{output}");
            assert!(output.contains("[REDACTED_SECRET]"), "{output}");
        }
    }

    #[test]
    fn assignment_case_quotes_spacing_and_arrow_delimiters_are_redacted() {
        let secret = "abcDEF0123456789";
        for input in [
            format!("token : '{secret}' safe"),
            format!("\"api-key\" = \"{secret}\" safe"),
            format!("COOKIE: session={secret}; safe"),
            format!("client_secret=>{secret}, safe"),
        ] {
            let output = sanitizer().sanitize_text(&input);
            assert!(!output.contains(secret), "{output}");
            assert!(output.contains("[REDACTED_SECRET]"), "{output}");
        }
    }

    #[test]
    fn credential_urls_and_websocket_urls_are_fully_tokenized() {
        for input in [
            "https://user:pass@example.test/v1",
            "http://token@example.test/x",
            "https://example.test/?access_token=abcDEF0123456789#token=secret",
            "wss://user:pass@example.test/socket",
            "ws://token@example.test/socket",
        ] {
            let output = sanitizer().sanitize_text(&format!("before {input} after"));
            assert!(!output.contains("example.test"), "{output}");
            assert!(!output.contains("abcDEF0123456789"), "{output}");
            assert!(output.contains("[REDACTED_URL]"), "{output}");
        }
    }

    #[test]
    fn identities_and_known_or_unknown_absolute_paths_are_redacted() {
        let mut sanitizer = sanitizer();
        sanitizer.identity_replacements = vec!["Alice".into(), "Al".into(), "PC".into()];
        let output = sanitizer.sanitize_text(concat!(
            "user=Al machine=PC ",
            "C:\\Users\\Alice\\AppData\\Local\\x ",
            "C:/Users/Alice/AppData/x ",
            "\\\\server\\share\\Alice\\x ",
            "\\\\?\\C:\\Users\\Alice\\x ",
            "c:\\uSeRs\\aLiCe\\mixed"
        ));

        for forbidden in ["user=Al", "machine=PC", "Alice", "aLiCe", "server", "share"] {
            assert!(!output.contains(forbidden), "{output}");
        }
        assert!(output.contains("[REDACTED_IDENTITY]"), "{output}");
        assert!(output.contains("%USERPROFILE%"), "{output}");
        assert!(output.contains("[REDACTED_PATH]"), "{output}");

        let github = format!("cause:{}{}", "ghp_", "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789");
        let jwt = concat!(
            "cause:eyJhbGciOiJIUzI1NiJ9.",
            "eyJzdWIiOiIxMjM0NTY3ODkwIn0.",
            "c2lnbmF0dXJlYWJjZGVm"
        );
        let mut raw = event();
        raw.safe_message = Some(format!("user=Al machine=PC {github}"));
        raw.safe_context
            .insert("detail".to_string(), Value::String(jwt.to_string()));
        let safe = sanitizer.sanitize_event(&raw);
        let persisted = serde_json::to_string(&safe).unwrap();
        for forbidden in ["user=Al", "machine=PC", github.as_str(), jwt] {
            assert!(!persisted.contains(forbidden), "{persisted}");
        }
    }

    #[test]
    fn embedded_secret_redaction_is_idempotent_and_unicode_safe() {
        let input = concat!(
            "切换失败（cause:eyJhbGciOiJIUzI1NiJ9.",
            "eyJzdWIiOiIxMjM0NTY3ODkwIn0.",
            "c2lnbmF0dXJlYWJjZGVm）；",
            "auth=Basic QWxhZGRpbjpvcGVuU2VzYW1l；请重试"
        );
        let mut sanitizer = sanitizer();
        sanitizer.identity_replacements = vec!["REDACTED".into(), "IDENTITY".into(), "HOME".into()];
        let once = sanitizer.sanitize_text(input);
        let twice = sanitizer.sanitize_text(&once);

        assert_eq!(once, twice);
        assert!(once.contains("切换失败"), "{once}");
        assert!(once.contains("请重试"), "{once}");
    }

    #[test]
    fn free_form_uuid_like_business_ids_are_redacted_but_structured_ids_remain() {
        let mut raw = event();
        raw.event_id = "123e4567-e89b-12d3-a456-426614174000".into();
        raw.session_id = "123e4567-e89b-12d3-a456-426614174001".into();
        raw.attempt_id = Some("123e4567-e89b-12d3-a456-426614174002".into());
        raw.operation_id = Some("123e4567-e89b-12d3-a456-426614174003".into());
        let expected = "123e4567-e89b-12d3-a456-426614174004";
        let actual = "123e4567-e89b-12d3-a456-426614174005";
        raw.safe_message = Some(format!(
            "source session JSONL id changed from {expected} to {actual}"
        ));
        raw.safe_context.insert(
            "detail".into(),
            Value::String(format!("provider mismatch ({expected} != {actual})")),
        );
        raw.safe_context.insert(
            "nested".into(),
            json!({"items": [expected, {"sessionId": actual}]}),
        );

        let safe = sanitizer().sanitize_event(&raw);
        assert_eq!(safe.event_id, raw.event_id);
        assert_eq!(safe.session_id, raw.session_id);
        assert_eq!(safe.attempt_id, raw.attempt_id);
        assert_eq!(safe.operation_id, raw.operation_id);
        let message = safe.safe_message.unwrap();
        assert!(message.contains("source session JSONL id changed from"));
        assert_eq!(message.matches("[REDACTED_ID]").count(), 2);
        let detail = safe.safe_context["detail"].as_str().unwrap();
        assert!(detail.contains("provider mismatch"));
        assert_eq!(detail.matches("[REDACTED_ID]").count(), 2);
        assert!(!message.contains(expected));
        assert!(!detail.contains(actual));
        let nested = serde_json::to_string(&safe.safe_context["nested"]).unwrap();
        assert!(!nested.contains(expected), "{nested}");
        assert!(!nested.contains(actual), "{nested}");
        assert_eq!(nested.matches("[REDACTED_ID]").count(), 2);
        assert_eq!(sanitizer().sanitize_text(&message), message);
        assert!(contains_uuid_like_id(expected));
        assert!(!contains_uuid_like_id(&message));
    }

    #[test]
    fn forward_unc_device_paths_and_unknown_roots_are_redacted_without_url_false_positives() {
        let sanitizer = sanitizer();
        let input = concat!(
            "失败路径 //server/share/Alice/x | ",
            "设备路径 //?/C:/Users/Alice/private.txt | ",
            "相似用户 C:\\Users\\AliceOther\\state.db | ",
            "合法链接 https://example.test/Users/Alice/docs 和 wss://example.test/socket"
        );
        let once = sanitizer.sanitize_text(input);
        let twice = sanitizer.sanitize_text(&once);

        assert_eq!(once, twice);
        for forbidden in ["server", "share", "AliceOther", "private.txt", "state.db"] {
            assert!(!once.contains(forbidden), "{once}");
        }
        assert!(!once.contains("%USERPROFILE%Other"), "{once}");
        assert!(once.contains("[REDACTED_PATH]"), "{once}");
        assert!(once.contains("[REDACTED_URL]"), "{once}");

        for path in [
            "//server/share/Alice/x",
            "//?/C:/Users/Alice/x",
            r"\\server\share\Alice\x",
            r"C:\Other\state.db",
            "/home/alice/state.db",
        ] {
            assert!(contains_absolute_path(path), "{path}");
        }
        for url in [
            "https://example.test/Users/Alice/docs",
            "http://example.test/home/alice",
            "wss://example.test/socket",
            "ws://example.test/tmp/cache",
        ] {
            assert!(!contains_absolute_path(url), "{url}");
        }

        for path in [
            r"D:\Other User\private.txt",
            "//server/share/My Documents/private.txt",
            r"D:\Other\a%b\secret.db",
            r"D:\Other\[draft]\secret.db",
            r"D:\Other\customer,2026.db",
            r"D:\Other\customer;2026.db",
            r"D:\Other\customer)2026.db",
            r"D:\Other\O'Brien\file.db",
            r"C:\Users\Alice Other\private.txt",
            r"C:\Users\Alice,Other\private.txt",
        ] {
            let input = format!("open failed: {path} | retry remains visible");
            let output = sanitizer.sanitize_text(&input);
            assert!(!output.contains(path), "{output}");
            assert!(!output.contains("private.txt"), "{output}");
            assert!(!output.contains("secret.db"), "{output}");
            assert!(!output.contains("customer"), "{output}");
            assert!(!output.contains("Brien"), "{output}");
            assert!(!output.contains("file.db"), "{output}");
            assert!(output.contains("retry remains visible"), "{output}");
            assert_eq!(sanitizer.sanitize_text(&output), output);
        }

        let quoted = sanitizer.sanitize_text(
            r#"open failed: "D:\Other User\[draft],2026);a%b\private.txt" | retry visible"#,
        );
        assert!(!quoted.contains("Other User"), "{quoted}");
        assert!(!quoted.contains("private.txt"), "{quoted}");
        assert!(quoted.contains("retry visible"), "{quoted}");

        let exact_root = sanitizer.sanitize_text(r"C:\Users\Alice");
        let root_child = sanitizer.sanitize_text(r"C:\Users\Alice\child.txt");
        assert_eq!(exact_root, "%USERPROFILE%");
        assert_eq!(root_child, r"%USERPROFILE%\child.txt");
        for collision in [
            r"C:\Users\Alice Other\private.txt",
            r"C:\Users\Alice,Other\private.txt",
        ] {
            let output = sanitizer.sanitize_text(&format!("failed: {collision} | retry"));
            assert!(output.starts_with("failed: [REDACTED_PATH]"), "{output}");
            assert!(output.contains("retry"), "{output}");
            assert!(!output.contains("%USERPROFILE%"));
        }
    }

    #[test]
    fn shared_final_scan_detects_embedded_shapes_and_allows_safe_placeholders() {
        let github = format!("cause:{}{}", "ghp_", "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789");
        let jwt = concat!(
            "cause:eyJhbGciOiJIUzI1NiJ9.",
            "eyJzdWIiOiIxMjM0NTY3ODkwIn0.",
            "c2lnbmF0dXJlYWJjZGVm"
        );
        for value in [
            github.as_str(),
            jwt,
            "auth failed:Basic QWxhZGRpbjpvcGVuU2VzYW1l",
            "Authorization:Basic QWxhZGRpbjpvcGVuU2VzYW1l",
            "token : 'abcDEF0123456789'",
            "wss://user:pass@example.test/socket",
        ] {
            assert!(contains_secret_shape(value), "{value}");
        }
        for value in [
            "Authorization: [REDACTED_SECRET]",
            "Bearer [REDACTED_SECRET]",
            "token=[REDACTED_SECRET]",
            "ordinary diagnostic message",
        ] {
            assert!(!contains_secret_shape(value), "{value}");
        }
        assert!(contains_secret_shape(&"x".repeat(MAX_RAW_TEXT_BYTES + 1)));
    }

    #[test]
    fn sanitizer_bounds_work_before_scanning_untrusted_text() {
        let input = format!("{} token=never-reaches-disk", "x".repeat(1024 * 1024));
        let output = sanitizer().sanitize_text(&input);

        assert!(output.len() <= super::MAX_SAFE_STRING_BYTES);
        assert!(!output.contains("never-reaches-disk"));
        assert!(output.contains("TRUNCATED"));
    }
}
