//! Host-only ChatGPT session support for a contained Docker Codex run.

use base64::Engine as _;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The only ChatGPT backend a subscription session may reach.
pub const CHATGPT_CODEX_UPSTREAM: &str = "https://chatgpt.com/backend-api/codex";
/// The custom provider id handed to `codex-acp` inside the container.
pub const MODEL_PROVIDER_ID: &str = "maxplayer-chatgpt";
/// Extra token lifetime required after the job timeout.
pub const ACCESS_TOKEN_MARGIN: Duration = Duration::from_secs(15 * 60);

/// A validated host session. It deliberately has no `Debug` implementation because both fields must
/// stay out of logs and errors.
pub struct ChatgptSession {
    access_token: String,
    account_id: String,
}

impl ChatgptSession {
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }
}

/// A host-session refusal. No variant carries auth-file content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    Metadata {
        path: PathBuf,
        detail: String,
    },
    NotAFile {
        path: PathBuf,
    },
    InsecureMode {
        path: PathBuf,
        mode: u32,
    },
    Read {
        path: PathBuf,
        detail: String,
    },
    Json {
        path: PathBuf,
        line: usize,
        column: usize,
        detail: String,
    },
    EmptyField {
        path: PathBuf,
        field: &'static str,
    },
    Jwt {
        detail: &'static str,
    },
    Clock,
    Lifetime {
        remaining_secs: u64,
        required_secs: u64,
    },
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Metadata { path, detail } => {
                write!(
                    f,
                    "cannot inspect Codex auth file {}: {detail}",
                    path.display()
                )
            }
            Self::NotAFile { path } => {
                write!(
                    f,
                    "Codex auth path {} is not a regular file",
                    path.display()
                )
            }
            Self::InsecureMode { path, mode } => write!(
                f,
                "Codex auth file {} has mode {mode:#o}; set mode 0600",
                path.display()
            ),
            Self::Read { path, detail } => {
                write!(
                    f,
                    "cannot read Codex auth file {}: {detail}",
                    path.display()
                )
            }
            Self::Json {
                path,
                line,
                column,
                detail,
            } => write!(
                f,
                "Codex auth file {} is not valid auth JSON at line {line}, column {column}: {detail}",
                path.display()
            ),
            Self::EmptyField { path, field } => write!(
                f,
                "Codex auth file {} has an empty tokens.{field} field",
                path.display()
            ),
            Self::Jwt { detail } => write!(f, "Codex access token is not a valid JWT: {detail}"),
            Self::Clock => write!(f, "system clock is before the Unix epoch"),
            Self::Lifetime {
                remaining_secs,
                required_secs,
            } => write!(
                f,
                "Codex access token remaining lifetime is {remaining_secs} seconds; at least \
                 {required_secs} seconds is required for the job and safety margin"
            ),
        }
    }
}

impl std::error::Error for SessionError {}

#[derive(Deserialize)]
struct AuthFile {
    tokens: AuthTokens,
}

#[derive(Deserialize)]
struct AuthTokens {
    access_token: String,
    account_id: String,
}

#[derive(Deserialize)]
struct JwtClaims {
    exp: u64,
}

/// Read and validate the two host values a contained Codex run needs.
///
/// The typed JSON shape has no refresh-token field. Serde ignores that neighboring value, so it is
/// never bound to a Rust value or made available to the launch path.
pub fn read_chatgpt_session(
    auth_file: &Path,
    required_lifetime: Duration,
    now: SystemTime,
) -> Result<ChatgptSession, SessionError> {
    let metadata = std::fs::metadata(auth_file).map_err(|error| SessionError::Metadata {
        path: auth_file.to_path_buf(),
        detail: error.to_string(),
    })?;
    if !metadata.is_file() {
        return Err(SessionError::NotAFile {
            path: auth_file.to_path_buf(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(SessionError::InsecureMode {
                path: auth_file.to_path_buf(),
                mode,
            });
        }
    }

    let raw = std::fs::read_to_string(auth_file).map_err(|error| SessionError::Read {
        path: auth_file.to_path_buf(),
        detail: error.to_string(),
    })?;
    let parsed: AuthFile = serde_json::from_str(&raw).map_err(|error| SessionError::Json {
        path: auth_file.to_path_buf(),
        line: error.line(),
        column: error.column(),
        detail: error.to_string(),
    })?;
    let access_token = parsed.tokens.access_token.trim().to_owned();
    if access_token.is_empty() {
        return Err(SessionError::EmptyField {
            path: auth_file.to_path_buf(),
            field: "access_token",
        });
    }
    let account_id = parsed.tokens.account_id.trim().to_owned();
    if account_id.is_empty() {
        return Err(SessionError::EmptyField {
            path: auth_file.to_path_buf(),
            field: "account_id",
        });
    }

    let mut parts = access_token.split('.');
    let _header = parts.next();
    let payload = parts
        .next()
        .filter(|value| !value.is_empty())
        .ok_or(SessionError::Jwt {
            detail: "missing payload",
        })?;
    let signature = parts.next().filter(|value| !value.is_empty());
    if _header.is_none() || signature.is_none() || parts.next().is_some() {
        return Err(SessionError::Jwt {
            detail: "expected three segments",
        });
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| SessionError::Jwt {
            detail: "payload is not URL-safe Base64",
        })?;
    let claims: JwtClaims = serde_json::from_slice(&payload).map_err(|_| SessionError::Jwt {
        detail: "payload has no numeric exp claim",
    })?;

    let now_secs = now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SessionError::Clock)?
        .as_secs();
    let required_secs = required_lifetime
        .checked_add(ACCESS_TOKEN_MARGIN)
        .unwrap_or(Duration::MAX)
        .as_secs();
    let remaining_secs = claims.exp.saturating_sub(now_secs);
    if remaining_secs < required_secs {
        return Err(SessionError::Lifetime {
            remaining_secs,
            required_secs,
        });
    }

    Ok(ChatgptSession {
        access_token,
        account_id,
    })
}

/// Build the container-facing Codex configuration. Every credential value is a per-job placeholder.
pub fn provider_config_json(
    proxy_url: &str,
    access_placeholder: &str,
    account_placeholder: &str,
) -> String {
    serde_json::json!({
        "model_provider": MODEL_PROVIDER_ID,
        "model_providers": {
            MODEL_PROVIDER_ID: {
                "name": "Maxplayer ChatGPT",
                "base_url": proxy_url,
                "wire_api": "responses",
                "requires_openai_auth": false,
                "http_headers": {
                    "Authorization": format!("Bearer {access_placeholder}"),
                    "ChatGPT-Account-ID": account_placeholder,
                }
            }
        }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const NOW_SECS: u64 = 1_800_000_000;

    fn synthetic_jwt(exp: u64) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!(r#"{{"exp":{exp}}}"#).as_bytes());
        format!("{header}.{payload}.synthetic-signature")
    }

    fn test_dir(label: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "maxplayer-codex-subscription-{label}-{}-{stamp}",
            std::process::id()
        ))
    }

    fn write_auth(path: &Path, access_token: &str, account_id: &str) {
        std::fs::create_dir_all(path.parent().expect("auth parent")).expect("create auth parent");
        let body = serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": access_token,
                "account_id": account_id,
                "refresh_token": "synthetic-refresh-value-that-must-not-escape"
            }
        });
        std::fs::write(path, serde_json::to_vec(&body).expect("serialize auth"))
            .expect("write auth");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .expect("set auth mode");
        }
    }

    #[test]
    fn codex_subscription_reads_only_a_live_access_session() {
        let dir = test_dir("live");
        let auth_file = dir.join("auth.json");
        let access_token = synthetic_jwt(NOW_SECS + 1_501);
        write_auth(&auth_file, &access_token, "account-test");

        let session = super::read_chatgpt_session(
            &auth_file,
            Duration::from_secs(600),
            UNIX_EPOCH + Duration::from_secs(NOW_SECS),
        )
        .expect("a token with the job lifetime and margin must pass");

        assert_eq!(session.access_token(), access_token);
        assert_eq!(session.account_id(), "account-test");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn codex_subscription_refuses_a_token_that_cannot_outlive_the_job_margin() {
        let dir = test_dir("short");
        let auth_file = dir.join("auth.json");
        write_auth(&auth_file, &synthetic_jwt(NOW_SECS + 1_499), "account-test");

        let error = super::read_chatgpt_session(
            &auth_file,
            Duration::from_secs(600),
            UNIX_EPOCH + Duration::from_secs(NOW_SECS),
        )
        .err()
        .expect("the remaining life is one second short");

        let message = error.to_string();
        assert!(message.contains("remaining lifetime"), "{message}");
        assert!(
            message.contains("1500"),
            "the required seconds must be visible: {message}"
        );
        assert!(
            !message.contains("synthetic-refresh-value"),
            "an error must not contain the ignored refresh token: {message}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn codex_subscription_refuses_malformed_or_incomplete_auth_data_without_echoing_it() {
        let dir = test_dir("malformed");
        let auth_file = dir.join("auth.json");
        write_auth(&auth_file, "not-a-jwt-sensitive-sentinel", "account-test");

        let error = super::read_chatgpt_session(
            &auth_file,
            Duration::from_secs(1),
            UNIX_EPOCH + Duration::from_secs(NOW_SECS),
        )
        .err()
        .expect("a non-JWT access token must be refused");
        let message = error.to_string();
        assert!(message.contains("JWT"), "{message}");
        assert!(
            !message.contains("not-a-jwt-sensitive-sentinel"),
            "an error must not echo the access token: {message}"
        );

        std::fs::write(&auth_file, br#"{"tokens":{"account_id":"account-test"}}"#)
            .expect("replace auth");
        let error = super::read_chatgpt_session(
            &auth_file,
            Duration::from_secs(1),
            UNIX_EPOCH + Duration::from_secs(NOW_SECS),
        )
        .err()
        .expect("a missing access token must be refused");
        assert!(error.to_string().contains("access_token"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn codex_subscription_refuses_group_or_world_access_to_the_auth_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = test_dir("mode");
        let auth_file = dir.join("auth.json");
        write_auth(
            &auth_file,
            &synthetic_jwt(NOW_SECS + 10_000),
            "account-test",
        );
        std::fs::set_permissions(&auth_file, std::fs::Permissions::from_mode(0o640))
            .expect("make auth file too open");

        let error = super::read_chatgpt_session(
            &auth_file,
            Duration::from_secs(1),
            UNIX_EPOCH + Duration::from_secs(NOW_SECS),
        )
        .err()
        .expect("group-readable auth must be refused");
        let message = error.to_string();
        assert!(
            message.contains("0600"),
            "the safe mode must be named: {message}"
        );
        assert!(
            message.contains("0o640"),
            "the current mode must be named: {message}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn codex_subscription_provider_config_contains_placeholders_only() {
        let json = super::provider_config_json(
            "http://maxplayer-proxy:9123",
            "access-placeholder",
            "account-placeholder",
        );
        let config: serde_json::Value = serde_json::from_str(&json).expect("provider config JSON");
        let provider = &config["model_providers"]["maxplayer-chatgpt"];

        assert_eq!(config["model_provider"], "maxplayer-chatgpt");
        assert_eq!(provider["name"], "Maxplayer ChatGPT");
        assert_eq!(provider["base_url"], "http://maxplayer-proxy:9123");
        assert_eq!(provider["wire_api"], "responses");
        assert_eq!(provider["requires_openai_auth"], false);
        assert_eq!(
            provider["http_headers"]["Authorization"],
            "Bearer access-placeholder"
        );
        assert_eq!(
            provider["http_headers"]["ChatGPT-Account-ID"],
            "account-placeholder"
        );
        assert!(!json.contains("real-access"));
        assert!(!json.contains("real-account"));
    }
}
