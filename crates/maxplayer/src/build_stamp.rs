//! The one line `maxplayer --version` prints, and the build-time provenance inside it (#818).
//!
//! `maxplayer_core::version()` stays exactly what it was — the bare crate semver — because two other
//! callers publish it as a protocol field where a parenthesised suffix would be a wire change: the
//! MCP server info (`mcp.rs`) and the buyer's JSON (`maxplayer-core/src/buyer/mod.rs`). The stamp is
//! additive and lives here, next to the surface that shows it to a human.

/// The commit this binary was built from: 40 lowercase hex, or the literal `unknown` when the build
/// had no `.git` to read and no `MAXPLAYER_BUILD_COMMIT` to be told. See `build.rs` for the order.
///
/// `unknown` is a value, not a gap. #818's measurement was of a binary that yielded 40-hex strings
/// which resolved to nothing, so silence about a missing commit is the honest answer and a
/// plausible-looking one is the defect.
/// Read from a file in `OUT_DIR` rather than from a `cargo::rustc-env` variable: cargo puts those in
/// the environment of the processes it launches too, and this repo refuses an unmapped `MAXPLAYER_*`
/// variable fail-closed. `build.rs` has the measurement.
pub fn build_commit() -> &'static str {
    include_str!(concat!(env!("OUT_DIR"), "/build_stamp"))
}

/// `maxplayer <semver> (<stamp>)` — the single line both `version` and `--version` print.
///
/// One function so the two dispatch arms cannot drift: #818's acceptance is a predicate over what
/// the artifact prints, and `verify-release-version.sh` asserts the two forms print the SAME line.
pub fn version_line() -> String {
    format!(
        "maxplayer {} ({})",
        maxplayer_core::version(),
        build_commit()
    )
}

/// The FIRST stderr line every long-running `maxplayer` command prints on start (buyer daemon,
/// seller, reviewer, mcp): `maxplayer <semver> (<stamp>) — <role> starting, <where>`.
///
/// An operator who starts a daemon by bare name with an older binary first on `PATH` must be able to
/// tell from the log alone which build is serving. The prefix is [`version_line`] verbatim, so the
/// log and `--version` cannot disagree. `starting` is deliberately verb-neutral (`serve`, the
/// deprecated verbless `seller`/`buyer`, chat's `watch`). A deprecation line follows it, never leads.
pub fn start_line(role: &str, place: &str) -> String {
    format!("{} — {role} starting, {place}", version_line())
}

#[cfg(test)]
mod tests {
    #[test]
    fn start_line_leads_with_the_version_line_and_names_the_role() {
        let line = super::start_line("seller", "home /h");
        assert_eq!(
            line,
            format!("{} — seller starting, home /h", super::version_line())
        );
        assert!(line.starts_with(&format!("maxplayer {} (", maxplayer_core::version())));
    }
}
