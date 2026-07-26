#[cfg(windows)]
pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    use std::ptr::null_mut;
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB},
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: dpapi_blob_len(plaintext.len())?,
        pbData: plaintext.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: null_mut(),
    };

    let ok = unsafe {
        CryptProtectData(
            &input,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err("CryptProtectData failed".to_string());
    }

    let bytes =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(bytes)
}

#[cfg(windows)]
pub fn unprotect(ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    use std::ptr::null_mut;
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: dpapi_blob_len(ciphertext.len())?,
        pbData: ciphertext.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: null_mut(),
    };

    let ok = unsafe {
        CryptUnprotectData(
            &input,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err("CryptUnprotectData failed".to_string());
    }

    let bytes =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(bytes)
}

#[cfg(windows)]
fn dpapi_blob_len(len: usize) -> Result<u32, String> {
    u32::try_from(len)
        .map_err(|_| "credential data exceeds the Windows DPAPI size limit".to_string())
}

#[cfg(not(windows))]
pub fn protect(_plaintext: &[u8]) -> Result<Vec<u8>, String> {
    Err("credential encryption is not supported on this platform".to_string())
}

#[cfg(not(windows))]
pub fn unprotect(_ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    Err("credential encryption is not supported on this platform".to_string())
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::dpapi_blob_len;
    use super::{protect, unprotect};

    #[test]
    #[cfg(windows)]
    fn protects_and_unprotects_bytes() {
        let secret = b"{\"auth_mode\":\"chatgpt\",\"tokens\":{\"access_token\":\"fake\"}}";

        let encrypted = protect(secret).unwrap();
        assert_ne!(encrypted, secret);

        let decrypted = unprotect(&encrypted).unwrap();
        assert_eq!(decrypted, secret);
    }

    #[test]
    #[cfg(all(windows, target_pointer_width = "64"))]
    fn rejects_dpapi_lengths_that_do_not_fit_the_windows_blob_contract() {
        let oversized = u32::MAX as usize + 1;

        let error = dpapi_blob_len(oversized).unwrap_err();

        assert!(error.contains("DPAPI size limit"), "{error}");
        assert_eq!(dpapi_blob_len(u32::MAX as usize).unwrap(), u32::MAX);
    }

    #[test]
    #[cfg(not(windows))]
    fn rejects_credentials_without_a_platform_keystore() {
        assert!(protect(b"secret").unwrap_err().contains("not supported"));
        assert!(unprotect(b"ciphertext")
            .unwrap_err()
            .contains("not supported"));
    }
}
