//! L2: Windows DPAPI for engine API keys.
//!
//! Keys (e.g. the PhotoRoom `API_key.txt`) are never stored as plaintext on
//! disk. They are protected with `CryptProtectData`, which encrypts the blob
//! bound to the current Windows user + machine — reading the binary or the
//! file yields nothing usable; only the same user on the same machine can
//! decrypt at runtime. Used by future engines that need an API key (vectorize
//! v2 itself is anonymous and needs no key).

#[cfg(windows)]
mod imp {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    pub fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
        let blob = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        let hr = unsafe {
            CryptProtectData(
                &blob,
                PWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if hr.is_err() {
            return Err(format!("CryptProtectData failed: {hr:?}"));
        }
        let result =
            unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe {
            LocalFree(HLOCAL(out.pbData as *mut core::ffi::c_void));
        }
        Ok(result)
    }

    pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>, String> {
        let data = CRYPT_INTEGER_BLOB {
            cbData: blob.len() as u32,
            pbData: blob.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        let hr = unsafe {
            CryptUnprotectData(
                &data,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if hr.is_err() {
            return Err(format!("CryptUnprotectData failed: {hr:?}"));
        }
        let result =
            unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe {
            LocalFree(HLOCAL(out.pbData as *mut core::ffi::c_void));
        }
        Ok(result)
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn protect(_plain: &[u8]) -> Result<Vec<u8>, String> {
        Err("DPAPI unavailable on this platform".into())
    }
    pub fn unprotect(_blob: &[u8]) -> Result<Vec<u8>, String> {
        Err("DPAPI unavailable on this platform".into())
    }
}

pub use imp::{protect, unprotect};

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn dpapi_roundtrip() {
        let blob = protect(b"secret-key-123").unwrap();
        assert_ne!(blob, b"secret-key-123", "blob must be encrypted");
        assert_eq!(unprotect(&blob).unwrap(), b"secret-key-123");
    }

    #[cfg(not(windows))]
    #[test]
    fn dpapi_stub_errors() {
        assert!(protect(b"x").is_err());
        assert!(unprotect(b"x").is_err());
    }
}
