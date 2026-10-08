use super::*;
use core_foundation::base::TCFType;
use security_framework::os::macos::keychain::CreateOptions;
use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::{Builder, TempDir};

const TEST_PASSWORD: &str = "isolated-test-keychain-no-user-credentials";
const CHILD_TEST: &str = "crypto::macos::tests::keychain_race_child";
const TEST_PATH_ENV: &str = "CODEX_SWITCH_CRYPTO_TEST_KEYCHAIN";
const TEST_INDEX_ENV: &str = "CODEX_SWITCH_CRYPTO_TEST_INDEX";

#[link(name = "Security", kind = "framework")]
extern "C" {
    fn SecKeychainDelete(keychain: *mut std::ffi::c_void) -> i32;
}

struct TestKeychain {
    keychain: SecKeychain,
    directory: TempDir,
    deleted: bool,
}

impl TestKeychain {
    fn new() -> Self {
        let directory = Builder::new()
            .prefix("codex-switch-crypto-test-")
            .tempdir()
            .expect("create isolated crypto test directory");
        let keychain = CreateOptions::new()
            .password(TEST_PASSWORD)
            .create(directory.path().join("test.keychain-db"))
            .expect("create isolated test Keychain");
        fs::write(
            directory.path().join("test-owner"),
            b"codex-switch-crypto-v1",
        )
        .expect("write isolated test owner marker");
        Self {
            keychain,
            directory,
            deleted: false,
        }
    }

    fn path(&self) -> PathBuf {
        self.directory.path().join("test.keychain-db")
    }

    fn close(mut self) {
        // SecKeychainDelete also removes the test Keychain's search-list entry.
        let result = unsafe { SecKeychainDelete(self.keychain.as_concrete_TypeRef().cast()) };
        self.deleted = result == 0;
        assert_eq!(result, 0, "delete isolated test Keychain");
        assert!(!self.path().exists(), "isolated Keychain file was removed");
    }
}

impl Drop for TestKeychain {
    fn drop(&mut self) {
        if !self.deleted {
            // The handle was created here; this never selects the login Keychain.
            unsafe { SecKeychainDelete(self.keychain.as_concrete_TypeRef().cast()) };
        }
    }
}

#[test]
fn authenticated_envelope_roundtrips_empty_and_binary_payloads() {
    let key = [0x42; KEY_LEN];
    for plaintext in [&b""[..], &b"fixture\0binary\xffpayload"[..]] {
        let encrypted = seal(plaintext, &key, &[0x24; NONCE_LEN], CONTEXT).unwrap();
        assert_eq!(encrypted.len(), plaintext.len() + ENVELOPE_OVERHEAD);
        assert_ne!(encrypted.as_slice(), plaintext);
        assert_eq!(open(&encrypted, &key, CONTEXT).unwrap(), plaintext);
    }
}

#[test]
fn authenticated_envelope_rejects_tampering_wrong_key_and_wrong_context() {
    let key = [0x42; KEY_LEN];
    let encrypted = seal(b"fixture payload", &key, &[0x24; NONCE_LEN], CONTEXT).unwrap();
    for offset in [MAGIC.len() + 1, HEADER_LEN, encrypted.len() - 1] {
        let mut changed = encrypted.clone();
        changed[offset] ^= 1;
        assert!(open(&changed, &key, CONTEXT).is_err());
    }
    assert!(open(&encrypted, &[0x43; KEY_LEN], CONTEXT).is_err());
    assert!(open(&encrypted, &key, b"another application context").is_err());
}

#[test]
fn authenticated_envelope_rejects_every_truncated_prefix() {
    let key = [0x42; KEY_LEN];
    let encrypted = seal(b"fixture payload", &key, &[0x24; NONCE_LEN], CONTEXT).unwrap();
    for end in 0..encrypted.len() {
        assert!(open(&encrypted[..end], &key, CONTEXT).is_err());
    }
    let mut extended = encrypted.clone();
    extended.push(0);
    assert!(open(&extended, &key, CONTEXT).is_err());
}

#[test]
fn envelope_rejects_windows_dpapi_and_unknown_versions_before_keychain_access() {
    let fixture = TestKeychain::new();
    let mut windows_blob = WINDOWS_DPAPI_PREFIX.to_vec();
    windows_blob.extend_from_slice(b"opaque Windows fixture");
    let error = unprotect_in(&fixture.keychain, &windows_blob).unwrap_err();
    assert!(error.contains("Windows DPAPI"));
    assert!(!error.contains("opaque Windows fixture"));

    let mut encrypted = seal(b"fixture", &[0x42; KEY_LEN], &[0x24; NONCE_LEN], CONTEXT).unwrap();
    encrypted[MAGIC.len()] = FORMAT_VERSION + 1;
    assert!(unprotect_in(&fixture.keychain, &encrypted)
        .unwrap_err()
        .contains("version"));
    assert!(read_master_key(&fixture.keychain).unwrap().is_none());
    fixture.close();
}

#[test]
fn macos_size_limit_is_checked_without_allocating_the_payload() {
    assert!(encrypted_len(u32::MAX as usize).is_ok());
    if usize::BITS > 32 {
        assert!(encrypted_len(u32::MAX as usize + 1).is_err());
    }
}

#[test]
fn keychain_protection_uses_fresh_nonces_and_survives_reopen() {
    let fixture = TestKeychain::new();
    let first = protect_in(&fixture.keychain, b"fixture secret").unwrap();
    let second = protect_in(&fixture.keychain, b"fixture secret").unwrap();
    assert_ne!(first, second);
    let reopened = SecKeychain::open(fixture.path()).expect("reopen isolated Keychain");
    assert_eq!(unprotect_in(&reopened, &first).unwrap(), b"fixture secret");
    assert_eq!(unprotect_in(&reopened, &second).unwrap(), b"fixture secret");
    drop(reopened);
    fixture.close();
}

#[test]
fn large_backup_is_a_file_envelope_and_keychain_contains_only_the_master_key() {
    let fixture = TestKeychain::new();
    let plaintext = vec![0x5a; 2 * 1024 * 1024];
    let encrypted = protect_in(&fixture.keychain, &plaintext).unwrap();
    assert_eq!(encrypted.len(), plaintext.len() + ENVELOPE_OVERHEAD);
    let (stored, _) = fixture
        .keychain
        .find_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .unwrap();
    assert_eq!(stored.len(), KEY_LEN);
    drop(stored);
    assert_eq!(
        unprotect_in(&fixture.keychain, &encrypted).unwrap(),
        plaintext
    );
    fixture.close();
}

#[test]
fn missing_key_during_decryption_is_never_recreated() {
    let fixture = TestKeychain::new();
    let encrypted = seal(b"fixture", &[0x42; KEY_LEN], &[0x24; NONCE_LEN], CONTEXT).unwrap();
    assert!(unprotect_in(&fixture.keychain, &encrypted)
        .unwrap_err()
        .contains("missing"));
    assert!(read_master_key(&fixture.keychain).unwrap().is_none());
    fixture.close();
}

#[test]
fn malformed_existing_key_is_preserved_and_never_becomes_plaintext_fallback() {
    let fixture = TestKeychain::new();
    fixture
        .keychain
        .add_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, b"invalid")
        .unwrap();
    let error = protect_in(&fixture.keychain, b"fixture secret").unwrap_err();
    assert!(error.contains("invalid"));
    assert!(!error.contains("fixture secret"));
    let (stored, _) = fixture
        .keychain
        .find_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .unwrap();
    assert!(stored.as_ref() == b"invalid");
    drop(stored);
    fixture.close();
}

#[test]
fn duplicate_creation_reads_the_existing_key_without_overwriting_it() {
    let fixture = TestKeychain::new();
    let first = insert_or_read_master_key(&fixture.keychain, &[0x42; KEY_LEN]).unwrap();
    let second = insert_or_read_master_key(&fixture.keychain, &[0x24; KEY_LEN]).unwrap();
    assert!(first.as_ref() == second.as_ref());
    let encrypted = seal(b"fixture", &first, &[0x33; NONCE_LEN], CONTEXT).unwrap();
    assert_eq!(open(&encrypted, &second, CONTEXT).unwrap(), b"fixture");
    fixture.close();
}

struct TestChildren(Vec<Child>);

impl Drop for TestChildren {
    fn drop(&mut self) {
        for child in &mut self.0 {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                // These handles belong only to subprocesses spawned by this test.
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn wait_until(mut ready: impl FnMut() -> bool, duration: Duration) {
    let deadline = Instant::now() + duration;
    while !ready() {
        assert!(Instant::now() < deadline, "isolated crypto test timed out");
        thread::sleep(Duration::from_millis(20));
    }
}

fn child_fixture_path() -> PathBuf {
    let path = PathBuf::from(std::env::var_os(TEST_PATH_ENV).expect("test Keychain path"));
    let parent = path.parent().expect("test Keychain parent");
    assert_eq!(path.file_name().unwrap(), "test.keychain-db");
    assert!(parent
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("codex-switch-crypto-test-"));
    let canonical_parent = parent.canonicalize().expect("canonical test directory");
    let canonical_temp = std::env::temp_dir().canonicalize().expect("canonical temp");
    assert_eq!(canonical_parent.parent().unwrap(), canonical_temp);
    assert_eq!(
        fs::read(parent.join("test-owner")).expect("test owner marker"),
        b"codex-switch-crypto-v1"
    );
    path
}

#[test]
fn concurrent_processes_create_one_master_key_and_all_payloads_remain_readable() {
    const CHILD_COUNT: usize = 4;
    let fixture = TestKeychain::new();
    let mut children = TestChildren(Vec::new());
    for index in 0..CHILD_COUNT {
        children.0.push(
            Command::new(std::env::current_exe().expect("current test executable"))
                .args(["--exact", CHILD_TEST, "--ignored"])
                .env(TEST_PATH_ENV, fixture.path())
                .env(TEST_INDEX_ENV, index.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start isolated crypto child"),
        );
    }

    wait_until(
        || {
            (0..CHILD_COUNT).all(|index| {
                fixture
                    .directory
                    .path()
                    .join(format!("ready-{index}"))
                    .exists()
            })
        },
        Duration::from_secs(30),
    );
    fs::write(fixture.directory.path().join("go"), b"go").expect("release race barrier");
    wait_until(
        || {
            children
                .0
                .iter_mut()
                .all(|child| child.try_wait().expect("observe crypto child").is_some())
        },
        Duration::from_secs(30),
    );
    for child in &mut children.0 {
        assert!(child.wait().expect("wait crypto child").success());
    }
    for index in 0..CHILD_COUNT {
        let encrypted = fs::read(fixture.directory.path().join(format!("ciphertext-{index}")))
            .expect("read child ciphertext");
        assert_eq!(
            unprotect_in(&fixture.keychain, &encrypted).unwrap(),
            b"cross-process fixture"
        );
    }
    drop(children);
    fixture.close();
}

#[test]
#[ignore = "launched only by the isolated multi-process Keychain test"]
fn keychain_race_child() {
    let path = child_fixture_path();
    let directory = path.parent().unwrap();
    let index: usize = std::env::var(TEST_INDEX_ENV)
        .expect("test child index")
        .parse()
        .expect("numeric test child index");
    assert!(index < 4);
    let keychain = SecKeychain::open(&path).expect("open isolated test Keychain");
    assert!(read_master_key(&keychain).unwrap().is_none());
    let candidate = new_master_key().unwrap();
    fs::write(directory.join(format!("ready-{index}")), b"ready").expect("publish race readiness");
    wait_until(|| directory.join("go").exists(), Duration::from_secs(30));
    let key = insert_or_read_master_key(&keychain, &candidate).unwrap();
    let encrypted = seal(
        b"cross-process fixture",
        &key,
        &random_nonce().unwrap(),
        CONTEXT,
    )
    .unwrap();
    fs::write(directory.join(format!("ciphertext-{index}")), encrypted)
        .expect("write isolated child ciphertext");
}
