use std::fmt;

use serde::{Deserialize, Serialize};

use crate::driver::Artifact;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedFormat {
    pub mime: String,
    pub shape: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormatCheckError {
    InvalidToken(String),
    MimeMismatch {
        expected: String,
        actual: Option<String>,
    },
    MissingBytes,
    InvalidUtf8,
    InvalidJson(String),
    ShapeMismatch {
        shape: String,
        detail: String,
    },
}

impl fmt::Display for FormatCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidToken(token) => write!(f, "invalid expected result format token: {token}"),
            Self::MimeMismatch { expected, actual } => {
                write!(f, "MIME mismatch: expected {expected}, got {actual:?}")
            }
            Self::MissingBytes => write!(f, "artifact is missing inline bytes"),
            Self::InvalidUtf8 => write!(f, "artifact bytes are not valid UTF-8"),
            Self::InvalidJson(error) => write!(f, "artifact is not valid JSON: {error}"),
            Self::ShapeMismatch { shape, detail } => {
                write!(f, "shape {shape:?} mismatch: {detail}")
            }
        }
    }
}

impl std::error::Error for FormatCheckError {}

impl ExpectedFormat {
    pub fn parse(token: &str) -> Result<Self, FormatCheckError> {
        let token = token.trim();
        let (mime, shape) = match token.split_once(";shape=") {
            Some((mime, shape)) => (mime, Some(shape)),
            None => (token, None),
        };
        if !valid_mime(mime) {
            return Err(FormatCheckError::InvalidToken(token.into()));
        }
        let shape = shape
            .map(|shape| {
                if valid_shape(shape) {
                    Ok(shape.to_owned())
                } else {
                    Err(FormatCheckError::InvalidToken(token.into()))
                }
            })
            .transpose()?;
        Ok(Self {
            mime: mime.to_ascii_lowercase(),
            shape,
        })
    }

    pub fn check_artifact(&self, artifact: &Artifact) -> Result<(), FormatCheckError> {
        if artifact.mime.as_deref().map(str::to_ascii_lowercase) != Some(self.mime.clone()) {
            return Err(FormatCheckError::MimeMismatch {
                expected: self.mime.clone(),
                actual: artifact.mime.clone(),
            });
        }
        let bytes = artifact
            .bytes
            .as_deref()
            .ok_or(FormatCheckError::MissingBytes)?;
        self.check_bytes(bytes)
    }

    pub fn check_bytes(&self, bytes: &[u8]) -> Result<(), FormatCheckError> {
        match (self.mime.as_str(), self.shape.as_deref()) {
            ("application/json", Some(shape)) => check_json_shape(bytes, shape),
            ("application/json", None) => {
                serde_json::from_slice::<serde_json::Value>(bytes)
                    .map_err(|error| FormatCheckError::InvalidJson(error.to_string()))?;
                Ok(())
            }
            (mime, Some("nonempty")) if mime.starts_with("text/") => {
                let text = std::str::from_utf8(bytes).map_err(|_| FormatCheckError::InvalidUtf8)?;
                if text.trim().is_empty() {
                    Err(FormatCheckError::ShapeMismatch {
                        shape: "nonempty".into(),
                        detail: "text is empty".into(),
                    })
                } else {
                    Ok(())
                }
            }
            (mime, _) if mime.starts_with("text/") => {
                std::str::from_utf8(bytes).map_err(|_| FormatCheckError::InvalidUtf8)?;
                Ok(())
            }
            (_, Some(shape)) => Err(FormatCheckError::ShapeMismatch {
                shape: shape.into(),
                detail: "no checker for this MIME+shape pair".into(),
            }),
            _ => Ok(()),
        }
    }
}

fn check_json_shape(bytes: &[u8], shape: &str) -> Result<(), FormatCheckError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| FormatCheckError::InvalidJson(error.to_string()))?;
    let matches = match shape {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => {
            return Err(FormatCheckError::ShapeMismatch {
                shape: shape.into(),
                detail: "unsupported JSON shape token".into(),
            });
        }
    };
    if matches {
        Ok(())
    } else {
        Err(FormatCheckError::ShapeMismatch {
            shape: shape.into(),
            detail: format!("actual JSON value is {}", json_kind(&value)),
        })
    }
}

fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn valid_mime(value: &str) -> bool {
    let Some((top, sub)) = value.split_once('/') else {
        return false;
    };
    !top.is_empty()
        && !sub.is_empty()
        && top.chars().all(is_token_char)
        && sub.chars().all(is_token_char)
}

fn valid_shape(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn is_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '!' | '#' | '$' | '&' | '-' | '^' | '_' | '.' | '+')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(mime: Option<&str>, bytes: Option<&[u8]>) -> Artifact {
        Artifact {
            uri_or_path: "inline".into(),
            mime: mime.map(str::to_owned),
            bytes: bytes.map(<[u8]>::to_vec),
        }
    }

    #[test]
    fn parse_accepts_mime_plus_shape_token() {
        assert_eq!(
            ExpectedFormat::parse("application/json;shape=object"),
            Ok(ExpectedFormat {
                mime: "application/json".into(),
                shape: Some("object".into()),
            })
        );
        assert!(ExpectedFormat::parse("not-a-mime").is_err());
    }

    #[test]
    fn artifact_check_rejects_mime_mismatch() {
        let expected = ExpectedFormat::parse("application/json").unwrap();

        assert_eq!(
            expected.check_artifact(&artifact(Some("text/plain"), Some(b"{}"))),
            Err(FormatCheckError::MimeMismatch {
                expected: "application/json".into(),
                actual: Some("text/plain".into()),
            })
        );
    }

    #[test]
    fn artifact_check_requires_inline_bytes() {
        let expected = ExpectedFormat::parse("text/plain").unwrap();

        assert_eq!(
            expected.check_artifact(&artifact(Some("text/plain"), None)),
            Err(FormatCheckError::MissingBytes)
        );
    }

    #[test]
    fn text_check_requires_utf8() {
        let expected = ExpectedFormat::parse("text/plain").unwrap();

        assert_eq!(
            expected.check_bytes(&[0xff, 0xfe]),
            Err(FormatCheckError::InvalidUtf8)
        );
    }

    #[test]
    fn json_check_rejects_invalid_json() {
        let expected = ExpectedFormat::parse("application/json").unwrap();

        assert!(matches!(
            expected.check_bytes(b"{not-json"),
            Err(FormatCheckError::InvalidJson(_))
        ));
    }

    #[test]
    fn json_shape_check_rejects_wrong_structure() {
        let expected = ExpectedFormat::parse("application/json;shape=object").unwrap();

        assert_eq!(expected.check_bytes(br#"{"ok":true}"#), Ok(()));
        assert!(matches!(
            expected.check_bytes(br#"["not-object"]"#),
            Err(FormatCheckError::ShapeMismatch { .. })
        ));
    }

    #[test]
    fn artifact_check_accepts_inline_utf8_for_text() {
        let expected = ExpectedFormat::parse("text/plain;shape=nonempty").unwrap();

        assert_eq!(
            expected.check_artifact(&artifact(Some("text/plain"), Some(b"mobee"))),
            Ok(())
        );
        assert!(matches!(
            expected.check_bytes(b"   "),
            Err(FormatCheckError::ShapeMismatch { .. })
        ));
    }
}
