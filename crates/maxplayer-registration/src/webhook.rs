//! Resend webhooks (signed by Svix): signature check and payload handling.
//!
//! Signed content is `<svix-id>.<svix-timestamp>.<raw body>`, HMAC-SHA256 with the
//! base64 part of the `whsec_...` secret; `svix-signature` holds space-separated
//! `v1,<base64>` entries. Timestamps more than 5 minutes off are refused.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

pub const TOLERANCE_SECS: i64 = 300;

/// `whsec_<base64>` (or bare base64) → key bytes.
pub fn parse_secret(raw: &str) -> Option<Vec<u8>> {
    let b64 = raw.trim().strip_prefix("whsec_").unwrap_or(raw.trim());
    STANDARD.decode(b64).ok().filter(|k| !k.is_empty())
}

pub fn sign(secret: &[u8], id: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("hmac key");
    mac.update(id.as_bytes());
    mac.update(b".");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    STANDARD.encode(mac.finalize().into_bytes())
}

#[derive(Debug, PartialEq, Eq)]
pub enum WebhookError {
    MissingHeaders,
    Stale,
    BadSignature,
}

pub fn verify(
    secret: &[u8],
    id: Option<&str>,
    timestamp: Option<&str>,
    signatures: Option<&str>,
    body: &[u8],
    now_unix: i64,
) -> Result<(), WebhookError> {
    let (Some(id), Some(ts), Some(sigs)) = (id, timestamp, signatures) else {
        return Err(WebhookError::MissingHeaders);
    };
    let ts: i64 = ts
        .trim()
        .parse()
        .map_err(|_| WebhookError::MissingHeaders)?;
    if (now_unix - ts).abs() > TOLERANCE_SECS {
        return Err(WebhookError::Stale);
    }
    let want = sign(secret, id, ts, body);
    let ok = sigs.split_whitespace().any(|entry| {
        entry
            .strip_prefix("v1,")
            .is_some_and(|sig| bool::from(sig.as_bytes().ct_eq(want.as_bytes())))
    });
    if ok {
        Ok(())
    } else {
        Err(WebhookError::BadSignature)
    }
}

/// What the service does with one Resend event.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// `email.bounced`: record a bounce per recipient.
    Bounce {
        emails: Vec<String>,
    },
    /// `email.complained`: record it as a bounce-like delivery problem and stop
    /// product news (unsubscribe method `complaint`).
    Complaint {
        emails: Vec<String>,
    },
    /// `contact.updated` with `unsubscribed: true` (the unsubscribe link in a
    /// Resend broadcast): stop product news (method `link`).
    Unsubscribed {
        email: String,
    },
    Ignore,
}

fn recipients(data: &serde_json::Value) -> Vec<String> {
    let to = &data["to"];
    let list: Vec<String> = match to {
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        serde_json::Value::String(s) => vec![s.clone()],
        _ => vec![],
    };
    list.iter()
        .filter_map(|e| crate::validate::normalize_email(e))
        .collect()
}

pub fn classify(payload: &serde_json::Value) -> Action {
    let data = &payload["data"];
    match payload["type"].as_str().unwrap_or("") {
        "email.bounced" => Action::Bounce {
            emails: recipients(data),
        },
        "email.complained" => Action::Complaint {
            emails: recipients(data),
        },
        "contact.updated" if data["unsubscribed"].as_bool() == Some(true) => {
            match data["email"]
                .as_str()
                .and_then(crate::validate::normalize_email)
            {
                Some(email) => Action::Unsubscribed { email },
                None => Action::Ignore,
            }
        }
        _ => Action::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";

    #[test]
    fn svix_signatures_verify() {
        let key = parse_secret(SECRET).unwrap();
        let body = br#"{"type":"email.bounced"}"#;
        let sig = sign(&key, "msg_1", 1_800_000_000, body);
        let header = format!("v1,bogus v1,{sig}");
        assert_eq!(
            verify(
                &key,
                Some("msg_1"),
                Some("1800000000"),
                Some(&header),
                body,
                1_800_000_100
            ),
            Ok(())
        );
        assert_eq!(
            verify(
                &key,
                Some("msg_2"),
                Some("1800000000"),
                Some(&header),
                body,
                1_800_000_100
            ),
            Err(WebhookError::BadSignature)
        );
        assert_eq!(
            verify(
                &key,
                Some("msg_1"),
                Some("1800000000"),
                Some(&header),
                b"{}",
                1_800_000_100
            ),
            Err(WebhookError::BadSignature)
        );
        assert_eq!(
            verify(
                &key,
                Some("msg_1"),
                Some("1800000000"),
                Some(&header),
                body,
                1_800_000_301
            ),
            Err(WebhookError::Stale)
        );
        assert_eq!(
            verify(&key, None, Some("1"), Some(&header), body, 1),
            Err(WebhookError::MissingHeaders)
        );
        assert_eq!(
            verify(
                &key,
                Some("msg_1"),
                Some("1800000000"),
                Some(&sig),
                body,
                1_800_000_000
            ),
            Err(WebhookError::BadSignature),
            "an entry without the v1, prefix is not accepted"
        );
    }

    #[test]
    fn known_svix_vector() {
        // Example from Svix's "verifying manually" docs.
        let key = parse_secret("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap();
        let body = br#"{"test": 2432232314}"#;
        assert_eq!(
            sign(&key, "msg_p5jXN8AQM9LWM0D4loKWxJek", 1614265330, body),
            "g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE="
        );
    }

    #[test]
    fn payloads_are_classified() {
        let v = serde_json::json!({"type":"email.bounced","data":{"to":["A@B.com","junk"]}});
        assert_eq!(
            classify(&v),
            Action::Bounce {
                emails: vec!["a@b.com".into()]
            }
        );
        let v = serde_json::json!({"type":"email.complained","data":{"to":"x@y.io"}});
        assert_eq!(
            classify(&v),
            Action::Complaint {
                emails: vec!["x@y.io".into()]
            }
        );
        let v = serde_json::json!({"type":"contact.updated","data":{"email":"x@y.io","unsubscribed":true}});
        assert_eq!(
            classify(&v),
            Action::Unsubscribed {
                email: "x@y.io".into()
            }
        );
        let v = serde_json::json!({"type":"contact.updated","data":{"email":"x@y.io","unsubscribed":false}});
        assert_eq!(classify(&v), Action::Ignore);
        assert_eq!(
            classify(&serde_json::json!({"type":"email.delivered"})),
            Action::Ignore
        );
    }
}
