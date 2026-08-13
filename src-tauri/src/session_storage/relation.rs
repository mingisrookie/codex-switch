use super::{model::SessionRelation, semantic::SemanticSession};

pub fn compare_sessions(left: &SemanticSession, right: &SemanticSession) -> SessionRelation {
    if left.thread_id != right.thread_id {
        return SessionRelation::Unknown;
    }
    if left.raw_sha256 == right.raw_sha256 {
        return SessionRelation::Equal;
    }
    if left.normalized_line_sha256 == right.normalized_line_sha256 {
        return SessionRelation::EqualExceptProvider;
    }
    if is_prefix(&left.normalized_line_sha256, &right.normalized_line_sha256) {
        return SessionRelation::LeftPrefix;
    }
    if is_prefix(&right.normalized_line_sha256, &left.normalized_line_sha256) {
        return SessionRelation::RightPrefix;
    }
    SessionRelation::Divergent
}

fn is_prefix<T: PartialEq>(prefix: &[T], value: &[T]) -> bool {
    prefix.len() < value.len() && value.starts_with(prefix)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::compare_sessions;
    use crate::session_storage::{model::SessionRelation, semantic::read_semantic_session};

    fn session(
        root: &std::path::Path,
        name: &str,
        provider: &str,
        tail: &[&str],
    ) -> std::path::PathBuf {
        let path = root.join(name);
        let mut lines = vec![format!(
            r#"{{"type":"session_meta","payload":{{"id":"thread-a","model_provider":"{provider}"}}}}"#
        )];
        lines.extend(tail.iter().map(|message| {
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "user_message", "message": message}
            })
            .to_string()
        }));
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    #[test]
    fn classifies_equal_provider_only_prefix_and_divergence() {
        let root = tempdir().unwrap();
        let base =
            read_semantic_session(&session(root.path(), "base", "openai", &["one"])).unwrap();
        let same =
            read_semantic_session(&session(root.path(), "same", "openai", &["one"])).unwrap();
        let provider =
            read_semantic_session(&session(root.path(), "provider", "openai_custom", &["one"]))
                .unwrap();
        let extended = read_semantic_session(&session(
            root.path(),
            "extended",
            "openai_custom",
            &["one", "two"],
        ))
        .unwrap();
        let divergent =
            read_semantic_session(&session(root.path(), "divergent", "openai", &["other"]))
                .unwrap();

        assert_eq!(compare_sessions(&base, &same), SessionRelation::Equal);
        assert_eq!(
            compare_sessions(&base, &provider),
            SessionRelation::EqualExceptProvider
        );
        assert_eq!(
            compare_sessions(&base, &extended),
            SessionRelation::LeftPrefix
        );
        assert_eq!(
            compare_sessions(&extended, &base),
            SessionRelation::RightPrefix
        );
        assert_eq!(
            compare_sessions(&base, &divergent),
            SessionRelation::Divergent
        );
    }

    #[test]
    fn message_order_changes_are_divergent() {
        let root = tempdir().unwrap();
        let ordered =
            read_semantic_session(&session(root.path(), "ordered", "openai", &["one", "two"]))
                .unwrap();
        let reversed =
            read_semantic_session(&session(root.path(), "reversed", "openai", &["two", "one"]))
                .unwrap();

        assert_eq!(
            compare_sessions(&ordered, &reversed),
            SessionRelation::Divergent
        );
    }
}
