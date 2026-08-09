use std::{
    collections::{BTreeMap, HashSet},
    env,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    },
};

use serde_json::{json, Value};

use super::{
    diagnostics_root,
    event::{new_diagnostic_id, DiagnosticEventInput, DiagnosticEventKind, DiagnosticLevel},
    sanitize::DiagnosticSanitizer,
    store::DiagnosticStore,
    DiagnosticRecorder,
};

static GLOBAL_RUNTIME: OnceLock<DiagnosticRuntime> = OnceLock::new();

#[derive(Debug)]
pub struct DiagnosticRuntime {
    store: DiagnosticStore,
    recorder: DiagnosticRecorder,
    lifecycle: DiagnosticLifecycle,
}

impl DiagnosticRuntime {
    pub fn new(appdata: &Path) -> Self {
        let session_id = new_diagnostic_id("session");
        let sanitizer = DiagnosticSanitizer::from_environment_with_appdata(appdata.to_path_buf());
        let store = DiagnosticStore::new(diagnostics_root(appdata), session_id.clone(), sanitizer);
        let recorder = DiagnosticRecorder::new(store.clone(), session_id);
        let lifecycle = DiagnosticLifecycle::start(recorder.clone());
        Self {
            store,
            recorder,
            lifecycle,
        }
    }

    pub fn store(&self) -> &DiagnosticStore {
        &self.store
    }

    pub fn recorder(&self) -> DiagnosticRecorder {
        self.recorder.clone()
    }

    pub fn lifecycle(&self) -> &DiagnosticLifecycle {
        &self.lifecycle
    }

    pub fn session_id(&self) -> &str {
        self.recorder.session_id()
    }
}

#[derive(Debug)]
pub struct DiagnosticLifecycle {
    recorder: DiagnosticRecorder,
    ready_recorded: AtomicBool,
    ended: AtomicBool,
}

impl DiagnosticLifecycle {
    fn start(recorder: DiagnosticRecorder) -> Self {
        let previous_unclean = recorder
            .store()
            .read_events()
            .ok()
            .and_then(|events| latest_unclean_session(&events));
        let mut start_context = BTreeMap::from([
            ("processId".to_string(), json!(std::process::id())),
            ("timestampUnit".to_string(), json!("unixEpochMilliseconds")),
        ]);
        if let Some(process_started_at) = current_process_started_at_100ns() {
            start_context.insert(
                "processStartedAt100ns".to_string(),
                Value::String(process_started_at.to_string()),
            );
        }
        let _ = recorder.record(
            DiagnosticEventInput::new(
                DiagnosticLevel::Info,
                "lifecycle",
                DiagnosticEventKind::SessionStarted,
            )
            .with_context(start_context),
        );
        if let Some(previous) = previous_unclean {
            let _ = recorder.record(
                DiagnosticEventInput::new(
                    DiagnosticLevel::Warning,
                    "lifecycle",
                    DiagnosticEventKind::PreviousSessionUnclean,
                )
                .with_error(
                    "lifecycle.previousSessionUnclean",
                    "the previous diagnostic session ended without a clean terminal",
                )
                .with_context(BTreeMap::from([(
                    "previousSessionId".to_string(),
                    Value::String(previous),
                )])),
            );
        }
        Self {
            recorder,
            ready_recorded: AtomicBool::new(false),
            ended: AtomicBool::new(false),
        }
    }

    pub fn mark_ready(&self) -> bool {
        if self
            .ready_recorded
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        let recorded = self.recorder.record(DiagnosticEventInput::new(
            DiagnosticLevel::Info,
            "lifecycle",
            DiagnosticEventKind::AppReady,
        ));
        if !recorded {
            self.ready_recorded.store(false, Ordering::Release);
        }
        recorded
    }

    pub fn record_startup_failure(
        &self,
        error_code: impl Into<String>,
        safe_message: impl Into<String>,
    ) -> bool {
        self.recorder.record(
            DiagnosticEventInput::new(
                DiagnosticLevel::Error,
                "lifecycle",
                DiagnosticEventKind::StartupFailure,
            )
            .with_error(error_code, safe_message),
        )
    }

    pub fn record_exit_requested(&self, reason: impl Into<String>, prevented: bool) -> bool {
        self.recorder.record(
            DiagnosticEventInput::new(
                if prevented {
                    DiagnosticLevel::Warning
                } else {
                    DiagnosticLevel::Info
                },
                "lifecycle",
                DiagnosticEventKind::ExitRequested,
            )
            .with_context(BTreeMap::from([
                ("reason".to_string(), Value::String(reason.into())),
                ("prevented".to_string(), Value::Bool(prevented)),
            ])),
        )
    }

    pub fn end_session(&self, reason: impl Into<String>, exit_code: Option<i32>) -> bool {
        if self
            .ended
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        let mut context = BTreeMap::from([("reason".to_string(), Value::String(reason.into()))]);
        if let Some(exit_code) = exit_code {
            context.insert("exitCode".to_string(), json!(exit_code));
        }
        let recorded = self.recorder.record(
            DiagnosticEventInput::new(
                DiagnosticLevel::Info,
                "lifecycle",
                DiagnosticEventKind::SessionEnded,
            )
            .with_context(context),
        );
        if !recorded {
            self.ended.store(false, Ordering::Release);
        }
        recorded
    }

    pub fn recorder(&self) -> DiagnosticRecorder {
        self.recorder.clone()
    }
}

pub fn initialize_global(appdata: &Path) -> &'static DiagnosticRuntime {
    GLOBAL_RUNTIME.get_or_init(|| DiagnosticRuntime::new(appdata))
}

pub fn initialize_global_from_environment() -> Option<&'static DiagnosticRuntime> {
    let appdata = env::var_os("APPDATA").map(std::path::PathBuf::from)?;
    appdata.is_absolute().then(|| initialize_global(&appdata))
}

pub fn global_runtime() -> Option<&'static DiagnosticRuntime> {
    GLOBAL_RUNTIME.get()
}

pub fn install_panic_hook(recorder: DiagnosticRecorder) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = if let Some(message) = info.payload().downcast_ref::<&str>() {
            *message
        } else if let Some(message) = info.payload().downcast_ref::<String>() {
            message.as_str()
        } else {
            "Rust panic with a non-string payload"
        };
        let location = info.location().map(|location| {
            format!(
                "{}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )
        });
        let _ = recorder.record_panic_best_effort(message, location.as_deref());
        previous(info);
    }));
}

fn latest_unclean_session(events: &[super::DiagnosticEvent]) -> Option<String> {
    let ended = events
        .iter()
        .filter(|event| event.event_kind == DiagnosticEventKind::SessionEnded)
        .map(|event| event.session_id.as_str())
        .collect::<HashSet<_>>();
    let latest_started = events
        .iter()
        .rev()
        .find(|event| event.event_kind == DiagnosticEventKind::SessionStarted)?;
    if ended.contains(latest_started.session_id.as_str())
        || session_process_is_still_running(latest_started)
    {
        return None;
    }
    Some(latest_started.session_id.clone())
}

fn session_process_is_still_running(event: &super::DiagnosticEvent) -> bool {
    let Some(process_id) = event.safe_context.get("processId").and_then(Value::as_u64) else {
        return false;
    };
    let Ok(process_id) = u32::try_from(process_id) else {
        return false;
    };
    let Some(started_at) = event
        .safe_context
        .get("processStartedAt100ns")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return false;
    };
    process_started_at_100ns(process_id).is_some_and(|live| live == started_at)
}

#[cfg(windows)]
fn current_process_started_at_100ns() -> Option<u64> {
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    process_handle_started_at_100ns(unsafe { GetCurrentProcess() })
}

#[cfg(not(windows))]
fn current_process_started_at_100ns() -> Option<u64> {
    None
}

#[cfg(windows)]
fn process_started_at_100ns(process_id: u32) -> Option<u64> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_TIMEOUT},
        System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE,
        },
    };

    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            process_id,
        )
    };
    if handle.is_null() {
        return None;
    }
    let running = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
    let started_at = running
        .then(|| process_handle_started_at_100ns(handle))
        .flatten();
    unsafe {
        CloseHandle(handle);
    }
    started_at
}

#[cfg(not(windows))]
fn process_started_at_100ns(_process_id: u32) -> Option<u64> {
    None
}

#[cfg(windows)]
fn process_handle_started_at_100ns(handle: windows_sys::Win32::Foundation::HANDLE) -> Option<u64> {
    use windows_sys::Win32::{Foundation::FILETIME, System::Threading::GetProcessTimes};

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let succeeded =
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } != 0;
    succeeded
        .then(|| (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use crate::diagnostics::{
        event::{DiagnosticEventInput, DiagnosticEventKind, DiagnosticLevel},
        sanitize::{DiagnosticSanitizer, SanitizerRoots},
        store::DiagnosticStore,
        DiagnosticRecorder,
    };

    use super::{DiagnosticLifecycle, DiagnosticRuntime};

    fn recorder(root: &std::path::Path, session: &str) -> DiagnosticRecorder {
        let store = DiagnosticStore::new(
            root.to_path_buf(),
            session.to_string(),
            DiagnosticSanitizer::new(SanitizerRoots::default()),
        );
        DiagnosticRecorder::new(store, session.to_string())
    }

    #[test]
    fn clean_session_records_start_ready_and_end_once() {
        let temp = tempdir().unwrap();
        let recorder = recorder(temp.path(), "session-one");
        let lifecycle = DiagnosticLifecycle::start(recorder.clone());

        assert!(lifecycle.mark_ready());
        assert!(!lifecycle.mark_ready());
        assert!(lifecycle.end_session("runEventExit", Some(0)));
        assert!(!lifecycle.end_session("duplicate", Some(0)));

        let events = recorder.store().read_events().unwrap();
        assert_eq!(events[0].event_kind, DiagnosticEventKind::SessionStarted);
        assert_eq!(events[1].event_kind, DiagnosticEventKind::AppReady);
        assert_eq!(events[2].event_kind, DiagnosticEventKind::SessionEnded);
    }

    #[test]
    fn next_session_marks_the_latest_session_without_an_end_terminal() {
        let temp = tempdir().unwrap();
        let first = recorder(temp.path(), "session-one");
        assert!(first.record(
            DiagnosticEventInput::new(
                DiagnosticLevel::Info,
                "lifecycle",
                DiagnosticEventKind::SessionStarted,
            )
            .with_context(BTreeMap::from([
                ("processId".to_string(), serde_json::json!(u32::MAX)),
                ("processStartedAt100ns".to_string(), serde_json::json!("1"),),
            ])),
        ));
        let second = recorder(temp.path(), "session-two");
        let _second_lifecycle = DiagnosticLifecycle::start(second.clone());

        let events = second.store().read_events().unwrap();

        assert!(events
            .iter()
            .any(|event| event.event_kind == DiagnosticEventKind::PreviousSessionUnclean));
    }

    #[test]
    fn panic_record_is_best_effort_and_sanitized() {
        let temp = tempdir().unwrap();
        let recorder = recorder(temp.path(), "session-panic");
        assert!(recorder.record_panic_best_effort(
            r"panic token=secret at C:\Users\Alice\private.rs",
            Some(r"C:\repo\src\main.rs:1:1"),
        ));

        let events = recorder.store().read_events().unwrap();
        let panic = events.last().unwrap();
        let encoded = serde_json::to_string(panic).unwrap();
        assert_eq!(panic.event_kind, DiagnosticEventKind::Panic);
        assert!(!encoded.contains("secret"));
        assert!(!encoded.contains("Alice"));
    }

    #[test]
    fn runtime_uses_a_fresh_session_and_timestamp_unit_context() {
        let temp = tempdir().unwrap();
        let runtime = DiagnosticRuntime::new(temp.path());
        let events = runtime.store().read_events().unwrap();

        assert!(runtime.session_id().starts_with("session-"));
        assert_eq!(
            events[0].safe_context["timestampUnit"],
            serde_json::json!("unixEpochMilliseconds")
        );
    }

    #[test]
    fn previous_clean_session_is_not_marked_unclean() {
        let temp = tempdir().unwrap();
        let first = recorder(temp.path(), "session-one");
        let lifecycle = DiagnosticLifecycle::start(first);
        assert!(lifecycle.end_session("clean", Some(0)));
        let second = recorder(temp.path(), "session-two");
        let _lifecycle = DiagnosticLifecycle::start(second.clone());

        let events = second.store().read_events().unwrap();
        let unclean_for_second = events.iter().filter(|event| {
            event.session_id == "session-two"
                && event.event_kind == DiagnosticEventKind::PreviousSessionUnclean
        });
        assert_eq!(unclean_for_second.count(), 0);
    }

    #[test]
    fn an_older_unclean_session_is_not_reported_when_the_latest_session_was_clean() {
        let temp = tempdir().unwrap();
        let oldest = recorder(temp.path(), "session-old-unclean");
        let _oldest_lifecycle = DiagnosticLifecycle::start(oldest);
        let latest = recorder(temp.path(), "session-latest-clean");
        let latest_lifecycle = DiagnosticLifecycle::start(latest);
        assert!(latest_lifecycle.end_session("clean", Some(0)));

        let current = recorder(temp.path(), "session-current");
        let _current_lifecycle = DiagnosticLifecycle::start(current.clone());
        let events = current.store().read_events().unwrap();
        let current_unclean = events.iter().filter(|event| {
            event.session_id == "session-current"
                && event.event_kind == DiagnosticEventKind::PreviousSessionUnclean
        });

        assert_eq!(current_unclean.count(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn a_concurrent_live_session_is_not_reported_as_unclean() {
        let temp = tempdir().unwrap();
        let first = recorder(temp.path(), "session-live");
        let _first_lifecycle = DiagnosticLifecycle::start(first);
        let second = recorder(temp.path(), "session-current");
        let _second_lifecycle = DiagnosticLifecycle::start(second.clone());

        let events = second.store().read_events().unwrap();
        assert!(!events.iter().any(|event| {
            event.session_id == "session-current"
                && event.event_kind == DiagnosticEventKind::PreviousSessionUnclean
        }));
    }

    #[test]
    fn lifecycle_read_failure_does_not_block_session_start_attempt() {
        let temp = tempdir().unwrap();
        let occupied = temp.path().join("occupied");
        std::fs::write(&occupied, b"not-a-directory").unwrap();
        let recorder = recorder(&occupied, "session-one");

        let lifecycle = DiagnosticLifecycle::start(recorder);

        assert!(!lifecycle.recorder().session_id().is_empty());
    }
}
