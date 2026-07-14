use std::fs;

use codex_switch_lib::skill_manager::{
    install_skill_at, list_skills_at, recover_skill_transaction_at, save_skill_config_at,
    SkillConfigInput, SkillId, SkillState,
};
use tempfile::tempdir;

#[test]
fn installs_both_fixed_packages_and_detects_managed_state() {
    let codex_home = tempdir().unwrap();
    let appdata = tempdir().unwrap();

    let image =
        install_skill_at(codex_home.path(), appdata.path(), SkillId::Image2, false).unwrap();
    let grok = install_skill_at(
        codex_home.path(),
        appdata.path(),
        SkillId::GrokSearch,
        false,
    )
    .unwrap();

    assert!(image.restart_required);
    assert!(grok.restart_required);
    assert!(codex_home
        .path()
        .join("skills/newapi-image2-client/SKILL.md")
        .exists());
    assert!(codex_home
        .path()
        .join("skills/grok-search/scripts/grok-search.ps1")
        .exists());
    let image_source = fs::read_to_string(
        codex_home
            .path()
            .join("skills/newapi-image2-client/SOURCE.json"),
    )
    .unwrap();
    assert!(
        image_source.contains("648C192C2414BBFD9DBA36E264C01932BDCF7E2057A8BA2DA7006B40A94B332B")
    );
    let grok_skill =
        fs::read_to_string(codex_home.path().join("skills/grok-search/SKILL.md")).unwrap();
    assert!(!grok_skill.contains("C:\\Users\\admin"));
    assert!(!grok_skill.contains("x666.me"));

    let statuses = list_skills_at(codex_home.path(), appdata.path()).unwrap();
    assert_eq!(statuses.len(), 2);
    assert!(statuses
        .iter()
        .all(|status| status.state == SkillState::Current));
}

#[test]
fn unmanaged_or_drifted_skill_requires_explicit_replacement_and_is_backed_up() {
    let codex_home = tempdir().unwrap();
    let appdata = tempdir().unwrap();
    let target = codex_home.path().join("skills/grok-search");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("custom.txt"), "keep me").unwrap();

    let error = install_skill_at(
        codex_home.path(),
        appdata.path(),
        SkillId::GrokSearch,
        false,
    )
    .unwrap_err();
    assert!(error.contains("confirmation"));
    assert!(target.join("custom.txt").exists());

    let receipt =
        install_skill_at(codex_home.path(), appdata.path(), SkillId::GrokSearch, true).unwrap();
    let backup = receipt.backup_dir.expect("replacement must keep a backup");
    assert!(backup.join("custom.txt").exists());

    fs::write(target.join("SKILL.md"), "local drift").unwrap();
    let status = list_skills_at(codex_home.path(), appdata.path()).unwrap();
    assert_eq!(status[1].state, SkillState::Drifted);
    assert!(install_skill_at(
        codex_home.path(),
        appdata.path(),
        SkillId::GrokSearch,
        false,
    )
    .is_err());
}

#[test]
fn skill_configuration_encrypts_keys_and_blank_update_preserves_ciphertext() {
    let codex_home = tempdir().unwrap();
    let appdata = tempdir().unwrap();
    install_skill_at(codex_home.path(), appdata.path(), SkillId::Image2, false).unwrap();
    install_skill_at(
        codex_home.path(),
        appdata.path(),
        SkillId::GrokSearch,
        false,
    )
    .unwrap();

    let secret = "sk-fake-user-secret";
    save_skill_config_at(
        codex_home.path(),
        appdata.path(),
        SkillConfigInput {
            skill_id: SkillId::Image2,
            base_url: "https://api.lcming951.com/v1/".to_string(),
            api_key: secret.to_string(),
        },
    )
    .unwrap();
    save_skill_config_at(
        codex_home.path(),
        appdata.path(),
        SkillConfigInput {
            skill_id: SkillId::GrokSearch,
            base_url: "https://research.example.com/v1".to_string(),
            api_key: secret.to_string(),
        },
    )
    .unwrap();

    let image_root = appdata.path().join("codex-switch/skills/image2");
    let grok_root = appdata.path().join("codex-switch/skills/grok-search");
    let image_cipher = fs::read(image_root.join("credential.enc")).unwrap();
    let grok_cipher = fs::read(grok_root.join("credential.enc")).unwrap();
    assert!(!String::from_utf8_lossy(&image_cipher).contains(secret));
    assert!(!String::from_utf8_lossy(&grok_cipher).contains(secret));
    assert!(!fs::read_to_string(image_root.join("config.json"))
        .unwrap()
        .contains(secret));
    assert!(!fs::read_to_string(grok_root.join("config.json"))
        .unwrap()
        .contains(secret));

    save_skill_config_at(
        codex_home.path(),
        appdata.path(),
        SkillConfigInput {
            skill_id: SkillId::GrokSearch,
            base_url: "https://research.example.com".to_string(),
            api_key: String::new(),
        },
    )
    .unwrap();
    assert_eq!(
        fs::read(grok_root.join("credential.enc")).unwrap(),
        grok_cipher
    );

    let statuses = list_skills_at(codex_home.path(), appdata.path()).unwrap();
    let image = statuses
        .iter()
        .find(|item| item.id == SkillId::Image2)
        .unwrap();
    let grok = statuses
        .iter()
        .find(|item| item.id == SkillId::GrokSearch)
        .unwrap();
    assert_eq!(image.base_url, "https://api.lcming951.com/v1");
    assert_eq!(grok.base_url, "https://research.example.com");
    assert!(image.credential_configured && grok.credential_configured);
}

#[test]
fn rejects_unsafe_provider_urls_without_echoing_secret_input() {
    let codex_home = tempdir().unwrap();
    let appdata = tempdir().unwrap();
    install_skill_at(
        codex_home.path(),
        appdata.path(),
        SkillId::GrokSearch,
        false,
    )
    .unwrap();

    for url in [
        "file:///tmp/provider",
        "https://user:pass@example.com",
        "https://example.com/path?api_key=secret-marker",
        "http://example.com",
    ] {
        let error = save_skill_config_at(
            codex_home.path(),
            appdata.path(),
            SkillConfigInput {
                skill_id: SkillId::GrokSearch,
                base_url: url.to_string(),
                api_key: "sk-fake".to_string(),
            },
        )
        .unwrap_err();
        assert!(!error.contains("secret-marker"));
        assert!(!error.contains("user:pass"));
    }
}

#[cfg(windows)]
#[test]
fn rust_dpapi_credential_is_readable_by_windows_powershell() {
    use std::process::Command;

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
            base_url: "https://research.example.com".to_string(),
            api_key: "sk-fake-cross-runtime".to_string(),
        },
    )
    .unwrap();
    let credential = appdata
        .path()
        .join("codex-switch/skills/grok-search/credential.enc");
    let script = r#"
if (-not ('System.Security.Cryptography.ProtectedData' -as [type])) { Add-Type -AssemblyName System.Security }
$protected = [Convert]::FromBase64String((Get-Content -LiteralPath $env:TEST_CREDENTIAL -Raw).Trim())
$plain = [System.Security.Cryptography.ProtectedData]::Unprotect($protected, $null, [System.Security.Cryptography.DataProtectionScope]::CurrentUser)
try { if ([Text.Encoding]::UTF8.GetString($plain) -ne 'sk-fake-cross-runtime') { exit 2 } }
finally { [Array]::Clear($plain, 0, $plain.Length) }
"#;
    let status = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", script])
        .env("TEST_CREDENTIAL", credential)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn recovers_an_interrupted_directory_swap_before_the_next_install() {
    let codex_home = tempdir().unwrap();
    let appdata = tempdir().unwrap();
    install_skill_at(
        codex_home.path(),
        appdata.path(),
        SkillId::GrokSearch,
        false,
    )
    .unwrap();
    let target = codex_home.path().join("skills/grok-search");
    let stage = codex_home
        .path()
        .join("skills/.codex-switch-stage-crash-test");
    fs::create_dir_all(&stage).unwrap();
    fs::write(stage.join("partial.txt"), "partial").unwrap();
    let backup = codex_home
        .path()
        .join(".codex-switch/skill-backups/crash-test/grok-search");
    fs::create_dir_all(backup.parent().unwrap()).unwrap();
    fs::rename(&target, &backup).unwrap();
    let journal = codex_home
        .path()
        .join(".codex-switch/skill-transactions/grok-search.json");
    fs::create_dir_all(journal.parent().unwrap()).unwrap();
    fs::write(
        &journal,
        serde_json::to_vec(&serde_json::json!({
            "skillId": "grokSearch",
            "target": target,
            "stage": stage,
            "backup": backup,
        }))
        .unwrap(),
    )
    .unwrap();

    assert!(recover_skill_transaction_at(codex_home.path(), SkillId::GrokSearch).unwrap());
    assert!(target.join("SKILL.md").exists());
    assert!(!stage.exists());
    assert!(!journal.exists());
}
