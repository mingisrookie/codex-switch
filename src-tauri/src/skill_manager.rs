use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::{Host, Url};
use walkdir::WalkDir;

use crate::{
    crypto::{protect, unprotect},
    file_ops::atomic_write,
    operation_log::operation_id,
};

const PACKAGE_MANIFEST: &str = ".codex-switch-package.json";
const PACKAGE_VERSION: &str = "2026.07.14";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum SkillId {
    Image2,
    GrokSearch,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SkillState {
    Missing,
    Current,
    UpdateAvailable,
    Drifted,
    Unmanaged,
    Invalid,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SkillMutationAction {
    Install,
    Update,
    Configure,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillStatus {
    pub id: SkillId,
    pub display_name: String,
    pub description: String,
    pub installed_path: PathBuf,
    pub state: SkillState,
    pub bundled_version: String,
    pub installed_version: Option<String>,
    pub can_install: bool,
    pub can_update: bool,
    pub base_url: String,
    pub credential_configured: bool,
    pub restart_required: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillConfigInput {
    pub skill_id: SkillId,
    pub base_url: String,
    pub api_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillMutationReceipt {
    pub operation_id: String,
    pub skill_id: SkillId,
    pub action: SkillMutationAction,
    pub installed_version: String,
    pub backup_dir: Option<PathBuf>,
    pub rolled_back: bool,
    pub restart_required: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PackageManifest {
    skill_id: SkillId,
    version: String,
    files: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSkillConfig {
    base_url: String,
    model: String,
    credential_path: PathBuf,
}

#[derive(Debug)]
struct SkillConfigFileSnapshot {
    path: PathBuf,
    contents: Option<Vec<u8>>,
}

#[derive(Debug)]
struct SkillConfigPairSnapshot {
    config: SkillConfigFileSnapshot,
    credential: SkillConfigFileSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillConfigWritePhase {
    Apply,
    Rollback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SkillTransaction {
    skill_id: SkillId,
    target: PathBuf,
    stage: PathBuf,
    backup: Option<PathBuf>,
    #[serde(default)]
    phase: SkillTransactionPhase,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
enum SkillTransactionPhase {
    #[default]
    Prepared,
    BackupCreated,
}

struct SkillPackage {
    id: SkillId,
    display_name: &'static str,
    description: &'static str,
    folder_name: &'static str,
    config_folder_name: &'static str,
    default_base_url: &'static str,
    model: &'static str,
    files: &'static [(&'static str, &'static [u8])],
}

const IMAGE2_FILES: &[(&str, &[u8])] = &[
    (
        "LICENSE.txt",
        include_bytes!("../resources/skills/newapi-image2-client/LICENSE.txt"),
    ),
    (
        "SOURCE.json",
        include_bytes!("../resources/skills/newapi-image2-client/SOURCE.json"),
    ),
    (
        "SKILL.md",
        include_bytes!("../resources/skills/newapi-image2-client/SKILL.md"),
    ),
    (
        "agents/openai.yaml",
        include_bytes!("../resources/skills/newapi-image2-client/agents/openai.yaml"),
    ),
    (
        "references/curl.md",
        include_bytes!("../resources/skills/newapi-image2-client/references/curl.md"),
    ),
    (
        "references/node-openai.md",
        include_bytes!("../resources/skills/newapi-image2-client/references/node-openai.md"),
    ),
    (
        "references/python-openai.md",
        include_bytes!("../resources/skills/newapi-image2-client/references/python-openai.md"),
    ),
    (
        "references/raw-fetch.md",
        include_bytes!("../resources/skills/newapi-image2-client/references/raw-fetch.md"),
    ),
    (
        "scripts/image2.ps1",
        include_bytes!("../resources/skills/newapi-image2-client/scripts/image2.ps1"),
    ),
    (
        "scripts/smoke_image2.py",
        include_bytes!("../resources/skills/newapi-image2-client/scripts/smoke_image2.py"),
    ),
];

const GROK_FILES: &[(&str, &[u8])] = &[
    (
        "LICENSE.txt",
        include_bytes!("../resources/skills/grok-search/LICENSE.txt"),
    ),
    (
        "SOURCE.json",
        include_bytes!("../resources/skills/grok-search/SOURCE.json"),
    ),
    (
        "SKILL.md",
        include_bytes!("../resources/skills/grok-search/SKILL.md"),
    ),
    (
        "agents/openai.yaml",
        include_bytes!("../resources/skills/grok-search/agents/openai.yaml"),
    ),
    (
        "scripts/grok-search.ps1",
        include_bytes!("../resources/skills/grok-search/scripts/grok-search.ps1"),
    ),
    (
        "scripts/GrokSearch.psm1",
        include_bytes!("../resources/skills/grok-search/scripts/GrokSearch.psm1"),
    ),
];

pub fn list_skills_at(codex_home: &Path, appdata: &Path) -> Result<Vec<SkillStatus>, String> {
    require_absolute_root(codex_home, "CODEX_HOME")?;
    require_absolute_root(appdata, "APPDATA")?;
    [SkillId::Image2, SkillId::GrokSearch]
        .into_iter()
        .map(|id| scan_skill(codex_home, appdata, package(id)))
        .collect()
}

pub fn install_skill_at(
    codex_home: &Path,
    appdata: &Path,
    skill_id: SkillId,
    confirm_replace: bool,
) -> Result<SkillMutationReceipt, String> {
    require_absolute_root(codex_home, "CODEX_HOME")?;
    require_absolute_root(appdata, "APPDATA")?;
    let package = package(skill_id);
    recover_skill_transaction_at(codex_home, skill_id)?;
    let skills_root = codex_home.join("skills");
    fs::create_dir_all(&skills_root)
        .map_err(|error| format!("failed to create the Codex skills directory: {error}"))?;
    reject_reparse_tree(&skills_root)?;
    let target = skills_root.join(package.folder_name);
    let current_status = scan_skill(codex_home, appdata, package)?;
    if current_status.state == SkillState::Current {
        return Ok(SkillMutationReceipt {
            operation_id: operation_id("install-skill")?,
            skill_id,
            action: SkillMutationAction::Update,
            installed_version: PACKAGE_VERSION.to_string(),
            backup_dir: None,
            rolled_back: false,
            restart_required: false,
            warnings: Vec::new(),
        });
    }
    if target.exists() && !confirm_replace {
        return Err("skill replacement requires explicit confirmation".to_string());
    }
    if target.exists() {
        reject_reparse_tree(&target)?;
    }

    let operation_id = operation_id("install-skill")?;
    let stage = skills_root.join(format!(".codex-switch-stage-{operation_id}"));
    if stage.exists() {
        return Err("skill staging directory already exists".to_string());
    }
    let had_target = target.exists();
    let backup = had_target.then(|| {
        codex_home
            .join(".codex-switch")
            .join("skill-backups")
            .join(&operation_id)
            .join(package.folder_name)
    });
    let mut transaction = SkillTransaction {
        skill_id,
        target: target.clone(),
        stage: stage.clone(),
        backup: backup.clone(),
        phase: SkillTransactionPhase::Prepared,
    };
    write_skill_transaction(codex_home, package, &transaction)?;
    if let Err(error) = write_package(&stage, package) {
        let _ = remove_directory_if_present(&stage);
        let _ = clear_skill_transaction(codex_home, package);
        return Err(error);
    }

    if let Some(path) = backup.as_ref() {
        let parent = path
            .parent()
            .ok_or_else(|| "skill backup path has no parent".to_string())?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create the skill backup directory: {error}"))?;
        reject_reparse_tree(parent)?;
        fs::rename(&target, path)
            .map_err(|error| format!("failed to move the existing skill into backup: {error}"))?;
        transaction.phase = SkillTransactionPhase::BackupCreated;
        write_skill_transaction(codex_home, package, &transaction)?;
    }

    if let Err(error) = fs::rename(&stage, &target) {
        let restored = restore_previous_target(&target, backup.as_deref());
        let _ = clear_skill_transaction(codex_home, package);
        let suffix = if restored {
            "rolled back"
        } else {
            "rollback failed"
        };
        return Err(format!(
            "failed to activate the staged skill: {error}; {suffix}"
        ));
    }
    let verified = scan_skill(codex_home, appdata, package)
        .map(|status| status.state == SkillState::Current)
        .unwrap_or(false);
    if !verified {
        let restored = restore_previous_target(&target, backup.as_deref());
        let _ = clear_skill_transaction(codex_home, package);
        let suffix = if restored {
            "rolled back"
        } else {
            "rollback failed"
        };
        return Err(format!("installed skill verification failed; {suffix}"));
    }

    let mut warnings = Vec::new();
    if clear_skill_transaction(codex_home, package).is_err() {
        warnings.push("技能已安装，但事务记录清理失败；下次安装会自动复核".to_string());
    }
    Ok(SkillMutationReceipt {
        operation_id,
        skill_id,
        action: if had_target {
            SkillMutationAction::Update
        } else {
            SkillMutationAction::Install
        },
        installed_version: PACKAGE_VERSION.to_string(),
        backup_dir: backup,
        rolled_back: false,
        restart_required: true,
        warnings,
    })
}

pub fn recover_skill_transaction_at(codex_home: &Path, skill_id: SkillId) -> Result<bool, String> {
    require_absolute_root(codex_home, "CODEX_HOME")?;
    let package = package(skill_id);
    let journal = skill_transaction_path(codex_home, package);
    if !journal.is_file() {
        return Ok(false);
    }
    let transaction: SkillTransaction = serde_json::from_slice(
        &fs::read(&journal)
            .map_err(|error| format!("failed to read the skill transaction record: {error}"))?,
    )
    .map_err(|error| format!("failed to parse the skill transaction record: {error}"))?;
    validate_skill_transaction(codex_home, package, &transaction)?;

    let expected = package_manifest(package);
    let target_is_current = transaction.target.is_dir()
        && read_manifest(&transaction.target)
            .ok()
            .flatten()
            .is_some_and(|manifest| manifest == expected)
        && verify_installed_files(&transaction.target, &expected).unwrap_or(false);
    if target_is_current {
        remove_directory_if_present(&transaction.stage)?;
        clear_skill_transaction(codex_home, package)?;
        return Ok(true);
    }

    let existing_backup = transaction.backup.as_deref().filter(|path| path.exists());
    if transaction.phase == SkillTransactionPhase::Prepared && existing_backup.is_none() {
        // The swap never started. In particular, keep an existing unmanaged or
        // drifted target instead of deleting user data merely because staging
        // or journal cleanup was interrupted.
        remove_directory_if_present(&transaction.stage)?;
        clear_skill_transaction(codex_home, package)?;
        return Ok(true);
    }

    remove_directory_if_present(&transaction.target)?;
    if let Some(backup) = existing_backup {
        reject_reparse_tree(backup)?;
        fs::rename(backup, &transaction.target)
            .map_err(|error| format!("failed to recover the previous skill directory: {error}"))?;
    }
    remove_directory_if_present(&transaction.stage)?;
    clear_skill_transaction(codex_home, package)?;
    Ok(true)
}

pub fn save_skill_config_at(
    codex_home: &Path,
    appdata: &Path,
    input: SkillConfigInput,
) -> Result<SkillMutationReceipt, String> {
    save_skill_config_at_with(codex_home, appdata, input, |_, path, bytes| {
        atomic_write(path, bytes)
    })
}

fn save_skill_config_at_with<F>(
    codex_home: &Path,
    appdata: &Path,
    input: SkillConfigInput,
    mut write_file: F,
) -> Result<SkillMutationReceipt, String>
where
    F: FnMut(SkillConfigWritePhase, &Path, &[u8]) -> Result<(), String>,
{
    require_absolute_root(codex_home, "CODEX_HOME")?;
    require_absolute_root(appdata, "APPDATA")?;
    let package = package(input.skill_id);
    let target = codex_home.join("skills").join(package.folder_name);
    let manifest = read_manifest(&target)?
        .ok_or_else(|| "install the managed skill before saving its configuration".to_string())?;
    if manifest.skill_id != input.skill_id {
        return Err(
            "installed skill package metadata does not match the requested skill".to_string(),
        );
    }

    let base_url = normalize_provider_url(input.skill_id, &input.base_url)?;
    let config_root = config_root(appdata, package);
    let credential_path = config_root.join("credential.enc");
    let config_path = config_root.join("config.json");
    let previous = snapshot_skill_config_pair(&config_path, &credential_path)?;
    let key = input.api_key.trim();
    let encoded_credential = if key.is_empty() {
        let previous_config =
            previous.config.contents.as_deref().ok_or_else(|| {
                "API key is required for the first skill configuration".to_string()
            })?;
        let stored_previous: StoredSkillConfig =
            serde_json::from_slice(previous_config).map_err(|_| {
                "stored skill configuration is invalid; enter a new API key".to_string()
            })?;
        if stored_previous.credential_path != credential_path {
            return Err("stored skill configuration is invalid; enter a new API key".to_string());
        }
        let previous_base_url = normalize_provider_url(input.skill_id, &stored_previous.base_url)
            .map_err(|_| {
            "stored skill configuration is invalid; enter a new API key".to_string()
        })?;
        if normalized_provider_origin(&previous_base_url)? != normalized_provider_origin(&base_url)?
        {
            return Err("API key is required when changing the service origin".to_string());
        }
        let previous_credential =
            previous.credential.contents.as_ref().ok_or_else(|| {
                "stored skill credential is invalid; enter a new API key".to_string()
            })?;
        validate_stored_skill_credential(previous_credential)?;
        None
    } else {
        Some(BASE64.encode(protect(key.as_bytes())?).into_bytes())
    };

    let stored = StoredSkillConfig {
        base_url,
        model: package.model.to_string(),
        credential_path: credential_path.clone(),
    };
    let mut config_bytes = serde_json::to_vec_pretty(&stored)
        .map_err(|error| format!("failed to serialize the skill configuration: {error}"))?;
    config_bytes.push(b'\n');
    let expected_credential = encoded_credential
        .as_deref()
        .or(previous.credential.contents.as_deref())
        .ok_or_else(|| "API key is required for the first skill configuration".to_string())?;
    let operation_id = operation_id("configure-skill")?;

    fs::create_dir_all(&config_root)
        .map_err(|error| format!("failed to create the skill configuration directory: {error}"))?;
    reject_reparse_tree(&config_root)?;
    write_skill_config_pair_with(
        &previous,
        &config_bytes,
        encoded_credential.as_deref(),
        expected_credential,
        &mut write_file,
    )?;

    Ok(SkillMutationReceipt {
        operation_id,
        skill_id: input.skill_id,
        action: SkillMutationAction::Configure,
        installed_version: manifest.version,
        backup_dir: None,
        rolled_back: false,
        restart_required: true,
        warnings: Vec::new(),
    })
}

fn snapshot_skill_config_pair(
    config_path: &Path,
    credential_path: &Path,
) -> Result<SkillConfigPairSnapshot, String> {
    Ok(SkillConfigPairSnapshot {
        config: snapshot_skill_config_file(config_path)?,
        credential: snapshot_skill_config_file(credential_path)?,
    })
}

fn snapshot_skill_config_file(path: &Path) -> Result<SkillConfigFileSnapshot, String> {
    let contents = match fs::read(path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("failed to snapshot skill configuration: {error}")),
    };
    Ok(SkillConfigFileSnapshot {
        path: path.to_path_buf(),
        contents,
    })
}

fn write_skill_config_pair_with<F>(
    previous: &SkillConfigPairSnapshot,
    config_bytes: &[u8],
    credential_update: Option<&[u8]>,
    expected_credential: &[u8],
    write_file: &mut F,
) -> Result<(), String>
where
    F: FnMut(SkillConfigWritePhase, &Path, &[u8]) -> Result<(), String>,
{
    let result = (|| {
        write_file(
            SkillConfigWritePhase::Apply,
            &previous.config.path,
            config_bytes,
        )?;
        if let Some(credential) = credential_update {
            write_file(
                SkillConfigWritePhase::Apply,
                &previous.credential.path,
                credential,
            )?;
        }
        verify_skill_config_file(&previous.config.path, config_bytes)?;
        verify_skill_config_file(&previous.credential.path, expected_credential)
    })();

    if let Err(error) = result {
        return match restore_skill_config_pair_with(previous, write_file) {
            Ok(()) => Err(format!("{error}; rolled back previous skill configuration")),
            Err(rollback_error) => Err(format!("{error}; rollback failed: {rollback_error}")),
        };
    }
    Ok(())
}

fn restore_skill_config_pair_with<F>(
    previous: &SkillConfigPairSnapshot,
    write_file: &mut F,
) -> Result<(), String>
where
    F: FnMut(SkillConfigWritePhase, &Path, &[u8]) -> Result<(), String>,
{
    let mut first_error = None;
    for snapshot in [&previous.config, &previous.credential] {
        let result = match snapshot.contents.as_deref() {
            Some(contents) => write_file(SkillConfigWritePhase::Rollback, &snapshot.path, contents),
            None => match fs::remove_file(&snapshot.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(format!(
                    "failed to remove a new skill configuration file: {error}"
                )),
            },
        };
        if first_error.is_none() {
            first_error = result.err();
        }
    }
    for snapshot in [&previous.config, &previous.credential] {
        if let Err(error) = verify_skill_config_snapshot(snapshot) {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn verify_skill_config_snapshot(snapshot: &SkillConfigFileSnapshot) -> Result<(), String> {
    match snapshot.contents.as_deref() {
        Some(expected) => verify_skill_config_file(&snapshot.path, expected),
        None => match snapshot.path.try_exists() {
            Ok(false) => Ok(()),
            Ok(true) => Err("new skill configuration file remained after rollback".to_string()),
            Err(error) => Err(format!(
                "failed to verify removed skill configuration file: {error}"
            )),
        },
    }
}

fn verify_skill_config_file(path: &Path, expected: &[u8]) -> Result<(), String> {
    match fs::read(path) {
        Ok(actual) if actual == expected => Ok(()),
        Ok(_) => Err("skill configuration pair verification failed".to_string()),
        Err(error) => Err(format!(
            "failed to verify skill configuration pair: {error}"
        )),
    }
}

fn validate_stored_skill_credential(encoded: &[u8]) -> Result<(), String> {
    let encoded = std::str::from_utf8(encoded)
        .map(str::trim)
        .map_err(|_| "stored skill credential is invalid; enter a new API key".to_string())?;
    let protected = BASE64
        .decode(encoded)
        .map_err(|_| "stored skill credential is invalid; enter a new API key".to_string())?;
    let plaintext = unprotect(&protected)
        .map_err(|_| "stored skill credential is invalid; enter a new API key".to_string())?;
    if std::str::from_utf8(&plaintext)
        .ok()
        .is_none_or(|key| key.trim().is_empty())
    {
        return Err("stored skill credential is invalid; enter a new API key".to_string());
    }
    Ok(())
}

fn scan_skill(
    codex_home: &Path,
    appdata: &Path,
    package: &SkillPackage,
) -> Result<SkillStatus, String> {
    let target = codex_home.join("skills").join(package.folder_name);
    let (state, installed_version, message) = if !target.exists() {
        (SkillState::Missing, None, "尚未安装".to_string())
    } else if reject_reparse_tree(&target).is_err() {
        (
            SkillState::Invalid,
            None,
            "目录包含不受支持的链接或重解析点".to_string(),
        )
    } else {
        match read_manifest(&target) {
            Ok(None) => (SkillState::Unmanaged, None, "检测到非受管目录".to_string()),
            Err(_) => (SkillState::Invalid, None, "安装元数据损坏".to_string()),
            Ok(Some(manifest)) if manifest.skill_id != package.id => (
                SkillState::Invalid,
                Some(manifest.version),
                "安装元数据与技能不匹配".to_string(),
            ),
            Ok(Some(manifest)) if manifest.version != PACKAGE_VERSION => (
                SkillState::UpdateAvailable,
                Some(manifest.version),
                "有可用更新".to_string(),
            ),
            Ok(Some(manifest)) => {
                let expected = package_manifest(package);
                let current = verify_installed_files(&target, &expected).unwrap_or(false)
                    && manifest == expected;
                if current {
                    (
                        SkillState::Current,
                        Some(manifest.version),
                        "已是最新版本".to_string(),
                    )
                } else {
                    (
                        SkillState::Drifted,
                        Some(manifest.version),
                        "检测到本地修改".to_string(),
                    )
                }
            }
        }
    };

    let config_path = config_root(appdata, package).join("config.json");
    let config = fs::read(&config_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<StoredSkillConfig>(&bytes).ok());
    let base_url = config
        .as_ref()
        .map(|value| value.base_url.clone())
        .unwrap_or_else(|| package.default_base_url.to_string());
    let credential_path = config
        .as_ref()
        .map(|value| value.credential_path.clone())
        .unwrap_or_else(|| config_root(appdata, package).join("credential.enc"));
    let credential_configured = fs::read(&credential_path)
        .ok()
        .and_then(|bytes| BASE64.decode(bytes).ok())
        .is_some_and(|bytes| !bytes.is_empty());

    Ok(SkillStatus {
        id: package.id,
        display_name: package.display_name.to_string(),
        description: package.description.to_string(),
        installed_path: target,
        state,
        bundled_version: PACKAGE_VERSION.to_string(),
        installed_version,
        can_install: state == SkillState::Missing,
        can_update: matches!(
            state,
            SkillState::UpdateAvailable
                | SkillState::Drifted
                | SkillState::Unmanaged
                | SkillState::Invalid
        ),
        base_url,
        credential_configured,
        restart_required: false,
        message,
    })
}

fn write_package(stage: &Path, package: &SkillPackage) -> Result<(), String> {
    fs::create_dir_all(stage)
        .map_err(|error| format!("failed to create the skill staging directory: {error}"))?;
    for (relative, bytes) in package.files {
        validate_relative_path(relative)?;
        let path = stage.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("failed to create a staged skill directory: {error}"))?;
        }
        atomic_write(&path, bytes)?;
        if hash_bytes(
            &fs::read(&path)
                .map_err(|error| format!("failed to verify a staged skill file: {error}"))?,
        ) != hash_bytes(bytes)
        {
            return Err("staged skill file checksum mismatch".to_string());
        }
    }
    let mut manifest = serde_json::to_vec_pretty(&package_manifest(package))
        .map_err(|error| format!("failed to serialize the skill package manifest: {error}"))?;
    manifest.push(b'\n');
    atomic_write(&stage.join(PACKAGE_MANIFEST), &manifest)
}

fn verify_installed_files(target: &Path, manifest: &PackageManifest) -> Result<bool, String> {
    let expected_paths = manifest
        .files
        .keys()
        .cloned()
        .chain(std::iter::once(PACKAGE_MANIFEST.to_string()))
        .collect::<BTreeSet<_>>();
    let mut actual_paths = BTreeSet::new();
    for entry in WalkDir::new(target).follow_links(false) {
        let entry =
            entry.map_err(|error| format!("failed to scan the installed skill: {error}"))?;
        if entry.file_type().is_file() {
            let relative = entry
                .path()
                .strip_prefix(target)
                .map_err(|_| "installed skill path escaped its root".to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            actual_paths.insert(relative);
        }
    }
    if actual_paths != expected_paths {
        return Ok(false);
    }
    for (relative, expected_hash) in &manifest.files {
        let bytes = fs::read(target.join(relative))
            .map_err(|error| format!("failed to read an installed skill file: {error}"))?;
        if &hash_bytes(&bytes) != expected_hash {
            return Ok(false);
        }
    }
    Ok(true)
}

fn package_manifest(package: &SkillPackage) -> PackageManifest {
    PackageManifest {
        skill_id: package.id,
        version: PACKAGE_VERSION.to_string(),
        files: package
            .files
            .iter()
            .map(|(path, bytes)| ((*path).to_string(), hash_bytes(bytes)))
            .collect(),
    }
}

fn read_manifest(target: &Path) -> Result<Option<PackageManifest>, String> {
    let path = target.join(PACKAGE_MANIFEST);
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = fs::read(path)
        .map_err(|error| format!("failed to read skill package metadata: {error}"))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("failed to parse skill package metadata: {error}"))
}

fn normalize_provider_url(skill_id: SkillId, value: &str) -> Result<String, String> {
    let mut url = Url::parse(value.trim()).map_err(|_| "service URL is invalid".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host().is_none()
    {
        return Err("service URL is not allowed".to_string());
    }
    if url.scheme() == "http" && !is_loopback_host(url.host()) {
        return Err("non-loopback service URLs must use HTTPS".to_string());
    }

    let path = url.path().trim_end_matches('/').to_string();
    let normalized_path = match skill_id {
        SkillId::Image2 if path.is_empty() => "/v1".to_string(),
        SkillId::Image2 if path.ends_with("/v1") => path,
        SkillId::Image2 => return Err("Image2 service URL must end with /v1".to_string()),
        SkillId::GrokSearch if path == "/v1" => String::new(),
        SkillId::GrokSearch if path.ends_with("/v1") => path.trim_end_matches("/v1").to_string(),
        SkillId::GrokSearch => path,
    };
    url.set_path(if normalized_path.is_empty() {
        "/"
    } else {
        &normalized_path
    });
    Ok(url.to_string().trim_end_matches('/').to_string())
}

#[derive(Debug, PartialEq, Eq)]
struct NormalizedProviderOrigin {
    scheme: String,
    host: String,
    port: u16,
}

fn normalized_provider_origin(value: &str) -> Result<NormalizedProviderOrigin, String> {
    let url = Url::parse(value)
        .map_err(|_| "stored skill configuration is invalid; enter a new API key".to_string())?;
    let host = url
        .host_str()
        .ok_or_else(|| "stored skill configuration is invalid; enter a new API key".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "stored skill configuration is invalid; enter a new API key".to_string())?;
    Ok(NormalizedProviderOrigin {
        scheme: url.scheme().to_ascii_lowercase(),
        host: host.to_ascii_lowercase(),
        port,
    })
}

fn is_loopback_host(host: Option<Host<&str>>) -> bool {
    match host {
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

fn package(id: SkillId) -> &'static SkillPackage {
    static IMAGE2: SkillPackage = SkillPackage {
        id: SkillId::Image2,
        display_name: "Image2",
        description: "通过自定义 New API 使用 gpt-image-2 生成和编辑图片",
        folder_name: "newapi-image2-client",
        config_folder_name: "image2",
        default_base_url: "https://api.lcming951.com/v1",
        model: "gpt-image-2",
        files: IMAGE2_FILES,
    };
    static GROK: SkillPackage = SkillPackage {
        id: SkillId::GrokSearch,
        display_name: "Grok 搜索",
        description: "为 ChatGPT 提供 Grok Web 与 X 实时搜索能力",
        folder_name: "grok-search",
        config_folder_name: "grok-search",
        default_base_url: "",
        model: "grok-4.5",
        files: GROK_FILES,
    };
    match id {
        SkillId::Image2 => &IMAGE2,
        SkillId::GrokSearch => &GROK,
    }
}

fn config_root(appdata: &Path, package: &SkillPackage) -> PathBuf {
    appdata
        .join("codex-switch")
        .join("skills")
        .join(package.config_folder_name)
}

fn skill_transaction_path(codex_home: &Path, package: &SkillPackage) -> PathBuf {
    codex_home
        .join(".codex-switch")
        .join("skill-transactions")
        .join(format!("{}.json", package.config_folder_name))
}

fn write_skill_transaction(
    codex_home: &Path,
    package: &SkillPackage,
    transaction: &SkillTransaction,
) -> Result<(), String> {
    validate_skill_transaction(codex_home, package, transaction)?;
    let journal = skill_transaction_path(codex_home, package);
    let parent = journal
        .parent()
        .ok_or_else(|| "skill transaction path has no parent".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create the skill transaction directory: {error}"))?;
    reject_reparse_tree(parent)?;
    let mut bytes = serde_json::to_vec_pretty(transaction)
        .map_err(|error| format!("failed to serialize the skill transaction record: {error}"))?;
    bytes.push(b'\n');
    atomic_write(&journal, &bytes)
}

fn clear_skill_transaction(codex_home: &Path, package: &SkillPackage) -> Result<(), String> {
    let journal = skill_transaction_path(codex_home, package);
    if journal.exists() {
        fs::remove_file(journal)
            .map_err(|error| format!("failed to clear the skill transaction record: {error}"))?;
    }
    Ok(())
}

fn validate_skill_transaction(
    codex_home: &Path,
    package: &SkillPackage,
    transaction: &SkillTransaction,
) -> Result<(), String> {
    let skills_root = codex_home.join("skills");
    let expected_target = skills_root.join(package.folder_name);
    let backup_root = codex_home.join(".codex-switch").join("skill-backups");
    let stage_name = transaction
        .stage
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let backup_is_safe = transaction.backup.as_ref().is_none_or(|path| {
        is_normal_absolute(path)
            && path.starts_with(&backup_root)
            && path.file_name() == Some(std::ffi::OsStr::new(package.folder_name))
    });
    if transaction.skill_id != package.id
        || transaction.target != expected_target
        || !is_normal_absolute(&transaction.target)
        || !is_normal_absolute(&transaction.stage)
        || transaction.stage.parent() != Some(skills_root.as_path())
        || !stage_name.starts_with(".codex-switch-stage-")
        || !backup_is_safe
    {
        return Err("skill transaction record contains an unsafe path".to_string());
    }
    Ok(())
}

fn is_normal_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::Prefix(_)
                    | std::path::Component::RootDir
                    | std::path::Component::Normal(_)
            )
        })
}

fn require_absolute_root(path: &Path, label: &str) -> Result<(), String> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(format!(
            "{label} must be an absolute path for skill operations"
        ))
    }
}

fn validate_relative_path(path: &str) -> Result<(), String> {
    let candidate = Path::new(path);
    if path.is_empty()
        || candidate.is_absolute()
        || candidate
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        || path.contains(':')
    {
        Err("embedded skill package contains an unsafe path".to_string())
    } else {
        Ok(())
    }
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn restore_previous_target(target: &Path, backup: Option<&Path>) -> bool {
    if remove_directory_if_present(target).is_err() {
        return false;
    }
    match backup {
        Some(path) => fs::rename(path, target).is_ok(),
        None => true,
    }
}

fn remove_directory_if_present(path: &Path) -> Result<(), String> {
    if path.exists() {
        reject_reparse_tree(path)?;
        fs::remove_dir_all(path)
            .map_err(|error| format!("failed to remove a skill staging directory: {error}"))?;
    }
    Ok(())
}

fn reject_reparse_tree(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry.map_err(|error| format!("failed to inspect a skill path: {error}"))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| format!("failed to inspect skill path metadata: {error}"))?;
        if metadata.file_type().is_symlink() || has_reparse_attribute(&metadata) {
            return Err("skill paths must not contain links or reparse points".to_string());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn has_reparse_attribute(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn has_reparse_attribute(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::fs;

    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    use tempfile::tempdir;

    use super::{
        install_skill_at, package, save_skill_config_at, save_skill_config_at_with,
        SkillConfigInput, SkillConfigWritePhase, SkillId, StoredSkillConfig,
    };

    #[test]
    fn user_visible_skill_metadata_uses_chatgpt_brand() {
        for skill_id in [SkillId::Image2, SkillId::GrokSearch] {
            let skill = package(skill_id);
            assert!(!skill.description.contains("Codex"));
        }
        assert_eq!(
            package(SkillId::GrokSearch).description,
            "为 ChatGPT 提供 Grok Web 与 X 实时搜索能力"
        );
    }

    #[test]
    fn blank_key_preserves_skill_credential_for_same_origin_path_change() {
        let codex_home = tempdir().unwrap();
        let appdata = tempdir().unwrap();
        install_skill_at(
            codex_home.path(),
            appdata.path(),
            SkillId::GrokSearch,
            false,
        )
        .unwrap();
        save_skill_config_at(
            codex_home.path(),
            appdata.path(),
            SkillConfigInput {
                skill_id: SkillId::GrokSearch,
                base_url: "https://research.example.com/api".to_string(),
                api_key: "sk-preserved".to_string(),
            },
        )
        .unwrap();
        let config_root = appdata.path().join("codex-switch/skills/grok-search");
        let credential_path = config_root.join("credential.enc");
        let before_credential = fs::read(&credential_path).unwrap();

        save_skill_config_at(
            codex_home.path(),
            appdata.path(),
            SkillConfigInput {
                skill_id: SkillId::GrokSearch,
                base_url: "https://research.example.com/search".to_string(),
                api_key: String::new(),
            },
        )
        .unwrap();

        let stored: StoredSkillConfig =
            serde_json::from_slice(&fs::read(config_root.join("config.json")).unwrap()).unwrap();
        assert_eq!(stored.base_url, "https://research.example.com/search");
        assert_eq!(fs::read(credential_path).unwrap(), before_credential);
    }

    #[test]
    fn blank_key_rejects_skill_origin_changes_without_file_changes() {
        for (old_url, new_url) in [
            (
                "https://research.example.com/api",
                "https://other.example.com/api",
            ),
            (
                "https://research.example.com/api",
                "https://research.example.com:8443/api",
            ),
            ("https://localhost/api", "http://localhost/api"),
        ] {
            let codex_home = tempdir().unwrap();
            let appdata = tempdir().unwrap();
            install_skill_at(
                codex_home.path(),
                appdata.path(),
                SkillId::GrokSearch,
                false,
            )
            .unwrap();
            save_skill_config_at(
                codex_home.path(),
                appdata.path(),
                SkillConfigInput {
                    skill_id: SkillId::GrokSearch,
                    base_url: old_url.to_string(),
                    api_key: "sk-preserved".to_string(),
                },
            )
            .unwrap();
            let config_root = appdata.path().join("codex-switch/skills/grok-search");
            let config_path = config_root.join("config.json");
            let credential_path = config_root.join("credential.enc");
            let before_config = fs::read(&config_path).unwrap();
            let before_credential = fs::read(&credential_path).unwrap();

            let error = save_skill_config_at(
                codex_home.path(),
                appdata.path(),
                SkillConfigInput {
                    skill_id: SkillId::GrokSearch,
                    base_url: new_url.to_string(),
                    api_key: String::new(),
                },
            )
            .unwrap_err();

            assert!(error.contains("API key is required"), "{error}");
            assert!(!error.contains(old_url), "{error}");
            assert!(!error.contains(new_url), "{error}");
            assert_eq!(fs::read(config_path).unwrap(), before_config);
            assert_eq!(fs::read(credential_path).unwrap(), before_credential);
        }
    }

    #[test]
    fn new_key_allows_skill_origin_change() {
        let codex_home = tempdir().unwrap();
        let appdata = tempdir().unwrap();
        install_skill_at(
            codex_home.path(),
            appdata.path(),
            SkillId::GrokSearch,
            false,
        )
        .unwrap();
        save_skill_config_at(
            codex_home.path(),
            appdata.path(),
            SkillConfigInput {
                skill_id: SkillId::GrokSearch,
                base_url: "https://old.example.com/api".to_string(),
                api_key: "sk-old".to_string(),
            },
        )
        .unwrap();

        save_skill_config_at(
            codex_home.path(),
            appdata.path(),
            SkillConfigInput {
                skill_id: SkillId::GrokSearch,
                base_url: "https://new.example.com/search".to_string(),
                api_key: "sk-new".to_string(),
            },
        )
        .unwrap();

        let config_root = appdata.path().join("codex-switch/skills/grok-search");
        let stored: StoredSkillConfig =
            serde_json::from_slice(&fs::read(config_root.join("config.json")).unwrap()).unwrap();
        let protected = BASE64
            .decode(
                fs::read_to_string(config_root.join("credential.enc"))
                    .unwrap()
                    .trim(),
            )
            .unwrap();
        assert_eq!(stored.base_url, "https://new.example.com/search");
        assert_eq!(crate::crypto::unprotect(&protected).unwrap(), b"sk-new");
    }

    #[test]
    fn skill_config_write_failure_restores_pair_and_reports_rollback_failure() {
        let codex_home = tempdir().unwrap();
        let appdata = tempdir().unwrap();
        install_skill_at(
            codex_home.path(),
            appdata.path(),
            SkillId::GrokSearch,
            false,
        )
        .unwrap();
        save_skill_config_at(
            codex_home.path(),
            appdata.path(),
            SkillConfigInput {
                skill_id: SkillId::GrokSearch,
                base_url: "https://old.example.com/api".to_string(),
                api_key: "sk-old".to_string(),
            },
        )
        .unwrap();
        let config_root = appdata.path().join("codex-switch/skills/grok-search");
        let config_path = config_root.join("config.json");
        let credential_path = config_root.join("credential.enc");
        let before_config = fs::read(&config_path).unwrap();
        let before_credential = fs::read(&credential_path).unwrap();
        let mut apply_failed = false;

        let error = save_skill_config_at_with(
            codex_home.path(),
            appdata.path(),
            SkillConfigInput {
                skill_id: SkillId::GrokSearch,
                base_url: "https://new.example.com/search".to_string(),
                api_key: "sk-new-secret-marker".to_string(),
            },
            |phase, path, bytes| {
                if phase == SkillConfigWritePhase::Apply
                    && path == credential_path.as_path()
                    && !apply_failed
                {
                    apply_failed = true;
                    Err("injected credential write failure".to_string())
                } else {
                    crate::file_ops::atomic_write(path, bytes)
                }
            },
        )
        .unwrap_err();

        assert!(error.contains("rolled back previous"), "{error}");
        assert!(!error.contains("sk-new-secret-marker"), "{error}");
        assert_eq!(fs::read(&config_path).unwrap(), before_config);
        assert_eq!(fs::read(&credential_path).unwrap(), before_credential);

        let mut apply_failed = false;
        let error = save_skill_config_at_with(
            codex_home.path(),
            appdata.path(),
            SkillConfigInput {
                skill_id: SkillId::GrokSearch,
                base_url: "https://new.example.com/search".to_string(),
                api_key: "sk-new-secret-marker".to_string(),
            },
            |phase, path, bytes| {
                if phase == SkillConfigWritePhase::Apply
                    && path == credential_path.as_path()
                    && !apply_failed
                {
                    apply_failed = true;
                    Err("injected credential write failure".to_string())
                } else if phase == SkillConfigWritePhase::Rollback && path == config_path.as_path()
                {
                    Err("injected config rollback failure".to_string())
                } else {
                    crate::file_ops::atomic_write(path, bytes)
                }
            },
        )
        .unwrap_err();

        assert!(error.contains("rollback failed"), "{error}");
        assert!(!error.contains("sk-new-secret-marker"), "{error}");
        assert_ne!(fs::read(&config_path).unwrap(), before_config);
        assert_eq!(fs::read(&credential_path).unwrap(), before_credential);
    }
}
