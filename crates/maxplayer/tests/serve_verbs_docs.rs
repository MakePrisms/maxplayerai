//! `serve` is the documented start verb for every daemon (`buyer serve`, `seller serve`,
//! `reviewer serve`, `maxplayer-mint serve`); the verbless forms and `maxplayer-mint run` are
//! deprecated aliases that still work. This guard reads the operator-facing docs, units and site
//! pages and fails if any of them teaches an old start form again. A line that says "deprecated" is
//! the alias note itself and is allowed.

/// (path for messages, contents)
const SOURCES: &[(&str, &str)] = &[
    ("README.md", include_str!("../../../README.md")),
    ("AGENTS.md", include_str!("../../../AGENTS.md")),
    ("Dockerfile", include_str!("../../../Dockerfile")),
    (
        "docker-compose.yml",
        include_str!("../../../docker-compose.yml"),
    ),
    ("install.sh", include_str!("../../../install.sh")),
    (
        "npm/maxplayer/README.md",
        include_str!("../../../npm/maxplayer/README.md"),
    ),
    ("docs/README.md", include_str!("../../../docs/README.md")),
    (
        "docs/BUYER-QUICKSTART.md",
        include_str!("../../../docs/BUYER-QUICKSTART.md"),
    ),
    (
        "docs/SELLER-QUICKSTART.md",
        include_str!("../../../docs/SELLER-QUICKSTART.md"),
    ),
    (
        "docs/DEPLOYMENT.md",
        include_str!("../../../docs/DEPLOYMENT.md"),
    ),
    ("docs/DOCKER.md", include_str!("../../../docs/DOCKER.md")),
    (
        "docs/SANDBOXING.md",
        include_str!("../../../docs/SANDBOXING.md"),
    ),
    (
        "crates/maxplayer-mint/README.md",
        include_str!("../../maxplayer-mint/README.md"),
    ),
    (
        "crates/maxplayer-mint/e2e/README.md",
        include_str!("../../maxplayer-mint/e2e/README.md"),
    ),
    (
        "crates/maxplayer-trade/README.md",
        include_str!("../../maxplayer-trade/README.md"),
    ),
    (
        "web/app/.well-known/skills/default/skill.md",
        include_str!("../../../web/app/.well-known/skills/default/skill.md"),
    ),
    (
        "web/app/.well-known/skills/seller-operate/skill.md",
        include_str!("../../../web/app/.well-known/skills/seller-operate/skill.md"),
    ),
    (
        "web/app/.well-known/skills/debug-selling/skill.md",
        include_str!("../../../web/app/.well-known/skills/debug-selling/skill.md"),
    ),
    (
        "web/app/.well-known/skills/buyer-operate/skill.md",
        include_str!("../../../web/app/.well-known/skills/buyer-operate/skill.md"),
    ),
    (
        "web/app/.well-known/skills/debug-buying/skill.md",
        include_str!("../../../web/app/.well-known/skills/debug-buying/skill.md"),
    ),
    (
        "web/app/.well-known/skills/grok-bot-operate/skill.md",
        include_str!("../../../web/app/.well-known/skills/grok-bot-operate/skill.md"),
    ),
    (
        "web/app/.well-known/skills/mint-and-trade/skill.md",
        include_str!("../../../web/app/.well-known/skills/mint-and-trade/skill.md"),
    ),
    (
        "web/app/.well-known/skills/index.json",
        include_str!("../../../web/app/.well-known/skills/index.json"),
    ),
    (
        "web/app/public/llms.txt",
        include_str!("../../../web/app/public/llms.txt"),
    ),
    // The seller page is gone; its selling section is on the market page now.
    (
        "web/app/public/market.html",
        include_str!("../../../web/app/public/market.html"),
    ),
    (
        "web/app/public/mint.html",
        include_str!("../../../web/app/public/mint.html"),
    ),
    (
        "scripts/challenges/e2e.py",
        include_str!("../../../scripts/challenges/e2e.py"),
    ),
];

/// The command prefixes that START a daemon when nothing (or only flags) follows them.
const VERBLESS: &[&str] = &[
    "maxplayer seller",
    "maxplayer buyer",
    "-- seller",
    ":latest seller",
];

/// True when `rest` (the text after a [`VERBLESS`] prefix) makes it the verbless START form: end of
/// the command (closing quote/backtick, line end, comment, continuation) or a start flag. A
/// subcommand (`seller fees`, `buyer status`, `seller serve`) or `--help` is not a start.
fn is_verbless_start(rest: &str) -> bool {
    let Some(first) = rest.chars().next() else {
        return true;
    };
    if first.is_alphanumeric() || first == '-' || first == '_' {
        return false; // `maxplayer seller-operate`, `maxplayer sellers`
    }
    if "`'\")]<".contains(first) {
        return true;
    }
    let next = rest.trim_start_matches(' ');
    if next.is_empty() || next.starts_with('\\') || next.starts_with('#') {
        return true;
    }
    next.starts_with("--") && !next.starts_with("--help")
}

/// Every old start form on one line, as a description, or empty.
fn old_forms(line: &str) -> Vec<String> {
    if line.to_ascii_lowercase().contains("deprecat") {
        return Vec::new();
    }
    let mut found = Vec::new();
    for prefix in VERBLESS {
        for (at, _) in line.match_indices(prefix) {
            if is_verbless_start(&line[at + prefix.len()..]) {
                found.push(format!("verbless `{prefix}`"));
            }
        }
    }
    for (at, _) in line.match_indices("maxplayer-mint run") {
        let after = line[at + "maxplayer-mint run".len()..].chars().next();
        if !after.is_some_and(|c| c.is_alphanumeric() || c == '-' || c == '_') {
            found.push("`maxplayer-mint run`".into());
        }
    }
    // Docker argv: `["seller"` must be followed by `"serve"`.
    for (at, _) in line.match_indices("[\"seller\"") {
        if !line[at..].starts_with("[\"seller\", \"serve\"") {
            found.push("docker argv `[\"seller\", …]` without \"serve\"".into());
        }
    }
    for (at, _) in line.match_indices("[BIN, 'seller'") {
        if !line[at..].starts_with("[BIN, 'seller', 'serve'") {
            found.push("runner argv `[BIN, 'seller', …]` without 'serve'".into());
        }
    }
    found
}

#[test]
fn docs_units_and_site_teach_only_the_serve_verb() {
    let mut hits = Vec::new();
    for (path, text) in SOURCES {
        for (index, line) in text.lines().enumerate() {
            for form in old_forms(line) {
                hits.push(format!("{path}:{}: {form}: {}", index + 1, line.trim()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "these lines teach a deprecated daemon start form; use `serve` (or say it is deprecated):\n{}",
        hits.join("\n")
    );
}

/// Every systemd `ExecStart=` in the docs that starts a maxplayer daemon ends in its `serve` verb.
#[test]
fn every_documented_unit_starts_its_daemon_with_serve() {
    let mut units = 0;
    for (path, text) in SOURCES {
        for line in text.lines().map(str::trim) {
            let Some(cmd) = line.strip_prefix("ExecStart=") else {
                continue;
            };
            if !(cmd.contains("/maxplayer ") || cmd.contains("/maxplayer-mint ")) {
                continue;
            }
            units += 1;
            let words: Vec<&str> = cmd.split_whitespace().collect();
            // `maxplayer-mint serve` (verb right after the binary) or `maxplayer <daemon> serve`.
            let verb = if words[0].ends_with("/maxplayer-mint") {
                words.get(1)
            } else {
                words.get(2)
            };
            assert!(
                verb == Some(&"serve"),
                "{path}: unit must start the daemon with `serve`: {line}"
            );
        }
    }
    // SELLER-QUICKSTART (seller) and the mint README (mint) both document one; if this drops, the
    // guard has stopped reading them.
    assert!(
        units >= 2,
        "expected the seller and mint units, found {units}"
    );
}

/// The guard itself: each old form is caught, and the new forms, subcommands and alias notes are not.
#[test]
fn the_guard_catches_old_forms_and_passes_new_ones() {
    for old in [
        "maxplayer seller --agent claude --rate-sats 100",
        "a bare `maxplayer seller` relaunches",
        "ExecStart=%h/.local/bin/maxplayer seller",
        "start it with `maxplayer buyer`.",
        "nix run --refresh github:MakePrisms/maxplayerai -- seller    # comment",
        "maxplayer:latest seller --non-interactive",
        "ExecStart=%h/.local/bin/maxplayer-mint run",
        "tmux new-session -d 'maxplayer-mint run'",
        "CMD [\"seller\"]",
        "command: [\"seller\", \"--non-interactive\"]",
        "start(name, [BIN, 'seller', '--non-interactive'], env)",
    ] {
        assert!(!old_forms(old).is_empty(), "guard missed: {old}");
    }
    for new in [
        "maxplayer seller serve --agent claude --rate-sats 100",
        "maxplayer seller fees remit --confirm",
        "maxplayer seller drain --no-wait",
        "maxplayer buyer status",
        "maxplayer seller --help",
        "ExecStart=%h/.local/bin/maxplayer-mint serve",
        "CMD [\"seller\", \"serve\"]",
        "start(name, [BIN, 'seller', 'serve', '--skip-doctor'], env)",
        "maxplayer seller [flags]   # deprecated alias for `maxplayer seller serve [flags]`",
        "`maxplayer-mint run` (the name before 0.8.0) is a deprecated alias",
        "maxplayer seller home=/x key_present=true",
        "maxplayer-seller-operate",
    ] {
        assert!(old_forms(new).is_empty(), "guard false positive: {new}");
    }
}

/// The `--help`/usage text the binary prints names only the `serve` start forms (the alias appears
/// only on a line that says it is deprecated).
#[test]
fn usage_text_teaches_only_the_serve_verb() {
    let mut hits = Vec::new();
    for args in [
        &["--help"][..],
        &["buyer", "--help"][..],
        &["seller", "--help"][..],
    ] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_maxplayer"))
            .args(args)
            .output()
            .expect("run maxplayer");
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        for line in text.lines() {
            for form in old_forms(line) {
                hits.push(format!(
                    "maxplayer {}: {form}: {}",
                    args.join(" "),
                    line.trim()
                ));
            }
        }
        assert!(
            text.contains("serve"),
            "{args:?} usage names no `serve` form:\n{text}"
        );
    }
    assert!(
        hits.is_empty(),
        "usage teaches a deprecated start form:\n{}",
        hits.join("\n")
    );
}
