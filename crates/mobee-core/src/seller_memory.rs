//! Distilled memory (Layer 1): `MOBEE_HOME/memory/` — a `MEMORY.md` index plus topic
//! files (plain markdown, `[[wikilinks]]`), read at job start and written by the seller's own
//! agent in a post-job retro.
//!
//! Layer 1 is a **cache**; Layer 0 (`episodes.jsonl`) is the source of truth. Nothing here is
//! ever an input to the pay gate, the journal, or the receipt bind.
//!
//! Provenance (file-level ownership): every topic file carries YAML frontmatter
//! `author: agent | operator`. The retro regenerates only `author: agent` files; `author:
//! operator` files (including `operator-notes.md`) are read as input and passed through untouched
//! (merge-not-clobber, enforced at runtime by [`snapshot_operator_files`]/[`restore_snapshot`]).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Sub-directory of `MOBEE_HOME` holding the distilled memory.
pub const MEMORY_DIR_NAME: &str = "memory";
/// The index file loaded at job start.
pub const MEMORY_INDEX_FILE: &str = "MEMORY.md";
/// The always-operator-owned topic file, seeded on first creation.
pub const OPERATOR_NOTES_FILE: &str = "operator-notes.md";

/// Frontmatter `author:` value for agent-written (retro-regenerated) files.
pub const AUTHOR_AGENT: &str = "agent";
/// Frontmatter `author:` value for operator-written (never-regenerated) files.
pub const AUTHOR_OPERATOR: &str = "operator";

/// Placeholder tokens the in-repo templates (and operator overrides) may reference. Rendering is
/// a literal token replace — no format!(), so `{`/`}` in prose is safe.
pub const TOKEN_MEMORY_DIR: &str = "{memory_dir}";
pub const TOKEN_MEMORY_INDEX: &str = "{memory_index}";
pub const TOKEN_EPISODE_JSON: &str = "{episode_json}";
pub const TOKEN_TRANSCRIPT_REF: &str = "{transcript_ref}";

/// In-repo default framing for how `MEMORY.md` is inlined into the job prompt (read-on-start seam).
pub const DEFAULT_READ_ON_START_TEMPLATE: &str = "\
--- SELLER MEMORY (read-on-start) ---
You have persistent memory from past jobs. Your durable memory lives at:
  {memory_dir}
Below is its index (MEMORY.md). Read the topic files it links when they are relevant to this job.

{memory_index}
--- END SELLER MEMORY ---";

/// In-repo default retro/distiller prompt (retro seam). This is where memory *policy* lives; an
/// operator points `retro_prompt_path` at their own template to change what the agent distills.
pub const DEFAULT_RETRO_TEMPLATE: &str = "\
You just finished a paid job as an autonomous seller. Update your DURABLE MEMORY with what this
job taught you, so future jobs go better.

Your memory directory is your current working directory:
  {memory_dir}
- Keep MEMORY.md a current index: one line per topic file, linked with [[wikilinks]].
- Write or update topic files (plain markdown) for durable lessons: task shapes that went well or
  badly, buyers worth noting, what a class of job actually took to deliver.
- Every file YOU write MUST start with YAML frontmatter `author: agent`.
- NEVER edit or overwrite any file whose frontmatter says `author: operator` (including
  operator-notes.md) — read those as guidance, but leave them exactly as they are.

Here is the episode you are distilling (Layer-0 capture, JSON):
{episode_json}

The full raw transcript of the job is on disk at (read it if you need detail):
{transcript_ref}

Make your edits now, then stop.";

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The memory dir path for a given `MOBEE_HOME` root.
pub fn memory_dir(home_root: &Path) -> PathBuf {
    home_root.join(MEMORY_DIR_NAME)
}

/// Seed content for the always-operator-owned notes file.
fn operator_notes_seed() -> String {
    format!(
        "---\nauthor: {AUTHOR_OPERATOR}\nupdated_at: {}\n---\n\n\
         <!-- Operator-authored guidance for the seller agent: house rules, buyers to avoid, task\n\
         \x20    shapes to prefer. This file is author: operator and is NEVER overwritten by the\n\
         \x20    agent's retro (merge-not-clobber). Edit freely. -->\n",
        now_unix()
    )
}

/// Seed content for the memory index (non-empty by construction so read-on-start always has text).
fn memory_index_seed() -> String {
    format!(
        "# Seller memory index\n\n\
         One line per topic file. Loaded into the agent's context at the start of each job;\n\
         linked topic files carry the detail. Cross-link with [[wikilinks]].\n\n\
         - [operator-notes]({OPERATOR_NOTES_FILE}) — operator-authored guidance (author: {AUTHOR_OPERATOR})\n"
    )
}

/// Create `memory/` on demand, seeding `operator-notes.md` (author: operator) and a non-empty
/// `MEMORY.md` index. Idempotent: existing files are left exactly as they are (never clobbered).
pub fn ensure_memory_dir(memory_dir: &Path) -> io::Result<()> {
    fs::create_dir_all(memory_dir)?;
    let notes = memory_dir.join(OPERATOR_NOTES_FILE);
    if !notes.exists() {
        fs::write(&notes, operator_notes_seed())?;
    }
    let index = memory_dir.join(MEMORY_INDEX_FILE);
    if !index.exists() {
        fs::write(&index, memory_index_seed())?;
    }
    Ok(())
}

/// Load a template from the operator's `path` if set and readable, else the in-repo `default`.
/// Best-effort: an unreadable override falls back to the default (a memory seam must never break
/// a job).
fn load_template(path: Option<&Path>, default: &str) -> String {
    path.and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_else(|| default.to_owned())
}

/// Literal token replace (no format!): each `(token, value)` pair is substituted in order.
fn render(template: &str, substitutions: &[(&str, &str)]) -> String {
    let mut out = template.to_owned();
    for (token, value) in substitutions {
        out = out.replace(token, value);
    }
    out
}

/// Render the read-on-start memory section to inline into the job prompt, or `None` when there is
/// no non-empty index to inline. `template_path` overrides the in-repo default (read-on-start seam).
pub fn read_on_start_section(
    memory_dir: &Path,
    template_path: Option<&Path>,
) -> io::Result<Option<String>> {
    let index_path = memory_dir.join(MEMORY_INDEX_FILE);
    let index = match fs::read_to_string(&index_path) {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let template = load_template(template_path, DEFAULT_READ_ON_START_TEMPLATE);
    let rendered = render(
        &template,
        &[
            (TOKEN_MEMORY_DIR, memory_dir.display().to_string().as_str()),
            (TOKEN_MEMORY_INDEX, index.trim_end()),
        ],
    );
    Ok(Some(rendered))
}

/// Compose the retro/distiller prompt (retro seam). `template_path` overrides the in-repo default.
pub fn retro_prompt(
    memory_dir: &Path,
    episode_json: &str,
    transcript_ref: &str,
    template_path: Option<&Path>,
) -> String {
    let template = load_template(template_path, DEFAULT_RETRO_TEMPLATE);
    render(
        &template,
        &[
            (TOKEN_MEMORY_DIR, memory_dir.display().to_string().as_str()),
            (TOKEN_EPISODE_JSON, episode_json),
            (TOKEN_TRANSCRIPT_REF, transcript_ref),
        ],
    )
}

/// Read the `author:` value from a file's leading YAML frontmatter, if present. A file with no
/// `---`-delimited frontmatter, or no `author:` key, returns `None`.
pub fn frontmatter_author(contents: &str) -> Option<String> {
    let mut lines = contents.lines();
    if lines.next().map(str::trim) != Some("---") {
        return None;
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("author:") {
            return Some(value.trim().to_owned());
        }
    }
    None
}

/// Whether `path` is operator-owned and must be preserved across a retro. `operator-notes.md` is
/// operator-owned by convention regardless of frontmatter; any other file is operator-owned iff
/// its frontmatter `author:` is `operator`. A file that cannot be read is treated as NOT
/// operator-owned (the retro may regenerate it) — conservative for preservation is the opposite,
/// but an unreadable file has nothing to preserve.
pub fn is_operator_owned(path: &Path) -> bool {
    if path.file_name().and_then(|n| n.to_str()) == Some(OPERATOR_NOTES_FILE) {
        return true;
    }
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| frontmatter_author(&contents))
        .map(|author| author == AUTHOR_OPERATOR)
        .unwrap_or(false)
}

/// A byte-snapshot of every operator-owned file in the memory dir, taken BEFORE a retro so it can
/// be restored after (merge-not-clobber enforced at runtime, not by prompt prose alone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorSnapshot {
    files: Vec<(PathBuf, Vec<u8>)>,
}

impl OperatorSnapshot {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
    pub fn len(&self) -> usize {
        self.files.len()
    }
}

/// Snapshot the bytes of every operator-owned file directly in `memory_dir` (non-recursive; the
/// memory dir is flat). Used to guarantee operator files are byte-unchanged across a retro.
pub fn snapshot_operator_files(memory_dir: &Path) -> io::Result<OperatorSnapshot> {
    let mut files = Vec::new();
    let entries = match fs::read_dir(memory_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(OperatorSnapshot { files });
        }
        Err(error) => return Err(error),
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_file() && is_operator_owned(&path) {
            let bytes = fs::read(&path)?;
            files.push((path, bytes));
        }
    }
    Ok(OperatorSnapshot { files })
}

/// Restore every snapshotted operator file to its pre-retro bytes, overwriting any change (or
/// deletion) the retro made. This is the runtime enforcement of merge-not-clobber: whatever the
/// agent did to an `author: operator` file, it is byte-reverted here. Non-operator (author:
/// agent) files are left as the retro wrote them.
pub fn restore_snapshot(snapshot: &OperatorSnapshot) -> io::Result<()> {
    for (path, bytes) in &snapshot.files {
        let current = fs::read(path).ok();
        if current.as_deref() != Some(bytes.as_slice()) {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, bytes)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "mobee-mem-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn ensure_memory_dir_seeds_operator_notes_and_nonempty_index() {
        let root = temp_dir("ensure");
        let dir = memory_dir(&root);
        ensure_memory_dir(&dir).expect("ensure");

        let notes = dir.join(OPERATOR_NOTES_FILE);
        assert!(notes.is_file(), "operator-notes.md seeded");
        let notes_text = fs::read_to_string(&notes).expect("read notes");
        assert_eq!(
            frontmatter_author(&notes_text).as_deref(),
            Some(AUTHOR_OPERATOR),
            "operator-notes.md stamped author: operator"
        );

        let index = fs::read_to_string(dir.join(MEMORY_INDEX_FILE)).expect("read index");
        assert!(!index.trim().is_empty(), "index is non-empty");
        assert!(index.contains(OPERATOR_NOTES_FILE), "index links operator-notes");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_memory_dir_is_idempotent_and_never_clobbers() {
        let root = temp_dir("idem");
        let dir = memory_dir(&root);
        ensure_memory_dir(&dir).expect("first");
        // Operator edits their notes.
        let notes = dir.join(OPERATOR_NOTES_FILE);
        let edited = "---\nauthor: operator\n---\n\nAvoid buyer deadbeef.\n";
        fs::write(&notes, edited).expect("edit notes");
        // A second ensure must NOT overwrite the operator's edit.
        ensure_memory_dir(&dir).expect("second");
        assert_eq!(fs::read_to_string(&notes).expect("read"), edited);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_on_start_uses_default_template_with_index_and_absolute_dir() {
        let root = temp_dir("ros-default");
        let dir = memory_dir(&root);
        ensure_memory_dir(&dir).expect("ensure");
        let section = read_on_start_section(&dir, None)
            .expect("read")
            .expect("some section");
        assert!(section.contains("SELLER MEMORY"), "default framing present");
        assert!(
            section.contains(&dir.display().to_string()),
            "absolute memory dir named"
        );
        assert!(section.contains("Seller memory index"), "index text inlined");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_on_start_none_when_no_index() {
        let root = temp_dir("ros-none");
        let dir = memory_dir(&root);
        fs::create_dir_all(&dir).expect("mkdir");
        // No MEMORY.md ⇒ nothing to inline.
        assert!(read_on_start_section(&dir, None).expect("read").is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_on_start_template_seam_overrides_default() {
        let root = temp_dir("ros-seam");
        let dir = memory_dir(&root);
        ensure_memory_dir(&dir).expect("ensure");
        let template = root.join("custom-read.tmpl");
        fs::write(&template, "OPERATOR-FRAMING >> {memory_index} << at {memory_dir}")
            .expect("write template");
        let section = read_on_start_section(&dir, Some(&template))
            .expect("read")
            .expect("some");
        assert!(section.starts_with("OPERATOR-FRAMING >>"), "uses operator template");
        assert!(!section.contains("SELLER MEMORY"), "default framing NOT used");
        assert!(section.contains("Seller memory index"), "index still substituted");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn retro_prompt_default_and_seam() {
        let root = temp_dir("retro-seam");
        fs::create_dir_all(&root).expect("mkdir root");
        let dir = memory_dir(&root);
        let default = retro_prompt(&dir, "{\"job_id\":\"j1\"}", "seller-jobs/j1/seller-run.jsonl", None);
        assert!(default.contains("DURABLE MEMORY"), "default retro framing");
        assert!(default.contains("{\"job_id\":\"j1\"}"), "episode json substituted");
        assert!(default.contains("seller-jobs/j1/seller-run.jsonl"), "transcript ref substituted");
        assert!(default.contains(&dir.display().to_string()), "memory dir substituted");

        let template = root.join("custom-retro.tmpl");
        fs::write(&template, "MY DISTILLER for {episode_json} @ {memory_dir}").expect("write");
        let seam = retro_prompt(&dir, "EJSON", "TREF", Some(&template));
        assert!(seam.starts_with("MY DISTILLER for EJSON"), "uses operator retro template");
        assert!(!seam.contains("DURABLE MEMORY"), "default NOT used");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn frontmatter_author_parses_operator_agent_and_none() {
        assert_eq!(
            frontmatter_author("---\nauthor: operator\nupdated_at: 5\n---\nbody").as_deref(),
            Some("operator")
        );
        assert_eq!(
            frontmatter_author("---\nauthor:agent\n---\n").as_deref(),
            Some("agent")
        );
        assert_eq!(frontmatter_author("no frontmatter here").as_deref(), None);
        // `author:` appearing only in the body (after the closing ---) is not frontmatter.
        assert_eq!(
            frontmatter_author("---\ntitle: x\n---\nauthor: sneaky").as_deref(),
            None
        );
    }

    #[test]
    fn is_operator_owned_by_frontmatter_and_by_notes_convention() {
        let root = temp_dir("owned");
        let dir = memory_dir(&root);
        ensure_memory_dir(&dir).expect("ensure");
        assert!(is_operator_owned(&dir.join(OPERATOR_NOTES_FILE)), "notes always operator");

        let agent_file = dir.join("task-shapes.md");
        fs::write(&agent_file, "---\nauthor: agent\n---\nlessons").expect("write");
        assert!(!is_operator_owned(&agent_file), "author: agent is not operator-owned");

        let operator_topic = dir.join("house-rules.md");
        fs::write(&operator_topic, "---\nauthor: operator\n---\nrules").expect("write");
        assert!(is_operator_owned(&operator_topic), "author: operator preserved");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn snapshot_restore_reverts_operator_files_and_leaves_agent_files() {
        let root = temp_dir("merge");
        let dir = memory_dir(&root);
        ensure_memory_dir(&dir).expect("ensure");
        let operator_topic = dir.join("house-rules.md");
        fs::write(&operator_topic, "---\nauthor: operator\n---\nORIGINAL RULES").expect("write");
        let agent_file = dir.join("lessons.md");
        fs::write(&agent_file, "---\nauthor: agent\n---\nold lessons").expect("write");

        let snapshot = snapshot_operator_files(&dir).expect("snapshot");
        assert!(snapshot.len() >= 2, "operator-notes + house-rules snapshotted");

        // Simulate a misbehaving retro: it clobbers an operator file and rewrites an agent file.
        fs::write(&operator_topic, "CLOBBERED BY AGENT").expect("clobber");
        fs::write(&agent_file, "---\nauthor: agent\n---\nNEW lessons").expect("rewrite agent");
        let notes = dir.join(OPERATOR_NOTES_FILE);
        let notes_original = fs::read(&notes).expect("read notes");
        fs::write(&notes, "CLOBBERED NOTES").expect("clobber notes");

        restore_snapshot(&snapshot).expect("restore");

        assert_eq!(
            fs::read_to_string(&operator_topic).expect("read"),
            "---\nauthor: operator\n---\nORIGINAL RULES",
            "operator topic byte-reverted"
        );
        assert_eq!(fs::read(&notes).expect("read"), notes_original, "operator-notes byte-reverted");
        assert_eq!(
            fs::read_to_string(&agent_file).expect("read"),
            "---\nauthor: agent\n---\nNEW lessons",
            "agent file left as the retro wrote it"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
