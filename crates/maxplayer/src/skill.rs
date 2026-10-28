//! `maxplayer skill` — where the agent-facing documentation lives, and the ONE constant every
//! other surface prints that pointer from.
//!
//! Before this module, the whole binary carried exactly one route to the docs: the MCP
//! `initialize.instructions` text in `mcp.rs`. A buyer that registers `maxplayer mcp` was told
//! where the guides are; nobody else ever was — not `maxplayer --help`, not `maxplayer doctor`,
//! not a seller configuring a new seat — and `mcp.rs` itself records that an MCP client may
//! discard `instructions`. An operator whose box never touches MCP (a seller-only seat, or an
//! agent that drives the CLI directly) therefore had no route at all.
//!
//! The URL text is deliberately a single constant, referenced from every path that prints it and
//! asserted against by every test that guards it. Two copies of a URL drift silently — the web
//! build already refuses to keep a second copy of `skill.md` for exactly this reason
//! (`web/app/scripts/build.mjs`, the `/skill.md` alias comment).
//!
//! `maxplayer skill` itself is pure: no home bootstrap, no key, no wallet, no network. It must
//! work on a box that has installed nothing but the binary, because that is the moment an agent
//! needs the pointer.

use std::io::Write;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;

/// The site every published skill is served from, as a literal so `concat!` can derive each URL
/// from it at compile time. The ONE place the origin is spelled: every URL below is built from it.
macro_rules! site_base {
    () => {
        "https://www.maxplayer.ai"
    };
}

/// The site path of a published skill, as `web/app/.well-known/skills/index.json` lists it.
macro_rules! skill_path {
    ($name:literal) => {
        concat!("/.well-known/skills/", $name, "/skill.md")
    };
}

/// The site origin (no trailing slash). Exposed so tests can assert every URL derives from it.
pub const SITE_BASE: &str = site_base!();

/// The orientation page: the homepage skill, which links every companion skill (buyer, seller,
/// multi-turn buying, debugging, per-box operator notes). Also the URL the MCP handshake points at.
pub const SKILL_URL: &str = concat!(site_base!(), "/skill.md");

/// The machine-readable inventory of every published skill: `{name, description, path}` entries.
pub const SKILL_INDEX_URL: &str = concat!(site_base!(), "/.well-known/skills/index.json");

/// Site path of the buyer setup/operation skill (`maxplayer-buyer-operate`).
pub const BUYER_OPERATE_SKILL_PATH: &str = skill_path!("buyer-operate");

/// The buyer setup/operation skill: wallet funding, MCP registration, the post_job → collect loop.
/// The MCP `post_job` description LEADS with it, and the first successful post_job result of each
/// MCP server process carries it (`mcp.rs`).
pub const BUYER_OPERATE_SKILL_URL: &str = concat!(site_base!(), skill_path!("buyer-operate"));

/// Site path of the buyer debugging skill (`maxplayer-debug-buying`).
pub const DEBUG_BUYING_SKILL_PATH: &str = skill_path!("debug-buying");

/// The buyer debugging skill: stalled jobs, lapsed claims, unpaid sellers, stuck reservations. The
/// MCP `collect` description LEADS with it, and every MCP tool error result points at it (`mcp.rs`).
pub const DEBUG_BUYING_SKILL_URL: &str = concat!(site_base!(), skill_path!("debug-buying"));

/// The one-line pointer shared by `maxplayer --help`, `maxplayer doctor` and the seller first-run
/// path. One function, not three strings, so the pointer cannot be present on one surface and
/// stale on another.
pub fn docs_pointer_line() -> String {
    format!("Docs for agents: {SKILL_URL}  (or run `maxplayer skill`)")
}

/// Entry from `cli::run` for `maxplayer skill`.
///
/// Prints a few lines of plain text an agent can act on: the orientation URL, the skill index, and
/// what to do with them. Any argument other than a sole `--help` is a usage error — there is
/// nothing to configure here, and a flag that is silently ignored teaches the wrong lesson.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // #570: a sole `--help` prints usage to STDOUT and exits 0.
    if crate::cli::is_help_request(args) {
        write_usage(out);
        return SUCCESS;
    }
    if !args.is_empty() {
        write_usage(err);
        return USAGE_ERROR;
    }
    let _ = write!(out, "{}", render());
    SUCCESS
}

/// The text `maxplayer skill` prints. Pure, so the test asserts the exact output.
pub fn render() -> String {
    format!(
        "maxplayer documentation for agents\n\
         \x20 orientation:  {SKILL_URL}\n\
         \x20 skill index:  {SKILL_INDEX_URL}\n\
         Fetch the orientation page first: it links every companion skill (buyer setup, seller \
         setup, multi-turn buying, debugging, and per-box operator notes). The index lists the same \
         skills as machine-readable {{name, description, path}} entries.\n"
    )
}

fn write_usage(out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "Usage:\n  maxplayer skill   # print where the agent documentation lives (orientation URL + skill index); needs no wallet, key or network"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captured(tail: &[&str]) -> (i32, String, String) {
        let args: Vec<String> = tail.iter().map(|s| (*s).to_owned()).collect();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(&args, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).expect("stdout utf8"),
            String::from_utf8(err).expect("stderr utf8"),
        )
    }

    #[test]
    fn skill_prints_both_urls_to_stdout_and_nothing_else_happens() {
        let (code, out, err) = captured(&[]);
        assert_eq!(code, 0);
        assert!(err.is_empty(), "stderr must stay empty:\n{err}");
        assert_eq!(out, render());
        assert!(out.contains(SKILL_URL), "orientation URL missing:\n{out}");
        assert!(
            out.contains(SKILL_INDEX_URL),
            "skill index URL missing:\n{out}"
        );
        // Plain text an agent can act on: the two URLs each sit on their own line.
        assert!(
            out.lines()
                .any(|line| line.trim_start().starts_with("orientation:"))
        );
        assert!(
            out.lines()
                .any(|line| line.trim_start().starts_with("skill index:"))
        );
    }

    #[test]
    fn skill_index_url_is_under_the_same_origin_as_the_orientation_page() {
        // The index is derived by the web build from the same tree that publishes /skill.md; a
        // pointer to some other origin would send an agent to an inventory that is not this one.
        let origin = "https://www.maxplayer.ai/";
        assert!(SKILL_URL.starts_with(origin));
        assert!(SKILL_INDEX_URL.starts_with(origin));
    }

    // The specific skill URLs the MCP surface links are the site base + the published path, never a
    // second hand-typed copy of either.
    #[test]
    fn specific_skill_urls_are_the_site_base_plus_their_published_path() {
        assert_eq!(SITE_BASE, "https://www.maxplayer.ai");
        assert_eq!(
            BUYER_OPERATE_SKILL_URL,
            format!("{SITE_BASE}{BUYER_OPERATE_SKILL_PATH}")
        );
        assert_eq!(
            DEBUG_BUYING_SKILL_URL,
            format!("{SITE_BASE}{DEBUG_BUYING_SKILL_PATH}")
        );
        assert_eq!(
            BUYER_OPERATE_SKILL_URL,
            "https://www.maxplayer.ai/.well-known/skills/buyer-operate/skill.md"
        );
        assert_eq!(
            DEBUG_BUYING_SKILL_URL,
            "https://www.maxplayer.ai/.well-known/skills/debug-buying/skill.md"
        );
        assert_eq!(SKILL_URL, format!("{SITE_BASE}/skill.md"));
        assert_eq!(
            SKILL_INDEX_URL,
            format!("{SITE_BASE}/.well-known/skills/index.json")
        );
    }

    // Link-rot guard: every skill the MCP surface links by path must be listed in the published
    // inventory (`web/app/.well-known/skills/index.json`, compiled in so a move fails the build of
    // this test) and its file must exist in the tree the site is built from. Renaming or removing
    // either skill without updating these constants fails here, not on an agent's first fetch.
    #[test]
    fn linked_skill_paths_exist_in_the_published_skill_index() {
        let index: serde_json::Value = serde_json::from_str(include_str!(
            "../../../web/app/.well-known/skills/index.json"
        ))
        .expect("index.json parses");
        let listed: Vec<&str> = index["skills"]
            .as_array()
            .expect("index.json has a skills array")
            .iter()
            .filter_map(|skill| skill["path"].as_str())
            .collect();
        let web_app = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/app");
        for path in [BUYER_OPERATE_SKILL_PATH, DEBUG_BUYING_SKILL_PATH] {
            assert!(
                listed.contains(&path),
                "{path} is not listed in web/app/.well-known/skills/index.json: {listed:?}"
            );
            let file = web_app.join(path.trim_start_matches('/'));
            assert!(file.is_file(), "{} does not exist", file.display());
        }
    }

    #[test]
    fn docs_pointer_line_carries_the_shared_url_and_names_the_subcommand() {
        let line = docs_pointer_line();
        assert!(line.contains(SKILL_URL));
        assert!(line.contains("maxplayer skill"));
        assert!(!line.contains('\n'), "the pointer is ONE line: {line:?}");
    }

    #[test]
    fn skill_help_prints_usage_to_stdout_and_stray_arguments_are_refused() {
        let (code, out, err) = captured(&["--help"]);
        assert_eq!(code, 0);
        assert!(out.contains("Usage:") && out.contains("maxplayer skill"));
        assert!(err.is_empty());

        let (code, out, err) = captured(&["--verbose"]);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(err.contains("Usage:"));
    }
}
