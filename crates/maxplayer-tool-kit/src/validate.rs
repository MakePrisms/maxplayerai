//! Parameter validation and job-directory confinement.
//!
//! Two jobs of the same seller share one offering, one holder and one login. What they do **not**
//! share is a directory. Every path a job names is resolved inside that job's own root, and a
//! path that leaves it is refused — including by way of a symlink, which is why resolution is
//! done with `canonicalize` and not by string prefix.

use crate::config::{ParamKind, SellerToolConfig};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// Why a call was refused. One variant per reason so tests can assert the *reason*, not just
/// that something failed — a validator that rejects everything would otherwise look correct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reject {
    UnknownOperation { op: String },
    UnknownParam { op: String, param: String },
    MissingParam { op: String, param: String },
    NotAString { param: String },
    TextTooLong { param: String, len: usize, max: usize },
    ControlCharacter { param: String },
    LooksLikeFlag { param: String },
    ShellMetacharacter { param: String, ch: char },
    NotAChoice { param: String },
    EmptyPath { param: String },
    AbsolutePath { param: String },
    NonNormalComponent { param: String },
    EscapesJobDir { param: String },
    SymlinkedPath { param: String },
    MissingInput { param: String },
    NotARegularFile { param: String },
    OutputParentMissing { param: String },
    BadJobRoot,
}

impl fmt::Display for Reject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reject::UnknownOperation { op } => {
                write!(f, "operation {op:?} is not in this seller's offering")
            }
            Reject::UnknownParam { op, param } => write!(f, "{op}: parameter {param:?} is not declared"),
            Reject::MissingParam { op, param } => write!(f, "{op}: parameter {param:?} is required"),
            Reject::NotAString { param } => write!(f, "{param}: must be a string"),
            Reject::TextTooLong { param, len, max } => write!(f, "{param}: {len} bytes exceeds max {max}"),
            Reject::ControlCharacter { param } => write!(f, "{param}: control characters are not accepted"),
            Reject::LooksLikeFlag { param } => {
                write!(f, "{param}: a value starting with '-' would read as a flag")
            }
            Reject::ShellMetacharacter { param, ch } => {
                write!(f, "{param}: character {ch:?} is not accepted")
            }
            Reject::NotAChoice { param } => write!(f, "{param}: not one of the declared choices"),
            Reject::EmptyPath { param } => write!(f, "{param}: empty path"),
            Reject::AbsolutePath { param } => write!(f, "{param}: absolute paths are not accepted"),
            Reject::NonNormalComponent { param } => {
                write!(f, "{param}: '.' and '..' components are not accepted")
            }
            Reject::EscapesJobDir { param } => write!(f, "{param}: resolves outside this job's directory"),
            Reject::SymlinkedPath { param } => write!(f, "{param}: symlinks are not accepted"),
            Reject::MissingInput { param } => write!(f, "{param}: no such file in this job's directory"),
            Reject::NotARegularFile { param } => write!(f, "{param}: not a regular file"),
            Reject::OutputParentMissing { param } => write!(f, "{param}: output directory does not exist"),
            Reject::BadJobRoot => write!(f, "job directory is missing or unreadable"),
        }
    }
}

/// A call that passed validation: a fixed subcommand and one argument per declared parameter, in
/// spec order. Nothing here is interpreted again downstream — no shell, no string splitting, no
/// template expansion.
///
/// Note what a file argument carries: a **job-relative path**, not a canonicalized absolute
/// string. Validation deliberately does not resolve a file path, because a path resolved here and
/// opened later is the check/use race this whole module used to have (advisor F2). The holder
/// resolves and opens each file itself, once, following no symlink — see [`crate::safeio`].
#[derive(Clone, Debug)]
pub struct ValidatedCall {
    pub operation: String,
    pub subcommand: String,
    pub args: Vec<CallArg>,
    pub max_output_bytes: usize,
}

/// One validated argument. The holder turns each into exactly one flag plus one operand; a file
/// operand is resolved race-safely at that point, never here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallArg {
    /// Literal data (text or an enum choice). Safe to place in argv as-is.
    Literal { flag: String, value: String },
    /// A file the job supplies. Carries the job-relative path; the holder opens it no-follow.
    Input { flag: String, rel: PathBuf },
    /// A file the holder will create for the job. Carries the job-relative path; the holder
    /// creates it no-follow.
    Output { flag: String, rel: PathBuf },
}

/// Characters refused in literal text. The holder never invokes a shell, so this is defence in
/// depth rather than the primary control — kept because "no shell today" is a property of the
/// current code, not of every future edit.
const REFUSED: &[char] = &[
    ';', '|', '&', '$', '`', '<', '>', '(', ')', '{', '}', '[', ']', '*', '?', '!', '\\', '"', '\'',
    '\n', '\r', '\0',
];

/// Validate a call against the seller's operation list. This checks **grammar and confinement by
/// name only**: it never touches the filesystem, so it cannot canonicalize a path that is then
/// re-opened later. File arguments come back as job-relative paths for the holder to open
/// race-safely; see [`ValidatedCall`] and [`crate::safeio`].
///
/// `job_root` is accepted for signature stability and future use but is deliberately not resolved
/// here — resolving it, and the operands under it, is the holder's job at consumption time.
pub fn validate_call(
    cfg: &SellerToolConfig,
    operation: &str,
    params: &BTreeMap<String, serde_json::Value>,
    _job_root: &Path,
) -> Result<ValidatedCall, Reject> {
    let spec = cfg
        .operation(operation)
        .ok_or_else(|| Reject::UnknownOperation { op: operation.to_string() })?;

    // Every supplied parameter must be declared. Unknown parameters are refused rather than
    // ignored: silently dropping one is how a caller ends up believing a limit was applied.
    for key in params.keys() {
        if !spec.params.iter().any(|p| &p.name == key) {
            return Err(Reject::UnknownParam { op: operation.to_string(), param: key.clone() });
        }
    }

    let mut args = Vec::new();

    // Iterate the *spec*, not the input: argument order is fixed by configuration.
    for p in &spec.params {
        let raw = params
            .get(&p.name)
            .ok_or_else(|| Reject::MissingParam { op: operation.to_string(), param: p.name.clone() })?;
        let value = raw.as_str().ok_or_else(|| Reject::NotAString { param: p.name.clone() })?;

        let arg = match &p.kind {
            ParamKind::Text { max_len } => {
                check_text(&p.name, value, *max_len)?;
                CallArg::Literal { flag: p.flag.clone(), value: value.to_string() }
            }
            ParamKind::Choice { choices } => {
                if !choices.iter().any(|c| c == value) {
                    return Err(Reject::NotAChoice { param: p.name.clone() });
                }
                CallArg::Literal { flag: p.flag.clone(), value: value.to_string() }
            }
            ParamKind::JobInputFile => {
                let rel = check_rel_path(value, &p.name)?;
                CallArg::Input { flag: p.flag.clone(), rel }
            }
            ParamKind::JobOutputFile => {
                let rel = check_rel_path(value, &p.name)?;
                CallArg::Output { flag: p.flag.clone(), rel }
            }
        };
        args.push(arg);
    }

    Ok(ValidatedCall {
        operation: spec.name.clone(),
        subcommand: spec.subcommand.clone(),
        args,
        max_output_bytes: spec.max_output_bytes,
    })
}

fn check_text(param: &str, value: &str, max_len: usize) -> Result<(), Reject> {
    if value.len() > max_len {
        return Err(Reject::TextTooLong { param: param.to_string(), len: value.len(), max: max_len });
    }
    if value.chars().any(|c| c.is_control()) {
        return Err(Reject::ControlCharacter { param: param.to_string() });
    }
    if value.starts_with('-') {
        return Err(Reject::LooksLikeFlag { param: param.to_string() });
    }
    if let Some(ch) = value.chars().find(|c| REFUSED.contains(c)) {
        return Err(Reject::ShellMetacharacter { param: param.to_string(), ch });
    }
    Ok(())
}

/// Check that `raw` is a well-formed **job-relative** path, without touching the filesystem.
///
/// This is confinement by name: it refuses an empty path, control characters, an absolute path,
/// and any `.`/`..` component before any `open` happens. What it deliberately does **not** do is
/// resolve the path, stat it, or test it for a symlink — doing that here and opening it later is
/// the check/use race (advisor F2). Existence, regular-file-ness and symlink refusal are decided
/// at open time by [`crate::safeio`], on the descriptor that is actually used.
pub fn check_rel_path(raw: &str, param: &str) -> Result<PathBuf, Reject> {
    if raw.is_empty() {
        return Err(Reject::EmptyPath { param: param.to_string() });
    }
    if raw.chars().any(|c| c.is_control()) {
        return Err(Reject::ControlCharacter { param: param.to_string() });
    }

    let rel = Path::new(raw);
    if rel.is_absolute() {
        return Err(Reject::AbsolutePath { param: param.to_string() });
    }
    // Only ordinary names. This refuses `../other-job/secret` by grammar, independent of what the
    // filesystem currently looks like, and is intentionally stricter than "no `..` after
    // normalization".
    for c in rel.components() {
        match c {
            Component::Normal(_) => {}
            _ => return Err(Reject::NonNormalComponent { param: param.to_string() }),
        }
    }
    Ok(rel.to_path_buf())
}
