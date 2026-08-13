use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, TryLockError},
    time::{Duration, SystemTime},
};

use super::{
    event::{timestamp_millis, DiagnosticEvent, DIAGNOSTIC_SCHEMA_VERSION},
    sanitize::DiagnosticSanitizer,
};

#[cfg(windows)]
use sha2::{Digest, Sha256};

pub const DEFAULT_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 10 * 1024 * 1024;
pub const DEFAULT_MAX_SEGMENT_BYTES: u64 = 512 * 1024;

const SEGMENT_PREFIX: &str = "events-";
const SEGMENT_SUFFIX: &str = ".jsonl";
const STORE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct DiagnosticStoreConfig {
    pub retention: Duration,
    pub max_total_bytes: u64,
    pub max_segment_bytes: u64,
}

impl Default for DiagnosticStoreConfig {
    fn default() -> Self {
        Self {
            retention: DEFAULT_RETENTION,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_segment_bytes: DEFAULT_MAX_SEGMENT_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticStoreStatus {
    pub segment_count: usize,
    pub total_bytes: u64,
    pub oldest_modified_at_ms: Option<u128>,
    pub newest_modified_at_ms: Option<u128>,
}

#[derive(Debug, Clone)]
pub struct DiagnosticStore {
    inner: Arc<StoreInner>,
}

#[derive(Debug)]
struct StoreInner {
    root: PathBuf,
    session_id: String,
    sanitizer: DiagnosticSanitizer,
    config: DiagnosticStoreConfig,
    state: Mutex<StoreState>,
    #[cfg(windows)]
    mutex_name: Vec<u16>,
}

#[derive(Debug, Default)]
struct StoreState {
    current_segment: Option<PathBuf>,
    next_segment: u64,
}

#[derive(Debug)]
struct Segment {
    path: PathBuf,
    bytes: u64,
    created_at_ms: u128,
    modified: SystemTime,
}

#[derive(Debug, Clone, Copy)]
struct SegmentInspection {
    tail_complete: bool,
    all_events_expired: bool,
}

impl DiagnosticStore {
    pub fn new(root: PathBuf, session_id: String, sanitizer: DiagnosticSanitizer) -> Self {
        Self::with_config(
            root,
            session_id,
            sanitizer,
            DiagnosticStoreConfig::default(),
        )
    }

    pub fn with_config(
        root: PathBuf,
        session_id: String,
        sanitizer: DiagnosticSanitizer,
        config: DiagnosticStoreConfig,
    ) -> Self {
        #[cfg(windows)]
        let mutex_name = diagnostic_mutex_name(&root)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        Self {
            inner: Arc::new(StoreInner {
                root,
                session_id: safe_file_token(&session_id),
                sanitizer,
                config,
                state: Mutex::new(StoreState::default()),
                #[cfg(windows)]
                mutex_name,
            }),
        }
    }

    pub fn from_appdata(
        appdata: &Path,
        session_id: String,
        sanitizer: DiagnosticSanitizer,
    ) -> Self {
        Self::new(
            appdata.join("codex-switch/logs/diagnostics"),
            session_id,
            sanitizer,
        )
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    pub fn sanitizer(&self) -> &DiagnosticSanitizer {
        &self.inner.sanitizer
    }

    #[cfg(all(test, windows))]
    pub(crate) fn with_root_lock_for_test<T>(
        &self,
        operation: impl FnOnce() -> T,
    ) -> Result<T, String> {
        let _root_guard = self.acquire_root_lock(STORE_LOCK_TIMEOUT)?;
        Ok(operation())
    }

    pub fn append(&self, event: &DiagnosticEvent) -> Result<(), String> {
        let _root_guard = self.acquire_root_lock(STORE_LOCK_TIMEOUT)?;
        let mut state = self.lock_state()?;
        self.append_locked(&mut state, event)
    }

    pub fn append_best_effort(&self, event: &DiagnosticEvent) -> bool {
        self.append(event).is_ok()
    }

    pub fn try_append_best_effort(&self, event: &DiagnosticEvent) -> bool {
        let _root_guard = match self.acquire_root_lock(Duration::ZERO) {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        let mut state = match self.inner.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => return false,
        };
        self.append_locked(&mut state, event).is_ok()
    }

    pub fn read_events(&self) -> Result<Vec<DiagnosticEvent>, String> {
        let _root_guard = self.acquire_root_lock(STORE_LOCK_TIMEOUT)?;
        let mut state = self.lock_state()?;
        self.prune_locked(&mut state, 0)?;
        let cutoff_ms = retention_cutoff(timestamp_millis(), self.inner.config.retention);
        let mut events = Vec::new();
        for segment in self.managed_segments()? {
            let payload = fs::read(&segment.path)
                .map_err(|error| format!("failed to read diagnostic segment: {error}"))?;
            let last_nonempty = payload
                .split(|byte| *byte == b'\n')
                .enumerate()
                .filter(|(_, line)| line.iter().any(|byte| !byte.is_ascii_whitespace()))
                .map(|(index, _)| index)
                .last();
            for (index, line) in payload.split(|byte| *byte == b'\n').enumerate() {
                if !line.iter().any(|byte| !byte.is_ascii_whitespace()) {
                    continue;
                }
                match serde_json::from_slice::<DiagnosticEvent>(line) {
                    Ok(event) => {
                        validate_schema_version(&event)?;
                        if event.timestamp >= cutoff_ms {
                            events.push(self.inner.sanitizer.sanitize_for_export(&event));
                        }
                    }
                    Err(_) if Some(index) == last_nonempty && !payload.ends_with(b"\n") => break,
                    Err(error) => {
                        return Err(format!("failed to parse diagnostic event: {error}"));
                    }
                }
            }
        }
        Ok(sort_events_causally(events))
    }

    pub fn sanitize_for_export(&self, event: &DiagnosticEvent) -> DiagnosticEvent {
        self.inner.sanitizer.sanitize_for_export(event)
    }

    pub fn prune(&self) -> Result<DiagnosticStoreStatus, String> {
        let _root_guard = self.acquire_root_lock(STORE_LOCK_TIMEOUT)?;
        let mut state = self.lock_state()?;
        self.prune_locked(&mut state, 0)?;
        self.status_locked()
    }

    pub fn status(&self) -> Result<DiagnosticStoreStatus, String> {
        let _root_guard = self.acquire_root_lock(STORE_LOCK_TIMEOUT)?;
        let _state = self.lock_state()?;
        self.status_locked()
    }

    pub fn clear(&self) -> Result<(), String> {
        let _root_guard = self.acquire_root_lock(STORE_LOCK_TIMEOUT)?;
        let mut state = self.lock_state()?;
        for segment in self.managed_segments()? {
            match fs::remove_file(&segment.path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!("failed to remove diagnostic segment: {error}"));
                }
            }
        }
        state.current_segment = None;
        Ok(())
    }

    fn append_locked(&self, state: &mut StoreState, event: &DiagnosticEvent) -> Result<(), String> {
        validate_schema_version(event)?;
        let event = self.inner.sanitizer.sanitize_event(event);
        let mut encoded = serde_json::to_vec(&event)
            .map_err(|error| format!("failed to serialize diagnostic event: {error}"))?;
        encoded.push(b'\n');
        let encoded_bytes = u64::try_from(encoded.len())
            .map_err(|_| "diagnostic event size overflow".to_string())?;
        if encoded_bytes > self.inner.config.max_segment_bytes
            || encoded_bytes > self.inner.config.max_total_bytes
        {
            return Err("diagnostic event exceeds the configured size limit".to_string());
        }

        self.prune_locked(state, encoded_bytes)?;
        let segment = self.select_segment(state, encoded_bytes)?;
        let mut output = match OpenOptions::new().append(true).open(&segment) {
            Ok(output) => output,
            Err(error) => {
                state.current_segment = None;
                return Err(format!("failed to open diagnostic segment: {error}"));
            }
        };
        if let Err(error) = output.write_all(&encoded) {
            state.current_segment = None;
            return Err(format!("failed to persist diagnostic event: {error}"));
        }
        if let Err(error) = output.sync_data() {
            state.current_segment = None;
            return Err(format!("failed to persist diagnostic event: {error}"));
        }

        // The event is already durable. Retention cleanup is best-effort here and
        // must not turn a successful append into a false failure receipt.
        let _ = self.prune_locked(state, 0);
        Ok(())
    }

    fn select_segment(
        &self,
        state: &mut StoreState,
        incoming_bytes: u64,
    ) -> Result<PathBuf, String> {
        if let Some(current) = state.current_segment.as_ref() {
            let created_at_ms = segment_created_at_ms(current);
            match safe_segment_metadata(current).and_then(|metadata| {
                let appendable = segment_has_complete_tail(current, metadata.len())?;
                Ok((metadata, appendable))
            }) {
                Ok((metadata, true))
                    if created_at_ms.is_some_and(|created_at_ms| {
                        !segment_is_expired(
                            created_at_ms,
                            timestamp_millis(),
                            self.inner.config.retention,
                        )
                    }) && metadata
                        .len()
                        .checked_add(incoming_bytes)
                        .is_some_and(|bytes| bytes <= self.inner.config.max_segment_bytes) =>
                {
                    return Ok(current.clone());
                }
                Ok(_) | Err(_) => state.current_segment = None,
            }
        }

        self.ensure_root()?;
        loop {
            let index = state.next_segment;
            state.next_segment = state.next_segment.saturating_add(1);
            let name = format!(
                "{SEGMENT_PREFIX}{}-{}-{}-{index:06}{SEGMENT_SUFFIX}",
                timestamp_millis(),
                std::process::id(),
                self.inner.session_id
            );
            let path = self.inner.root.join(name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    if let Err(error) = file.sync_all() {
                        state.current_segment = None;
                        return Err(format!("failed to initialize diagnostic segment: {error}"));
                    }
                    state.current_segment = Some(path.clone());
                    return Ok(path);
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    state.current_segment = None;
                    return Err(format!("failed to create diagnostic segment: {error}"));
                }
            }
        }
    }

    fn prune_locked(&self, state: &mut StoreState, incoming_bytes: u64) -> Result<(), String> {
        self.ensure_root()?;
        let now_ms = timestamp_millis();
        let mut segments = self.managed_segments()?;
        let cutoff_ms = retention_cutoff(now_ms, self.inner.config.retention);
        let mut expired_paths = Vec::new();
        for segment in segments.iter().filter(|segment| {
            segment_is_expired(segment.created_at_ms, now_ms, self.inner.config.retention)
        }) {
            let inspection = inspect_segment_for_prune(&segment.path, cutoff_ms)?;
            if inspection.tail_complete && inspection.all_events_expired {
                expired_paths.push(segment.path.clone());
            }
        }
        for path in &expired_paths {
            self.remove_segment(state, path)?;
        }
        segments.retain(|segment| !expired_paths.iter().any(|path| path == &segment.path));
        let mut total = segments.iter().try_fold(0_u64, |total, segment| {
            total
                .checked_add(segment.bytes)
                .ok_or_else(|| "diagnostic storage size overflow".to_string())
        })?;
        while total
            .checked_add(incoming_bytes)
            .is_none_or(|bytes| bytes > self.inner.config.max_total_bytes)
        {
            let mut removable_index = None;
            for (index, segment) in segments.iter().enumerate() {
                if inspect_segment_for_prune(&segment.path, cutoff_ms)?.tail_complete {
                    removable_index = Some(index);
                    break;
                }
            }
            let Some(removable_index) = removable_index else {
                return Err("diagnostic storage limit is smaller than one event".to_string());
            };
            let removable = segments.remove(removable_index);
            total = total.saturating_sub(removable.bytes);
            self.remove_segment(state, &removable.path)?;
        }
        Ok(())
    }

    fn remove_segment(&self, state: &mut StoreState, path: &Path) -> Result<(), String> {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("failed to prune diagnostic segment: {error}"));
            }
        }
        if state.current_segment.as_deref() == Some(path) {
            state.current_segment = None;
        }
        Ok(())
    }

    fn status_locked(&self) -> Result<DiagnosticStoreStatus, String> {
        let segments = self.managed_segments()?;
        let total_bytes = segments.iter().try_fold(0_u64, |total, segment| {
            total
                .checked_add(segment.bytes)
                .ok_or_else(|| "diagnostic storage size overflow".to_string())
        })?;
        Ok(DiagnosticStoreStatus {
            segment_count: segments.len(),
            total_bytes,
            oldest_modified_at_ms: segments
                .iter()
                .filter_map(|segment| system_time_millis(segment.modified))
                .min(),
            newest_modified_at_ms: segments
                .iter()
                .filter_map(|segment| system_time_millis(segment.modified))
                .max(),
        })
    }

    fn managed_segments(&self) -> Result<Vec<Segment>, String> {
        validate_existing_directory_chain(&self.inner.root)?;
        match fs::symlink_metadata(&self.inner.root) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(format!("failed to inspect diagnostic storage: {error}"));
            }
        }
        let mut segments = Vec::new();
        for entry in fs::read_dir(&self.inner.root)
            .map_err(|error| format!("failed to list diagnostic storage: {error}"))?
        {
            let entry =
                entry.map_err(|error| format!("failed to inspect diagnostic storage: {error}"))?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Some(created_at_ms) = segment_created_at_ms_from_name(&name) else {
                continue;
            };
            let metadata = safe_segment_metadata(&entry.path())?;
            let modified = metadata
                .modified()
                .map_err(|error| format!("failed to timestamp diagnostic segment: {error}"))?;
            segments.push(Segment {
                path: entry.path(),
                bytes: metadata.len(),
                created_at_ms,
                modified,
            });
        }
        segments.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.path.cmp(&right.path))
        });
        Ok(segments)
    }

    fn ensure_root(&self) -> Result<(), String> {
        validate_existing_directory_chain(&self.inner.root)?;
        fs::create_dir_all(&self.inner.root)
            .map_err(|error| format!("failed to create diagnostic storage: {error}"))?;
        validate_existing_directory_chain(&self.inner.root)
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, StoreState>, String> {
        self.inner
            .state
            .lock()
            .map_err(|_| "diagnostic store lock is poisoned".to_string())
    }

    #[cfg(windows)]
    fn acquire_root_lock(&self, timeout: Duration) -> Result<RootOperationGuard, String> {
        RootOperationGuard::acquire(&self.inner.mutex_name, timeout)
    }

    #[cfg(not(windows))]
    fn acquire_root_lock(&self, timeout: Duration) -> Result<RootOperationGuard<'static>, String> {
        RootOperationGuard::acquire(timeout)
    }
}

fn validate_schema_version(event: &DiagnosticEvent) -> Result<(), String> {
    if event.schema_version == DIAGNOSTIC_SCHEMA_VERSION {
        Ok(())
    } else {
        Err("unsupported diagnostic schema version".to_string())
    }
}

fn retention_cutoff(now_ms: u128, retention: Duration) -> u128 {
    now_ms.saturating_sub(retention.as_millis())
}

fn segment_is_expired(created_at_ms: u128, now_ms: u128, retention: Duration) -> bool {
    created_at_ms < retention_cutoff(now_ms, retention)
}

fn segment_has_complete_tail(path: &Path, bytes: u64) -> Result<bool, String> {
    if bytes == 0 {
        return Ok(true);
    }

    let mut input = File::open(path)
        .map_err(|error| format!("failed to inspect diagnostic segment tail: {error}"))?;
    input
        .seek(SeekFrom::End(-1))
        .map_err(|error| format!("failed to inspect diagnostic segment tail: {error}"))?;
    let mut tail = [0_u8; 1];
    input
        .read_exact(&mut tail)
        .map_err(|error| format!("failed to inspect diagnostic segment tail: {error}"))?;
    Ok(tail[0] == b'\n')
}

fn inspect_segment_for_prune(path: &Path, cutoff_ms: u128) -> Result<SegmentInspection, String> {
    let payload =
        fs::read(path).map_err(|error| format!("failed to read diagnostic segment: {error}"))?;
    let tail_complete = payload.is_empty() || payload.ends_with(b"\n");
    let last_nonempty = payload
        .split(|byte| *byte == b'\n')
        .enumerate()
        .filter(|(_, line)| line.iter().any(|byte| !byte.is_ascii_whitespace()))
        .map(|(index, _)| index)
        .last();
    let mut all_events_expired = true;
    for (index, line) in payload.split(|byte| *byte == b'\n').enumerate() {
        if !line.iter().any(|byte| !byte.is_ascii_whitespace()) {
            continue;
        }
        match serde_json::from_slice::<DiagnosticEvent>(line) {
            Ok(event) => {
                validate_schema_version(&event)?;
                if event.timestamp >= cutoff_ms {
                    all_events_expired = false;
                }
            }
            Err(_) if Some(index) == last_nonempty && !tail_complete => {
                all_events_expired = false;
            }
            Err(error) => {
                return Err(format!("failed to parse diagnostic event: {error}"));
            }
        }
    }
    if !tail_complete {
        all_events_expired = false;
    }
    Ok(SegmentInspection {
        tail_complete,
        all_events_expired,
    })
}

fn segment_created_at_ms(path: &Path) -> Option<u128> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(segment_created_at_ms_from_name)
}

fn segment_created_at_ms_from_name(name: &str) -> Option<u128> {
    let body = name
        .strip_prefix(SEGMENT_PREFIX)?
        .strip_suffix(SEGMENT_SUFFIX)?;
    if body.is_empty()
        || !body
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return None;
    }

    let (created_at, remainder) = body.split_once('-')?;
    let (pid, session_and_index) = remainder.split_once('-')?;
    let (session, index) = session_and_index.rsplit_once('-')?;
    if created_at.is_empty()
        || !created_at.bytes().all(|byte| byte.is_ascii_digit())
        || pid.is_empty()
        || !pid.bytes().all(|byte| byte.is_ascii_digit())
        || session.is_empty()
        || index.is_empty()
        || !index.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    created_at.parse().ok()
}

fn sort_events_causally(mut events: Vec<DiagnosticEvent>) -> Vec<DiagnosticEvent> {
    events.sort_by(|left, right| {
        left.session_id
            .cmp(&right.session_id)
            .then_with(|| left.sequence.cmp(&right.sequence))
            .then_with(|| left.timestamp.cmp(&right.timestamp))
            .then_with(|| left.event_id.cmp(&right.event_id))
    });

    let mut decorated = Vec::with_capacity(events.len());
    let mut last_session: Option<String> = None;
    let mut causal_timestamp = 0_u128;
    for event in events {
        if last_session.as_deref() == Some(event.session_id.as_str()) {
            causal_timestamp = causal_timestamp.max(event.timestamp);
        } else {
            causal_timestamp = event.timestamp;
            last_session = Some(event.session_id.clone());
        }
        decorated.push((causal_timestamp, event));
    }
    decorated.sort_by(|(left_timestamp, left), (right_timestamp, right)| {
        left_timestamp
            .cmp(right_timestamp)
            .then_with(|| left.session_id.cmp(&right.session_id))
            .then_with(|| left.sequence.cmp(&right.sequence))
            .then_with(|| left.timestamp.cmp(&right.timestamp))
            .then_with(|| left.event_id.cmp(&right.event_id))
    });
    decorated.into_iter().map(|(_, event)| event).collect()
}

fn validate_existing_directory_chain(root: &Path) -> Result<(), String> {
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("failed to resolve diagnostic storage: {error}"))?
            .join(root)
    };
    let mut ancestors = absolute
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if is_reparse_point(&metadata) => {
                return Err(
                    "diagnostic storage ancestors must not be symlinks or reparse points"
                        .to_string(),
                );
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err("diagnostic storage ancestors must be directories".to_string());
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to inspect diagnostic storage ancestor: {error}"
                ));
            }
        }
    }
    Ok(())
}

fn safe_segment_metadata(path: &Path) -> Result<fs::Metadata, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect diagnostic segment: {error}"))?;
    if !metadata.is_file() || is_reparse_point(&metadata) {
        return Err("diagnostic segment must be a regular file".to_string());
    }
    Ok(metadata)
}

fn safe_file_token(value: &str) -> String {
    let token = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(96)
        .collect::<String>();
    if token.is_empty() {
        "unknown-session".to_string()
    } else {
        token
    }
}

fn system_time_millis(time: SystemTime) -> Option<u128> {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis())
}

#[cfg(windows)]
fn diagnostic_mutex_name(root: &Path) -> String {
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(root))
            .unwrap_or_else(|_| root.to_path_buf())
    };
    let normalized = absolute
        .as_os_str()
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    format!("Local\\CodexSwitchDiagnostics-{:x}", hasher.finalize())
}

#[cfg(windows)]
#[derive(Debug)]
struct RootOperationGuard {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl RootOperationGuard {
    fn acquire(name: &[u16], timeout: Duration) -> Result<Self, String> {
        use std::ptr;
        use windows_sys::Win32::{
            Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{CreateMutexW, WaitForSingleObject},
        };

        let handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err("failed to create diagnostic store lock".to_string());
        }
        let timeout_ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
        match unsafe { WaitForSingleObject(handle, timeout_ms) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self { handle }),
            WAIT_TIMEOUT => {
                unsafe {
                    CloseHandle(handle);
                }
                Err("diagnostic store lock timed out".to_string())
            }
            _ => {
                unsafe {
                    CloseHandle(handle);
                }
                Err("failed to acquire diagnostic store lock".to_string())
            }
        }
    }
}

#[cfg(windows)]
impl Drop for RootOperationGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::ReleaseMutex};

        unsafe {
            ReleaseMutex(self.handle);
            CloseHandle(self.handle);
        }
    }
}

#[cfg(not(windows))]
static PROCESS_STORE_LOCK: Mutex<()> = Mutex::new(());

#[cfg(not(windows))]
#[derive(Debug)]
struct RootOperationGuard<'a> {
    _guard: MutexGuard<'a, ()>,
}

#[cfg(not(windows))]
impl RootOperationGuard<'static> {
    fn acquire(timeout: Duration) -> Result<Self, String> {
        let started = std::time::Instant::now();
        loop {
            match PROCESS_STORE_LOCK.try_lock() {
                Ok(guard) => return Ok(Self { _guard: guard }),
                Err(TryLockError::Poisoned(error)) => {
                    return Ok(Self {
                        _guard: error.into_inner(),
                    });
                }
                Err(TryLockError::WouldBlock)
                    if timeout.is_zero() || started.elapsed() >= timeout =>
                {
                    return Err("diagnostic store lock timed out".to_string());
                }
                Err(TryLockError::WouldBlock) => {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        fs::OpenOptions,
        io::Write,
        sync::{mpsc, Barrier},
        thread,
        time::Duration,
    };

    use tempfile::tempdir;

    use crate::diagnostics::{
        event::{
            timestamp_millis, DiagnosticEvent, DiagnosticEventKind, DiagnosticLevel,
            DIAGNOSTIC_SCHEMA_VERSION,
        },
        sanitize::{DiagnosticSanitizer, SanitizerRoots},
    };

    use super::{DiagnosticStore, DiagnosticStoreConfig, SEGMENT_PREFIX, SEGMENT_SUFFIX};

    fn event(sequence: u64, message: &str) -> DiagnosticEvent {
        event_at(sequence, message, timestamp_millis())
    }

    fn event_at(sequence: u64, message: &str, timestamp: u128) -> DiagnosticEvent {
        DiagnosticEvent {
            schema_version: DIAGNOSTIC_SCHEMA_VERSION,
            event_id: format!("event-{sequence}"),
            session_id: "session-test".to_string(),
            sequence,
            timestamp,
            level: DiagnosticLevel::Info,
            component: "test".to_string(),
            event_kind: DiagnosticEventKind::OperationPhase,
            attempt_id: Some("attempt-test".to_string()),
            operation_id: None,
            action: Some("testAction".to_string()),
            phase: Some("testPhase".to_string()),
            terminal_status: None,
            error_code: None,
            safe_message: Some(message.to_string()),
            safe_context: BTreeMap::new(),
        }
    }

    fn store(root: &std::path::Path, config: DiagnosticStoreConfig) -> DiagnosticStore {
        DiagnosticStore::with_config(
            root.to_path_buf(),
            "session-test".to_string(),
            DiagnosticSanitizer::new(SanitizerRoots::default()),
            config,
        )
    }

    #[test]
    fn appends_segments_and_reads_events_in_sequence_order() {
        let temp = tempdir().unwrap();
        let store = store(
            temp.path(),
            DiagnosticStoreConfig {
                retention: Duration::from_secs(60),
                max_total_bytes: 64 * 1024,
                max_segment_bytes: 700,
            },
        );
        for sequence in 1..=6 {
            store.append(&event(sequence, &"x".repeat(120))).unwrap();
        }

        let events = store.read_events().unwrap();
        let status = store.status().unwrap();

        assert_eq!(events.len(), 6);
        assert_eq!(events[0].sequence, 1);
        assert_eq!(events[5].sequence, 6);
        assert!(status.segment_count > 1);
    }

    #[test]
    fn reader_ignores_only_one_truncated_tail() {
        let temp = tempdir().unwrap();
        let store = store(temp.path(), DiagnosticStoreConfig::default());
        store.append(&event(1, "complete")).unwrap();
        let segment = store.managed_segments().unwrap().pop().unwrap().path;
        OpenOptions::new()
            .append(true)
            .open(&segment)
            .unwrap()
            .write_all(br#"{"schemaVersion":1"#)
            .unwrap();

        let events = store.read_events().unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, 1);
    }

    #[test]
    fn reader_rejects_internal_corruption() {
        let temp = tempdir().unwrap();
        let store = store(temp.path(), DiagnosticStoreConfig::default());
        store.append(&event(1, "first")).unwrap();
        let segment = store.managed_segments().unwrap().pop().unwrap().path;
        let second = serde_json::to_string(&event(2, "second")).unwrap();
        OpenOptions::new()
            .append(true)
            .open(segment)
            .unwrap()
            .write_all(format!("not-json\n{second}\n").as_bytes())
            .unwrap();

        let error = store.read_events().unwrap_err();

        assert!(
            error.contains("failed to parse diagnostic event"),
            "{error}"
        );
    }

    #[test]
    fn total_size_is_pruned_to_the_configured_limit() {
        let temp = tempdir().unwrap();
        let store = store(
            temp.path(),
            DiagnosticStoreConfig {
                retention: Duration::from_secs(60),
                max_total_bytes: 3_000,
                max_segment_bytes: 900,
            },
        );
        for sequence in 1..=30 {
            assert!(store.append_best_effort(&event(sequence, &"z".repeat(180))));
        }

        let status = store.status().unwrap();

        assert!(status.total_bytes <= 3_000, "{status:?}");
        assert!(store.read_events().unwrap().len() < 30);
    }

    #[test]
    fn independent_stores_serialize_writes_and_preserve_the_total_cap() {
        let temp = tempdir().unwrap();
        let config = DiagnosticStoreConfig {
            retention: Duration::from_secs(60),
            max_total_bytes: 3_000,
            max_segment_bytes: 900,
        };
        let first = store(temp.path(), config.clone());
        let second = store(temp.path(), config);
        let barrier = std::sync::Arc::new(Barrier::new(2));

        let first_barrier = barrier.clone();
        let first_thread = thread::spawn(move || {
            first_barrier.wait();
            for sequence in 1..=20 {
                first.append(&event(sequence, &"a".repeat(180))).unwrap();
            }
        });
        let second_thread = thread::spawn(move || {
            barrier.wait();
            for sequence in 21..=40 {
                second.append(&event(sequence, &"b".repeat(180))).unwrap();
            }
        });
        first_thread.join().unwrap();
        second_thread.join().unwrap();

        let verifier = store(
            temp.path(),
            DiagnosticStoreConfig {
                retention: Duration::from_secs(60),
                max_total_bytes: 3_000,
                max_segment_bytes: 900,
            },
        );
        let status = verifier.status().unwrap();
        assert!(status.total_bytes <= 3_000, "{status:?}");
        assert!(!verifier.read_events().unwrap().is_empty());
    }

    #[test]
    fn panic_try_path_does_not_wait_for_the_root_lock() {
        let temp = tempdir().unwrap();
        let holder_store = store(temp.path(), DiagnosticStoreConfig::default());
        let contender = store(temp.path(), DiagnosticStoreConfig::default());
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let holder = thread::spawn(move || {
            let _guard = holder_store
                .acquire_root_lock(Duration::from_secs(1))
                .unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        ready_rx.recv().unwrap();

        let (result_tx, result_rx) = mpsc::channel();
        let contender_thread = thread::spawn(move || {
            result_tx
                .send(contender.try_append_best_effort(&event(1, "panic")))
                .unwrap();
        });
        let result = result_rx.recv_timeout(Duration::from_secs(1));
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        contender_thread.join().unwrap();
        assert!(!result.unwrap());
    }

    #[test]
    fn prune_keeps_recent_events_inside_an_old_segment() {
        let temp = tempdir().unwrap();
        let now = timestamp_millis();
        let old_created_at = now.saturating_sub(120_000);
        let path = temp.path().join(format!(
            "{SEGMENT_PREFIX}{old_created_at}-1-session-test-000000{SEGMENT_SUFFIX}"
        ));
        let expired = serde_json::to_string(&event_at(1, "expired", old_created_at)).unwrap();
        let recent = serde_json::to_string(&event_at(2, "recent payload", now)).unwrap();
        let payload = format!("{expired}\n{recent}\n");
        fs::write(&path, payload).unwrap();
        let store = store(
            temp.path(),
            DiagnosticStoreConfig {
                retention: Duration::from_secs(60),
                ..DiagnosticStoreConfig::default()
            },
        );

        assert_eq!(store.prune().unwrap().segment_count, 1);
        assert!(path.exists());
        let events = store.read_events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, 2);
    }

    #[test]
    fn prune_removes_an_old_segment_when_every_event_is_expired() {
        let temp = tempdir().unwrap();
        let now = timestamp_millis();
        let old_created_at = now.saturating_sub(120_000);
        let path = temp.path().join(format!(
            "{SEGMENT_PREFIX}{old_created_at}-1-session-test-000000{SEGMENT_SUFFIX}"
        ));
        let first = serde_json::to_string(&event_at(1, "expired one", old_created_at)).unwrap();
        let second = serde_json::to_string(&event_at(2, "expired two", old_created_at)).unwrap();
        fs::write(&path, format!("{first}\n{second}\n")).unwrap();
        let store = store(
            temp.path(),
            DiagnosticStoreConfig {
                retention: Duration::from_secs(60),
                ..DiagnosticStoreConfig::default()
            },
        );

        assert_eq!(store.prune().unwrap().segment_count, 0);
        assert!(!path.exists());
    }

    #[test]
    fn prune_does_not_remove_an_old_segment_with_a_dirty_tail() {
        let temp = tempdir().unwrap();
        let now = timestamp_millis();
        let old_created_at = now.saturating_sub(120_000);
        let path = temp.path().join(format!(
            "{SEGMENT_PREFIX}{old_created_at}-1-session-test-000000{SEGMENT_SUFFIX}"
        ));
        let expired = serde_json::to_string(&event_at(1, "expired", old_created_at)).unwrap();
        fs::write(&path, format!("{expired}\n{{\"schemaVersion\":1")).unwrap();
        let store = store(
            temp.path(),
            DiagnosticStoreConfig {
                retention: Duration::from_secs(60),
                ..DiagnosticStoreConfig::default()
            },
        );

        assert_eq!(store.prune().unwrap().segment_count, 1);
        assert!(path.exists());
        assert!(store.read_events().unwrap().is_empty());
    }

    #[test]
    fn prune_reports_old_internal_corruption_without_removing_the_segment() {
        let temp = tempdir().unwrap();
        let now = timestamp_millis();
        let old_created_at = now.saturating_sub(120_000);
        let path = temp.path().join(format!(
            "{SEGMENT_PREFIX}{old_created_at}-1-session-test-000000{SEGMENT_SUFFIX}"
        ));
        let expired = serde_json::to_string(&event_at(1, "expired", old_created_at)).unwrap();
        fs::write(&path, format!("{expired}\nnot-json\n")).unwrap();
        let store = store(
            temp.path(),
            DiagnosticStoreConfig {
                retention: Duration::from_secs(60),
                ..DiagnosticStoreConfig::default()
            },
        );

        let error = store.prune().unwrap_err();
        assert!(
            error.contains("failed to parse diagnostic event"),
            "{error}"
        );
        assert!(path.exists());
    }

    #[test]
    fn prune_reports_old_unknown_schema_without_removing_the_segment() {
        let temp = tempdir().unwrap();
        let now = timestamp_millis();
        let old_created_at = now.saturating_sub(120_000);
        let path = temp.path().join(format!(
            "{SEGMENT_PREFIX}{old_created_at}-1-session-test-000000{SEGMENT_SUFFIX}"
        ));
        let mut unsupported = event_at(1, "unsupported", old_created_at);
        unsupported.schema_version = DIAGNOSTIC_SCHEMA_VERSION + 1;
        let mut payload = serde_json::to_vec(&unsupported).unwrap();
        payload.push(b'\n');
        fs::write(&path, payload).unwrap();
        let store = store(
            temp.path(),
            DiagnosticStoreConfig {
                retention: Duration::from_secs(60),
                ..DiagnosticStoreConfig::default()
            },
        );

        let error = store.prune().unwrap_err();
        assert!(error.contains("unsupported diagnostic schema"), "{error}");
        assert!(path.exists());
    }

    #[test]
    fn reader_filters_expired_events_inside_a_recent_segment() {
        let temp = tempdir().unwrap();
        let store = store(
            temp.path(),
            DiagnosticStoreConfig {
                retention: Duration::from_secs(1),
                ..DiagnosticStoreConfig::default()
            },
        );
        let now = timestamp_millis();
        store
            .append(&event_at(1, "expired", now.saturating_sub(2_000)))
            .unwrap();
        store.append(&event_at(2, "retained", now)).unwrap();

        let events = store.read_events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, 2);
    }

    #[test]
    fn expired_current_segment_is_not_reused() {
        let temp = tempdir().unwrap();
        let old_created_at = timestamp_millis().saturating_sub(2_000);
        let old = temp.path().join(format!(
            "{SEGMENT_PREFIX}{old_created_at}-1-session-test-000000{SEGMENT_SUFFIX}"
        ));
        fs::write(&old, b"").unwrap();
        let store = store(
            temp.path(),
            DiagnosticStoreConfig {
                retention: Duration::from_secs(1),
                ..DiagnosticStoreConfig::default()
            },
        );
        let mut state = store.inner.state.lock().unwrap();
        state.current_segment = Some(old.clone());

        let selected = store.select_segment(&mut state, 1).unwrap();
        assert_ne!(selected, old);
    }

    #[test]
    fn incomplete_current_tail_is_sealed_before_the_next_append() {
        let temp = tempdir().unwrap();
        let store = store(temp.path(), DiagnosticStoreConfig::default());
        store.append(&event(1, "complete")).unwrap();
        let first = store.managed_segments().unwrap().pop().unwrap().path;
        OpenOptions::new()
            .append(true)
            .open(&first)
            .unwrap()
            .write_all(br#"{"schemaVersion":1"#)
            .unwrap();

        store.append(&event(2, "next segment")).unwrap();

        let segments = store.managed_segments().unwrap();
        assert_eq!(segments.len(), 2);
        let events = store.read_events().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence, 1);
        assert_eq!(events[1].sequence, 2);
    }

    #[cfg(windows)]
    #[test]
    fn append_open_failure_seals_the_current_segment() {
        let temp = tempdir().unwrap();
        let store = store(temp.path(), DiagnosticStoreConfig::default());
        store.append(&event(1, "first")).unwrap();
        let segment = store.managed_segments().unwrap().pop().unwrap().path;
        let original_permissions = fs::metadata(&segment).unwrap().permissions();
        let mut read_only_permissions = original_permissions.clone();
        read_only_permissions.set_readonly(true);
        fs::set_permissions(&segment, read_only_permissions).unwrap();

        let error = store.append(&event(2, "blocked")).unwrap_err();
        fs::set_permissions(&segment, original_permissions).unwrap();

        assert!(
            error.contains("failed to open diagnostic segment"),
            "{error}"
        );
        assert!(store.inner.state.lock().unwrap().current_segment.is_none());
    }

    #[test]
    fn unsupported_schema_is_rejected_on_append_and_read() {
        let temp = tempdir().unwrap();
        let store = store(temp.path(), DiagnosticStoreConfig::default());
        let mut unsupported = event(1, "unsupported");
        unsupported.schema_version = DIAGNOSTIC_SCHEMA_VERSION + 1;

        let append_error = store.append(&unsupported).unwrap_err();
        assert!(append_error.contains("unsupported diagnostic schema"));
        assert_eq!(store.status().unwrap().segment_count, 0);

        store.append(&event(2, "supported")).unwrap();
        let segment = store.managed_segments().unwrap().pop().unwrap().path;
        let mut encoded = serde_json::to_vec(&unsupported).unwrap();
        encoded.push(b'\n');
        OpenOptions::new()
            .append(true)
            .open(segment)
            .unwrap()
            .write_all(&encoded)
            .unwrap();

        let read_error = store.read_events().unwrap_err();
        assert!(read_error.contains("unsupported diagnostic schema"));
    }

    #[test]
    fn reader_preserves_sequence_causality_within_a_session() {
        let temp = tempdir().unwrap();
        let store = store(temp.path(), DiagnosticStoreConfig::default());
        let now = timestamp_millis();
        let mut other_session = event_at(1, "other", now.saturating_add(100));
        other_session.session_id = "session-other".to_string();
        store.append(&event_at(2, "later sequence", now)).unwrap();
        store.append(&other_session).unwrap();
        store
            .append(&event_at(1, "earlier sequence", now.saturating_add(200)))
            .unwrap();

        let sequences = store
            .read_events()
            .unwrap()
            .into_iter()
            .filter(|event| event.session_id == "session-test")
            .map(|event| event.sequence)
            .collect::<Vec<_>>();
        assert_eq!(sequences, vec![1, 2]);
    }

    #[test]
    fn symlink_or_reparse_ancestor_is_rejected_before_directory_creation() {
        let temp = tempdir().unwrap();
        let actual = temp.path().join("actual");
        let link = temp.path().join("link");
        fs::create_dir(&actual).unwrap();
        if !create_directory_symlink(&actual, &link) {
            return;
        }
        let nested = link.join("nested");
        let store = store(&nested, DiagnosticStoreConfig::default());

        let error = store.append(&event(1, "blocked")).unwrap_err();
        assert!(error.contains("symlinks or reparse points"), "{error}");
        assert!(!actual.join("nested").exists());
    }

    #[test]
    fn clear_removes_only_managed_diagnostic_segments() {
        let temp = tempdir().unwrap();
        let store = store(temp.path(), DiagnosticStoreConfig::default());
        store.append(&event(1, "event")).unwrap();
        let unrelated = temp.path().join("operations.jsonl");
        fs::write(&unrelated, b"audit").unwrap();

        store.clear().unwrap();

        assert!(unrelated.exists());
        assert_eq!(store.status().unwrap().segment_count, 0);
    }

    #[test]
    fn invalid_storage_fails_without_panicking() {
        let temp = tempdir().unwrap();
        let root_file = temp.path().join("not-a-directory");
        fs::write(&root_file, b"occupied").unwrap();
        let store = store(&root_file, DiagnosticStoreConfig::default());

        assert!(!store.append_best_effort(&event(1, "ignored")));
    }

    #[cfg(unix)]
    fn create_directory_symlink(target: &std::path::Path, link: &std::path::Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(windows)]
    fn create_directory_symlink(target: &std::path::Path, link: &std::path::Path) -> bool {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
    }

    #[cfg(not(any(unix, windows)))]
    fn create_directory_symlink(_target: &std::path::Path, _link: &std::path::Path) -> bool {
        false
    }
}
