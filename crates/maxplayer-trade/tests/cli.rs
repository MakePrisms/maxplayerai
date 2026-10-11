use std::process::Command;
fn home() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}
#[test]
fn fresh_home_balance_is_zero_and_private() {
    let h = home();
    let out = Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            h.path().to_str().unwrap(),
            "balance",
            "https://testnut.cashu.space/",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let balance: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(balance["balance"], 0);
    assert_eq!(balance["mint"], "https://testnut.cashu.space");
    assert!(
        !h.path().join("wallet.seed").exists(),
        "read-only balance must not create a wallet"
    );
    let second = Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            h.path().to_str().unwrap(),
            "balance",
            "https://testnut.cashu.space",
        ])
        .output()
        .unwrap();
    assert!(second.status.success());
    assert_eq!(out.stdout, second.stdout);
}
#[test]
fn real_mint_balance_needs_no_opt_in() {
    let h = home();
    let out = Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            h.path().to_str().unwrap(),
            "balance",
            "https://mint.minibits.cash/Bitcoin",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["balance"],
        0
    );
}
#[test]
fn balance_and_status_work_while_writer_owns_home() {
    use fs2::FileExt;
    let h = home();
    let f = std::fs::File::create(h.path().join("owner.lock")).unwrap();
    f.lock_exclusive().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            h.path().to_str().unwrap(),
            "balance",
            "https://testnut.cashu.space",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "SAFETY: live watcher must not block read-only balance"
    );
    let status = Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args(["--home", h.path().to_str().unwrap(), "status"])
        .output()
        .unwrap();
    assert!(status.status.success());
    assert!(!h.path().join("trade.sqlite").exists());
}
#[test]
fn removed_opt_in_flag_is_not_supported() {
    let h = home();
    let out = Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            h.path().to_str().unwrap(),
            "--real-mint-allow",
            "balance",
            "https://mint.minibits.cash/Bitcoin",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unexpected argument"));
    assert!(!h.path().join("wallet.seed").exists());
}

#[test]
fn skill_commands_and_flags_match_binary_help() {
    use std::collections::{BTreeMap, BTreeSet};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/maxplayer-trade");
    let mut paths = vec![root.join("SKILL.md")];
    for entry in std::fs::read_dir(root.join("references")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "md") {
            paths.push(path);
        }
    }
    let docs: Vec<_> = paths
        .iter()
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect();
    assert!(docs[0].len() < 10_000, "keep SKILL.md tight");
    let binary = env!("CARGO_BIN_EXE_maxplayer-trade");
    let read_help = |args: &[&str]| {
        let out = Command::new(binary).args(args).output().unwrap();
        assert!(out.status.success(), "help failed: {args:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    let root_help = read_help(&["--help"]);
    assert_eq!(read_help(&["-h"]), root_help);
    assert_eq!(read_help(&["help"]), root_help);
    let commands: BTreeSet<_> = root_help
        .split("Commands:")
        .nth(1)
        .unwrap()
        .split("Options:")
        .next()
        .unwrap()
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|word| *word != "help")
        .collect();
    let helps: BTreeMap<_, _> = commands
        .iter()
        .map(|command| (*command, read_help(&[command, "--help"])))
        .collect();
    for command in &commands {
        assert_eq!(read_help(&[command, "-h"]), helps[command]);
        assert_eq!(read_help(&["help", command]), helps[command]);
    }
    let has_flag = |help: &str, flag: &str| help.split_whitespace().any(|word| word == flag);
    let mut checked = BTreeSet::new();
    // The canonical block is an executable-interface inventory, not a second
    // hardcoded command list: adding an unsupported command/flag fails here.
    let reference = std::fs::read_to_string(root.join("references/commands.md")).unwrap();
    let signatures = reference
        .split("```text\n")
        .nth(1)
        .unwrap()
        .split("```")
        .next()
        .unwrap();
    for signature in signatures.lines().filter(|line| !line.trim().is_empty()) {
        let command = signature.split_whitespace().next().unwrap();
        let help = helps
            .get(command)
            .unwrap_or_else(|| panic!("unknown documented command {command}"));
        checked.insert(command);
        for word in signature.split_whitespace() {
            let word = word.trim_matches(['[', ']']);
            if word.starts_with("--") {
                assert!(has_flag(help, word), "{command} missing {word}");
            }
        }
        for flag in help
            .split("Options:")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .filter(|word| word.starts_with("--"))
        {
            assert!(
                flag == "--help" || signature.contains(flag),
                "undocumented {command} flag {flag}"
            );
        }
        println!("PASS {signature}");
    }
    assert_eq!(checked, commands, "document every CLI command");
    // Scan ALL prose and examples for long flags, including references. Check
    // command-local inline examples against that command, not the union alone.
    let all_help = format!(
        "{} {}",
        root_help,
        helps.values().cloned().collect::<Vec<_>>().join(" ")
    );
    let mut flags = BTreeSet::new();
    for doc in &docs {
        for piece in doc.split("--").skip(1) {
            let name: String = piece
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            if !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
                continue;
            }
            let flag = format!("--{name}");
            assert!(has_flag(&all_help, &flag), "unknown documented flag {flag}");
            flags.insert(flag);
        }
        for snippet in doc.split('`').skip(1).step_by(2) {
            let Some(command) = snippet.split_whitespace().next() else {
                continue;
            };
            if let Some(help) = helps.get(command) {
                for piece in snippet.split("--").skip(1) {
                    let name: String = piece
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                        .collect();
                    let flag = format!("--{name}");
                    assert!(has_flag(help, &flag), "{command} missing {flag}");
                }
            }
        }
    }
    let supported_flags: BTreeSet<_> = all_help
        .split_whitespace()
        .filter(|word| word.starts_with("--"))
        .map(str::to_owned)
        .collect();
    assert_eq!(flags, supported_flags, "document every supported long flag");
    for command in ["list", "take"] {
        assert!(
            helps[command]
                .lines()
                .any(|line| line.contains("--max-fees") && line.contains("[default: 16]")),
            "documented {command} fee default changed"
        );
    }
    for flag in ["--home", "--relay"] {
        assert!(has_flag(&root_help, flag), "global {flag}");
    }
    println!(
        "PASS {} commands; {} distinct flags; list/take fee default 16; {} skill documents",
        checked.len(),
        flags.len(),
        docs.len()
    );
}
