//! macOS encryption uses one random master key in the current user's Keychain.
//!
//! Only that 32-byte key is stored in Keychain. Credentials and backup payloads
//! remain versioned AES-256-GCM envelopes on disk. There is no plaintext fallback
//! or automatic key replacement; losing the Keychain key makes old envelopes
//! unreadable. Windows DPAPI blobs are intentionally not portable to macOS.

use aes_gcm::{
    aead::{AeadInPlace, KeyInit},
    Aes256Gcm, Nonce, Tag,
};
use security_framework::{os::macos::keychain::SecKeychain, random::SecRandom};
use zeroize::{Zeroize, Zeroizing};

const KEYCHAIN_SERVICE: &str = "com.codex-switch.local-encryption";
const KEYCHAIN_ACCOUNT: &str = "master-key-v1";
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const MAGIC: &[u8; 8] = b"CDXSWMAC";
const FORMAT_VERSION: u8 = 1;
const HEADER_LEN: usize = MAGIC.len() + 1 + NONCE_LEN;
const ENVELOPE_OVERHEAD: usize = HEADER_LEN + TAG_LEN;
const CONTEXT: &[u8] = b"codex-switch:macos:keychain:aes-256-gcm:v1";

// Security.framework OSStatus values. Only item-not-found permits creation.
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
const ERR_SEC_DUPLICATE_ITEM: i32 = -25299;
const WINDOWS_DPAPI_PREFIX: &[u8] = &[
    0x01, 0x00, 0x00, 0x00, 0xd0, 0x8c, 0x9d, 0xdf, 0x01, 0x15, 0xd1, 0x11, 0x8c, 0x7a, 0x00, 0xc0,
    0x4f, 0xc2, 0x97, 0xeb,
];

type MasterKey = Zeroizing<[u8; KEY_LEN]>;

pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    encrypted_len(plaintext.len())?;
    let keychain = SecKeychain::default().map_err(keychain_error)?;
    protect_in(&keychain, plaintext)
}

pub fn unprotect(ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    // Reject foreign or invalid framing before requesting access to Keychain.
    envelope_nonce(ciphertext)?;
    let keychain = SecKeychain::default().map_err(keychain_error)?;
    unprotect_in(&keychain, ciphertext)
}

fn protect_in(keychain: &SecKeychain, plaintext: &[u8]) -> Result<Vec<u8>, String> {
    encrypted_len(plaintext.len())?;
    let key = load_or_create_master_key(keychain)?;
    seal(plaintext, &key, &random_nonce()?, CONTEXT)
}

fn unprotect_in(keychain: &SecKeychain, ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    envelope_nonce(ciphertext)?;
    let key = read_master_key(keychain)?.ok_or_else(missing_key_error)?;
    open(ciphertext, &key, CONTEXT)
}

fn keychain_error(error: security_framework::base::Error) -> String {
    format!(
        "macOS Keychain access failed (status {}); unlock the Keychain and allow this application access",
        error.code()
    )
}

fn missing_key_error() -> String {
    "macOS Keychain encryption key is missing; restore the original Keychain to decrypt existing data"
        .to_string()
}

fn read_master_key(keychain: &SecKeychain) -> Result<Option<MasterKey>, String> {
    match keychain.find_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT) {
        Ok((stored, _)) => {
            if stored.len() != KEY_LEN {
                return Err("macOS Keychain encryption key is invalid; it was not replaced".into());
            }
            let mut key = Zeroizing::new([0; KEY_LEN]);
            key.copy_from_slice(stored.as_ref());
            Ok(Some(key))
        }
        Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
        Err(error) => Err(keychain_error(error)),
    }
}

fn new_master_key() -> Result<MasterKey, String> {
    let mut key = Zeroizing::new([0; KEY_LEN]);
    SecRandom::default()
        .copy_bytes(key.as_mut())
        .map_err(|_| "macOS secure random key generation failed".to_string())?;
    Ok(key)
}

fn load_or_create_master_key(keychain: &SecKeychain) -> Result<MasterKey, String> {
    if let Some(key) = read_master_key(keychain)? {
        return Ok(key);
    }
    let candidate = new_master_key()?;
    insert_or_read_master_key(keychain, &candidate)
}

fn insert_or_read_master_key(
    keychain: &SecKeychain,
    candidate: &[u8; KEY_LEN],
) -> Result<MasterKey, String> {
    // Do not use set_generic_password: it updates an existing item and would
    // destroy another process's winning key during concurrent first startup.
    match keychain.add_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, candidate) {
        Ok(()) => {}
        Err(error) if error.code() == ERR_SEC_DUPLICATE_ITEM => {}
        Err(error) => return Err(keychain_error(error)),
    }
    // Always read back the persisted winner, including after successful insert.
    read_master_key(keychain)?.ok_or_else(missing_key_error)
}

fn random_nonce() -> Result<[u8; NONCE_LEN], String> {
    let mut nonce = [0; NONCE_LEN];
    SecRandom::default()
        .copy_bytes(&mut nonce)
        .map_err(|_| "macOS secure random nonce generation failed".to_string())?;
    Ok(nonce)
}

fn encrypted_len(plaintext_len: usize) -> Result<usize, String> {
    // Keep the existing backup payload limit and stay below GCM's per-message
    // limit. Check the framing addition separately instead of allowing overflow.
    if u32::try_from(plaintext_len).is_err() {
        return Err("payload exceeds the macOS encryption size limit".to_string());
    }
    plaintext_len
        .checked_add(ENVELOPE_OVERHEAD)
        .ok_or_else(|| "macOS encrypted payload size overflow".to_string())
}

fn envelope_nonce(envelope: &[u8]) -> Result<[u8; NONCE_LEN], String> {
    if envelope.starts_with(WINDOWS_DPAPI_PREFIX) {
        return Err(
            "Windows DPAPI data cannot be decrypted on macOS; export it using the original Windows account"
                .to_string(),
        );
    }
    if !envelope.starts_with(MAGIC) {
        return Err("data is not a supported macOS encryption envelope".to_string());
    }
    if envelope.len() < ENVELOPE_OVERHEAD {
        return Err("macOS encrypted payload is truncated".to_string());
    }
    if envelope[MAGIC.len()] != FORMAT_VERSION {
        return Err("unsupported macOS encryption envelope version".to_string());
    }
    encrypted_len(envelope.len() - ENVELOPE_OVERHEAD)?;
    envelope[MAGIC.len() + 1..HEADER_LEN]
        .try_into()
        .map_err(|_| "macOS encryption nonce is invalid".to_string())
}

fn associated_data(context: &[u8], header: &[u8]) -> Vec<u8> {
    let mut aad = context.to_vec();
    aad.extend_from_slice(header);
    aad
}

fn seal(
    plaintext: &[u8],
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    context: &[u8],
) -> Result<Vec<u8>, String> {
    let total_len = encrypted_len(plaintext.len())?;
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| "macOS encryption key is invalid".to_string())?;
    let mut envelope = Vec::new();
    envelope
        .try_reserve_exact(total_len)
        .map_err(|_| "not enough memory to protect macOS payload".to_string())?;
    envelope.extend_from_slice(MAGIC);
    envelope.push(FORMAT_VERSION);
    envelope.extend_from_slice(nonce);
    let aad = associated_data(context, &envelope);
    envelope.extend_from_slice(plaintext);
    let tag = match cipher.encrypt_in_place_detached(
        Nonce::from_slice(nonce),
        &aad,
        &mut envelope[HEADER_LEN..],
    ) {
        Ok(tag) => tag,
        Err(_) => {
            envelope.zeroize();
            return Err("macOS payload encryption failed".to_string());
        }
    };
    envelope.extend_from_slice(&tag);
    Ok(envelope)
}

fn open(envelope: &[u8], key: &[u8; KEY_LEN], context: &[u8]) -> Result<Vec<u8>, String> {
    let nonce = envelope_nonce(envelope)?;
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| "macOS encryption key is invalid".to_string())?;
    let tag_offset = envelope.len() - TAG_LEN;
    let aad = associated_data(context, &envelope[..HEADER_LEN]);
    let mut plaintext = Vec::new();
    plaintext
        .try_reserve_exact(tag_offset - HEADER_LEN)
        .map_err(|_| "not enough memory to decrypt macOS payload".to_string())?;
    plaintext.extend_from_slice(&envelope[HEADER_LEN..tag_offset]);
    if cipher
        .decrypt_in_place_detached(
            Nonce::from_slice(&nonce),
            &aad,
            &mut plaintext,
            Tag::from_slice(&envelope[tag_offset..]),
        )
        .is_err()
    {
        plaintext.zeroize();
        return Err(
            "macOS encrypted payload authentication failed; the data or Keychain key does not match"
                .to_string(),
        );
    }
    Ok(plaintext)
}

#[cfg(test)]
#[path = "crypto_macos_tests.rs"]
mod tests;
