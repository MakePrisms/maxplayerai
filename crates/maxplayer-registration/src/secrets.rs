//! Secret loading. Every secret comes from a private file on the box (or, for the
//! two Resend values only, the environment). None is ever in the repo, the config
//! file, the Nix store, logs or error messages.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Read a secret file: it must be a regular file (not a symlink) that no group or
/// other user can read or write (the `secret_file_must_be_private` rule used by the
/// reviewer). Surrounding whitespace is trimmed; an empty file is refused.
pub fn read_secret_file(path: &Path) -> Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::symlink_metadata(path)
            .with_context(|| format!("secret file {} is unavailable", path.display()))?;
        if !meta.is_file() {
            bail!("secret file {} must be a regular file", path.display());
        }
        if meta.permissions().mode() & 0o077 != 0 {
            bail!(
                "secret_file_must_be_private: {} is readable or writable by group/other (chmod 600)",
                path.display()
            );
        }
    }
    let value = std::fs::read_to_string(path)
        .with_context(|| format!("secret file {} is unreadable", path.display()))?;
    let value = value.trim();
    if value.is_empty() {
        bail!("secret file {} is empty", path.display());
    }
    Ok(value.to_string())
}

/// A file if configured, otherwise the named environment variable, otherwise `None`.
pub fn file_or_env(file: Option<&Path>, env: &str) -> Result<Option<String>> {
    if let Some(path) = file {
        return read_secret_file(path).map(Some);
    }
    match std::env::var(env) {
        Ok(v) if !v.trim().is_empty() => Ok(Some(v.trim().to_string())),
        _ => Ok(None),
    }
}

/// The relay admin key: 64-hex or `nsec1...`.
pub fn load_admin_keys(path: &Path) -> Result<nostr::Keys> {
    let raw = read_secret_file(path)?;
    // Never include `raw` in the error.
    nostr::Keys::parse(&raw).map_err(|_| {
        anyhow!(
            "relay admin key file {} does not hold a 64-hex or nsec secret key",
            path.display()
        )
    })
}

/// Server-side key for hashing codes. A 6-digit code has only a million values, so a
/// plain hash in a leaked database dump would be reversed in milliseconds. Keying the
/// hash with a secret that is not in the database stops that. It is derived from the
/// relay admin key (already a required private file), so there is no extra secret to
/// provision; rotating the admin key only invalidates codes that are still live
/// (10 minutes at most).
pub fn code_pepper(admin: &nostr::Keys) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(admin.secret_key().as_secret_bytes())
        .expect("hmac accepts any key length");
    mac.update(b"maxplayer-registration/code-pepper/v1");
    mac.finalize().into_bytes().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &tempfile::TempDir, name: &str, body: &str, mode: u32) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    #[test]
    fn private_file_is_read_and_trimmed() {
        let d = tempfile::tempdir().unwrap();
        let p = write(&d, "k", "  re_abc\n", 0o600);
        assert_eq!(read_secret_file(&p).unwrap(), "re_abc");
        let p = write(&d, "k2", "re_abc", 0o400);
        assert_eq!(read_secret_file(&p).unwrap(), "re_abc");
    }

    #[test]
    fn shared_empty_or_symlinked_files_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let p = write(&d, "k", "re_abc", 0o640);
        let err = read_secret_file(&p).unwrap_err().to_string();
        assert!(err.contains("secret_file_must_be_private"), "{err}");
        assert!(!err.contains("re_abc"));
        let p = write(&d, "e", "  \n", 0o600);
        assert!(read_secret_file(&p).is_err());
        let target = write(&d, "t", "re_abc", 0o600);
        let link = d.path().join("l");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_secret_file(&link).is_err());
    }

    #[test]
    fn admin_key_accepts_hex_and_nsec_and_never_echoes_bad_content() {
        use nostr::ToBech32;
        let d = tempfile::tempdir().unwrap();
        let keys = nostr::Keys::generate();
        let p = write(&d, "hex", &keys.secret_key().to_secret_hex(), 0o600);
        assert_eq!(load_admin_keys(&p).unwrap().public_key(), keys.public_key());
        let p = write(&d, "nsec", &keys.secret_key().to_bech32().unwrap(), 0o600);
        assert_eq!(load_admin_keys(&p).unwrap().public_key(), keys.public_key());
        let p = write(
            &d,
            "bad",
            "BUZZ_RELAY_PRIVATE_KEY=nothex-secretvalue",
            0o600,
        );
        let err = load_admin_keys(&p).unwrap_err().to_string();
        assert!(!err.contains("secretvalue"), "{err}");
    }

    #[test]
    fn pepper_is_stable_per_key_and_differs_across_keys() {
        let a = nostr::Keys::generate();
        let b = nostr::Keys::generate();
        assert_eq!(code_pepper(&a), code_pepper(&a));
        assert_ne!(code_pepper(&a), code_pepper(&b));
    }
}
