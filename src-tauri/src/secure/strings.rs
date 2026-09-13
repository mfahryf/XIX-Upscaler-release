//! L2: compile-time XOR obfuscation for sensitive strings.
//!
//! All endpoint URLs, payload field names and headers live in the binary as
//! XOR-encrypted bytes (never plaintext), so `strings`/binary editors reveal
//! nothing. The key is a single shared byte; this is deterrence against casual
//! RE only — a determined reverser can always recover a single-byte XOR. The
//! real protection is that every secret is consumed inside the Rust core and
//! never handed to the JS frontend.

/// Single shared XOR key for all `xstr!` literals.
pub const XKEY: u8 = 0x5A;

/// XOR-encrypt `s` at compile time into an exactly-sized array.
pub const fn enc<const N: usize>(s: &str) -> [u8; N] {
    let bytes = s.as_bytes();
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N && i < bytes.len() {
        out[i] = bytes[i] ^ XKEY;
        i += 1;
    }
    out
}

/// Decrypt bytes produced by `enc`/`xstr!` back into a `String`.
pub fn decrypt(bytes: &[u8], key: u8) -> String {
    let plain: Vec<u8> = bytes.iter().map(|b| b ^ key).collect();
    String::from_utf8_lossy(&plain).into_owned()
}

/// Embed a string literal as compile-time XOR-encrypted bytes.
///
/// ```rust
/// let url = xix_upscaler_lib::xstr!("https://svg.new/api/image/vectorize");
/// assert_eq!(
///     xix_upscaler_lib::secure::strings::decrypt(url, xix_upscaler_lib::secure::strings::XKEY),
///     "https://svg.new/api/image/vectorize"
/// );
/// ```
#[macro_export]
macro_rules! xstr {
    ($s:literal) => {{
        const S: &str = $s;
        const N: usize = S.len();
        const OUT: [u8; N] = $crate::secure::strings::enc::<N>(S);
        &OUT
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xstr_roundtrip_recovers_original() {
        let enc = crate::xstr!("https://svg.new/api/image/vectorize");
        assert_eq!(decrypt(enc, XKEY), "https://svg.new/api/image/vectorize");
    }

    #[test]
    fn encrypted_bytes_do_not_contain_plaintext() {
        let enc = crate::xstr!("https://svg.new/api/image/vectorize");
        let s = String::from_utf8_lossy(enc);
        assert!(
            !s.contains("svg.new"),
            "plaintext must not appear in binary"
        );
    }

    #[test]
    fn decrypt_is_exact_length() {
        let enc = crate::xstr!("/api/agent/edit/convert");
        assert_eq!(decrypt(enc, XKEY).len(), "/api/agent/edit/convert".len());
    }
}
