#[cfg(windows)]
pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    use std::ptr::null_mut;
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB},
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: plaintext.len() as u32,
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

    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
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
        Security::Cryptography::{CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB},
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: ciphertext.len() as u32,
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

    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(bytes)
}

#[cfg(not(windows))]
pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let mut bytes = b"DEV-NON-WINDOWS-ONLY:".to_vec();
    bytes.extend(plaintext.iter().rev());
    Ok(bytes)
}

#[cfg(not(windows))]
pub fn unprotect(ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    let prefix = b"DEV-NON-WINDOWS-ONLY:";
    if !ciphertext.starts_with(prefix) {
        return Err("invalid non-Windows development ciphertext".to_string());
    }
    Ok(ciphertext[prefix.len()..].iter().rev().copied().collect())
}

#[cfg(test)]
mod tests {
    use super::{protect, unprotect};

    #[test]
    fn protects_and_unprotects_bytes() {
        let secret = b"{\"auth_mode\":\"chatgpt\",\"tokens\":{\"access_token\":\"fake\"}}";

        let encrypted = protect(secret).unwrap();
        assert_ne!(encrypted, secret);

        let decrypted = unprotect(&encrypted).unwrap();
        assert_eq!(decrypted, secret);
    }
}
