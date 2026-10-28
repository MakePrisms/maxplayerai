//! Input validation and code helpers.

use hmac::{Hmac, Mac};
use nostr::{PublicKey, ToBech32};
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// Trim + lowercase, then a deliberately plain shape check (one `@`, a dotted
/// domain, no spaces or control characters, at most 254 bytes). Deliverability is
/// proven by the code email itself, not here.
pub fn normalize_email(raw: &str) -> Option<String> {
    let email = raw.trim().to_lowercase();
    if email.is_empty() || email.len() > 254 {
        return None;
    }
    if email.chars().any(|c| {
        c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | ',' | ';' | '\\')
    }) {
        return None;
    }
    let (local, domain) = email.split_once('@')?;
    if local.is_empty() || local.len() > 64 || domain.contains('@') {
        return None;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2
        || labels
            .iter()
            .any(|l| l.is_empty() || l.starts_with('-') || l.ends_with('-'))
    {
        return None;
    }
    Some(email)
}

/// Agent labels are shown inside the code and welcome emails. Resend's `{{{VAR}}}`
/// substitution is unescaped HTML, so labels are limited to letters, digits, space,
/// `.`, `_` and `-` (1–64 chars after trimming) and are HTML-escaped again when sent.
pub fn normalize_label(raw: &str) -> Option<String> {
    let label = raw.trim();
    let ok = !label.is_empty()
        && label.chars().count() <= 64
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-'));
    ok.then(|| label.to_string())
}

/// The CLI may send the code with spaces or a dash ("482 913"); anything else
/// that is not exactly six digits is refused.
pub fn normalize_code(raw: &str) -> Option<String> {
    let code: String = raw.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    (code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit())).then_some(code)
}

/// A uniformly random 6-digit code from the OS RNG (rejection sampling, no modulo bias).
pub fn generate_code() -> String {
    const RANGE: u32 = 1_000_000;
    const LIMIT: u32 = u32::MAX - (u32::MAX % RANGE);
    loop {
        let mut buf = [0u8; 4];
        getrandom::fill(&mut buf).expect("OS RNG");
        let n = u32::from_le_bytes(buf);
        if n < LIMIT {
            return format!("{:06}", n % RANGE);
        }
    }
}

/// Keyed hash of a code, bound to the key it was issued for. Only this is stored.
pub fn code_hash(pepper: &[u8; 32], pubkey_hex: &str, code: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(pepper).expect("hmac key");
    mac.update(b"link:");
    mac.update(pubkey_hex.as_bytes());
    mac.update(b":");
    mac.update(code.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

pub fn code_matches(pepper: &[u8; 32], pubkey_hex: &str, code: &str, stored: &[u8]) -> bool {
    code_hash(pepper, pubkey_hex, code).ct_eq(stored).into()
}

/// `npub1qz7…8k3x`: the first 8 and last 4 characters of the npub, as in the code
/// email's sample (artifacts/signup-emails, `NPUB_SHORT`).
pub fn npub_short(pk: &PublicKey) -> String {
    let npub = pk.to_bech32().expect("npub encoding");
    format!("{}…{}", &npub[..8], &npub[npub.len() - 4..])
}

pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Hex SHA-256 of an email, used as the rate-limit bucket key so counters do not
/// spread the address itself.
pub fn email_bucket(email: &str) -> String {
    use sha2::Digest;
    hex::encode(Sha256::digest(email.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_trimmed_lowercased_and_shape_checked() {
        assert_eq!(
            normalize_email("  You@Example.COM \n").as_deref(),
            Some("you@example.com")
        );
        assert_eq!(
            normalize_email("a.b+tag@mail.example.co.uk").as_deref(),
            Some("a.b+tag@mail.example.co.uk")
        );
        for bad in [
            "",
            "no-at",
            "@x.com",
            "a@",
            "a@b",
            "a@@b.com",
            "a b@c.com",
            "a@b..com",
            "a@-b.com",
            "<a@b.com>",
            "a@b.com,c@d.com",
            "a@b.com\r\nBcc: x@y.com",
            &format!("{}@b.com", "x".repeat(65)),
        ] {
            assert_eq!(normalize_email(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn labels_are_plain_text_only() {
        assert_eq!(normalize_label(" my-laptop ").as_deref(), Some("my-laptop"));
        assert_eq!(
            normalize_label("VPS 2_prod.1").as_deref(),
            Some("VPS 2_prod.1")
        );
        for bad in [
            "",
            "   ",
            "<b>x</b>",
            "a\"b",
            "x&y",
            "tab\there",
            "é",
            &"x".repeat(65),
        ] {
            assert_eq!(normalize_label(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn codes_are_six_digits() {
        assert_eq!(normalize_code("482 913").as_deref(), Some("482913"));
        assert_eq!(normalize_code("482-913").as_deref(), Some("482913"));
        for bad in ["", "12345", "1234567", "12a456", "１２３４５６"] {
            assert_eq!(normalize_code(bad), None, "{bad:?}");
        }
        for _ in 0..200 {
            let c = generate_code();
            assert_eq!(normalize_code(&c).as_deref(), Some(c.as_str()));
        }
    }

    #[test]
    fn code_hash_is_keyed_and_bound_to_the_pubkey() {
        let pepper = [7u8; 32];
        let h = code_hash(&pepper, "aa", "123456");
        assert!(code_matches(&pepper, "aa", "123456", &h));
        assert!(!code_matches(&pepper, "aa", "123457", &h));
        assert!(!code_matches(&pepper, "bb", "123456", &h));
        assert!(!code_matches(&[8u8; 32], "aa", "123456", &h));
        assert_ne!(h, b"123456".to_vec());
    }

    #[test]
    fn npub_short_matches_the_template_sample_shape() {
        let pk = nostr::Keys::generate().public_key();
        let s = npub_short(&pk);
        let full = pk.to_bech32().unwrap();
        assert!(s.starts_with(&full[..8]) && s.ends_with(&full[full.len() - 4..]));
        assert_eq!(s.chars().count(), 13);
        assert!(s.contains('…'));
    }

    #[test]
    fn html_escape_covers_markup() {
        assert_eq!(
            html_escape("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }
}
