mod event;
pub mod export;
mod lifecycle;
pub mod platform;
mod sanitize;
mod store;

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use serde_json::Value;

pub use event::{
    new_diagnostic_id, DiagnosticEvent, DiagnosticEventInput, DiagnosticEventKind, DiagnosticLevel,
    DiagnosticTerminalStatus, SafeContext, DIAGNOSTIC_SCHEMA_VERSION,
};
pub use lifecycle::{
    global_runtime, initialize_global, initialize_global_from_environment, install_panic_hook,
    DiagnosticLifecycle, DiagnosticRuntime,
};
pub use sanitize::{
    DiagnosticSanitizer, SanitizerRoots, MAX_CONTEXT_FIELDS, MAX_SAFE_MESSAGE_BYTES,
    MAX_SAFE_STRING_BYTES,
};
pub use store::{
    DiagnosticStore, DiagnosticStoreConfig, DiagnosticStoreStatus, DEFAULT_MAX_SEGMENT_BYTES,
    DEFAULT_MAX_TOTAL_BYTES, DEFAULT_RETENTION,
};

use event::timestamp_millis;

#[derive(Debug, Clone)]
pub struct DiagnosticRecorder {
    inner: Arc<RecorderInner>,
}

#[derive(Debug)]
struct RecorderInner {
    store: DiagnosticStore,
    session_id: String,
    sequence: AtomicU64,
}

impl DiagnosticRecorder {
    pub fn new(store: DiagnosticStore, session_id: String) -> Self {
        Self {
            inner: Arc::new(RecorderInner {
                store,
                session_id,
                sequence: AtomicU64::new(0),
            }),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.inner.session_id
    }

    pub fn store(&self) -> &DiagnosticStore {
        &self.inner.store
    }

    pub fn record(&self, input: DiagnosticEventInput) -> bool {
        let event = self.build_event(input);
        self.inner.store.try_append_best_effort(&event)
    }

    pub fn begin_operation(
        &self,
        component: impl Into<String>,
        action: impl Into<String>,
    ) -> DiagnosticOperation {
        let operation = DiagnosticOperation {
            recorder: self.clone(),
            component: component.into(),
            action: action.into(),
            state: Arc::new(Mutex::new(DiagnosticOperationState {
                attempt_id: new_diagnostic_id("attempt"),
                operation_id: None,
                binding_recorded: false,
                terminal_recorded: false,
            })),
        };
        let state = operation.lock_state();
        let attempt_id = state.attempt_id.clone();
        drop(state);
        let _ = operation.recorder.record(
            DiagnosticEventInput::new(
                DiagnosticLevel::Info,
                operation.component.clone(),
                DiagnosticEventKind::OperationStarted,
            )
            .with_attempt_id(attempt_id)
            .with_action(operation.action.clone()),
        );
        operation
    }

    pub fn record_panic_best_effort(
        &self,
        safe_message: impl Into<String>,
        location: Option<&str>,
    ) -> bool {
        let mut context = BTreeMap::new();
        if let Some(location) = location {
            context.insert("location".to_string(), Value::String(location.to_string()));
        }
        let event = self.build_event(
            DiagnosticEventInput::new(DiagnosticLevel::Error, "rust", DiagnosticEventKind::Panic)
                .with_error("rust.panic", safe_message)
                .with_context(context),
        );
        self.inner.store.try_append_best_effort(&event)
    }

    fn build_event(&self, input: DiagnosticEventInput) -> DiagnosticEvent {
        let sequence = self
            .inner
            .sequence
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        DiagnosticEvent {
            schema_version: DIAGNOSTIC_SCHEMA_VERSION,
            event_id: format!("{}-{sequence}", self.inner.session_id),
            session_id: self.inner.session_id.clone(),
            sequence,
            timestamp: timestamp_millis(),
            level: input.level,
            component: input.component,
            event_kind: input.event_kind,
            attempt_id: input.attempt_id,
            operation_id: input.operation_id,
            action: input.action,
            phase: input.phase,
            terminal_status: input.terminal_status,
            error_code: input.error_code,
            safe_message: input.safe_message,
            safe_context: input.safe_context,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiagnosticOperation {
    recorder: DiagnosticRecorder,
    component: String,
    action: String,
    state: Arc<Mutex<DiagnosticOperationState>>,
}

#[derive(Debug)]
struct DiagnosticOperationState {
    attempt_id: String,
    operation_id: Option<String>,
    binding_recorded: bool,
    terminal_recorded: bool,
}

impl DiagnosticOperation {
    pub fn attempt_id(&self) -> String {
        self.lock_state().attempt_id.clone()
    }

    pub fn operation_id(&self) -> Option<String> {
        self.lock_state().operation_id.clone()
    }

    pub fn bind_operation_id(&self, operation_id: impl Into<String>) -> bool {
        let operation_id = operation_id.into();
        if operation_id.trim().is_empty() {
            return false;
        }
        let mut state = self.lock_state();
        if state.terminal_recorded {
            return false;
        }
        if let Some(existing) = state.operation_id.as_deref() {
            if existing != operation_id {
                return false;
            }
            if state.binding_recorded {
                return true;
            }
        } else {
            state.operation_id = Some(operation_id.clone());
        }
        let attempt_id = state.attempt_id.clone();
        let recorded = self.recorder.record(
            DiagnosticEventInput::new(
                DiagnosticLevel::Info,
                self.component.clone(),
                DiagnosticEventKind::OperationBound,
            )
            .with_attempt_id(attempt_id)
            .with_operation_id(operation_id)
            .with_action(self.action.clone()),
        );
        if recorded {
            state.binding_recorded = true;
        }
        recorded
    }

    pub fn phase(&self, phase: impl Into<String>, safe_context: SafeContext) -> bool {
        let phase = phase.into();
        let state = self.lock_state();
        if state.terminal_recorded {
            return false;
        }
        let input = self.base_input(
            &state,
            DiagnosticLevel::Info,
            DiagnosticEventKind::OperationPhase,
        );
        self.recorder
            .record(input.with_phase(phase).with_context(safe_context))
    }

    pub fn branch(
        &self,
        level: DiagnosticLevel,
        phase: Option<&str>,
        error_code: Option<&str>,
        safe_message: Option<&str>,
        safe_context: SafeContext,
    ) -> bool {
        let state = self.lock_state();
        if state.terminal_recorded {
            return false;
        }
        let mut input = self.base_input(&state, level, DiagnosticEventKind::OperationBranch);
        if let Some(phase) = phase {
            input = input.with_phase(phase);
        }
        if let Some(error_code) = error_code {
            input.error_code = Some(error_code.to_string());
        }
        if let Some(safe_message) = safe_message {
            input.safe_message = Some(safe_message.to_string());
        }
        self.recorder.record(input.with_context(safe_context))
    }

    pub fn terminal(
        &self,
        status: DiagnosticTerminalStatus,
        phase: Option<&str>,
        error_code: Option<&str>,
        safe_message: Option<&str>,
        safe_context: SafeContext,
    ) -> bool {
        let mut state = self.lock_state();
        if state.terminal_recorded {
            return false;
        }
        let level = match status {
            DiagnosticTerminalStatus::Succeeded => DiagnosticLevel::Info,
            DiagnosticTerminalStatus::Partial
            | DiagnosticTerminalStatus::Blocked
            | DiagnosticTerminalStatus::Cancelled
            | DiagnosticTerminalStatus::RolledBack
            | DiagnosticTerminalStatus::Unknown => DiagnosticLevel::Warning,
            DiagnosticTerminalStatus::Failed | DiagnosticTerminalStatus::RollbackFailed => {
                DiagnosticLevel::Error
            }
        };
        let mut input = self.base_input(&state, level, DiagnosticEventKind::OperationTerminal);
        input.terminal_status = Some(status);
        if let Some(phase) = phase {
            input.phase = Some(phase.to_string());
        }
        if let Some(error_code) = error_code {
            input.error_code = Some(error_code.to_string());
        }
        if let Some(safe_message) = safe_message {
            input.safe_message = Some(safe_message.to_string());
        }
        input.safe_context = safe_context;
        let recorded = self.recorder.record(input);
        if recorded {
            state.terminal_recorded = true;
        }
        recorded
    }

    pub fn is_terminal_recorded(&self) -> bool {
        self.lock_state().terminal_recorded
    }

    fn base_input(
        &self,
        state: &DiagnosticOperationState,
        level: DiagnosticLevel,
        event_kind: DiagnosticEventKind,
    ) -> DiagnosticEventInput {
        let mut input = DiagnosticEventInput::new(level, self.component.clone(), event_kind)
            .with_attempt_id(state.attempt_id.clone())
            .with_action(self.action.clone());
        if let Some(operation_id) = state.operation_id.as_ref() {
            input = input.with_operation_id(operation_id.clone());
        }
        input
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, DiagnosticOperationState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub fn empty_context() -> SafeContext {
    BTreeMap::new()
}

pub fn diagnostics_root(appdata: &Path) -> std::path::PathBuf {
    appdata.join("codex-switch/logs/diagnostics")
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    #[cfg(windows)]
    use std::{sync::mpsc, thread, time::Duration};

    use tempfile::tempdir;

    #[cfg(windows)]
    use super::DiagnosticEventInput;

    use super::{
        DiagnosticEventKind, DiagnosticLevel, DiagnosticOperation, DiagnosticRecorder,
        DiagnosticSanitizer, DiagnosticStore, DiagnosticTerminalStatus, SanitizerRoots,
    };

    fn recorder(root: &std::path::Path) -> DiagnosticRecorder {
        let store = DiagnosticStore::new(
            root.to_path_buf(),
            "session-test".to_string(),
            DiagnosticSanitizer::new(SanitizerRoots::default()),
        );
        DiagnosticRecorder::new(store, "session-test".to_string())
    }

    #[test]
    fn operation_binds_attempt_to_durable_operation_and_records_terminal_once() {
        let temp = tempdir().unwrap();
        let recorder = recorder(temp.path());
        let operation = recorder.begin_operation("runtime", "switchRuntime");

        assert!(operation.bind_operation_id("switch-runtime-1"));
        assert!(operation.phase("preflight", BTreeMap::new()));
        assert!(operation.terminal(
            DiagnosticTerminalStatus::Failed,
            Some("preflight"),
            Some("runtime.preflight"),
            Some("preflight failed"),
            BTreeMap::new(),
        ));
        assert!(!operation.terminal(
            DiagnosticTerminalStatus::Succeeded,
            Some("complete"),
            None,
            None,
            BTreeMap::new(),
        ));

        let events = recorder.store().read_events().unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].event_kind, DiagnosticEventKind::OperationStarted);
        assert_eq!(events[1].event_kind, DiagnosticEventKind::OperationBound);
        assert_eq!(events[3].operation_id.as_deref(), Some("switch-runtime-1"));
        assert!(operation.is_terminal_recorded());
        assert!(!operation.phase("tooLate", BTreeMap::new()));
        assert!(!operation.branch(
            DiagnosticLevel::Warning,
            Some("tooLate"),
            None,
            None,
            BTreeMap::new(),
        ));
        assert!(!operation.bind_operation_id("switch-runtime-1"));
        assert_eq!(recorder.store().read_events().unwrap().len(), 4);
    }

    #[test]
    fn recorder_failures_are_visible_as_false_without_changing_control_flow() {
        let temp = tempdir().unwrap();
        let invalid_root = temp.path().join("occupied");
        fs::write(&invalid_root, b"file").unwrap();
        let recorder = recorder(&invalid_root);

        let operation: DiagnosticOperation = recorder.begin_operation("runtime", "switchRuntime");

        assert!(!operation.phase("preflight", BTreeMap::new()));
        assert!(!operation.terminal(
            DiagnosticTerminalStatus::Failed,
            Some("preflight"),
            Some("runtime.preflight"),
            Some("failed"),
            BTreeMap::new(),
        ));
        assert!(!operation.is_terminal_recorded());
    }

    #[cfg(windows)]
    #[test]
    fn ordinary_recording_is_nonblocking_while_the_root_mutex_is_busy() {
        let temp = tempdir().unwrap();
        let recorder = recorder(temp.path());
        let operation = recorder.begin_operation("runtime", "switchRuntime");
        let lock_store = recorder.store().clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let holder = thread::spawn(move || {
            lock_store
                .with_root_lock_for_test(|| {
                    ready_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                })
                .unwrap();
        });
        ready_rx.recv().unwrap();

        let contender_recorder = recorder.clone();
        let contender_operation = operation.clone();
        let (result_tx, result_rx) = mpsc::channel();
        let contender = thread::spawn(move || {
            let direct = contender_recorder.record(DiagnosticEventInput::new(
                DiagnosticLevel::Info,
                "business",
                DiagnosticEventKind::OperationPhase,
            ));
            let phase = contender_operation.phase("apply", BTreeMap::new());
            let terminal = contender_operation.terminal(
                DiagnosticTerminalStatus::Succeeded,
                Some("complete"),
                None,
                None,
                BTreeMap::new(),
            );
            result_tx
                .send((
                    direct,
                    phase,
                    terminal,
                    contender_operation.is_terminal_recorded(),
                    "business-result-unchanged",
                ))
                .unwrap();
        });
        let outcome = result_rx.recv_timeout(Duration::from_secs(1));
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        contender.join().unwrap();
        let (direct, phase, terminal, terminal_recorded, business_result) =
            outcome.expect("ordinary diagnostic recording waited for the busy root mutex");

        assert!(!direct);
        assert!(!phase);
        assert!(!terminal);
        assert!(!terminal_recorded);
        assert_eq!(business_result, "business-result-unchanged");
        assert!(operation.terminal(
            DiagnosticTerminalStatus::Succeeded,
            Some("complete"),
            None,
            None,
            BTreeMap::new(),
        ));
        assert!(operation.is_terminal_recorded());
    }

    #[test]
    fn branch_context_is_sanitized_by_the_store() {
        let temp = tempdir().unwrap();
        let recorder = recorder(temp.path());
        let operation = recorder.begin_operation("relay", "verifyRelay");
        let context = BTreeMap::from([(
            "apiKey".to_string(),
            serde_json::Value::String("sk-secret".to_string()),
        )]);

        assert!(operation.branch(
            DiagnosticLevel::Error,
            Some("verify"),
            Some("relay.http"),
            Some("request failed"),
            context,
        ));

        let events = recorder.store().read_events().unwrap();
        assert_eq!(
            events.last().unwrap().safe_context["apiKey"],
            serde_json::json!("[REDACTED_SECRET]")
        );
    }

    #[test]
    fn a_failed_operation_binding_can_be_retried_after_storage_recovers() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("occupied");
        fs::write(&root, b"file").unwrap();
        let recorder = recorder(&root);
        let operation = recorder.begin_operation("runtime", "switchRuntime");

        assert!(!operation.bind_operation_id("switch-runtime-retry"));
        fs::remove_file(&root).unwrap();
        fs::create_dir(&root).unwrap();
        assert!(operation.bind_operation_id("switch-runtime-retry"));

        let events = recorder.store().read_events().unwrap();
        let attempt_id = operation.attempt_id();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_kind, DiagnosticEventKind::OperationBound);
        assert_eq!(events[0].attempt_id.as_deref(), Some(attempt_id.as_str()));
    }
}
