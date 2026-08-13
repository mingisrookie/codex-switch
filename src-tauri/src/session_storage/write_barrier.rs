// Transaction methods intentionally return the owned handle token on failure.
// Boxing it would complicate the fail-closed retry contract and add allocation
// failure to the exact-handle recovery path.
#![allow(clippy::result_large_err)]

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Stable regular-file identity persisted by operation journals. A matching
/// digest is never sufficient ownership proof without this identity.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RegularFileIdentity {
    pub(crate) volume_serial_number: u64,
    pub(crate) file_index: u64,
}

/// Durable identity bindings for one replacement operation. The directory
/// identity prevents a persisted set of leaf names from being reinterpreted
/// after its parent namespace is renamed or replaced.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HandleReplaceIdentityBindings {
    pub(crate) parent_identity: RegularFileIdentity,
    pub(crate) original_identity: RegularFileIdentity,
    pub(crate) replacement_identity: RegularFileIdentity,
}

/// Pins one exact, non-reparse parent directory. The Windows handle never
/// shares DELETE access, so the directory cannot be renamed or deleted while
/// any replacement token is live.
struct ParentDirectoryGuard {
    path: PathBuf,
    file: File,
    identity: RegularFileIdentity,
}

/// Keeps an existing managed file readable while refusing any new writer.
///
/// Replacement is deliberately not exposed as a path-based clobber. Callers
/// must use the handle-bound two-phase API and durably record its recovery
/// path before publishing.
pub(crate) struct WriteExclusionGuard {
    path: PathBuf,
    file: File,
    parent: ParentDirectoryGuard,
}

/// Owns the only normal read/write/delete handle window for a file that is
/// about to be quarantined or deleted. DELETE sharing is deliberately denied,
/// so a same-object rename/delete contender cannot enter after acquisition.
pub(crate) struct DestructiveFileGuard {
    path: PathBuf,
    file: File,
    parent: ParentDirectoryGuard,
}

/// Pins a hard-link source name and exact file identity without requesting
/// READ/WRITE/DELETE access on the file object. This remains compatible with a
/// destructive guard acquired through another hard-link name while the parent
/// guard prevents the source namespace from being renamed underneath recovery.
pub(crate) struct HardlinkSourceGuard {
    path: PathBuf,
    file: File,
    parent: ParentDirectoryGuard,
    identity: RegularFileIdentity,
    expected_sha256: String,
}

/// Operation-bound names that must already be persisted in the caller's plan
/// before any handle-bound replacement starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HandleReplacePaths {
    target_path: PathBuf,
    recovery_path: PathBuf,
    staging_path: PathBuf,
    rollback_tombstone_path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandleReplaceCrashState {
    Original,
    Staged,
    Prepared,
    ReplacementWithRecovery,
    ReplacementOnly,
    RollbackPrepared,
    RolledBack,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandleReplaceRecoveryDecision {
    Commit,
    Restore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HandleCreatePaths {
    target_path: PathBuf,
    staging_path: PathBuf,
    rollback_tombstone_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HandleCreateIdentityBindings {
    pub(crate) parent_identity: RegularFileIdentity,
    pub(crate) created_identity: RegularFileIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandleCreateCrashState {
    Absent,
    Staged,
    Published,
    RollbackPrepared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandleCreateRecoveryDecision {
    Commit,
    Restore,
}

pub(crate) struct StagedHandleCreate {
    paths: HandleCreatePaths,
    created: DestructiveFileGuard,
    expected_sha256: String,
}

pub(crate) struct PublishedHandleCreate {
    paths: HandleCreatePaths,
    created: DestructiveFileGuard,
    expected_sha256: String,
}

pub(crate) struct ResolvedHandleCreate {
    paths: HandleCreatePaths,
    guard: Option<WriteExclusionGuard>,
    cleanup_artifacts: Vec<CleanupArtifact>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HandleDeletePaths {
    target_path: PathBuf,
    recovery_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HandleDeleteIdentityBindings {
    pub(crate) parent_identity: RegularFileIdentity,
    pub(crate) deleted_identity: RegularFileIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandleDeleteCrashState {
    Original,
    Prepared,
    Deleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandleDeleteRecoveryDecision {
    Commit,
    Restore,
}

pub(crate) struct StagedHandleDelete {
    paths: HandleDeletePaths,
    target: DestructiveFileGuard,
    expected_sha256: String,
}

pub(crate) struct PreparedHandleDelete {
    paths: HandleDeletePaths,
    target: DestructiveFileGuard,
    expected_sha256: String,
}

pub(crate) struct ResolvedHandleDelete {
    paths: HandleDeletePaths,
    guard: Option<WriteExclusionGuard>,
    cleanup_artifacts: Vec<CleanupArtifact>,
}

/// Terminal handle state. Files are never cleaned up implicitly: after the
/// caller persists its terminal ledger state it must choose cleanup or retain.
pub(crate) struct ResolvedHandleReplace {
    guard: WriteExclusionGuard,
    paths: HandleReplacePaths,
    cleanup_artifacts: Vec<CleanupArtifact>,
}

struct CleanupArtifact {
    path: PathBuf,
    expected_sha256: String,
    expected_identity: RegularFileIdentity,
}

/// Owns a handle-bound replacement whose previous target has already moved to
/// a caller-owned recovery name.
///
/// The caller must durably record `recovery_path()` before calling
/// `publish()`. Dropping this token never removes the recovery file.
pub(crate) struct PreparedHandleReplace {
    paths: HandleReplacePaths,
    previous: WriteExclusionGuard,
    replacement: DestructiveFileGuard,
    expected_replacement_sha256: String,
}

/// Owns a fully written and identity-verified replacement before the previous
/// target is moved. Every fallible content and namespace check happens before
/// `prepare`; after a successful handle rename, constructing the prepared token
/// is infallible.
pub(crate) struct StagedHandleReplace {
    paths: HandleReplacePaths,
    previous: WriteExclusionGuard,
    replacement: DestructiveFileGuard,
    expected_replacement_sha256: String,
}

/// Owns both sides of a published replacement until the caller either keeps
/// the new target or restores the exact previous file object.
///
/// Dropping this token is deliberately non-destructive: the replacement stays
/// at the target and the previous file stays at the recovery path.
pub(crate) struct PublishedHandleReplace {
    paths: HandleReplacePaths,
    previous: WriteExclusionGuard,
    replacement: DestructiveFileGuard,
    expected_replacement_sha256: String,
}

impl std::fmt::Debug for WriteExclusionGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WriteExclusionGuard")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for DestructiveFileGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DestructiveFileGuard")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for PreparedHandleReplace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedHandleReplace")
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for StagedHandleReplace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StagedHandleReplace")
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for PublishedHandleReplace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PublishedHandleReplace")
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ResolvedHandleReplace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedHandleReplace")
            .field("paths", &self.paths)
            .field("cleanup_artifact_count", &self.cleanup_artifacts.len())
            .finish_non_exhaustive()
    }
}

macro_rules! debug_token_with_paths {
    ($type:ty, $name:literal) => {
        impl std::fmt::Debug for $type {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter
                    .debug_struct($name)
                    .field("paths", &self.paths)
                    .finish_non_exhaustive()
            }
        }
    };
}

debug_token_with_paths!(StagedHandleCreate, "StagedHandleCreate");
debug_token_with_paths!(PublishedHandleCreate, "PublishedHandleCreate");
debug_token_with_paths!(ResolvedHandleCreate, "ResolvedHandleCreate");
debug_token_with_paths!(StagedHandleDelete, "StagedHandleDelete");
debug_token_with_paths!(PreparedHandleDelete, "PreparedHandleDelete");
debug_token_with_paths!(ResolvedHandleDelete, "ResolvedHandleDelete");

impl HandleReplacePaths {
    /// The caller must construct and persist this complete set before calling a
    /// prepare method. Every name is no-clobber, same-directory, and distinct.
    pub(crate) fn from_persisted_plan(
        target_path: PathBuf,
        recovery_path: PathBuf,
        staging_path: PathBuf,
        rollback_tombstone_path: PathBuf,
    ) -> Result<Self, String> {
        let paths = Self {
            target_path,
            recovery_path,
            staging_path,
            rollback_tombstone_path,
        };
        validate_replace_paths(&paths)?;
        Ok(paths)
    }

    pub(crate) fn target_path(&self) -> &Path {
        &self.target_path
    }

    pub(crate) fn recovery_path(&self) -> &Path {
        &self.recovery_path
    }

    pub(crate) fn staging_path(&self) -> &Path {
        &self.staging_path
    }

    pub(crate) fn rollback_tombstone_path(&self) -> &Path {
        &self.rollback_tombstone_path
    }
}

impl HandleCreatePaths {
    pub(crate) fn from_persisted_plan(
        target_path: PathBuf,
        staging_path: PathBuf,
        rollback_tombstone_path: PathBuf,
    ) -> Result<Self, String> {
        let paths = Self {
            target_path,
            staging_path,
            rollback_tombstone_path,
        };
        validate_same_parent_distinct_paths(&[
            &paths.target_path,
            &paths.staging_path,
            &paths.rollback_tombstone_path,
        ])?;
        Ok(paths)
    }

    pub(crate) fn target_path(&self) -> &Path {
        &self.target_path
    }

    pub(crate) fn staging_path(&self) -> &Path {
        &self.staging_path
    }

    pub(crate) fn rollback_tombstone_path(&self) -> &Path {
        &self.rollback_tombstone_path
    }
}

impl HandleDeletePaths {
    pub(crate) fn from_persisted_plan(
        target_path: PathBuf,
        recovery_path: PathBuf,
    ) -> Result<Self, String> {
        let paths = Self {
            target_path,
            recovery_path,
        };
        validate_same_parent_distinct_paths(&[&paths.target_path, &paths.recovery_path])?;
        Ok(paths)
    }
}

impl HardlinkSourceGuard {
    pub(crate) fn acquire(
        path: &Path,
        expected_sha256: &str,
        expected_identity: RegularFileIdentity,
    ) -> Result<Self, String> {
        let parent = ParentDirectoryGuard::acquire_for_path(path)?;
        let mut readable = open_identity_read(path)?;
        if digest_handle(&mut readable)?.1 != expected_sha256
            || regular_file_identity(&readable)? != expected_identity
        {
            return Err("managed hardlink recovery source changed".to_string());
        }
        drop(readable);
        let file = open_identity_only(path)?;
        verify_path_identity(path, &file)?;
        if regular_file_identity(&file)? != expected_identity {
            return Err("managed hardlink recovery source identity changed".to_string());
        }
        let guard = Self {
            path: path.to_path_buf(),
            file,
            parent,
            identity: expected_identity,
            expected_sha256: expected_sha256.to_string(),
        };
        guard.verify_current_path()?;
        Ok(guard)
    }

    fn verify_current_path(&self) -> Result<(), String> {
        self.parent.verify_current_path()?;
        verify_path_identity(&self.path, &self.file)?;
        if regular_file_identity(&self.file)? != self.identity {
            return Err("managed hardlink recovery source identity changed".to_string());
        }
        Ok(())
    }

    fn verify_exclusive_current_path(&self) -> Result<(), String> {
        self.verify_current_path()?;
        let mut guard = WriteExclusionGuard::acquire(&self.path)?;
        guard.verify_current_path(Some(&self.expected_sha256))?;
        if guard.identity()? != self.identity {
            return Err("managed hardlink recovery source identity changed".to_string());
        }
        self.verify_current_path()
    }
}

impl WriteExclusionGuard {
    pub(crate) fn acquire(path: &Path) -> Result<Self, String> {
        let parent = ParentDirectoryGuard::acquire_for_path(path)?;
        let file = open_guarded(path, GuardMode::BlockWrites)?;
        let mut guard = Self {
            path: path.to_path_buf(),
            file,
            parent,
        };
        guard.verify_current_path(None)?;
        Ok(guard)
    }

    pub(crate) fn verify_current_path(
        &mut self,
        expected_sha256: Option<&str>,
    ) -> Result<(u64, String), String> {
        verify_guarded_path(&self.path, &mut self.file, expected_sha256)
    }

    /// Copies the exact held object into a new operation-owned file while the
    /// live writer/delete barrier remains active. This is used to run SQLite
    /// read-only validation without reopening the guarded live pathname (the
    /// SQLite Windows VFS does not share DELETE access with this handle).
    pub(crate) fn copy_current_to_new_file(
        &mut self,
        target: &Path,
        expected_sha256: Option<&str>,
    ) -> Result<(u64, String), String> {
        if !target.is_absolute() {
            return Err("managed guarded copy target must be absolute".to_string());
        }
        let parent = target
            .parent()
            .ok_or_else(|| "managed guarded copy target has no parent".to_string())?;
        let parent_metadata = fs::symlink_metadata(parent)
            .map_err(|_| "managed guarded copy parent is unavailable".to_string())?;
        if !parent_metadata.is_dir() || metadata_is_link_or_reparse(&parent_metadata) {
            return Err("managed guarded copy parent is unsafe".to_string());
        }

        let source_digest = self.verify_current_path(expected_sha256)?;
        let mut output = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(target)
            .map_err(|_| "managed guarded copy target could not be created".to_string())?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|_| "managed file barrier is unreadable".to_string())?;
        let copied = io::copy(&mut self.file, &mut output)
            .map_err(|_| "managed guarded copy failed".to_string())?;
        output
            .sync_all()
            .map_err(|_| "managed guarded copy could not be synced".to_string())?;
        if copied != source_digest.0
            || digest_handle(&mut output)? != source_digest
            || verify_path_identity(target, &output).is_err()
        {
            return Err("managed guarded copy verification failed".to_string());
        }
        if self.verify_current_path(Some(&source_digest.1))? != source_digest {
            return Err("managed file changed during guarded copy".to_string());
        }
        Ok(source_digest)
    }

    pub(crate) fn identity(&self) -> Result<RegularFileIdentity, String> {
        regular_file_identity(&self.file)
    }

    /// Verifies that `witness` names the exact file object held by this writer
    /// barrier without requesting a second READ/DELETE handle on that object.
    /// Identity-only access remains compatible with the barrier's no-delete
    /// share mode, so callers do not have to weaken or release the guard.
    pub(crate) fn verify_same_identity_path(
        &mut self,
        witness: &Path,
        expected_sha256: Option<&str>,
    ) -> Result<RegularFileIdentity, String> {
        self.verify_current_path(expected_sha256)?;
        let witness_file = open_identity_only(witness)?;
        verify_path_identity(witness, &witness_file)?;
        let identity = self.identity()?;
        if regular_file_identity(&witness_file)? != identity {
            return Err("managed ownership witness identity changed".to_string());
        }
        Ok(identity)
    }

    fn rename_no_replace_by_handle(&mut self, target: &Path) -> Result<(), String> {
        self.verify_current_path(None)?;
        self.parent.verify_current_path()?;
        rename_destructive_file(&self.file, &self.path, target, &self.parent)?;
        // SetFileInformationByHandle succeeded for this exact object. Do not
        // introduce a fallible pathname observation after the irreversible
        // rename; the caller must always receive the typed token.
        self.path = target.to_path_buf();
        Ok(())
    }

    /// Prepares a fail-closed two-phase replacement without ever performing a
    /// path-based clobber.
    ///
    /// This consumes the guard. The exact old object is first moved by handle
    /// to `recovery_path` with no-replace semantics. The copied replacement is
    /// independently held by a delete-access handle. The returned token keeps
    /// both identities guarded; the caller must durably persist the recovery
    /// path before publishing.
    pub(crate) fn stage_handle_replace(
        mut self,
        source: &Path,
        expected_source_sha256: &str,
        paths: &HandleReplacePaths,
    ) -> Result<StagedHandleReplace, String> {
        validate_replace_paths(paths)?;
        if self.path != paths.target_path {
            return Err("managed replacement plan target does not match the guard".to_string());
        }
        self.parent.verify_paths(paths)?;
        self.verify_current_path(None)?;
        let replacement_parent = self.parent.try_clone()?;

        let mut source = File::open(source)
            .map_err(|_| "managed replacement source is unavailable".to_string())?;
        let source_digest = digest_handle(&mut source)?;
        if source_digest.1 != expected_source_sha256 {
            return Err("managed replacement source changed".to_string());
        }
        source
            .seek(SeekFrom::Start(0))
            .map_err(|_| "managed replacement source is unreadable".to_string())?;

        let mut staging = create_replacement_staging(&paths.staging_path)?;
        let copied = (|| {
            io::copy(&mut source, &mut staging)
                .map_err(|_| "managed replacement target write failed".to_string())?;
            staging
                .flush()
                .and_then(|_| staging.sync_all())
                .map_err(|_| "managed replacement target flush failed".to_string())
        })();
        if let Err(error) = copied {
            let cleanup = delete_destructive_file(&staging);
            drop(staging);
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!(
                    "{error}; deterministic staging cleanup failed: {cleanup}"
                )),
            };
        }

        let mut replacement = DestructiveFileGuard {
            path: paths.staging_path.clone(),
            file: staging,
            parent: replacement_parent,
        };
        if let Err(error) = replacement.verify_current_path(Some(expected_source_sha256)) {
            return Err(cleanup_failed_staging(error, replacement));
        }
        Ok(StagedHandleReplace {
            paths: paths.clone(),
            previous: self,
            replacement,
            expected_replacement_sha256: expected_source_sha256.to_string(),
        })
    }

    /// Compatibility entry point. New durable callers should use
    /// `stage_handle_replace`, persist `identity_bindings()`, then call
    /// `StagedHandleReplace::prepare`.
    #[cfg(test)]
    pub(crate) fn prepare_handle_replace(
        self,
        source: &Path,
        expected_source_sha256: &str,
        paths: &HandleReplacePaths,
    ) -> Result<PreparedHandleReplace, String> {
        self.stage_handle_replace(source, expected_source_sha256, paths)?
            .prepare()
            .map_err(|(error, _staged)| error)
    }

    /// Prepares the same two-phase replacement while preserving the exact file
    /// identity of `source`. The staged name is a same-directory hard link,
    /// never a byte copy, and its handle identity is verified against the
    /// opened source before the old target moves to recovery.
    pub(crate) fn stage_handle_hardlink_replace(
        mut self,
        source: &Path,
        expected_source_sha256: &str,
        paths: &HandleReplacePaths,
    ) -> Result<StagedHandleReplace, String> {
        validate_replace_paths(paths)?;
        if self.path != paths.target_path {
            return Err("managed replacement plan target does not match the guard".to_string());
        }
        self.parent.verify_paths(paths)?;
        self.verify_current_path(None)?;
        let replacement_parent = self.parent.try_clone()?;

        let mut source_file = open_identity_read(source)
            .map_err(|_| "managed hardlink replacement source is unavailable".to_string())?;
        if digest_handle(&mut source_file)?.1 != expected_source_sha256 {
            return Err("managed hardlink replacement source changed".to_string());
        }

        fs::hard_link(source, &paths.staging_path).map_err(|_| {
            "managed hardlink replacement staging link could not be created".to_string()
        })?;
        let replacement_file = match open_prepared_replacement(&paths.staging_path) {
            Ok(file) => file,
            Err(error) => {
                return Err(cleanup_failed_hardlink_alias(
                    &paths.staging_path,
                    &source_file,
                    error,
                ));
            }
        };
        let mut replacement = DestructiveFileGuard {
            path: paths.staging_path.clone(),
            file: replacement_file,
            parent: replacement_parent,
        };
        if let Err(error) = same_regular_file_identity(&source_file, &replacement.file) {
            return Err(cleanup_failed_staging(error, replacement));
        }
        if let Err(error) = replacement.verify_current_path(Some(expected_source_sha256)) {
            return Err(cleanup_failed_staging(error, replacement));
        }
        if let Err(error) = self.verify_current_path(None) {
            return Err(cleanup_failed_staging(error, replacement));
        }
        if let Err(error) = same_regular_file_identity(&source_file, &replacement.file) {
            return Err(cleanup_failed_staging(error, replacement));
        }

        Ok(StagedHandleReplace {
            paths: paths.clone(),
            previous: self,
            replacement,
            expected_replacement_sha256: expected_source_sha256.to_string(),
        })
    }

    #[cfg(test)]
    pub(crate) fn prepare_handle_hardlink_replace(
        self,
        source: &Path,
        expected_source_sha256: &str,
        paths: &HandleReplacePaths,
    ) -> Result<PreparedHandleReplace, String> {
        self.stage_handle_hardlink_replace(source, expected_source_sha256, paths)?
            .prepare()
            .map_err(|(error, _staged)| error)
    }
}

impl StagedHandleReplace {
    pub(crate) fn paths(&self) -> &HandleReplacePaths {
        &self.paths
    }

    pub(crate) fn identity_bindings(&self) -> Result<HandleReplaceIdentityBindings, String> {
        Ok(HandleReplaceIdentityBindings {
            parent_identity: self.previous.parent.identity,
            original_identity: self.previous.identity()?,
            replacement_identity: self.replacement.identity()?,
        })
    }

    /// Moves the exact original object to its persisted recovery leaf. All
    /// fallible checks precede the syscall. A successful syscall is followed
    /// only by infallible field moves into the prepared token.
    pub(crate) fn prepare(mut self) -> Result<PreparedHandleReplace, (String, Self)> {
        if let Err(error) = self.previous.verify_current_path(None) {
            return Err((error, self));
        }
        if let Err(error) = self.previous.parent.verify_paths(&self.paths) {
            return Err((error, self));
        }
        if let Err(error) = self
            .replacement
            .verify_current_path(Some(&self.expected_replacement_sha256))
        {
            return Err((error, self));
        }
        if let Err(error) = rename_destructive_file(
            &self.previous.file,
            &self.previous.path,
            &self.paths.recovery_path,
            &self.previous.parent,
        ) {
            return Err((error, self));
        }
        self.previous.path = self.paths.recovery_path.clone();
        Ok(PreparedHandleReplace {
            paths: self.paths,
            previous: self.previous,
            replacement: self.replacement,
            expected_replacement_sha256: self.expected_replacement_sha256,
        })
    }

    /// Resolves a pre-prepare crash by retaining the original target and
    /// scheduling only the exact staged identity for terminal cleanup.
    pub(crate) fn restore(self) -> Result<ResolvedHandleReplace, (String, Self)> {
        let artifact = match cleanup_artifact(&self.replacement, &self.expected_replacement_sha256)
        {
            Ok(artifact) => artifact,
            Err(error) => return Err((error, self)),
        };
        Ok(ResolvedHandleReplace {
            guard: self.previous,
            paths: self.paths.clone(),
            cleanup_artifacts: vec![artifact],
        })
    }
}

impl PreparedHandleReplace {
    pub(crate) fn paths(&self) -> &HandleReplacePaths {
        &self.paths
    }

    #[cfg(test)]
    pub(crate) fn recovery_path(&self) -> &Path {
        &self.paths.recovery_path
    }

    /// Publishes the exact prepared replacement object with no-replace
    /// semantics. A DELETE-access contender that installs a different object at
    /// the target name makes this fail instead of being overwritten.
    pub(crate) fn publish(mut self) -> Result<PublishedHandleReplace, (String, Self)> {
        if let Err(error) = self.previous.verify_current_path(None) {
            return Err((error, self));
        }
        if let Err(error) = self
            .replacement
            .verify_current_path(Some(&self.expected_replacement_sha256))
        {
            return Err((error, self));
        }
        if let Err(error) = self.replacement.rename_no_replace(&self.paths.target_path) {
            return Err((error, self));
        }
        Ok(PublishedHandleReplace {
            paths: self.paths,
            previous: self.previous,
            replacement: self.replacement,
            expected_replacement_sha256: self.expected_replacement_sha256,
        })
    }

    /// Restores the exact previous file object when publication has not
    /// occurred. Restoration never replaces a contender at the target name.
    pub(crate) fn restore(mut self) -> Result<ResolvedHandleReplace, (String, Self)> {
        let artifact = match cleanup_artifact(&self.replacement, &self.expected_replacement_sha256)
        {
            Ok(artifact) => artifact,
            Err(error) => return Err((error, self)),
        };
        if let Err(error) = self
            .previous
            .rename_no_replace_by_handle(&self.paths.target_path)
        {
            return Err((error, self));
        }
        Ok(ResolvedHandleReplace {
            guard: self.previous,
            paths: self.paths.clone(),
            cleanup_artifacts: vec![artifact],
        })
    }
}

impl PublishedHandleReplace {
    pub(crate) fn paths(&self) -> &HandleReplacePaths {
        &self.paths
    }

    #[cfg(test)]
    pub(crate) fn recovery_path(&self) -> &Path {
        &self.paths.recovery_path
    }

    /// Commits the replacement and returns a continuous write exclusion guard.
    /// The caller remains responsible for retaining or deleting the recorded
    /// recovery object according to its durable ledger.
    pub(crate) fn commit(mut self) -> Result<ResolvedHandleReplace, (String, Self)> {
        let previous_artifact = match cleanup_write_artifact(&mut self.previous) {
            Ok(artifact) => artifact,
            Err(error) => return Err((error, self)),
        };
        match downgrade_to_write_exclusion(self.replacement) {
            Ok(guard) => Ok(ResolvedHandleReplace {
                guard,
                paths: self.paths.clone(),
                cleanup_artifacts: vec![previous_artifact],
            }),
            Err((error, replacement)) => Err((
                error,
                Self {
                    replacement,
                    ..self
                },
            )),
        }
    }

    /// Rolls back only when the target still names this operation's exact
    /// replacement object. Both renames are no-clobber and handle-bound.
    pub(crate) fn restore(mut self) -> Result<ResolvedHandleReplace, (String, Self)> {
        let mut artifact =
            match cleanup_artifact(&self.replacement, &self.expected_replacement_sha256) {
                Ok(artifact) => artifact,
                Err(error) => return Err((error, self)),
            };
        if let Err(error) = self
            .replacement
            .verify_current_path(Some(&self.expected_replacement_sha256))
        {
            return Err((error, self));
        }
        if let Err(error) = self
            .replacement
            .rename_no_replace(&self.paths.rollback_tombstone_path)
        {
            return Err((error, self));
        }
        // The cleanup descriptor was identity-bound before the irreversible
        // rename. It must follow that exact held object to its persisted
        // tombstone name rather than pointing back at the restored target.
        artifact.path = self.paths.rollback_tombstone_path.clone();
        if let Err(error) = self
            .previous
            .rename_no_replace_by_handle(&self.paths.target_path)
        {
            return Err((error, self));
        }
        Ok(ResolvedHandleReplace {
            guard: self.previous,
            paths: self.paths.clone(),
            cleanup_artifacts: vec![artifact],
        })
    }
}

impl ResolvedHandleReplace {
    pub(crate) fn paths(&self) -> &HandleReplacePaths {
        &self.paths
    }

    pub(crate) fn guard_mut(&mut self) -> &mut WriteExclusionGuard {
        &mut self.guard
    }

    /// Keeps every operation-bound artifact and returns the active guard. Use
    /// this when the terminal ledger could not be durably persisted.
    pub(crate) fn retain_for_recovery(self) -> WriteExclusionGuard {
        self.guard
    }

    /// Deletes only the exact typed artifacts selected by the completed
    /// transition, after the caller has durably persisted its terminal state.
    pub(crate) fn cleanup_after_durable_terminal(
        self,
    ) -> Result<WriteExclusionGuard, (String, Self)> {
        for artifact in &self.cleanup_artifacts {
            let mut guard = match DestructiveFileGuard::acquire(&artifact.path) {
                Ok(guard) => guard,
                Err(error) => match fs::symlink_metadata(&artifact.path) {
                    Err(missing) if missing.kind() == io::ErrorKind::NotFound => continue,
                    Ok(_) | Err(_) => return Err((error, self)),
                },
            };
            if regular_file_identity(&guard.file).ok() != Some(artifact.expected_identity) {
                return Err((
                    "managed replacement cleanup artifact identity changed".to_string(),
                    self,
                ));
            }
            if let Err(error) = guard.verify_current_path(Some(&artifact.expected_sha256)) {
                return Err((error, self));
            }
            if let Err(error) = guard.delete() {
                return Err((error, self));
            }
        }
        Ok(self.guard)
    }
}

/// Builds an operation-owned file at a deterministic persisted sibling name.
/// The target must be absent. The returned token pins both the parent namespace
/// and exact staged identity until the caller publishes or restores it.
#[cfg(test)]
pub(crate) fn stage_handle_create(
    source: &Path,
    expected_source_sha256: &str,
    paths: &HandleCreatePaths,
) -> Result<StagedHandleCreate, String> {
    validate_same_parent_distinct_paths(&[
        &paths.target_path,
        &paths.staging_path,
        &paths.rollback_tombstone_path,
    ])?;
    let parent = ParentDirectoryGuard::acquire_for_path(&paths.target_path)?;
    ensure_absent_leaf(&paths.target_path)?;
    ensure_absent_leaf(&paths.rollback_tombstone_path)?;
    let mut source = open_identity_read(source)?;
    if digest_handle(&mut source)?.1 != expected_source_sha256 {
        return Err("managed create source changed".to_string());
    }
    source
        .seek(SeekFrom::Start(0))
        .map_err(|_| "managed create source is unreadable".to_string())?;
    let mut staging = create_replacement_staging(&paths.staging_path)?;
    let copied = (|| {
        io::copy(&mut source, &mut staging)
            .map_err(|_| "managed create staging write failed".to_string())?;
        staging
            .flush()
            .and_then(|_| staging.sync_all())
            .map_err(|_| "managed create staging flush failed".to_string())
    })();
    if let Err(error) = copied {
        let cleanup = delete_destructive_file(&staging);
        drop(staging);
        return match cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => Err(format!(
                "{error}; deterministic create staging cleanup failed: {cleanup}"
            )),
        };
    }
    let mut created = DestructiveFileGuard {
        path: paths.staging_path.clone(),
        file: staging,
        parent,
    };
    if let Err(error) = created.verify_current_path(Some(expected_source_sha256)) {
        return Err(cleanup_failed_staging(error, created));
    }
    Ok(StagedHandleCreate {
        paths: paths.clone(),
        created,
        expected_sha256: expected_source_sha256.to_string(),
    })
}

/// Hard-link variant for callers that persisted an operation-owned witness
/// identity before live mutation. `source` and the staged/published object are
/// the exact same file object across a crash.
pub(crate) fn stage_handle_hardlink_create(
    source: &Path,
    expected_source_sha256: &str,
    paths: &HandleCreatePaths,
) -> Result<StagedHandleCreate, String> {
    validate_same_parent_distinct_paths(&[
        &paths.target_path,
        &paths.staging_path,
        &paths.rollback_tombstone_path,
    ])?;
    let parent = ParentDirectoryGuard::acquire_for_path(&paths.target_path)?;
    ensure_absent_leaf(&paths.target_path)?;
    ensure_absent_leaf(&paths.staging_path)?;
    ensure_absent_leaf(&paths.rollback_tombstone_path)?;
    let mut readable_source = open_identity_read(source)?;
    if digest_handle(&mut readable_source)?.1 != expected_source_sha256 {
        return Err("managed hardlink create source changed".to_string());
    }
    let source_identity = regular_file_identity(&readable_source)?;
    drop(readable_source);
    let source_file = open_identity_only(source)?;
    if regular_file_identity(&source_file)? != source_identity {
        return Err("managed hardlink create source identity changed".to_string());
    }
    fs::hard_link(source, &paths.staging_path)
        .map_err(|_| "managed hardlink create staging could not be created".to_string())?;
    (|| {
        let mut created = match DestructiveFileGuard::acquire(&paths.staging_path) {
            Ok(created) => created,
            Err(error) => {
                return Err(cleanup_failed_hardlink_alias(
                    &paths.staging_path,
                    &source_file,
                    error,
                ));
            }
        };
        let created_identity = match created.identity() {
            Ok(identity) => identity,
            Err(error) => return Err(cleanup_failed_staging(error, created)),
        };
        if created.parent.identity != parent.identity || created_identity != source_identity {
            return Err(cleanup_failed_staging(
                "managed hardlink create source identity changed".to_string(),
                created,
            ));
        }
        if let Err(error) = created.verify_current_path(Some(expected_source_sha256)) {
            return Err(cleanup_failed_staging(error, created));
        }
        if let Err(error) = created.verify_same_identity_path(source, Some(expected_source_sha256))
        {
            return Err(cleanup_failed_staging(error, created));
        }
        Ok(StagedHandleCreate {
            paths: paths.clone(),
            created,
            expected_sha256: expected_source_sha256.to_string(),
        })
    })()
}

impl StagedHandleCreate {
    pub(crate) fn identity_bindings(&self) -> Result<HandleCreateIdentityBindings, String> {
        Ok(HandleCreateIdentityBindings {
            parent_identity: self.created.parent.identity,
            created_identity: self.created.identity()?,
        })
    }

    pub(crate) fn publish(mut self) -> Result<PublishedHandleCreate, (String, Self)> {
        if let Err(error) = ensure_absent_leaf(&self.paths.target_path) {
            return Err((error, self));
        }
        if let Err(error) = self
            .created
            .verify_current_path(Some(&self.expected_sha256))
        {
            return Err((error, self));
        }
        if let Err(error) = self.created.rename_no_replace(&self.paths.target_path) {
            return Err((error, self));
        }
        Ok(PublishedHandleCreate {
            paths: self.paths,
            created: self.created,
            expected_sha256: self.expected_sha256,
        })
    }

    pub(crate) fn restore(self) -> Result<ResolvedHandleCreate, (String, Self)> {
        let artifact = match cleanup_artifact(&self.created, &self.expected_sha256) {
            Ok(artifact) => artifact,
            Err(error) => return Err((error, self)),
        };
        Ok(ResolvedHandleCreate {
            paths: self.paths.clone(),
            guard: None,
            cleanup_artifacts: vec![artifact],
        })
    }
}

impl PublishedHandleCreate {
    pub(crate) fn commit(self) -> Result<ResolvedHandleCreate, (String, Self)> {
        match downgrade_to_write_exclusion(self.created) {
            Ok(guard) => Ok(ResolvedHandleCreate {
                paths: self.paths.clone(),
                guard: Some(guard),
                cleanup_artifacts: Vec::new(),
            }),
            Err((error, created)) => Err((error, Self { created, ..self })),
        }
    }

    pub(crate) fn restore(mut self) -> Result<ResolvedHandleCreate, (String, Self)> {
        let artifact = match cleanup_artifact(&self.created, &self.expected_sha256) {
            Ok(mut artifact) => {
                artifact.path = self.paths.rollback_tombstone_path.clone();
                artifact
            }
            Err(error) => return Err((error, self)),
        };
        if let Err(error) = self
            .created
            .rename_no_replace(&self.paths.rollback_tombstone_path)
        {
            return Err((error, self));
        }
        Ok(ResolvedHandleCreate {
            paths: self.paths.clone(),
            guard: None,
            cleanup_artifacts: vec![artifact],
        })
    }
}

impl ResolvedHandleCreate {
    pub(crate) fn retain_for_recovery(self) -> Option<WriteExclusionGuard> {
        self.guard
    }

    pub(crate) fn cleanup_after_durable_terminal(
        self,
    ) -> Result<Option<WriteExclusionGuard>, (String, Self)> {
        if let Err(error) = cleanup_owned_artifacts(&self.cleanup_artifacts) {
            return Err((error, self));
        }
        Ok(self.guard)
    }
}

pub(crate) fn classify_handle_create_crash_state(
    paths: &HandleCreatePaths,
    identities: HandleCreateIdentityBindings,
    expected_sha256: &str,
) -> Result<HandleCreateCrashState, String> {
    validate_same_parent_distinct_paths(&[
        &paths.target_path,
        &paths.staging_path,
        &paths.rollback_tombstone_path,
    ])?;
    let parent = ParentDirectoryGuard::acquire_for_path(&paths.target_path)?;
    if parent.identity != identities.parent_identity {
        return Err("managed create parent directory identity changed".to_string());
    }
    let target = optional_path_observation(&paths.target_path)?;
    let staging = optional_path_observation(&paths.staging_path)?;
    let tombstone = optional_path_observation(&paths.rollback_tombstone_path)?;
    let owned = |value: &Option<(String, RegularFileIdentity)>| {
        value.as_ref().is_some_and(|(digest, identity)| {
            digest == expected_sha256 && *identity == identities.created_identity
        })
    };
    match (&target, &staging, &tombstone) {
        (None, None, None) => Ok(HandleCreateCrashState::Absent),
        (None, value, None) if owned(value) => Ok(HandleCreateCrashState::Staged),
        (value, None, None) if owned(value) => Ok(HandleCreateCrashState::Published),
        (None, None, value) if owned(value) => Ok(HandleCreateCrashState::RollbackPrepared),
        _ => Err("managed create crash state is unknown".to_string()),
    }
}

pub(crate) fn recover_handle_create(
    paths: &HandleCreatePaths,
    identities: HandleCreateIdentityBindings,
    expected_sha256: &str,
    decision: HandleCreateRecoveryDecision,
) -> Result<ResolvedHandleCreate, String> {
    recover_handle_create_inner(paths, identities, expected_sha256, decision, None)
}

/// Recovers a create whose operation-owned object is also reachable through a
/// retained source hard-link. The source parent and identity are pinned before
/// crash-state classification, and every destructive artifact handle is
/// compared with that exact source name before any rename or cleanup occurs.
pub(crate) fn recover_handle_hardlink_create(
    paths: &HandleCreatePaths,
    identities: HandleCreateIdentityBindings,
    expected_sha256: &str,
    decision: HandleCreateRecoveryDecision,
    source: &HardlinkSourceGuard,
) -> Result<ResolvedHandleCreate, String> {
    source.verify_current_path()?;
    if source.identity != identities.created_identity || source.expected_sha256 != expected_sha256 {
        return Err("managed hardlink recovery source binding changed".to_string());
    }
    recover_handle_create_inner(paths, identities, expected_sha256, decision, Some(source))
}

fn recover_handle_create_inner(
    paths: &HandleCreatePaths,
    identities: HandleCreateIdentityBindings,
    expected_sha256: &str,
    decision: HandleCreateRecoveryDecision,
    source: Option<&HardlinkSourceGuard>,
) -> Result<ResolvedHandleCreate, String> {
    match classify_handle_create_crash_state(paths, identities, expected_sha256)? {
        HandleCreateCrashState::Absent => {
            if decision != HandleCreateRecoveryDecision::Restore {
                return Err("managed create cannot commit from absent layout".to_string());
            }
            if let Some(source) = source {
                source.verify_exclusive_current_path()?;
            }
            Ok(ResolvedHandleCreate {
                paths: paths.clone(),
                guard: None,
                cleanup_artifacts: Vec::new(),
            })
        }
        HandleCreateCrashState::Staged => {
            let mut created =
                acquire_destructive_create_bound(&paths.staging_path, identities, expected_sha256)?;
            verify_hardlink_recovery_source(&mut created, source, expected_sha256)?;
            let staged = StagedHandleCreate {
                paths: paths.clone(),
                created,
                expected_sha256: expected_sha256.to_string(),
            };
            match decision {
                HandleCreateRecoveryDecision::Commit => staged
                    .publish()
                    .map_err(|(error, _)| error)?
                    .commit()
                    .map_err(|(error, _)| error),
                HandleCreateRecoveryDecision::Restore => {
                    staged.restore().map_err(|(error, _)| error)
                }
            }
        }
        HandleCreateCrashState::Published => {
            let mut created =
                acquire_destructive_create_bound(&paths.target_path, identities, expected_sha256)?;
            verify_hardlink_recovery_source(&mut created, source, expected_sha256)?;
            let published = PublishedHandleCreate {
                paths: paths.clone(),
                created,
                expected_sha256: expected_sha256.to_string(),
            };
            match decision {
                HandleCreateRecoveryDecision::Commit => {
                    published.commit().map_err(|(error, _)| error)
                }
                HandleCreateRecoveryDecision::Restore => {
                    published.restore().map_err(|(error, _)| error)
                }
            }
        }
        HandleCreateCrashState::RollbackPrepared => {
            if decision != HandleCreateRecoveryDecision::Restore {
                return Err("managed create rollback cannot be committed".to_string());
            }
            let mut created = acquire_destructive_create_bound(
                &paths.rollback_tombstone_path,
                identities,
                expected_sha256,
            )?;
            verify_hardlink_recovery_source(&mut created, source, expected_sha256)?;
            let artifact = cleanup_artifact(&created, expected_sha256)?;
            Ok(ResolvedHandleCreate {
                paths: paths.clone(),
                guard: None,
                cleanup_artifacts: vec![artifact],
            })
        }
    }
}

fn verify_hardlink_recovery_source(
    created: &mut DestructiveFileGuard,
    source: Option<&HardlinkSourceGuard>,
    expected_sha256: &str,
) -> Result<(), String> {
    let Some(source) = source else {
        return Ok(());
    };
    source.verify_current_path()?;
    if created.verify_same_identity_path(&source.path, Some(expected_sha256))? != source.identity {
        return Err("managed hardlink recovery source identity changed".to_string());
    }
    source.verify_current_path()
}

pub(crate) fn stage_handle_delete(
    paths: &HandleDeletePaths,
    expected_sha256: &str,
) -> Result<StagedHandleDelete, String> {
    validate_same_parent_distinct_paths(&[&paths.target_path, &paths.recovery_path])?;
    ensure_absent_leaf(&paths.recovery_path)?;
    let mut target = DestructiveFileGuard::acquire(&paths.target_path)?;
    target.verify_current_path(Some(expected_sha256))?;
    Ok(StagedHandleDelete {
        paths: paths.clone(),
        target,
        expected_sha256: expected_sha256.to_string(),
    })
}

impl StagedHandleDelete {
    pub(crate) fn identity_bindings(&self) -> Result<HandleDeleteIdentityBindings, String> {
        Ok(HandleDeleteIdentityBindings {
            parent_identity: self.target.parent.identity,
            deleted_identity: self.target.identity()?,
        })
    }

    pub(crate) fn prepare(mut self) -> Result<PreparedHandleDelete, (String, Self)> {
        if let Err(error) = ensure_absent_leaf(&self.paths.recovery_path) {
            return Err((error, self));
        }
        if let Err(error) = self.target.verify_current_path(Some(&self.expected_sha256)) {
            return Err((error, self));
        }
        if let Err(error) = self.target.rename_no_replace(&self.paths.recovery_path) {
            return Err((error, self));
        }
        Ok(PreparedHandleDelete {
            paths: self.paths,
            target: self.target,
            expected_sha256: self.expected_sha256,
        })
    }

    pub(crate) fn restore(self) -> Result<ResolvedHandleDelete, (String, Self)> {
        match downgrade_to_write_exclusion(self.target) {
            Ok(guard) => Ok(ResolvedHandleDelete {
                paths: self.paths.clone(),
                guard: Some(guard),
                cleanup_artifacts: Vec::new(),
            }),
            Err((error, target)) => Err((error, Self { target, ..self })),
        }
    }
}

impl PreparedHandleDelete {
    pub(crate) fn commit(self) -> Result<ResolvedHandleDelete, (String, Self)> {
        let artifact = match cleanup_artifact(&self.target, &self.expected_sha256) {
            Ok(artifact) => artifact,
            Err(error) => return Err((error, self)),
        };
        Ok(ResolvedHandleDelete {
            paths: self.paths.clone(),
            guard: None,
            cleanup_artifacts: vec![artifact],
        })
    }

    pub(crate) fn restore(mut self) -> Result<ResolvedHandleDelete, (String, Self)> {
        if let Err(error) = self.target.rename_no_replace(&self.paths.target_path) {
            return Err((error, self));
        }
        match downgrade_to_write_exclusion(self.target) {
            Ok(guard) => Ok(ResolvedHandleDelete {
                paths: self.paths.clone(),
                guard: Some(guard),
                cleanup_artifacts: Vec::new(),
            }),
            Err((error, target)) => Err((error, Self { target, ..self })),
        }
    }
}

impl ResolvedHandleDelete {
    pub(crate) fn cleanup_after_durable_terminal(
        self,
    ) -> Result<Option<WriteExclusionGuard>, (String, Self)> {
        if let Err(error) = cleanup_owned_artifacts(&self.cleanup_artifacts) {
            return Err((error, self));
        }
        Ok(self.guard)
    }
}

pub(crate) fn classify_handle_delete_crash_state(
    paths: &HandleDeletePaths,
    identities: HandleDeleteIdentityBindings,
    expected_sha256: &str,
) -> Result<HandleDeleteCrashState, String> {
    validate_same_parent_distinct_paths(&[&paths.target_path, &paths.recovery_path])?;
    let parent = ParentDirectoryGuard::acquire_for_path(&paths.target_path)?;
    if parent.identity != identities.parent_identity {
        return Err("managed delete parent directory identity changed".to_string());
    }
    let target = optional_path_observation(&paths.target_path)?;
    let recovery = optional_path_observation(&paths.recovery_path)?;
    let owned = |value: &Option<(String, RegularFileIdentity)>| {
        value.as_ref().is_some_and(|(digest, identity)| {
            digest == expected_sha256 && *identity == identities.deleted_identity
        })
    };
    match (&target, &recovery) {
        (value, None) if owned(value) => Ok(HandleDeleteCrashState::Original),
        (None, value) if owned(value) => Ok(HandleDeleteCrashState::Prepared),
        (None, None) => Ok(HandleDeleteCrashState::Deleted),
        _ => Err("managed delete crash state is unknown".to_string()),
    }
}

pub(crate) fn recover_handle_delete(
    paths: &HandleDeletePaths,
    identities: HandleDeleteIdentityBindings,
    expected_sha256: &str,
    decision: HandleDeleteRecoveryDecision,
) -> Result<ResolvedHandleDelete, String> {
    match classify_handle_delete_crash_state(paths, identities, expected_sha256)? {
        HandleDeleteCrashState::Original => {
            let staged = StagedHandleDelete {
                paths: paths.clone(),
                target: acquire_destructive_delete_bound(
                    &paths.target_path,
                    identities,
                    expected_sha256,
                )?,
                expected_sha256: expected_sha256.to_string(),
            };
            match decision {
                HandleDeleteRecoveryDecision::Commit => staged
                    .prepare()
                    .map_err(|(error, _)| error)?
                    .commit()
                    .map_err(|(error, _)| error),
                HandleDeleteRecoveryDecision::Restore => {
                    staged.restore().map_err(|(error, _)| error)
                }
            }
        }
        HandleDeleteCrashState::Prepared => {
            let prepared = PreparedHandleDelete {
                paths: paths.clone(),
                target: acquire_destructive_delete_bound(
                    &paths.recovery_path,
                    identities,
                    expected_sha256,
                )?,
                expected_sha256: expected_sha256.to_string(),
            };
            match decision {
                HandleDeleteRecoveryDecision::Commit => {
                    prepared.commit().map_err(|(error, _)| error)
                }
                HandleDeleteRecoveryDecision::Restore => {
                    prepared.restore().map_err(|(error, _)| error)
                }
            }
        }
        HandleDeleteCrashState::Deleted => {
            if decision != HandleDeleteRecoveryDecision::Commit {
                return Err("managed deleted file cannot be restored after cleanup".to_string());
            }
            Ok(ResolvedHandleDelete {
                paths: paths.clone(),
                guard: None,
                cleanup_artifacts: Vec::new(),
            })
        }
    }
}

fn cleanup_write_artifact(guard: &mut WriteExclusionGuard) -> Result<CleanupArtifact, String> {
    let sha256 = guard.verify_current_path(None)?.1;
    Ok(CleanupArtifact {
        path: guard.path.clone(),
        expected_sha256: sha256,
        expected_identity: regular_file_identity(&guard.file)?,
    })
}

fn cleanup_artifact(
    guard: &DestructiveFileGuard,
    expected_sha256: &str,
) -> Result<CleanupArtifact, String> {
    Ok(CleanupArtifact {
        path: guard.path.clone(),
        expected_sha256: expected_sha256.to_string(),
        expected_identity: regular_file_identity(&guard.file)?,
    })
}

fn downgrade_to_write_exclusion(
    destructive: DestructiveFileGuard,
) -> Result<WriteExclusionGuard, (String, DestructiveFileGuard)> {
    let DestructiveFileGuard { path, file, parent } = destructive;
    Ok(WriteExclusionGuard { path, file, parent })
}

#[cfg(windows)]
fn same_regular_file_identity(left: &File, right: &File) -> Result<(), String> {
    if regular_file_identity(left)? != regular_file_identity(right)? {
        return Err("managed hardlink replacement source identity changed".to_string());
    }
    Ok(())
}

pub(crate) fn regular_file_identity_at_path(path: &Path) -> Result<RegularFileIdentity, String> {
    let file = open_prepared_replacement(path)?;
    verify_path_identity(path, &file)?;
    regular_file_identity(&file)
}

pub(crate) fn parent_directory_identity_at_path(
    path: &Path,
) -> Result<RegularFileIdentity, String> {
    ParentDirectoryGuard::acquire_for_path(path).map(|guard| guard.identity)
}

pub(crate) fn same_persisted_regular_file_identity(
    path: &Path,
    expected: RegularFileIdentity,
) -> Result<bool, String> {
    regular_file_identity_at_path(path).map(|actual| actual == expected)
}

#[cfg(not(windows))]
fn same_regular_file_identity(_left: &File, _right: &File) -> Result<(), String> {
    Err("managed hardlink replacement requires Windows".to_string())
}

fn validate_replace_paths(paths: &HandleReplacePaths) -> Result<(), String> {
    let all = [
        &paths.target_path,
        &paths.recovery_path,
        &paths.staging_path,
        &paths.rollback_tombstone_path,
    ];
    validate_same_parent_distinct_paths(&all)
}

fn validate_same_parent_distinct_paths(paths: &[&PathBuf]) -> Result<(), String> {
    let parent = paths
        .first()
        .and_then(|path| path.parent())
        .ok_or_else(|| "managed operation target has no parent".to_string())?;
    if paths
        .iter()
        .any(|path| path.parent().is_none_or(|candidate| candidate != parent))
    {
        return Err("managed operation paths must share the target directory".to_string());
    }
    for (index, path) in paths.iter().enumerate() {
        if path.file_name().is_none() {
            return Err("managed operation plan contains an invalid file name".to_string());
        }
        if paths.iter().skip(index + 1).any(|other| *other == *path) {
            return Err("managed operation plan paths must be distinct".to_string());
        }
    }
    Ok(())
}

/// Classifies only the valid operation-bound crash states. Unknown,
/// partial, extra, or hash-mismatched layouts fail closed.
/// Identity-bound classifier used by durable callers. Hashes are content
/// checks only; every occupied leaf must also match its persisted file object.
pub(crate) fn classify_handle_replace_crash_state(
    paths: &HandleReplacePaths,
    identities: HandleReplaceIdentityBindings,
    expected_original_sha256: &str,
    expected_replacement_sha256: &str,
) -> Result<HandleReplaceCrashState, String> {
    validate_replace_paths(paths)?;
    let parent = ParentDirectoryGuard::acquire_for_path(&paths.target_path)?;
    parent.verify_paths(paths)?;
    if parent.identity != identities.parent_identity {
        return Err("managed replacement parent directory identity changed".to_string());
    }
    let target = optional_path_observation(&paths.target_path)?;
    let recovery = optional_path_observation(&paths.recovery_path)?;
    let staging = optional_path_observation(&paths.staging_path)?;
    let tombstone = optional_path_observation(&paths.rollback_tombstone_path)?;
    let is_original = |value: &Option<(String, RegularFileIdentity)>| {
        value.as_ref().is_some_and(|(digest, identity)| {
            digest == expected_original_sha256 && *identity == identities.original_identity
        })
    };
    let is_replacement = |value: &Option<(String, RegularFileIdentity)>| {
        value.as_ref().is_some_and(|(digest, identity)| {
            digest == expected_replacement_sha256 && *identity == identities.replacement_identity
        })
    };

    match (&target, &recovery, &staging, &tombstone) {
        (value, None, None, None) if is_original(value) => Ok(HandleReplaceCrashState::Original),
        (value, None, stage, None) if is_original(value) && is_replacement(stage) => {
            Ok(HandleReplaceCrashState::Staged)
        }
        (None, value, stage, None) if is_original(value) && is_replacement(stage) => {
            Ok(HandleReplaceCrashState::Prepared)
        }
        (value, old, None, None) if is_replacement(value) && is_original(old) => {
            Ok(HandleReplaceCrashState::ReplacementWithRecovery)
        }
        (value, None, None, None) if is_replacement(value) => {
            Ok(HandleReplaceCrashState::ReplacementOnly)
        }
        (None, old, None, tombstone_value)
            if is_original(old) && is_replacement(tombstone_value) =>
        {
            Ok(HandleReplaceCrashState::RollbackPrepared)
        }
        (value, None, None, tombstone_value)
            if is_original(value) && is_replacement(tombstone_value) =>
        {
            Ok(HandleReplaceCrashState::RolledBack)
        }
        _ => {
            let describe = |value: &Option<(String, RegularFileIdentity)>| {
                if value.is_none() {
                    "absent"
                } else if is_original(value) {
                    "original"
                } else if is_replacement(value) {
                    "replacement"
                } else {
                    "unbound"
                }
            };
            Err(format!(
                "managed replacement crash state is unknown (target={}, recovery={}, staging={}, tombstone={})",
                describe(&target),
                describe(&recovery),
                describe(&staging),
                describe(&tombstone)
            ))
        }
    }
}

fn optional_path_observation(path: &Path) -> Result<Option<(String, RegularFileIdentity)>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
                return Err("managed replacement crash artifact is unsafe".to_string());
            }
            // Classification is read-only. Requesting DELETE access here would
            // self-conflict with the caller's already-held global writer
            // barrier and is unnecessary because every later mutation reopens
            // the exact persisted identity through the typed recovery API.
            let mut file = open_identity_read(path)?;
            verify_path_identity(path, &file)?;
            let digest = digest_handle(&mut file)?.1;
            let identity = regular_file_identity(&file)?;
            verify_path_identity(path, &file)?;
            Ok(Some((digest, identity)))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("managed replacement crash artifact is unavailable".to_string()),
    }
}

/// Reopens a deterministic crash layout using the caller's durable identity
/// bindings and either continues publication or restores the original. The
/// returned token still requires the caller to persist its terminal phase
/// before calling `cleanup_after_durable_terminal`.
pub(crate) fn recover_handle_replace(
    paths: &HandleReplacePaths,
    identities: HandleReplaceIdentityBindings,
    expected_original_sha256: &str,
    expected_replacement_sha256: &str,
    decision: HandleReplaceRecoveryDecision,
) -> Result<ResolvedHandleReplace, String> {
    let state = classify_handle_replace_crash_state(
        paths,
        identities,
        expected_original_sha256,
        expected_replacement_sha256,
    )?;
    match state {
        HandleReplaceCrashState::Original => {
            if decision != HandleReplaceRecoveryDecision::Restore {
                return Err(
                    "managed replacement cannot commit from the original layout".to_string()
                );
            }
            let guard = acquire_write_bound(
                &paths.target_path,
                identities,
                identities.original_identity,
                expected_original_sha256,
            )?;
            Ok(ResolvedHandleReplace {
                guard,
                paths: paths.clone(),
                cleanup_artifacts: Vec::new(),
            })
        }
        HandleReplaceCrashState::Staged => {
            let staged = reopen_staged_replace(
                paths,
                identities,
                &paths.target_path,
                &paths.staging_path,
                expected_original_sha256,
                expected_replacement_sha256,
            )?;
            match decision {
                HandleReplaceRecoveryDecision::Restore => {
                    staged.restore().map_err(|(error, _)| error)
                }
                HandleReplaceRecoveryDecision::Commit => staged
                    .prepare()
                    .map_err(|(error, _)| error)?
                    .publish()
                    .map_err(|(error, _)| error)?
                    .commit()
                    .map_err(|(error, _)| error),
            }
        }
        HandleReplaceCrashState::Prepared => {
            let prepared = reopen_prepared_replace(
                paths,
                identities,
                &paths.recovery_path,
                &paths.staging_path,
                expected_original_sha256,
                expected_replacement_sha256,
            )?;
            match decision {
                HandleReplaceRecoveryDecision::Restore => {
                    prepared.restore().map_err(|(error, _)| error)
                }
                HandleReplaceRecoveryDecision::Commit => prepared
                    .publish()
                    .map_err(|(error, _)| error)?
                    .commit()
                    .map_err(|(error, _)| error),
            }
        }
        HandleReplaceCrashState::ReplacementWithRecovery => {
            let published = reopen_published_replace(
                paths,
                identities,
                expected_original_sha256,
                expected_replacement_sha256,
            )?;
            match decision {
                HandleReplaceRecoveryDecision::Restore => {
                    published.restore().map_err(|(error, _)| error)
                }
                HandleReplaceRecoveryDecision::Commit => {
                    published.commit().map_err(|(error, _)| error)
                }
            }
        }
        HandleReplaceCrashState::ReplacementOnly => {
            if decision != HandleReplaceRecoveryDecision::Commit {
                return Err(
                    "managed committed replacement cannot be restored after cleanup".to_string(),
                );
            }
            let guard = acquire_write_bound(
                &paths.target_path,
                identities,
                identities.replacement_identity,
                expected_replacement_sha256,
            )?;
            Ok(ResolvedHandleReplace {
                guard,
                paths: paths.clone(),
                cleanup_artifacts: Vec::new(),
            })
        }
        HandleReplaceCrashState::RollbackPrepared => {
            if decision != HandleReplaceRecoveryDecision::Restore {
                return Err("managed replacement rollback cannot be committed".to_string());
            }
            let mut previous = acquire_write_bound(
                &paths.recovery_path,
                identities,
                identities.original_identity,
                expected_original_sha256,
            )?;
            let replacement = acquire_destructive_bound(
                &paths.rollback_tombstone_path,
                identities,
                identities.replacement_identity,
                expected_replacement_sha256,
            )?;
            let artifact = cleanup_artifact(&replacement, expected_replacement_sha256)?;
            previous.rename_no_replace_by_handle(&paths.target_path)?;
            Ok(ResolvedHandleReplace {
                guard: previous,
                paths: paths.clone(),
                cleanup_artifacts: vec![artifact],
            })
        }
        HandleReplaceCrashState::RolledBack => {
            if decision != HandleReplaceRecoveryDecision::Restore {
                return Err("managed replacement rollback cannot be committed".to_string());
            }
            let guard = acquire_write_bound(
                &paths.target_path,
                identities,
                identities.original_identity,
                expected_original_sha256,
            )?;
            let replacement = acquire_destructive_bound(
                &paths.rollback_tombstone_path,
                identities,
                identities.replacement_identity,
                expected_replacement_sha256,
            )?;
            let artifact = cleanup_artifact(&replacement, expected_replacement_sha256)?;
            Ok(ResolvedHandleReplace {
                guard,
                paths: paths.clone(),
                cleanup_artifacts: vec![artifact],
            })
        }
    }
}

fn reopen_staged_replace(
    paths: &HandleReplacePaths,
    identities: HandleReplaceIdentityBindings,
    original_path: &Path,
    replacement_path: &Path,
    expected_original_sha256: &str,
    expected_replacement_sha256: &str,
) -> Result<StagedHandleReplace, String> {
    Ok(StagedHandleReplace {
        paths: paths.clone(),
        previous: acquire_write_bound(
            original_path,
            identities,
            identities.original_identity,
            expected_original_sha256,
        )?,
        replacement: acquire_destructive_bound(
            replacement_path,
            identities,
            identities.replacement_identity,
            expected_replacement_sha256,
        )?,
        expected_replacement_sha256: expected_replacement_sha256.to_string(),
    })
}

fn reopen_prepared_replace(
    paths: &HandleReplacePaths,
    identities: HandleReplaceIdentityBindings,
    original_path: &Path,
    replacement_path: &Path,
    expected_original_sha256: &str,
    expected_replacement_sha256: &str,
) -> Result<PreparedHandleReplace, String> {
    let staged = reopen_staged_replace(
        paths,
        identities,
        original_path,
        replacement_path,
        expected_original_sha256,
        expected_replacement_sha256,
    )?;
    Ok(PreparedHandleReplace {
        paths: staged.paths,
        previous: staged.previous,
        replacement: staged.replacement,
        expected_replacement_sha256: staged.expected_replacement_sha256,
    })
}

fn reopen_published_replace(
    paths: &HandleReplacePaths,
    identities: HandleReplaceIdentityBindings,
    expected_original_sha256: &str,
    expected_replacement_sha256: &str,
) -> Result<PublishedHandleReplace, String> {
    let prepared = reopen_prepared_replace(
        paths,
        identities,
        &paths.recovery_path,
        &paths.target_path,
        expected_original_sha256,
        expected_replacement_sha256,
    )?;
    Ok(PublishedHandleReplace {
        paths: prepared.paths,
        previous: prepared.previous,
        replacement: prepared.replacement,
        expected_replacement_sha256: prepared.expected_replacement_sha256,
    })
}

fn acquire_write_bound(
    path: &Path,
    identities: HandleReplaceIdentityBindings,
    expected_identity: RegularFileIdentity,
    expected_sha256: &str,
) -> Result<WriteExclusionGuard, String> {
    let mut guard = WriteExclusionGuard::acquire(path)?;
    if guard.parent.identity != identities.parent_identity || guard.identity()? != expected_identity
    {
        return Err("managed replacement recovery identity changed".to_string());
    }
    guard.verify_current_path(Some(expected_sha256))?;
    Ok(guard)
}

fn acquire_destructive_bound(
    path: &Path,
    identities: HandleReplaceIdentityBindings,
    expected_identity: RegularFileIdentity,
    expected_sha256: &str,
) -> Result<DestructiveFileGuard, String> {
    let mut guard = DestructiveFileGuard::acquire(path)?;
    if guard.parent.identity != identities.parent_identity || guard.identity()? != expected_identity
    {
        return Err("managed replacement recovery identity changed".to_string());
    }
    guard.verify_current_path(Some(expected_sha256))?;
    Ok(guard)
}

fn acquire_destructive_create_bound(
    path: &Path,
    identities: HandleCreateIdentityBindings,
    expected_sha256: &str,
) -> Result<DestructiveFileGuard, String> {
    let mut guard = DestructiveFileGuard::acquire(path)?;
    if guard.parent.identity != identities.parent_identity
        || guard.identity()? != identities.created_identity
    {
        return Err("managed create recovery identity changed".to_string());
    }
    guard.verify_current_path(Some(expected_sha256))?;
    Ok(guard)
}

fn acquire_destructive_delete_bound(
    path: &Path,
    identities: HandleDeleteIdentityBindings,
    expected_sha256: &str,
) -> Result<DestructiveFileGuard, String> {
    let mut guard = DestructiveFileGuard::acquire(path)?;
    if guard.parent.identity != identities.parent_identity
        || guard.identity()? != identities.deleted_identity
    {
        return Err("managed delete recovery identity changed".to_string());
    }
    guard.verify_current_path(Some(expected_sha256))?;
    Ok(guard)
}

fn ensure_absent_leaf(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err("managed operation no-clobber target already exists".to_string()),
        Err(_) => Err("managed operation target availability is unknown".to_string()),
    }
}

fn cleanup_owned_artifacts(artifacts: &[CleanupArtifact]) -> Result<(), String> {
    for artifact in artifacts {
        let mut guard = DestructiveFileGuard::acquire(&artifact.path)?;
        if guard.identity()? != artifact.expected_identity {
            return Err("managed terminal cleanup artifact identity changed".to_string());
        }
        guard.verify_current_path(Some(&artifact.expected_sha256))?;
        guard.delete()?;
    }
    Ok(())
}

fn cleanup_failed_staging(error: String, staging: DestructiveFileGuard) -> String {
    match delete_destructive_file(&staging.file) {
        Ok(()) => {
            drop(staging);
            error
        }
        Err(cleanup) => format!("{error}; exact staging cleanup failed: {cleanup}"),
    }
}

fn cleanup_failed_hardlink_alias(path: &Path, source: &File, error: String) -> String {
    let guard = match DestructiveFileGuard::acquire(path) {
        Ok(guard) => guard,
        Err(cleanup) => {
            return format!("{error}; exact hardlink staging cleanup unavailable: {cleanup}");
        }
    };
    match (guard.identity(), regular_file_identity(source)) {
        (Ok(actual), Ok(expected)) if actual == expected => cleanup_failed_staging(error, guard),
        _ => format!("{error}; hardlink staging identity changed before cleanup"),
    }
}

#[cfg(windows)]
fn create_replacement_staging(path: &Path) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};

    const GENERIC_READ_WRITE_AND_DELETE: u32 = 0xC001_0000;
    OpenOptions::new()
        .read(true)
        .write(true)
        .access_mode(GENERIC_READ_WRITE_AND_DELETE)
        .create_new(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| "managed replacement staging file could not be created".to_string())
}

#[cfg(not(windows))]
fn create_replacement_staging(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| "managed replacement staging file could not be created".to_string())
}

#[cfg(windows)]
fn open_prepared_replacement(path: &Path) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};

    const GENERIC_READ_AND_DELETE: u32 = 0x8001_0000;
    OpenOptions::new()
        .read(true)
        .access_mode(GENERIC_READ_AND_DELETE)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| "managed replacement writer barrier could not be acquired".to_string())
}

#[cfg(not(windows))]
fn open_prepared_replacement(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|_| "managed replacement writer barrier could not be acquired".to_string())
}

impl DestructiveFileGuard {
    pub(crate) fn acquire(path: &Path) -> Result<Self, String> {
        let parent = ParentDirectoryGuard::acquire_for_path(path)?;
        let file = open_guarded(path, GuardMode::BlockReadersAndWriters)?;
        let mut guard = Self {
            path: path.to_path_buf(),
            file,
            parent,
        };
        guard.verify_current_path(None)?;
        Ok(guard)
    }

    pub(crate) fn verify_current_path(
        &mut self,
        expected_sha256: Option<&str>,
    ) -> Result<(u64, String), String> {
        verify_guarded_path(&self.path, &mut self.file, expected_sha256)
    }

    pub(crate) fn identity(&self) -> Result<RegularFileIdentity, String> {
        regular_file_identity(&self.file)
    }

    /// Verifies that `witness` is another name for this exact held file object.
    /// The witness is opened for identity only, so this remains compatible with
    /// the destructive guard's reader exclusion.
    pub(crate) fn verify_same_identity_path(
        &mut self,
        witness: &Path,
        expected_sha256: Option<&str>,
    ) -> Result<RegularFileIdentity, String> {
        self.verify_current_path(expected_sha256)?;
        let witness_file = open_identity_only(witness)?;
        verify_path_identity(witness, &witness_file)?;
        let identity = self.identity()?;
        if regular_file_identity(&witness_file)? != identity {
            return Err("managed ownership witness identity changed".to_string());
        }
        Ok(identity)
    }

    /// Atomically moves the exact opened file object without replacing an
    /// existing target. A competing path rename can only make this operation
    /// fail; it cannot redirect the mutation to a different file identity.
    pub(crate) fn rename_no_replace(&mut self, target: &Path) -> Result<(), String> {
        self.verify_current_path(None)?;
        self.parent.verify_current_path()?;
        rename_destructive_file(&self.file, &self.path, target, &self.parent)?;
        // See WriteExclusionGuard::rename_no_replace_by_handle: after the
        // kernel rename succeeds, return the typed state without a fallible
        // pathname re-open.
        self.path = target.to_path_buf();
        Ok(())
    }

    /// Deletes the exact opened file object by handle. The pathname is never
    /// reopened between the identity check and disposition update.
    pub(crate) fn delete(mut self) -> Result<(), String> {
        self.verify_current_path(None)?;
        delete_destructive_file(&self.file)?;
        drop(self.file);
        Ok(())
    }
}

impl ParentDirectoryGuard {
    fn acquire_for_path(path: &Path) -> Result<Self, String> {
        let parent = path
            .parent()
            .ok_or_else(|| "managed file path has no parent directory".to_string())?;
        let file = open_parent_directory(parent)?;
        let metadata = file
            .metadata()
            .map_err(|_| "managed parent directory is unreadable".to_string())?;
        if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
            return Err("managed parent directory is unsafe".to_string());
        }
        let identity = regular_file_identity(&file)?;
        let guard = Self {
            path: parent.to_path_buf(),
            file,
            identity,
        };
        guard.verify_current_path()?;
        Ok(guard)
    }

    fn verify_current_path(&self) -> Result<(), String> {
        let current = open_parent_directory_identity_only(&self.path)?;
        if regular_file_identity(&current)? != self.identity {
            return Err("managed parent directory identity changed".to_string());
        }
        Ok(())
    }

    fn verify_paths(&self, paths: &HandleReplacePaths) -> Result<(), String> {
        validate_replace_paths(paths)?;
        if paths.target_path.parent() != Some(self.path.as_path()) {
            return Err("managed replacement parent directory changed".to_string());
        }
        self.verify_current_path()
    }

    fn try_clone(&self) -> Result<Self, String> {
        let file = self
            .file
            .try_clone()
            .map_err(|_| "managed parent directory guard could not be retained".to_string())?;
        Ok(Self {
            path: self.path.clone(),
            file,
            identity: self.identity,
        })
    }
}

#[cfg(windows)]
fn rename_destructive_file(
    file: &File,
    source: &Path,
    target: &Path,
    parent_guard: &ParentDirectoryGuard,
) -> Result<(), String> {
    use std::{
        mem,
        os::windows::{ffi::OsStrExt, io::AsRawHandle},
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FileRenameInfo, SetFileInformationByHandle, FILE_RENAME_INFO,
    };

    if source.parent() != target.parent() || source.parent() != Some(parent_guard.path.as_path()) {
        return Err("managed destructive rename must stay in one directory".to_string());
    }
    if !target.is_absolute() {
        return Err("managed destructive target must be absolute".to_string());
    }
    parent_guard.verify_current_path()?;
    let target_wide = target.as_os_str().encode_wide().collect::<Vec<_>>();
    let prefix = mem::offset_of!(FILE_RENAME_INFO, FileName);
    let file_name_byte_len = target_wide
        .len()
        .checked_mul(mem::size_of::<u16>())
        .ok_or_else(|| "managed destructive target path is too long".to_string())?;
    let logical_len = prefix
        .checked_add(file_name_byte_len)
        .ok_or_else(|| "managed destructive target path is too long".to_string())?;
    // FILE_RENAME_INFO declares FileName as a one-element trailing array. Keep
    // one zero UTF-16 code unit in the allocation for Windows' namespace
    // parser, but do not include it in FileNameLength or dwBufferSize.
    let allocation_len = logical_len
        .checked_add(mem::size_of::<u16>())
        .ok_or_else(|| "managed destructive target path is too long".to_string())?
        .max(mem::size_of::<FILE_RENAME_INFO>());
    let words = allocation_len
        .checked_add(mem::size_of::<usize>() - 1)
        .ok_or_else(|| "managed destructive target path is too long".to_string())?
        / mem::size_of::<usize>();
    let mut buffer = vec![0_usize; words];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        // Standard FileRenameInfo with ReplaceIfExists=false is the widest
        // supported handle-bound no-clobber contract. With RootDirectory null,
        // FileName is the verified parent's absolute target path; the kernel
        // still renames the exact opened source identity and refuses an
        // existing target.
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = std::ptr::null_mut();
        (*info).FileNameLength = u32::try_from(file_name_byte_len)
            .map_err(|_| "managed destructive target path is too long".to_string())?;
        std::ptr::copy_nonoverlapping(
            target_wide.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            target_wide.len(),
        );
        if SetFileInformationByHandle(
            file.as_raw_handle().cast(),
            FileRenameInfo,
            info.cast(),
            u32::try_from(logical_len.max(mem::size_of::<FILE_RENAME_INFO>()))
                .map_err(|_| "managed destructive target path is too long".to_string())?,
        ) == 0
        {
            return Err(format!(
                "failed to quarantine managed file by handle: {}",
                io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn rename_destructive_file(
    _file: &File,
    source: &Path,
    target: &Path,
    _parent_guard: &ParentDirectoryGuard,
) -> Result<(), String> {
    fs::rename(source, target)
        .map_err(|_| "failed to quarantine managed file by handle".to_string())
}

#[cfg(windows)]
fn delete_destructive_file(file: &File) -> Result<(), String> {
    use std::{mem, os::windows::io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfoEx, SetFileInformationByHandle, FILE_DISPOSITION_FLAG_DELETE,
        FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX,
    };

    let mut disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle().cast(),
            FileDispositionInfoEx,
            std::ptr::addr_of_mut!(disposition).cast(),
            mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if ok == 0 {
        return Err(format!(
            "failed to delete managed file by handle: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn delete_destructive_file(_file: &File) -> Result<(), String> {
    Err("managed destructive deletion requires Windows".to_string())
}

#[derive(Clone, Copy)]
enum GuardMode {
    BlockWrites,
    BlockReadersAndWriters,
}

#[cfg(windows)]
fn open_guarded(path: &Path, mode: GuardMode) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};

    const GENERIC_READ_AND_DELETE: u32 = 0x8001_0000;

    let mut options = OpenOptions::new();
    match mode {
        GuardMode::BlockWrites => {
            options
                .access_mode(GENERIC_READ_AND_DELETE)
                .share_mode(FILE_SHARE_READ);
        }
        GuardMode::BlockReadersAndWriters => {
            options.access_mode(GENERIC_READ_AND_DELETE).share_mode(0);
        }
    }
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options
        .open(path)
        .map_err(|error| format!("managed file writer barrier could not be acquired: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|_| "managed file barrier is unreadable".to_string())?;
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
        return Err("managed file barrier target is unsafe".to_string());
    }
    Ok(file)
}

#[cfg(not(windows))]
fn open_guarded(_path: &Path, _mode: GuardMode) -> Result<File, String> {
    Err("managed destructive writer barriers require Windows".to_string())
}

#[cfg(windows)]
fn open_parent_directory(path: &Path) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    const GENERIC_READ: u32 = 0x8000_0000;
    OpenOptions::new()
        .access_mode(GENERIC_READ)
        // Deliberately omit FILE_SHARE_DELETE: pin this exact namespace.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| "managed parent directory guard could not be acquired".to_string())
}

#[cfg(windows)]
fn open_parent_directory_identity_only(path: &Path) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| "managed parent directory identity is unavailable".to_string())
}

#[cfg(not(windows))]
fn open_parent_directory(_path: &Path) -> Result<File, String> {
    Err("managed parent directory guards require Windows".to_string())
}

#[cfg(not(windows))]
fn open_parent_directory_identity_only(_path: &Path) -> Result<File, String> {
    Err("managed parent directory guards require Windows".to_string())
}

#[cfg(windows)]
fn open_identity_only(path: &Path) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| "managed ownership witness is unavailable".to_string())
}

#[cfg(windows)]
fn open_identity_read(path: &Path) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| "managed replacement source is unavailable".to_string())
}

#[cfg(not(windows))]
fn open_identity_read(path: &Path) -> Result<File, String> {
    File::open(path).map_err(|_| "managed replacement source is unavailable".to_string())
}

#[cfg(not(windows))]
fn open_identity_only(_path: &Path) -> Result<File, String> {
    Err("managed ownership identity checks require Windows".to_string())
}

fn verify_guarded_path(
    path: &Path,
    file: &mut File,
    expected_sha256: Option<&str>,
) -> Result<(u64, String), String> {
    verify_path_identity(path, file)?;
    let handle_digest = digest_handle(file)?;
    if expected_sha256.is_some_and(|expected| expected != handle_digest.1) {
        return Err("managed file changed before its guarded mutation".to_string());
    }
    verify_path_identity(path, file)?;
    Ok(handle_digest)
}

fn digest_handle(file: &mut File) -> Result<(u64, String), String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|_| "managed file barrier is unreadable".to_string())?;
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "managed file barrier is unreadable".to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| "managed file barrier size overflowed".to_string())?;
    }
    Ok((bytes, format!("{:x}", hasher.finalize())))
}

#[cfg(windows)]
fn verify_path_identity(path: &Path, guarded: &File) -> Result<(), String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = OpenOptions::new();
    options
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let current = options
        .open(path)
        .map_err(|_| "managed file barrier target is unavailable".to_string())?;
    if regular_file_identity(guarded)? != regular_file_identity(&current)? {
        return Err("managed file path changed after writer barrier acquisition".to_string());
    }
    Ok(())
}

#[cfg(windows)]
fn regular_file_identity(file: &File) -> Result<RegularFileIdentity, String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) };
    if ok == 0 {
        return Err("managed file barrier identity is unavailable".to_string());
    }
    Ok(RegularFileIdentity {
        volume_serial_number: u64::from(info.dwVolumeSerialNumber),
        file_index: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    })
}

#[cfg(not(windows))]
fn regular_file_identity(_file: &File) -> Result<RegularFileIdentity, String> {
    Err("managed regular-file identity requires Windows".to_string())
}

#[cfg(not(windows))]
fn verify_path_identity(_path: &Path, _guarded: &File) -> Result<(), String> {
    Err("managed destructive writer barriers require Windows".to_string())
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(all(test, windows))]
mod tests {
    use std::{fs, fs::OpenOptions, os::windows::fs::OpenOptionsExt, path::Path};

    use tempfile::tempdir;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};

    use super::{
        classify_handle_create_crash_state, classify_handle_delete_crash_state,
        classify_handle_replace_crash_state, recover_handle_create, recover_handle_delete,
        recover_handle_replace, stage_handle_create, stage_handle_delete,
        stage_handle_hardlink_create, DestructiveFileGuard, HandleCreateCrashState,
        HandleCreatePaths, HandleCreateRecoveryDecision, HandleDeleteCrashState, HandleDeletePaths,
        HandleDeleteRecoveryDecision, HandleReplaceCrashState, HandleReplacePaths,
        HandleReplaceRecoveryDecision, WriteExclusionGuard,
    };

    const AFTER_SHA256: &str = "f39592393ef0859cb196a52693d2cea00fb2df784b3c04ae54aa7cadb8e562f8";
    const BEFORE_SHA256: &str = "6db7d803e74f1ffa7d8f5adc0bf95b3e15bf4c8373fffadf546227cc6c6742cb";

    fn replace_paths(target: &Path) -> HandleReplacePaths {
        let parent = target.parent().unwrap();
        HandleReplacePaths::from_persisted_plan(
            target.to_path_buf(),
            parent.join("target.recovery.bin"),
            parent.join("target.staging.bin"),
            parent.join("target.rollback-tombstone.bin"),
        )
        .unwrap()
    }

    fn open_delete_access(path: &Path) -> fs::File {
        const GENERIC_READ_AND_DELETE: u32 = 0x8001_0000;

        OpenOptions::new()
            .access_mode(GENERIC_READ_AND_DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(path)
            .unwrap()
    }

    fn rename_delete_access_contender(source: &Path, target: &Path) -> fs::File {
        let file = open_delete_access(source);
        let parent = super::ParentDirectoryGuard::acquire_for_path(source).unwrap();
        super::rename_destructive_file(&file, source, target, &parent).unwrap();
        file
    }

    #[test]
    fn handle_replace_prepare_publish_commit_preserves_recovery() {
        let root = tempdir().unwrap();
        let target = root.path().join("target.bin");
        let source = root.path().join("source.bin");
        let recovery = root.path().join("target.recovery.bin");
        fs::write(&target, b"before").unwrap();
        fs::write(&source, b"after").unwrap();

        let prepared = WriteExclusionGuard::acquire(&target)
            .unwrap()
            .prepare_handle_replace(&source, AFTER_SHA256, &replace_paths(&target))
            .unwrap();
        assert_eq!(prepared.recovery_path(), recovery);
        assert!(!target.exists());
        assert!(OpenOptions::new().write(true).open(&recovery).is_err());

        let published = prepared.publish().unwrap();
        assert_eq!(published.recovery_path(), recovery);
        assert!(OpenOptions::new().write(true).open(&target).is_err());
        let mut resolved = published.commit().unwrap();
        resolved
            .guard_mut()
            .verify_current_path(Some(AFTER_SHA256))
            .unwrap();
        assert_eq!(
            fs::read(&target).unwrap(),
            b"after",
            "published target path must name the held replacement before the guard is released"
        );
        let guard = resolved.retain_for_recovery();
        drop(guard);

        assert_eq!(fs::read(target).unwrap(), b"after");
        assert_eq!(fs::read(recovery).unwrap(), b"before");
    }

    #[test]
    fn prepared_replace_can_restore_exact_previous_identity() {
        let root = tempdir().unwrap();
        let target = root.path().join("target.bin");
        let source = root.path().join("source.bin");
        let recovery = root.path().join("target.recovery.bin");
        fs::write(&target, b"before").unwrap();
        fs::write(&source, b"after").unwrap();

        let prepared = WriteExclusionGuard::acquire(&target)
            .unwrap()
            .prepare_handle_replace(&source, AFTER_SHA256, &replace_paths(&target))
            .unwrap();
        let guard = prepared.restore().unwrap().retain_for_recovery();
        drop(guard);

        assert_eq!(fs::read(target).unwrap(), b"before");
        assert!(!recovery.exists());
    }

    #[test]
    fn hardlink_replace_publishes_the_exact_source_identity() {
        let root = tempdir().unwrap();
        let target = root.path().join("target.bin");
        let source = root.path().join("source.bin");
        let recovery = root.path().join("target.recovery.bin");
        fs::write(&target, b"before").unwrap();
        fs::write(&source, b"after").unwrap();

        let published = WriteExclusionGuard::acquire(&target)
            .unwrap()
            .prepare_handle_hardlink_replace(&source, AFTER_SHA256, &replace_paths(&target))
            .unwrap()
            .publish()
            .unwrap();
        assert_eq!(
            super::regular_file_identity(&super::open_identity_only(&source).unwrap()).unwrap(),
            super::regular_file_identity(&published.replacement.file).unwrap()
        );
        let resolved = published.commit().unwrap();
        assert_eq!(
            super::regular_file_identity(&super::open_identity_only(&source).unwrap()).unwrap(),
            super::regular_file_identity(&resolved.guard.file).unwrap()
        );
        let guard = resolved.retain_for_recovery();
        drop(guard);
        assert_eq!(fs::read(target).unwrap(), b"after");
        assert_eq!(fs::read(recovery).unwrap(), b"before");
    }

    #[test]
    fn hardlink_replace_can_restore_before_publish() {
        let root = tempdir().unwrap();
        let target = root.path().join("target.bin");
        let source = root.path().join("source.bin");
        fs::write(&target, b"before").unwrap();
        fs::write(&source, b"after").unwrap();

        let prepared = WriteExclusionGuard::acquire(&target)
            .unwrap()
            .prepare_handle_hardlink_replace(&source, AFTER_SHA256, &replace_paths(&target))
            .unwrap();
        let guard = prepared.restore().unwrap().retain_for_recovery();
        drop(guard);
        assert_eq!(fs::read(target).unwrap(), b"before");
        assert_eq!(fs::read(source).unwrap(), b"after");
    }

    #[test]
    fn hardlink_replace_delete_access_contender_is_never_clobbered() {
        let root = tempdir().unwrap();
        let target = root.path().join("target.bin");
        let source = root.path().join("source.bin");
        let recovery = root.path().join("target.recovery.bin");
        let contender_source = root.path().join("contender.bin");
        fs::write(&target, b"before").unwrap();
        fs::write(&source, b"after").unwrap();
        fs::write(&contender_source, b"contender").unwrap();

        let prepared = WriteExclusionGuard::acquire(&target)
            .unwrap()
            .prepare_handle_hardlink_replace(&source, AFTER_SHA256, &replace_paths(&target))
            .unwrap();
        let contender = rename_delete_access_contender(&contender_source, &target);
        let (_error, prepared) = prepared.publish().unwrap_err();
        drop(contender);
        assert_eq!(fs::read(&target).unwrap(), b"contender");
        assert_eq!(fs::read(&recovery).unwrap(), b"before");
        assert!(prepared.restore().is_err());
        assert_eq!(fs::read(target).unwrap(), b"contender");
    }

    #[test]
    fn published_replace_can_restore_exact_previous_identity() {
        let root = tempdir().unwrap();
        let target = root.path().join("target.bin");
        let source = root.path().join("source.bin");
        fs::write(&target, b"before").unwrap();
        fs::write(&source, b"after").unwrap();

        let published = WriteExclusionGuard::acquire(&target)
            .unwrap()
            .prepare_handle_replace(&source, AFTER_SHA256, &replace_paths(&target))
            .unwrap()
            .publish()
            .unwrap();
        let guard = published.restore().unwrap().retain_for_recovery();
        drop(guard);

        assert_eq!(fs::read(target).unwrap(), b"before");
        assert_eq!(
            fs::read_dir(root.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .contains("rollback-tombstone")
                })
                .count(),
            1
        );
    }

    #[test]
    fn restored_replace_terminal_cleanup_deletes_the_exact_tombstone() {
        let root = tempdir().unwrap();
        let target = root.path().join("target.bin");
        let source = root.path().join("source.bin");
        let paths = replace_paths(&target);
        fs::write(&target, b"before").unwrap();
        fs::write(&source, b"after").unwrap();

        let resolved = WriteExclusionGuard::acquire(&target)
            .unwrap()
            .prepare_handle_replace(&source, AFTER_SHA256, &paths)
            .unwrap()
            .publish()
            .unwrap()
            .restore()
            .unwrap();
        let guard = resolved.cleanup_after_durable_terminal().unwrap();
        drop(guard);

        assert_eq!(fs::read(&target).unwrap(), b"before");
        assert!(!paths.rollback_tombstone_path().exists());
    }

    #[test]
    fn delete_access_contender_at_target_is_never_replaced() {
        let root = tempdir().unwrap();
        let target = root.path().join("target.bin");
        let source = root.path().join("source.bin");
        let recovery = root.path().join("target.recovery.bin");
        let contender_source = root.path().join("contender.bin");
        fs::write(&target, b"before").unwrap();
        fs::write(&source, b"after").unwrap();
        fs::write(&contender_source, b"contender").unwrap();

        let prepared = WriteExclusionGuard::acquire(&target)
            .unwrap()
            .prepare_handle_replace(&source, AFTER_SHA256, &replace_paths(&target))
            .unwrap();
        let contender = rename_delete_access_contender(&contender_source, &target);
        let (error, prepared) = prepared.publish().unwrap_err();
        assert!(error.contains("failed to quarantine managed file by handle"));
        drop(contender);

        assert_eq!(fs::read(&target).unwrap(), b"contender");
        assert_eq!(fs::read(&recovery).unwrap(), b"before");
        let (restore_error, _prepared) = prepared.restore().unwrap_err();
        assert!(restore_error.contains("failed to quarantine managed file by handle"));
        assert_eq!(fs::read(target).unwrap(), b"contender");
    }

    #[test]
    fn delete_access_contender_cannot_swap_guarded_target_name() {
        let root = tempdir().unwrap();
        let target = root.path().join("target.bin");
        let contender_source = root.path().join("contender.bin");
        fs::write(&target, b"before").unwrap();
        fs::write(&contender_source, b"contender").unwrap();

        let mut guard = WriteExclusionGuard::acquire(&target).unwrap();
        let contender = open_delete_access(&contender_source);
        let parent = super::ParentDirectoryGuard::acquire_for_path(&contender_source).unwrap();
        assert!(
            super::rename_destructive_file(&contender, &contender_source, &target, &parent)
                .is_err()
        );
        guard.verify_current_path(None).unwrap();
        drop(contender);
        drop(guard);
        assert_eq!(fs::read(target).unwrap(), b"before");
    }

    #[test]
    fn same_identity_delete_access_contender_is_excluded_for_guard_lifetime() {
        let root = tempdir().unwrap();
        let target = root.path().join("target.bin");
        fs::write(&target, b"before").unwrap();

        let guard = WriteExclusionGuard::acquire(&target).unwrap();
        // This contender requests DELETE on the exact guarded object. Omitting
        // FILE_SHARE_DELETE from the guard makes acquisition fail before it can
        // race the verify-to-rename interval.
        assert!(OpenOptions::new()
            .access_mode(0x8001_0000)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(&target)
            .is_err());
        drop(guard);
        assert_eq!(fs::read(target).unwrap(), b"before");
    }

    #[test]
    fn destructive_guard_excludes_readers_during_rename_and_delete() {
        let root = tempdir().unwrap();
        let source = root.path().join("source.bin");
        let quarantine = root.path().join("quarantine.bin");
        fs::write(&source, b"payload").unwrap();

        let mut guard = DestructiveFileGuard::acquire(&source).unwrap();
        assert!(fs::File::open(&source).is_err());
        guard.verify_current_path(None).unwrap();
        guard.rename_no_replace(&quarantine).unwrap();
        assert!(fs::File::open(&quarantine).is_err());
        guard.delete().unwrap();
        assert!(!quarantine.exists());
    }

    #[test]
    fn staged_token_persists_identities_and_recovers_every_forward_crash_boundary() {
        for (crash_state, commit) in [
            (HandleReplaceCrashState::Staged, true),
            (HandleReplaceCrashState::Prepared, true),
            (HandleReplaceCrashState::ReplacementWithRecovery, true),
        ] {
            let root = tempdir().unwrap();
            let target = root.path().join("target.bin");
            let source = root.path().join("source.bin");
            fs::write(&target, b"before").unwrap();
            fs::write(&source, b"after").unwrap();
            let paths = replace_paths(&target);
            let staged = WriteExclusionGuard::acquire(&target)
                .unwrap()
                .stage_handle_replace(&source, AFTER_SHA256, &paths)
                .unwrap();
            let identities = staged.identity_bindings().unwrap();
            match crash_state {
                HandleReplaceCrashState::Staged => drop(staged),
                HandleReplaceCrashState::Prepared => drop(staged.prepare().unwrap()),
                HandleReplaceCrashState::ReplacementWithRecovery => {
                    drop(staged.prepare().unwrap().publish().unwrap())
                }
                _ => unreachable!(),
            }
            assert_eq!(
                classify_handle_replace_crash_state(
                    &paths,
                    identities,
                    BEFORE_SHA256,
                    AFTER_SHA256,
                )
                .unwrap(),
                crash_state
            );
            let resolved = recover_handle_replace(
                &paths,
                identities,
                BEFORE_SHA256,
                AFTER_SHA256,
                if commit {
                    HandleReplaceRecoveryDecision::Commit
                } else {
                    HandleReplaceRecoveryDecision::Restore
                },
            )
            .unwrap();
            drop(resolved.retain_for_recovery());
            assert_eq!(fs::read(&target).unwrap(), b"after");
        }
    }

    #[test]
    fn crash_classifier_rejects_equal_hash_different_identity() {
        let root = tempdir().unwrap();
        let target = root.path().join("target.bin");
        let source = root.path().join("source.bin");
        fs::write(&target, b"before").unwrap();
        fs::write(&source, b"after").unwrap();
        let paths = replace_paths(&target);
        let staged = WriteExclusionGuard::acquire(&target)
            .unwrap()
            .stage_handle_replace(&source, AFTER_SHA256, &paths)
            .unwrap();
        let identities = staged.identity_bindings().unwrap();
        drop(staged);
        fs::remove_file(paths.staging_path()).unwrap();
        fs::write(paths.staging_path(), b"after").unwrap();
        assert!(classify_handle_replace_crash_state(
            &paths,
            identities,
            BEFORE_SHA256,
            AFTER_SHA256,
        )
        .is_err());
    }

    #[test]
    fn committed_cleanup_crash_reopens_replacement_only() {
        let root = tempdir().unwrap();
        let target = root.path().join("target.bin");
        let source = root.path().join("source.bin");
        fs::write(&target, b"before").unwrap();
        fs::write(&source, b"after").unwrap();
        let paths = replace_paths(&target);
        let staged = WriteExclusionGuard::acquire(&target)
            .unwrap()
            .stage_handle_replace(&source, AFTER_SHA256, &paths)
            .unwrap();
        let identities = staged.identity_bindings().unwrap();
        let resolved = staged
            .prepare()
            .unwrap()
            .publish()
            .unwrap()
            .commit()
            .unwrap();
        drop(resolved.cleanup_after_durable_terminal().unwrap());
        assert_eq!(
            classify_handle_replace_crash_state(&paths, identities, BEFORE_SHA256, AFTER_SHA256,)
                .unwrap(),
            HandleReplaceCrashState::ReplacementOnly
        );
        let resolved = recover_handle_replace(
            &paths,
            identities,
            BEFORE_SHA256,
            AFTER_SHA256,
            HandleReplaceRecoveryDecision::Commit,
        )
        .unwrap();
        drop(resolved.retain_for_recovery());
        assert_eq!(fs::read(target).unwrap(), b"after");
    }

    #[test]
    fn parent_directory_guard_blocks_namespace_rename() {
        let root = tempdir().unwrap();
        let parent = root.path().join("parent");
        let renamed = root.path().join("renamed");
        fs::create_dir(&parent).unwrap();
        let target = parent.join("target.bin");
        fs::write(&target, b"before").unwrap();
        let guard = WriteExclusionGuard::acquire(&target).unwrap();
        assert!(fs::rename(&parent, &renamed).is_err());
        drop(guard);
        fs::rename(&parent, &renamed).unwrap();
    }

    #[test]
    fn handle_rename_long_unicode_name_has_exact_namespace_result() {
        let root = tempdir().unwrap();
        let parent = root
            .path()
            .join("长目录-会话存储-边界验证")
            .join(format!("nested-{}", "x".repeat(40)));
        fs::create_dir_all(&parent).unwrap();
        let source = parent.join("source-原始.bin");
        let target = parent.join(format!(
            "目标-会话存储-{}-精确命名.bin",
            "长路径".repeat(18)
        ));
        fs::write(&source, b"exact bytes").unwrap();

        let mut guard = DestructiveFileGuard::acquire(&source).unwrap();
        guard.rename_no_replace(&target).unwrap();
        drop(guard);

        let mut entries = fs::read_dir(&parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert!(!source.exists(), "source remained; entries={entries:?}");
        let target_bytes = fs::read(&target)
            .unwrap_or_else(|error| panic!("target unreadable ({error}); entries={entries:?}"));
        assert_eq!(target_bytes, b"exact bytes", "entries={entries:?}");
        assert_eq!(entries, vec![target.file_name().unwrap().to_os_string()]);
    }

    #[test]
    fn handle_create_is_no_clobber_and_recovers_staged_crash() {
        let root = tempdir().unwrap();
        let target = root.path().join("created.bin");
        let source = root.path().join("source.bin");
        fs::write(&source, b"after").unwrap();
        let paths = HandleCreatePaths::from_persisted_plan(
            target.clone(),
            root.path().join("created.staging.bin"),
            root.path().join("created.tombstone.bin"),
        )
        .unwrap();
        let staged = stage_handle_create(&source, AFTER_SHA256, &paths).unwrap();
        let identities = staged.identity_bindings().unwrap();
        drop(staged);
        assert_eq!(
            classify_handle_create_crash_state(&paths, identities, AFTER_SHA256).unwrap(),
            HandleCreateCrashState::Staged
        );
        let resolved = recover_handle_create(
            &paths,
            identities,
            AFTER_SHA256,
            HandleCreateRecoveryDecision::Commit,
        )
        .unwrap();
        drop(resolved.retain_for_recovery());
        assert_eq!(fs::read(&target).unwrap(), b"after");

        let second_paths = HandleCreatePaths::from_persisted_plan(
            target.clone(),
            root.path().join("second.staging.bin"),
            root.path().join("second.tombstone.bin"),
        )
        .unwrap();
        assert!(stage_handle_create(&source, AFTER_SHA256, &second_paths).is_err());
        assert_eq!(fs::read(target).unwrap(), b"after");
    }

    #[test]
    fn hardlink_create_rebinds_identity_without_self_conflicting_handles() {
        let root = tempdir().unwrap();
        let target = root.path().join("created-hardlink.bin");
        let source = root.path().join("source-hardlink.bin");
        fs::write(&source, b"after").unwrap();
        let paths = HandleCreatePaths::from_persisted_plan(
            target.clone(),
            root.path().join("created-hardlink.staging.bin"),
            root.path().join("created-hardlink.tombstone.bin"),
        )
        .unwrap();

        let staged = stage_handle_hardlink_create(&source, AFTER_SHA256, &paths).unwrap();
        let identities = staged.identity_bindings().unwrap();
        let published = staged.publish().unwrap();
        let resolved = published.commit().unwrap();
        drop(resolved.retain_for_recovery());

        assert_eq!(fs::read(&target).unwrap(), b"after");
        assert_eq!(
            super::regular_file_identity(&super::open_identity_only(&source).unwrap()).unwrap(),
            identities.created_identity
        );
        assert_eq!(
            super::regular_file_identity(&super::open_identity_only(&target).unwrap()).unwrap(),
            identities.created_identity
        );
        let mut source_alias = DestructiveFileGuard::acquire(&source).unwrap();
        source_alias
            .verify_current_path(Some(AFTER_SHA256))
            .unwrap();
        source_alias.delete().unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(&target).unwrap(), b"after");
    }

    #[test]
    fn handle_delete_recovers_prepared_crash_and_terminal_cleanup() {
        let root = tempdir().unwrap();
        let target = root.path().join("deleted.bin");
        fs::write(&target, b"before").unwrap();
        let paths = HandleDeletePaths::from_persisted_plan(
            target.clone(),
            root.path().join("deleted.recovery.bin"),
        )
        .unwrap();
        let staged = stage_handle_delete(&paths, BEFORE_SHA256).unwrap();
        let identities = staged.identity_bindings().unwrap();
        drop(staged.prepare().unwrap());
        assert_eq!(
            classify_handle_delete_crash_state(&paths, identities, BEFORE_SHA256).unwrap(),
            HandleDeleteCrashState::Prepared
        );
        let resolved = recover_handle_delete(
            &paths,
            identities,
            BEFORE_SHA256,
            HandleDeleteRecoveryDecision::Commit,
        )
        .unwrap();
        assert!(resolved.cleanup_after_durable_terminal().unwrap().is_none());
        assert_eq!(
            classify_handle_delete_crash_state(&paths, identities, BEFORE_SHA256).unwrap(),
            HandleDeleteCrashState::Deleted
        );
        assert!(!target.exists());
    }
}
