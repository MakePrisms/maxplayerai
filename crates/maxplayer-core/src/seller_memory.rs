//! Distilled memory (Layer 1): `MAXPLAYER_HOME/memory/` — a `MEMORY.md` index plus topic
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

/// Sub-directory of `MAXPLAYER_HOME` holding the distilled memory.
pub const MEMORY_DIR_NAME: &str = "memory";
/// Upper bound, in bytes, on the `MEMORY.md` index injected into the job prompt (issue #81).
///
/// Raised from 16 KiB to 64 KiB (#828). The old bound was sized against an AGENT-written index, and
/// #81 measured that case at ~1–5 KB steady with a ~10 KB worst case. The same file now also has to
/// hold an OPERATOR's deliberate specialization — brand guidelines, house style — which is written
/// once rather than accumulated, so it does not grow on its own. A containerized seller has nowhere
/// else to put it: only this file's CONTENT is inlined into the prompt, while the topic files the
/// index links sit outside the job's mount namespace and cannot be opened. 64 KiB still clears #81's
/// worst case by 6.4x, so the runaway guard keeps biting an order of magnitude before genuine
/// runaway growth.
///
/// The cost is prompt tokens on every job, paid by the seller who chose to write the file, so it is
/// self-limiting. The section is appended LAST, so a larger block never pushes the buyer's task down.
///
/// An index over this bound is TRUNCATED at the injection site — [`read_on_start_section`] inlines
/// the head up to the last complete line at or before the budget that survives trimming (or, when
/// no such line exists, as much text as fits on a char boundary), plus a marker line saying what
/// was dropped, see [`fit_index_to_budget`] for the exact rule, and
/// stays `Ok(Some(..))`, so the seat keeps its specialization head rather than silently running
/// every job as a generalist. The daemon warns on the console (per job, and once at boot), and
/// `maxplayer doctor` reports it; the seam never blocks a job.
pub const MAX_MEMORY_INDEX_BYTES: usize = 64 * 1024;
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

/// The memory dir path for a given `MAXPLAYER_HOME` root.
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

/// What the injection site cut from an over-budget index. Carried out to the daemon so the console
/// warning can name the real numbers, and rendered INTO the injected text as a marker line so the
/// agent knows it is reading a fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexTruncation {
    /// Bytes of `MEMORY.md` that reached the prompt (the marker line is on top of this).
    pub shown_bytes: usize,
    /// Bytes `MEMORY.md` actually holds on disk.
    pub total_bytes: usize,
}

/// The rendered read-on-start section plus what, if anything, was cut to fit it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOnStart {
    /// The section text to inline into the job prompt.
    pub section: String,
    /// `Some` when the index was over [`MAX_MEMORY_INDEX_BYTES`] and its tail was dropped.
    pub truncation: Option<IndexTruncation>,
}

/// The marker line appended inside a truncated index so the agent reads it as a fragment, never as
/// the whole file. Always ONE line: the line-boundary cut above it depends on that.
pub fn truncation_marker(shown_bytes: usize, total_bytes: usize) -> String {
    format!(
        "[maxplayer: MEMORY.md truncated to the {MAX_MEMORY_INDEX_BYTES}-byte injection budget — \
         {shown_bytes} of {total_bytes} bytes shown, tail dropped]"
    )
}

/// Fit an index into [`MAX_MEMORY_INDEX_BYTES`]. An index at or under the budget comes back
/// trailing-trimmed and untouched. An index over it is cut at the LAST COMPLETE LINE at or before
/// the budget WHOSE HEAD SURVIVES TRIMMING — the head is `trim_end`ed after the cut, so the line is
/// chosen by what is left standing, not by where a newline happens to sit — and
/// [`truncation_marker`] is appended as the final line. When no such line exists — a single line
/// longer than the budget, or a file whose in-budget newlines all sit inside an all-whitespace
/// opening (one LF, two LFs, a CRLF blank line, an indented blank line: the head would trim to
/// empty and the seat would inject zero specialization) — the long-line fallback applies: the cut
/// lands on the nearest lower char boundary, never mid-UTF-8-character, and the file's own opening
/// bytes are kept as-is rather than skipped. An index whose whole in-budget window is whitespace
/// has no non-empty head to keep and gets none: no byte-bounded rule can reach text that starts
/// beyond the budget. The marker's bytes are reserved BEFORE the cut, so the returned text is
/// always `<= MAX_MEMORY_INDEX_BYTES` including the marker; the bound covers the index text plus
/// the marker, not the surrounding prompt template.
pub fn fit_index_to_budget(index: &str) -> (String, Option<IndexTruncation>) {
    let total_bytes = index.len();
    if total_bytes <= MAX_MEMORY_INDEX_BYTES {
        return (index.trim_end().to_owned(), None);
    }
    // Reserve the marker at its LONGEST: `shown <= total`, so a marker rendered with `total` in both
    // slots has at least as many digits as the real one will. Plus one byte for the newline that
    // joins the surviving head to the marker.
    let reserve = truncation_marker(total_bytes, total_bytes).len() + 1;
    let content_budget = MAX_MEMORY_INDEX_BYTES.saturating_sub(reserve);
    let head = &index[..line_boundary_cut(index, content_budget)];
    let head = head.trim_end();
    let truncation = IndexTruncation {
        shown_bytes: head.len(),
        total_bytes,
    };
    let marker = truncation_marker(truncation.shown_bytes, truncation.total_bytes);
    let fitted = format!("{head}\n{marker}");
    debug_assert!(fitted.len() <= MAX_MEMORY_INDEX_BYTES);
    (fitted, Some(truncation))
}

/// The byte offset to cut `text` at so the result is at most `budget` bytes: just after the last
/// newline at or before the budget whose head SURVIVES `trim_end` (so the cut lands on a complete
/// line and the head still carries specialization), else the nearest char boundary at or below the
/// budget — the long-line fallback, which covers both a single line longer than the whole budget
/// and a file whose in-budget newlines all sit inside an all-whitespace opening.
fn line_boundary_cut(text: &str, budget: usize) -> usize {
    let budget = budget.min(text.len());
    let window = &text.as_bytes()[..budget];
    if let Some(newline) = window.iter().rposition(|&byte| byte == b'\n') {
        // CONTENT, not offset. The caller trims the head, so a newline is only a usable complete-line
        // cut when what precedes it is not all whitespace — `text[..newline]` is exactly that head
        // minus its own trailing LF, and `head.trim_end()` is empty iff this is. Checking the
        // RIGHTMOST newline decides every one of them: each earlier newline's prefix is a prefix of
        // this one, so if this prefix is all whitespace, so are all of theirs. That covers a lone
        // leading LF (offset 0), two LFs, `\r\n`, and any indented blank opening alike. Contract:
        // PR #983 addendum 1 as revised by the round-2 verdict.
        if !text[..newline].trim_end().is_empty() {
            return newline + 1;
        }
    }
    let mut cut = budget;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

/// Render the read-on-start memory section to inline into the job prompt, or `None` when there is
/// no non-empty index to inline. `template_path` overrides the in-repo default (read-on-start seam).
///
/// An index over [`MAX_MEMORY_INDEX_BYTES`] is TRUNCATED, not refused: the surviving head plus a
/// marker line is injected (see [`fit_index_to_budget`]) and `truncation` reports what was cut, so
/// the daemon can warn where the operator will see it. This call never fails over size; the only
/// `Err` is an index that exists and cannot be read.
pub fn read_on_start(
    memory_dir: &Path,
    template_path: Option<&Path>,
) -> io::Result<Option<ReadOnStart>> {
    let index_path = memory_dir.join(MEMORY_INDEX_FILE);
    let index = match fs::read_to_string(&index_path) {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let (index, truncation) = fit_index_to_budget(&index);
    let template = load_template(template_path, DEFAULT_READ_ON_START_TEMPLATE);
    let section = render(
        &template,
        &[
            (TOKEN_MEMORY_DIR, memory_dir.display().to_string().as_str()),
            (TOKEN_MEMORY_INDEX, index.as_str()),
        ],
    );
    Ok(Some(ReadOnStart {
        section,
        truncation,
    }))
}

/// [`read_on_start`] without the truncation report — the rendered section alone.
pub fn read_on_start_section(
    memory_dir: &Path,
    template_path: Option<&Path>,
) -> io::Result<Option<String>> {
    read_on_start(memory_dir, template_path).map(|read| read.map(|read| read.section))
}

/// What the operator surfaces (boot warning, `maxplayer doctor`) see when they look at the index.
/// Read-only: inspecting never creates `memory/` or anything in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexState {
    /// No `memory/` directory at all — the state of nearly every seat. Nothing to say.
    NoMemoryDir,
    /// `memory/` exists but holds no `MEMORY.md`: the seat injects nothing.
    NoIndex,
    /// `MEMORY.md` exists but is empty or whitespace: the seat injects nothing.
    Empty,
    /// The index fits the budget and is injected whole.
    Fits { bytes: usize },
    /// The index is over [`MAX_MEMORY_INDEX_BYTES`]: every job prompt gets a truncated copy.
    OverBudget { bytes: usize },
}

impl IndexState {
    /// Bytes still available under the budget for a fitting index (`None` otherwise).
    pub fn headroom_bytes(self) -> Option<usize> {
        match self {
            IndexState::Fits { bytes } => Some(MAX_MEMORY_INDEX_BYTES - bytes),
            _ => None,
        }
    }
}

/// Inspect the index at `memory_dir` for the operator surfaces. Reads only; a missing directory or
/// file is a state, not an error, and the only `Err` is an index that exists and cannot be read.
pub fn inspect_index(memory_dir: &Path) -> io::Result<IndexState> {
    if !memory_dir.is_dir() {
        return Ok(IndexState::NoMemoryDir);
    }
    let index_path = memory_dir.join(MEMORY_INDEX_FILE);
    let index = match fs::read_to_string(&index_path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(IndexState::NoIndex),
        Err(error) => return Err(error),
    };
    if index.trim().is_empty() {
        return Ok(IndexState::Empty);
    }
    let bytes = index.len();
    if bytes > MAX_MEMORY_INDEX_BYTES {
        Ok(IndexState::OverBudget { bytes })
    } else {
        Ok(IndexState::Fits { bytes })
    }
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
            "maxplayer-mem-{label}-{}-{id}",
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

    /// A template that is the bare `{memory_index}` token, so the rendered section IS the injected
    /// index text and its length can be held against the budget directly.
    fn bare_index_template(root: &Path) -> PathBuf {
        let template = root.join("bare-index.tmpl");
        fs::write(&template, TOKEN_MEMORY_INDEX).expect("write bare template");
        template
    }

    /// The injected index text of a rendered bare-template section, split into the surviving head
    /// and the marker line (the marker is always the LAST line).
    fn split_head_and_marker(injected: &str) -> (&str, &str) {
        injected
            .rsplit_once('\n')
            .expect("a truncated index is at least head + marker line")
    }

    /// An index a single byte over [`MAX_MEMORY_INDEX_BYTES`] is TRUNCATED and still injected — never
    /// refused, never dropped. The property this protects is unchanged from the refusal it replaces:
    /// a runaway `MEMORY.md` cannot bloat every job prompt, because the injected text (marker
    /// included) stays within the budget. This goes red if the bound check is removed — the injected
    /// text would then exceed the budget and carry no marker.
    #[test]
    fn read_on_start_truncates_index_over_size_bound() {
        let root = temp_dir("ros-overbound");
        let dir = memory_dir(&root);
        fs::create_dir_all(&dir).expect("mkdir");
        let oversized = "x".repeat(MAX_MEMORY_INDEX_BYTES + 1);
        fs::write(dir.join(MEMORY_INDEX_FILE), &oversized).expect("write index");
        let template = bare_index_template(&root);

        let read = read_on_start(&dir, Some(&template))
            .expect("an over-bound index is not an error")
            .expect("and it still injects");
        let truncation = read.truncation.expect("the read reports what it cut");
        assert_eq!(
            truncation.total_bytes,
            MAX_MEMORY_INDEX_BYTES + 1,
            "the real size is reported"
        );
        assert!(
            read.section.len() <= MAX_MEMORY_INDEX_BYTES,
            "injected text incl. marker is {} bytes, over the {MAX_MEMORY_INDEX_BYTES} budget",
            read.section.len()
        );
        let (head, marker) = split_head_and_marker(&read.section);
        assert_eq!(
            head.len(),
            truncation.shown_bytes,
            "shown_bytes is the surviving head"
        );
        assert!(
            head.chars().all(|c| c == 'x') && !head.is_empty(),
            "the head is the file's own text"
        );
        assert_eq!(
            marker,
            truncation_marker(truncation.shown_bytes, truncation.total_bytes)
        );
        assert!(
            marker.contains(&MAX_MEMORY_INDEX_BYTES.to_string()),
            "marker names the budget"
        );
        assert!(
            marker.contains(&(MAX_MEMORY_INDEX_BYTES + 1).to_string()),
            "marker names the actual size: {marker}"
        );
        // The section-only wrapper sees the same text.
        assert_eq!(
            read_on_start_section(&dir, Some(&template))
                .expect("read")
                .as_deref(),
            Some(read.section.as_str())
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A 3x-over index: the cut lands on a LINE boundary (the last content line is a complete fixture
    /// line), the marker is present and last, and the whole injected text fits the budget.
    #[test]
    fn read_on_start_cuts_a_3x_over_index_on_a_line_boundary_within_budget() {
        let root = temp_dir("ros-3x");
        let dir = memory_dir(&root);
        fs::create_dir_all(&dir).expect("mkdir");
        // Every fixture line ends in a sentinel so a mid-line cut is detectable.
        let mut index = String::from("# Memory\n\nAcme brand: headings in Söhne.|\n");
        let mut n = 0usize;
        while index.len() < 3 * MAX_MEMORY_INDEX_BYTES {
            index.push_str(&format!(
                "- topic line {n:06}: durable lesson text, kept short on purpose |\n"
            ));
            n += 1;
        }
        fs::write(dir.join(MEMORY_INDEX_FILE), &index).expect("write index");
        let template = bare_index_template(&root);

        let read = read_on_start(&dir, Some(&template))
            .expect("read")
            .expect("injects");
        let truncation = read.truncation.expect("truncated");
        assert_eq!(truncation.total_bytes, index.len());
        assert!(
            read.section.len() <= MAX_MEMORY_INDEX_BYTES,
            "3x-over input must render to <= budget incl. marker, got {}",
            read.section.len()
        );
        let (head, marker) = split_head_and_marker(&read.section);
        assert!(
            head.starts_with("# Memory\n\nAcme brand: headings in Söhne.|"),
            "head is the file's start"
        );
        assert!(
            head.ends_with('|'),
            "cut is on a line boundary — the last kept line is complete: {:?}",
            &head[head.len().saturating_sub(80)..]
        );
        assert!(index.starts_with(head), "the head is a prefix of the file");
        assert_eq!(
            marker,
            truncation_marker(truncation.shown_bytes, truncation.total_bytes)
        );
        assert!(
            marker.starts_with("[maxplayer: MEMORY.md truncated"),
            "marker is the last line"
        );
        // Most of the budget is used: a cut that threw away far more than one line is a bug.
        assert!(
            read.section.len() > MAX_MEMORY_INDEX_BYTES - 256,
            "the cut left {} unused bytes under the budget",
            MAX_MEMORY_INDEX_BYTES - read.section.len()
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A single-line multi-byte index over the budget is cut at a CHAR boundary: the result is a
    /// valid `String` (slicing mid-character would panic), fits the budget, and keeps the marker.
    #[test]
    fn read_on_start_cuts_multibyte_utf8_on_a_char_boundary() {
        let root = temp_dir("ros-utf8");
        let dir = memory_dir(&root);
        fs::create_dir_all(&dir).expect("mkdir");
        // 3-byte characters, one line, no newline anywhere: every cut candidate but one in three is
        // mid-character. Sized so the budget lands off a boundary whatever the marker length is.
        let glyph = "日";
        assert_eq!(glyph.len(), 3);
        let index = glyph.repeat(MAX_MEMORY_INDEX_BYTES / 3 + 500);
        assert!(index.len() > MAX_MEMORY_INDEX_BYTES);
        fs::write(dir.join(MEMORY_INDEX_FILE), &index).expect("write index");
        let template = bare_index_template(&root);

        let read = read_on_start(&dir, Some(&template))
            .expect("read")
            .expect("injects");
        let truncation = read.truncation.expect("truncated");
        assert!(read.section.len() <= MAX_MEMORY_INDEX_BYTES);
        let (head, marker) = split_head_and_marker(&read.section);
        assert!(
            head.chars().all(|c| c == '日') && !head.is_empty(),
            "head is whole characters only"
        );
        assert_eq!(
            head.len() % 3,
            0,
            "head length is a whole number of 3-byte chars"
        );
        assert_eq!(head.len(), truncation.shown_bytes);
        assert_eq!(
            marker,
            truncation_marker(truncation.shown_bytes, truncation.total_bytes)
        );
        // Also directly: the pure fitter produces a String that round-trips as valid UTF-8 bytes.
        let (fitted, _) = fit_index_to_budget(&index);
        assert!(std::str::from_utf8(fitted.as_bytes()).is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    /// An index whose ONLY newline at or before the budget sits at offset 0 (one LF, then one
    /// 65,536-byte line): the last-complete-line rule would leave an EMPTY head, so the long-line
    /// fallback applies — the head is non-empty, cut on a char boundary inside the second line, the
    /// marker is the final line and the whole result fits the budget as valid UTF-8.
    #[test]
    fn read_on_start_leading_blank_line_then_overlong_line_keeps_a_non_empty_head() {
        let root = temp_dir("ros-leading-lf");
        let dir = memory_dir(&root);
        fs::create_dir_all(&dir).expect("mkdir");
        let mut index = String::from("\n");
        index.push_str(&"x".repeat(MAX_MEMORY_INDEX_BYTES));
        assert_eq!(index.len(), MAX_MEMORY_INDEX_BYTES + 1);
        assert_eq!(index.find('\n'), Some(0), "the only newline is at offset 0");
        fs::write(dir.join(MEMORY_INDEX_FILE), &index).expect("write index");
        let template = bare_index_template(&root);

        let read = read_on_start(&dir, Some(&template))
            .expect("a leading blank line is not an error")
            .expect("and the index still injects");
        let truncation = read.truncation.expect("truncated");
        assert_eq!(truncation.total_bytes, index.len());
        assert!(
            read.section.len() <= MAX_MEMORY_INDEX_BYTES,
            "result incl. marker is {} bytes, over the budget",
            read.section.len()
        );
        assert!(
            std::str::from_utf8(read.section.as_bytes()).is_ok(),
            "valid UTF-8"
        );
        let (head, marker) = split_head_and_marker(&read.section);
        assert_eq!(
            marker,
            truncation_marker(truncation.shown_bytes, truncation.total_bytes)
        );
        assert!(
            marker.starts_with("[maxplayer: MEMORY.md truncated"),
            "marker is the final line"
        );
        // Non-empty head, cut inside the second line: the head keeps the file's leading LF and then
        // a run of `x` from the second line — NOT the empty string an offset-0 cut would have given.
        assert!(
            !head.trim().is_empty(),
            "the head carries specialization, not an empty line"
        );
        assert!(
            head.starts_with('\n'),
            "the head is a prefix of the file, incl. its leading LF"
        );
        let second_line = &head[1..];
        assert!(
            !second_line.is_empty(),
            "the cut landed inside the second line"
        );
        assert!(
            second_line.chars().all(|c| c == 'x'),
            "and kept only that line's own bytes"
        );
        assert!(
            index.is_char_boundary(head.len()),
            "the cut is on a char boundary"
        );
        assert_eq!(head.len(), truncation.shown_bytes);
        let _ = fs::remove_dir_all(&root);
    }

    /// Every all-whitespace opening — two LFs, a `\r\n` blank line, indented blank lines — followed by
    /// one over-budget line takes the same long-line fallback the single leading LF takes. The head is
    /// the file's OWN opening bytes plus a run of its long line, never the empty string a
    /// newline-offset rule would have selected (round-2 finding 1). Each case checks retained
    /// specialization, prefix identity, the marker as final line, UTF-8 validity and the total bound.
    /// This goes red if `line_boundary_cut` ever decides on a newline's offset instead of on whether
    /// the head it selects survives trimming.
    #[test]
    fn read_on_start_whitespace_opening_then_overlong_line_keeps_the_files_own_head() {
        // (label, opening bytes) — every one trims away to nothing, so none of their newlines is a
        // usable complete-line cut.
        let openings = [
            ("two-lf", "\n\n"),
            ("crlf-blank-line", "\r\n"),
            ("indented-blanks", " \n\t\n"),
            ("lf-space-lf", "\n \n"),
            ("many-lf", "\n\n\n\n"),
        ];
        for (label, opening) in openings {
            assert!(
                opening.trim_end().is_empty(),
                "{label}: the fixture opening must be all whitespace"
            );
            let root = temp_dir(&format!("ros-ws-open-{label}"));
            let dir = memory_dir(&root);
            fs::create_dir_all(&dir).expect("mkdir");
            let mut index = String::from(opening);
            index.push_str(&"x".repeat(MAX_MEMORY_INDEX_BYTES));
            fs::write(dir.join(MEMORY_INDEX_FILE), &index).expect("write index");
            let template = bare_index_template(&root);

            let read = read_on_start(&dir, Some(&template))
                .expect("a whitespace opening is not an error")
                .expect("and the index still injects");
            let truncation = read
                .truncation
                .unwrap_or_else(|| panic!("{label}: an over-budget index reports its truncation"));
            assert_eq!(truncation.total_bytes, index.len(), "{label}: real size");
            assert!(
                read.section.len() <= MAX_MEMORY_INDEX_BYTES,
                "{label}: result incl. marker is {} bytes, over the {MAX_MEMORY_INDEX_BYTES}-byte budget",
                read.section.len()
            );
            assert!(
                std::str::from_utf8(read.section.as_bytes()).is_ok(),
                "{label}: valid UTF-8"
            );

            let (head, marker) = split_head_and_marker(&read.section);
            assert_eq!(
                marker,
                truncation_marker(truncation.shown_bytes, truncation.total_bytes),
                "{label}: marker is the final line and reports the real numbers"
            );
            assert!(
                !head.trim().is_empty(),
                "{label}: the head carries specialization, not an all-whitespace opening"
            );
            assert!(
                head.contains('x'),
                "{label}: the head reaches into the long line"
            );
            assert!(
                index.as_bytes().starts_with(head.as_bytes()),
                "{label}: the head is a PREFIX of the file — no text skipped or reordered"
            );
            assert!(
                head.starts_with(opening),
                "{label}: the file's own opening bytes are kept, not skipped"
            );
            assert!(
                index.is_char_boundary(head.len()),
                "{label}: the cut is on a char boundary"
            );
            assert_eq!(
                head.len(),
                truncation.shown_bytes,
                "{label}: shown_bytes is the surviving head"
            );
            let _ = fs::remove_dir_all(&root);
        }
    }

    /// The complement of the fallback: when a blank opening IS followed by in-budget content lines, a
    /// complete line still wins — the fix decides on surviving content, so it must not push ordinary
    /// indexes onto the char-boundary path. The head keeps the file's blank opening and ends on a whole
    /// fixture line.
    #[test]
    fn read_on_start_blank_opening_still_cuts_on_a_complete_line() {
        const LINE: &str = "- alpha: the seat's own specialization line\n";
        let root = temp_dir("ros-blank-then-lines");
        let dir = memory_dir(&root);
        fs::create_dir_all(&dir).expect("mkdir");
        let mut index = String::from("\n\n");
        while index.len() < MAX_MEMORY_INDEX_BYTES * 2 {
            index.push_str(LINE);
        }
        fs::write(dir.join(MEMORY_INDEX_FILE), &index).expect("write index");
        let template = bare_index_template(&root);

        let read = read_on_start(&dir, Some(&template))
            .expect("read")
            .expect("injects");
        let truncation = read.truncation.expect("truncated");
        assert!(
            read.section.len() <= MAX_MEMORY_INDEX_BYTES,
            "within budget"
        );
        let (head, marker) = split_head_and_marker(&read.section);
        assert_eq!(
            marker,
            truncation_marker(truncation.shown_bytes, truncation.total_bytes)
        );
        assert!(
            index.as_bytes().starts_with(head.as_bytes()) && head.starts_with("\n\n"),
            "the head is the file's own prefix, blank opening included"
        );
        assert!(
            head.ends_with(LINE.trim_end()),
            "the cut landed on a COMPLETE line, not inside one: {:?}",
            &head[head.len().saturating_sub(48)..]
        );
        assert!(
            head[2..].lines().all(|line| line == LINE.trim_end()),
            "and kept only whole fixture lines"
        );
        assert_eq!(head.len(), truncation.shown_bytes);
        let _ = fs::remove_dir_all(&root);
    }

    /// The documented limit, stated so nobody reads the non-empty-head rule as a promise it cannot
    /// keep: when the whole in-budget window is whitespace, the content starts BEYOND the budget and no
    /// byte-bounded rule can reach it. The index is still injected with its marker and still fits the
    /// bound — it just has no head.
    #[test]
    fn read_on_start_all_whitespace_in_budget_window_has_no_head_to_keep() {
        let root = temp_dir("ros-ws-window");
        let dir = memory_dir(&root);
        fs::create_dir_all(&dir).expect("mkdir");
        let mut index = " \n".repeat(MAX_MEMORY_INDEX_BYTES);
        index.push_str("tail-content-past-the-budget");
        assert!(
            !index.trim().is_empty(),
            "the file as a whole is not blank, so the seam accepts it"
        );
        fs::write(dir.join(MEMORY_INDEX_FILE), &index).expect("write index");
        let template = bare_index_template(&root);

        let read = read_on_start(&dir, Some(&template))
            .expect("read")
            .expect("injects");
        let truncation = read.truncation.expect("truncated");
        assert_eq!(
            truncation.shown_bytes, 0,
            "no non-empty head exists within the budget, and none is manufactured"
        );
        assert!(
            read.section.len() <= MAX_MEMORY_INDEX_BYTES,
            "within budget"
        );
        assert!(std::str::from_utf8(read.section.as_bytes()).is_ok());
        let (head, marker) = split_head_and_marker(&read.section);
        assert!(head.is_empty(), "the head is empty: {head:?}");
        assert_eq!(
            marker,
            truncation_marker(0, index.len()),
            "the marker is still the final line and still tells the truth"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The operator-surface inspector reports each state and creates nothing.
    #[test]
    fn inspect_index_reports_every_state_and_creates_nothing() {
        let root = temp_dir("inspect");
        let dir = memory_dir(&root);
        assert_eq!(
            inspect_index(&dir).expect("no dir"),
            IndexState::NoMemoryDir
        );
        assert!(!dir.exists(), "inspecting must not create memory/");

        fs::create_dir_all(&dir).expect("mkdir");
        assert_eq!(inspect_index(&dir).expect("no index"), IndexState::NoIndex);
        assert!(
            !dir.join(MEMORY_INDEX_FILE).exists(),
            "inspecting must not create MEMORY.md"
        );

        fs::write(dir.join(MEMORY_INDEX_FILE), "  \n\t\n").expect("write blank");
        assert_eq!(inspect_index(&dir).expect("blank"), IndexState::Empty);

        fs::write(dir.join(MEMORY_INDEX_FILE), "# index\nline\n").expect("write small");
        let fits = inspect_index(&dir).expect("fits");
        assert_eq!(fits, IndexState::Fits { bytes: 13 });
        assert_eq!(fits.headroom_bytes(), Some(MAX_MEMORY_INDEX_BYTES - 13));

        fs::write(
            dir.join(MEMORY_INDEX_FILE),
            "z".repeat(MAX_MEMORY_INDEX_BYTES + 7),
        )
        .expect("write big");
        let over = inspect_index(&dir).expect("over");
        assert_eq!(
            over,
            IndexState::OverBudget {
                bytes: MAX_MEMORY_INDEX_BYTES + 7
            }
        );
        assert_eq!(over.headroom_bytes(), None);
        let _ = fs::remove_dir_all(&root);
    }

    /// An index at exactly the bound is still injected — the cap is `>`, not `>=`.
    #[test]
    fn read_on_start_accepts_index_at_exact_bound() {
        let root = temp_dir("ros-atbound");
        let dir = memory_dir(&root);
        fs::create_dir_all(&dir).expect("mkdir");
        let mut index = String::from("# index\n");
        index.push_str(&"y".repeat(MAX_MEMORY_INDEX_BYTES - index.len()));
        assert_eq!(index.len(), MAX_MEMORY_INDEX_BYTES);
        fs::write(dir.join(MEMORY_INDEX_FILE), &index).expect("write index");

        let section = read_on_start_section(&dir, None)
            .expect("at-bound index is accepted")
            .expect("some section");
        assert!(section.contains("# index"), "index text inlined");
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
