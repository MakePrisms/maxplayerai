//! `maxplayer account link | status | marketing on|off` — link this agent's key to a human's email
//! (email signup spec v2.12 §3–4, §8.1). The relay only accepts writes from linked keys.
//!
//! Signup is agent-only. The agent runs `account link`, the service emails the human a 6-digit
//! code, and entering that code links the key and accepts the Terms. Two forms:
//!
//! - **Interactive** (stdin is a terminal): asks for whatever was not given as a flag (email, label,
//!   then the code) and confirms in one run.
//! - **Two steps** (an agent without a terminal): `account link --email <e> --label <l>` sends the
//!   code and exits; `account link --code <6 digits>` confirms it.
//!
//! There is no way to link without the emailed code, and no flag that accepts the Terms on the
//! human's behalf (spec §12: no non-interactive terms acceptance). There is no unlink (spec §4).
//!
//! The key is the home `key` file (the same key the wallet signs mint requests with, #1210), or
//! with `--mint` the mint server's `<home>/mint/nostr.key`. The secret is never printed.

use std::io::{BufRead, Write};
use std::path::PathBuf;

/// Shown wherever the label is asked for: the label is the agent's public name, not a private tag.
const PUBLIC_NAME_NOTICE: &str =
    "This name is public on Maxplayer (shown on your profile and in the welcome email).";

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const RUNTIME_ERROR: i32 = 2;

fn write_usage(out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "Usage:\n\
         \x20 maxplayer account link [--email <email>] [--label <name>] [--home <dir>] [--mint]\n\
         \x20 maxplayer account link --code <6 digits> [--entered-by human|agent] [--home <dir>] [--mint]\n\
         \x20 maxplayer account status [--json] [--home <dir>] [--mint]\n\
         \x20 maxplayer account marketing on|off [--home <dir>] [--mint]\n\
         \n\
         Links this agent's key to your email so the Maxplayer relay accepts its writes. `link`\n\
         emails you a 6-digit code; entering it links the key and accepts the Maxplayer Terms.\n\
         In a terminal, `link` asks for anything missing and then for the code. Without one,\n\
         run `link --email <email> --label <name>` first, then `link --code <code>`.\n\
         --label: a name for this agent. {PUBLIC_NAME_NOTICE} Once linked, it becomes the\n\
         agent's Nostr profile name unless the agent already has one (`maxplayer profile set`).\n\
         --entered-by: who typed the code (default: human at the prompt, agent with --code).\n\
         --mint: use this home's mint server key (<home>/mint/nostr.key) instead of the agent key.\n\
         Service: registration_url in config.toml or MAXPLAYER_REGISTRATION_URL\n\
         (default https://relay.maxplayer.ai/account).\n\
         Exit codes: 0 success, 1 usage error, 2 runtime error"
    );
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Opts {
    email: Option<String>,
    label: Option<String>,
    code: Option<String>,
    entered_by: Option<String>,
    home: Option<PathBuf>,
    mint: bool,
    json: bool,
}

fn parse(args: &[String], allowed: &[&str]) -> Result<Opts, String> {
    let mut opts = Opts::default();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        if !allowed.contains(&flag) {
            return Err(format!("unknown argument {flag}"));
        }
        let mut value = || {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match flag {
            "--email" => opts.email = Some(value()?),
            "--label" => opts.label = Some(value()?),
            "--code" => opts.code = Some(value()?),
            "--entered-by" => opts.entered_by = Some(value()?),
            "--home" => opts.home = Some(PathBuf::from(value()?)),
            "--mint" => opts.mint = true,
            "--json" => opts.json = true,
            _ => return Err(format!("unknown argument {flag}")),
        }
        index += 1;
    }
    Ok(opts)
}

const LINK_FLAGS: &[&str] = &[
    "--email",
    "--label",
    "--code",
    "--entered-by",
    "--home",
    "--mint",
];
const STATUS_FLAGS: &[&str] = &["--json", "--home", "--mint"];
const MARKETING_FLAGS: &[&str] = &["--home", "--mint"];

/// Entry from `cli::run` for `maxplayer account ...`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // #570: a sole `--help` prints usage to STDOUT and exits 0 before any parse, home or network.
    if crate::cli::is_help_request(args) {
        write_usage(out);
        return SUCCESS;
    }
    let (sub, rest) = match args.split_first() {
        Some((sub, rest)) => (sub.as_str(), rest),
        None => return usage_error(err, None),
    };
    let parsed = match sub {
        "link" => parse(rest, LINK_FLAGS).and_then(|o| check_link_opts(&o).map(|()| o)),
        "status" => parse(rest, STATUS_FLAGS),
        "marketing" => match rest.split_first() {
            Some((switch, rest)) if switch == "on" || switch == "off" => {
                parse(rest, MARKETING_FLAGS)
            }
            _ => Err("account marketing takes `on` or `off`".to_owned()),
        },
        other => Err(format!("unknown account command {other}")),
    };
    let opts = match parsed {
        Ok(opts) => opts,
        Err(message) => return usage_error(err, Some(&message)),
    };
    #[cfg(not(feature = "wallet"))]
    {
        let _ = (opts, out);
        let _ = writeln!(
            err,
            "maxplayer account requires the wallet feature (rebuild with default features)"
        );
        USAGE_ERROR
    }
    #[cfg(feature = "wallet")]
    {
        let json_errors = sub == "status" && opts.json;
        let ctx = match wallet::Context::open(&opts, err) {
            Ok(ctx) => ctx,
            Err(message) => {
                if json_errors {
                    wallet::json_error(out, "home", &message);
                } else {
                    let _ = writeln!(err, "{message}");
                }
                return RUNTIME_ERROR;
            }
        };
        match sub {
            "link" => {
                use std::io::IsTerminal;
                let stdin = std::io::stdin();
                let interactive = stdin.is_terminal();
                wallet::link(&ctx, &opts, &mut stdin.lock(), interactive, out, err)
            }
            "status" => wallet::status(&ctx, opts.json, out, err),
            _ => wallet::marketing(
                &ctx,
                args.get(1).map(String::as_str) == Some("on"),
                out,
                err,
            ),
        }
    }
}

fn usage_error(err: &mut dyn Write, message: Option<&str>) -> i32 {
    if let Some(message) = message {
        let _ = writeln!(err, "{message}");
    }
    write_usage(err);
    USAGE_ERROR
}

/// Flag combinations that are refused before anything is read or sent.
fn check_link_opts(opts: &Opts) -> Result<(), String> {
    if opts.code.is_some() && (opts.email.is_some() || opts.label.is_some()) {
        // A new request would replace the code being entered.
        return Err(
            "--code confirms a code that was already sent; do not combine it with --email or \
             --label"
                .to_owned(),
        );
    }
    if let Some(who) = &opts.entered_by {
        if opts.code.is_none() {
            return Err("--entered-by goes with --code".to_owned());
        }
        if who != "human" && who != "agent" {
            return Err("--entered-by must be human or agent".to_owned());
        }
    }
    Ok(())
}

#[cfg(feature = "wallet")]
mod wallet {
    use super::*;
    use maxplayer_core::home::{self, MaxplayerHome};
    use maxplayer_core::profile::{self, NameFill};
    use maxplayer_core::registration::{
        self, CodeSent, EnteredBy, Linked, MARKETING_NOTICE, RegistrationClient, RegistrationError,
        TERMS_VERSION,
    };
    use nostr_sdk::prelude::{Keys, ToBech32};

    /// The key being linked and where to send requests.
    pub struct Context {
        pub keys: Keys,
        pub npub: String,
        pub base_url: String,
        /// "this agent's key" / "this mint's key", for messages.
        pub whose: &'static str,
        /// The flags that select this key (` --mint`, ` --home <dir>`), repeated in every command
        /// a message tells the user to run, so it acts on the same key.
        pub flags: String,
        /// The agent home whose kind-0 the label may name. `None` for `--mint`: a mint key has no
        /// agent profile, so linking it publishes nothing.
        pub home: Option<MaxplayerHome>,
        /// `[profile].name` when it is a valid label: the label prompt's default.
        pub profile_name: Option<String>,
    }

    /// `{"error": <code>, "message": <text>}` on stdout: the `--json` failure shape.
    pub fn json_error(out: &mut dyn Write, code: &str, message: &str) {
        let _ = writeln!(
            out,
            "{}",
            serde_json::json!({ "error": code, "message": message })
        );
    }

    impl Context {
        /// `maxplayer account <sub>` with this key's flags.
        pub fn command(&self, sub: &str) -> String {
            format!("maxplayer account {sub}{}", self.flags)
        }

        /// Opens the key `opts` selects. `Err` is the message to show. `err` only receives the
        /// "Created a new agent key" notice (default agent home on first use).
        pub fn open(opts: &Opts, err: &mut dyn Write) -> Result<Self, String> {
            let mut flags = String::new();
            if opts.mint {
                flags.push_str(" --mint");
            }
            if let Some(home) = &opts.home {
                flags.push_str(&format!(" --home {}", home.display()));
            }
            let explicit = opts.home.is_some();
            let root = match &opts.home {
                Some(path) => path.clone(),
                None => home::default_home_dir().map_err(|e| e.to_string())?,
            };
            if opts.mint {
                return Self::open_mint(&root, flags);
            }
            // An explicit --home must already hold a key: linking a typo'd path would register a
            // brand-new identity nobody uses. The default home is created like any first command.
            if explicit && !home::is_initialized(&root) {
                return Err(format!(
                    "account refused: no maxplayer home with a key at {} (check the path, or run \
                     `maxplayer whoami --home {}` to create one)",
                    root.display(),
                    root.display()
                ));
            }
            let home = home::bootstrap(&root).map_err(|e| e.to_string())?;
            let secret = home::read_secret_key_hex(&home).map_err(|e| e.to_string())?;
            let keys =
                Keys::parse(&secret).map_err(|_| "the home key is not a valid key".to_owned())?;
            let npub = keys
                .public_key()
                .to_bech32()
                .map_err(|e| format!("npub encode: {e}"))?;
            if home.key_created {
                let _ = writeln!(
                    err,
                    "Created a new agent key in {} ({npub}).",
                    home.root.display()
                );
            }
            let profile_name = home
                .config
                .profile
                .as_ref()
                .and_then(|p| p.name.as_deref())
                .and_then(registration::normalize_label);
            Ok(Self {
                keys,
                npub,
                base_url: home.config.registration_url.clone(),
                whose: "this agent's key",
                flags,
                home: Some(home),
                profile_name,
            })
        }

        /// `--mint`: the mint server's `<root>/mint/nostr.key`, read with the home key's checks.
        /// Nothing is created: no agent key, `config.toml` or `wallet/` (a mint-only home has
        /// none, and making one would leave an unused identity the operator might link instead).
        fn open_mint(root: &std::path::Path, flags: String) -> Result<Self, String> {
            let path = root.join("mint").join("nostr.key");
            if std::fs::symlink_metadata(&path).is_err() {
                return Err(format!(
                    "account refused: no mint key at {} (run `maxplayer-mint init` with \
                     MAXPLAYER_HOME={} first, or check --home)",
                    path.display(),
                    root.display()
                ));
            }
            let secret = home::read_key_file_hex(&path).map_err(|e| e.to_string())?;
            let keys = Keys::parse(&secret)
                .map_err(|_| format!("{} is not a valid key", path.display()))?;
            let npub = keys
                .public_key()
                .to_bech32()
                .map_err(|e| format!("npub encode: {e}"))?;
            let config = home::resolve_config_read_only(root).map_err(|e| e.to_string())?;
            Ok(Self {
                keys,
                npub,
                base_url: config.registration_url,
                whose: "this mint's key",
                flags,
                home: None,
                profile_name: None,
            })
        }

        fn client(&self) -> Result<RegistrationClient, RegistrationError> {
            RegistrationClient::new(&self.base_url, self.keys.clone())
        }
    }

    fn report(ctx: &Context, err: &mut dyn Write, error: &RegistrationError) -> i32 {
        let _ = writeln!(err, "{}", error.message_for(&ctx.command("link")));
        RUNTIME_ERROR
    }

    /// Reads one line; `None` on end of input.
    fn prompt(input: &mut dyn BufRead, out: &mut dyn Write, question: &str) -> Option<String> {
        let _ = write!(out, "{question}");
        let _ = out.flush();
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim().to_owned()),
        }
    }

    fn next_step(ctx: &Context, out: &mut dyn Write) {
        let _ = writeln!(
            out,
            "When you have the code (it expires in 10 minutes), run:\n  {} --code <6 digits>\n(add \
             `--entered-by human` when the human typed or read the code to you, `--entered-by \
             agent` when you read it from the inbox yourself).",
            ctx.command("link")
        );
    }

    fn print_code_sent(out: &mut dyn Write, email: &str, sent: &CodeSent) {
        let minutes = sent.expires_in_secs.div_ceil(60);
        let _ = writeln!(
            out,
            "Check {email} for a 6-digit code (it expires in {minutes} minutes, {} tries).\n\
             Entering it accepts the Maxplayer Terms of Service, effective {TERMS_VERSION}: {}",
            sent.max_attempts, sent.terms_url
        );
    }

    fn print_linked(out: &mut dyn Write, ctx: &Context, linked: &Linked) {
        let _ = writeln!(
            out,
            "Done, linked to {} (agent \"{}\", {}).",
            linked.email, linked.label, ctx.npub
        );
        if !linked.first_link {
            let _ = writeln!(
                out,
                "This key was already linked to this email; the link was refreshed."
            );
        }
        let _ = writeln!(
            out,
            "Terms of Service (effective {}) accepted.",
            linked.terms_version
        );
        let relay = if linked.relay_member == "added" {
            "The relay now accepts writes from this key.".to_owned()
        } else {
            format!(
                "Relay membership queued; `{}` shows when it is added.",
                ctx.command("status")
            )
        };
        let _ = writeln!(out, "{relay}");
        let _ = writeln!(out, "{}", marketing_line(&linked.marketing));
    }

    /// After a confirmed link: publish the label as the agent's kind-0 `name` when it has none.
    /// Best effort: it prints one line and never changes the exit code. A queued membership
    /// publishes nothing now (the relay would refuse the write); the next buyer or seller start
    /// fills the name once the key is a member.
    fn publish_label(ctx: &Context, linked: &Linked, out: &mut dyn Write, err: &mut dyn Write) {
        let Some(home) = &ctx.home else {
            return;
        };
        // A "not linked" answer or a failure backoff remembered by the startup backfill is stale now.
        profile::forget_name_backfill(home);
        let label = &linked.label;
        if linked.relay_member != "added" {
            let _ = writeln!(
                out,
                "\"{label}\" becomes this agent's public name once the relay adds this key (the \
                 next `maxplayer buyer` or seller start sets it)."
            );
            return;
        }
        let set_hint = format!("maxplayer profile set --name \"{label}\"{}", ctx.flags);
        if !profile::registration_pairs_with_relay(
            &home.config.registration_url,
            &home.config.relay_url,
        ) {
            let _ = writeln!(
                err,
                "Did not publish \"{label}\" as this agent's public name: this home writes to {}, \
                 not the relay this link is for. To name it there, run `{set_hint}`.",
                home.config.relay_url
            );
            return;
        }
        let mut home = home.clone();
        match profile::fill_name_from_label(&mut home, label) {
            Ok(NameFill::Published { name, .. }) => {
                let _ = writeln!(out, "Published \"{name}\" as this agent's public name.");
            }
            Ok(NameFill::KeptLocal(name) | NameFill::KeptRemote(name)) => {
                let _ = writeln!(
                    out,
                    "This agent keeps its public name \"{name}\" (change it with `maxplayer \
                     profile set --name <name>{}`).",
                    ctx.flags
                );
            }
            Err(error) => {
                let _ = writeln!(
                    err,
                    "Could not publish \"{label}\" as this agent's public name ({error}). Run \
                     `{set_hint}` to set it."
                );
            }
        }
    }

    fn marketing_line(state: &str) -> String {
        match state {
            "subscribed" => "Product news: on. Turn it off any time with `maxplayer account \
                 marketing off`."
                .to_owned(),
            "unsubscribed" => "Product news: off. `maxplayer account marketing on` turns it back \
                 on."
            .to_owned(),
            _ => "Product news: off.".to_owned(),
        }
    }

    /// `account link`. `interactive` = stdin is a terminal (the prompts are allowed).
    pub fn link(
        ctx: &Context,
        opts: &Opts,
        input: &mut dyn BufRead,
        interactive: bool,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> i32 {
        let client = match ctx.client() {
            Ok(client) => client,
            Err(error) => return report(ctx, err, &error),
        };
        if let Some(code) = &opts.code {
            let who = match opts.entered_by.as_deref() {
                Some(who) => EnteredBy::parse(who).unwrap_or(EnteredBy::Agent),
                None => EnteredBy::Agent,
            };
            return match client.confirm(code, who) {
                Ok(linked) => {
                    print_linked(out, ctx, &linked);
                    publish_label(ctx, &linked, out, err);
                    SUCCESS
                }
                Err(error) => report(ctx, err, &error),
            };
        }

        let missing_email = opts.email.is_none();
        let missing_label = opts.label.is_none();
        if !interactive && (missing_email || missing_label) {
            let _ = writeln!(
                err,
                "account link needs --email and --label when it cannot ask (no terminal). Ask the \
                 human for their email, then run:\n  {} --email <email> --label <a name for this \
                 agent>\n{PUBLIC_NAME_NOTICE}",
                ctx.command("link")
            );
            return USAGE_ERROR;
        }

        let _ = writeln!(out, "Linking {} ({}) to an email.", ctx.whose, ctx.npub);
        // Spec §3 steps 1 and 6: both notices, when the email is asked for or before it is sent.
        let _ = writeln!(out, "{}", registration::privacy_notice());
        let _ = writeln!(out, "{MARKETING_NOTICE}");

        let email = match &opts.email {
            Some(email) => email.trim().to_owned(),
            None => loop {
                match prompt(input, out, "Email: ") {
                    None => return aborted(err),
                    Some(email) if registration::looks_like_email(&email) => break email,
                    Some(_) => {
                        let _ = writeln!(out, "That does not look like an email address.");
                    }
                }
            },
        };
        let label = match &opts.label {
            Some(label) => {
                let _ = writeln!(out, "{PUBLIC_NAME_NOTICE}");
                label.clone()
            }
            None => {
                let _ = writeln!(out, "{PUBLIC_NAME_NOTICE}");
                let question = match &ctx.profile_name {
                    Some(name) => format!("A name for this agent [{name}]: "),
                    None => "A name for this agent (e.g. my-laptop): ".to_owned(),
                };
                loop {
                    match prompt(input, out, &question) {
                        None => return aborted(err),
                        Some(label) if label.is_empty() && ctx.profile_name.is_some() => {
                            break ctx.profile_name.clone().unwrap_or_default();
                        }
                        Some(label) if registration::normalize_label(&label).is_some() => {
                            break label;
                        }
                        Some(_) => {
                            let _ =
                                writeln!(out, "Use 1-64 letters, digits, spaces, '.', '_' or '-'.");
                        }
                    }
                }
            }
        };

        let sent = match client.request_code(&email, &label) {
            Ok(sent) => sent,
            Err(error) => return report(ctx, err, &error),
        };
        print_code_sent(out, &email, &sent);
        if !interactive {
            next_step(ctx, out);
            return SUCCESS;
        }

        loop {
            let Some(code) = prompt(input, out, "Code from the email: ") else {
                let _ = writeln!(out);
                next_step(ctx, out);
                return RUNTIME_ERROR;
            };
            if code.is_empty() {
                next_step(ctx, out);
                return RUNTIME_ERROR;
            }
            if registration::normalize_code(&code).is_none() {
                // Caught here, so a typo never spends one of the code's tries.
                let _ = writeln!(out, "A code is the 6 digits from the email.");
                continue;
            }
            match client.confirm(&code, EnteredBy::Human) {
                Ok(linked) => {
                    print_linked(out, ctx, &linked);
                    publish_label(ctx, &linked, out, err);
                    return SUCCESS;
                }
                Err(error) if error.code() == Some("wrong_code") => {
                    let _ = writeln!(out, "{error}");
                }
                Err(error) => return report(ctx, err, &error),
            }
        }
    }

    fn aborted(err: &mut dyn Write) -> i32 {
        let _ = writeln!(err, "account link cancelled; nothing was sent.");
        RUNTIME_ERROR
    }

    pub fn status(ctx: &Context, json: bool, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
        let status = match ctx.client().and_then(|client| client.status()) {
            Ok(status) => status,
            Err(error) if json => {
                // An agent parsing `--json` gets a structured failure on stdout, not stderr text.
                json_error(
                    out,
                    error.json_code(),
                    &error.message_for(&ctx.command("link")),
                );
                return RUNTIME_ERROR;
            }
            Err(error) => return report(ctx, err, &error),
        };
        if json {
            let mut value = serde_json::to_value(&status).unwrap_or_default();
            value["npub"] = serde_json::json!(ctx.npub);
            value["pubkey"] = serde_json::json!(ctx.keys.public_key().to_hex());
            let _ = writeln!(out, "{value}");
            return SUCCESS;
        }
        if !status.registered {
            let _ = writeln!(
                out,
                "Not linked: {} ({}) is not linked to an email, so the relay may refuse its \
                 writes.\nRun `{}` to link it.",
                ctx.whose,
                ctx.npub,
                ctx.command("link")
            );
            return SUCCESS;
        }
        let field = |v: &Option<String>| v.clone().unwrap_or_else(|| "?".to_owned());
        let _ = writeln!(
            out,
            "Linked: {} ({})\n  email:     {}\n  label:     {}\n  terms:     {} (accepted {})\n  \
             privacy:   {}\n  linked at: {}\n  relay:     {}\n  {}",
            ctx.whose,
            ctx.npub,
            field(&status.email),
            field(&status.label),
            field(&status.terms_version),
            field(&status.accepted_at),
            field(&status.privacy_version),
            field(&status.linked_at),
            match status.relay_member.as_deref() {
                Some("added") => "member (writes accepted)",
                Some("pending") => "membership queued; this command shows when it is added",
                _ => "membership state unknown",
            },
            marketing_line(status.marketing.as_deref().unwrap_or("none")),
        );
        SUCCESS
    }

    pub fn marketing(ctx: &Context, on: bool, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
        match ctx.client().and_then(|client| client.set_marketing(on)) {
            Ok(answer) => {
                let _ = writeln!(out, "{}", marketing_line(&answer.marketing));
                if answer.marketing == "unsubscribed" {
                    let _ = writeln!(
                        out,
                        "Account emails (codes, terms changes, service notices) still arrive."
                    );
                }
                SUCCESS
            }
            Err(error) => report(ctx, err, &error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn help_needs_no_home_or_network() {
        for list in [
            &["--help"][..],
            &["link", "--help"],
            &["marketing", "--help"],
        ] {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            assert_eq!(run(&args(list), &mut out, &mut err), SUCCESS);
            let text = String::from_utf8(out).unwrap();
            assert!(text.contains("maxplayer account link"), "{text}");
            assert!(text.contains("marketing on|off"), "{text}");
        }
    }

    #[test]
    fn refused_combinations_are_usage_errors() {
        for list in [
            &["link", "--code", "123456", "--email", "a@b.co"][..],
            &["link", "--entered-by", "human"],
            &["link", "--code", "123456", "--entered-by", "robot"],
            &["link", "--accept-terms"],
            &["marketing"],
            &["marketing", "maybe"],
            &["status", "--email", "a@b.co"],
            &["unlink"],
            &[],
        ] {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            assert_eq!(
                run(&args(list), &mut out, &mut err),
                USAGE_ERROR,
                "{list:?}"
            );
        }
    }

    #[test]
    fn parse_reads_every_link_flag() {
        let opts = parse(
            &args(&[
                "--email", "a@b.co", "--label", "x", "--home", "/h", "--mint",
            ]),
            LINK_FLAGS,
        )
        .unwrap();
        assert_eq!(opts.email.as_deref(), Some("a@b.co"));
        assert_eq!(opts.label.as_deref(), Some("x"));
        assert_eq!(opts.home, Some(PathBuf::from("/h")));
        assert!(opts.mint);
        assert!(parse(&args(&["--email"]), LINK_FLAGS).is_err());
    }
}

/// Interactive-path tests: the binary tests run without a terminal, so the prompt loop is driven
/// here with a scripted stdin against the fake registration service.
#[cfg(all(test, feature = "wallet"))]
#[path = "../tests/support/fake_registration.rs"]
mod fake_registration;

#[cfg(all(test, feature = "wallet"))]
mod interactive_tests {
    use super::fake_registration::FakeRegistration;
    use super::wallet::{Context, link};
    use super::*;
    use nostr_sdk::prelude::{Keys, ToBech32};

    fn ctx(fake: &FakeRegistration, keys: &Keys) -> Context {
        Context {
            keys: keys.clone(),
            npub: keys.public_key().to_bech32().unwrap(),
            base_url: fake.base_url(),
            whose: "this agent's key",
            flags: String::new(),
            home: None,
            profile_name: None,
        }
    }

    fn drive(
        fake: &FakeRegistration,
        keys: &Keys,
        opts: Opts,
        stdin: &str,
    ) -> (i32, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = link(
            &ctx(fake, keys),
            &opts,
            &mut std::io::Cursor::new(stdin.as_bytes().to_vec()),
            true,
            &mut out,
            &mut err,
        );
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn interactive_link_asks_shows_notices_and_retries_a_wrong_code() {
        let fake = FakeRegistration::start();
        let keys = Keys::generate();
        // The fake hands out a known code; a malformed entry and a wrong code come first.
        fake.set_next_code("482913");
        let stdin = "not-an-email\nyou@example.com\n<bad>\nmy laptop\n12\n111111\n482 913\n";
        let (code, out, err) = drive(&fake, &keys, Opts::default(), stdin);
        assert_eq!(code, SUCCESS, "{out}\n{err}");
        assert!(
            out.contains("https://www.maxplayer.ai/privacy/20261029"),
            "{out}"
        );
        assert!(out.contains("maxplayer account marketing off"), "{out}");
        assert!(
            out.contains("accepts the Maxplayer Terms of Service, effective 20261029"),
            "{out}"
        );
        assert!(
            out.contains("https://www.maxplayer.ai/terms/20261029"),
            "{out}"
        );
        assert!(
            out.contains("That does not look like an email address."),
            "{out}"
        );
        assert!(out.contains("A code is the 6 digits"), "{out}");
        assert!(
            out.contains("That code is not right. 4 tries left."),
            "{out}"
        );
        assert!(out.contains("Done, linked to you@example.com"), "{out}");
        assert!(out.contains("Product news: on."), "{out}");
        // The notices come before the email is asked for.
        assert!(out.find("Privacy Policy").unwrap() < out.find("Email: ").unwrap());
        let reg = fake.registration(&keys.public_key().to_hex()).unwrap();
        assert_eq!(reg.label, "my laptop");
        assert_eq!(reg.entered_by, "human");
        // Only the real wrong code reached the service: the "12" typo spent no try.
        assert_eq!(fake.confirm_calls(), 2);
    }

    /// The label prompt warns the name is public, offers `[profile].name` as the default, and
    /// Enter accepts it.
    #[test]
    fn interactive_label_prompt_warns_and_accepts_the_profile_name_default() {
        let fake = FakeRegistration::start();
        let keys = Keys::generate();
        fake.set_next_code("482913");
        let mut context = ctx(&fake, &keys);
        context.profile_name = Some("ops-box".into());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let opts = Opts {
            email: Some("you@example.com".into()),
            ..Opts::default()
        };
        let code = link(
            &context,
            &opts,
            &mut std::io::Cursor::new(b"\n482913\n".to_vec()),
            true,
            &mut out,
            &mut err,
        );
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, SUCCESS, "{out}");
        let warning = out
            .find("This name is public on Maxplayer (shown on your profile and in the welcome email).")
            .expect("public-name warning");
        let prompt = out
            .find("A name for this agent [ops-box]: ")
            .expect("default offered");
        assert!(warning < prompt, "{out}");
        let reg = fake.registration(&keys.public_key().to_hex()).unwrap();
        assert_eq!(reg.label, "ops-box");
    }

    /// Without a usable `[profile].name` there is no default: an empty answer is asked again.
    #[test]
    fn interactive_label_prompt_without_a_default_asks_again_on_enter() {
        let fake = FakeRegistration::start();
        let keys = Keys::generate();
        fake.set_next_code("482913");
        let opts = Opts {
            email: Some("you@example.com".into()),
            ..Opts::default()
        };
        let (code, out, _) = drive(&fake, &keys, opts, "\nbox\n482913\n");
        assert_eq!(code, SUCCESS, "{out}");
        assert!(
            out.contains("A name for this agent (e.g. my-laptop): "),
            "{out}"
        );
        assert!(out.contains("This name is public on Maxplayer"), "{out}");
        assert_eq!(
            fake.registration(&keys.public_key().to_hex())
                .unwrap()
                .label,
            "box"
        );
    }

    #[test]
    fn interactive_end_of_input_at_the_code_prompt_says_how_to_finish() {
        let fake = FakeRegistration::start();
        let keys = Keys::generate();
        let opts = Opts {
            email: Some("you@example.com".into()),
            label: Some("box".into()),
            ..Opts::default()
        };
        let (code, out, _) = drive(&fake, &keys, opts, "");
        assert_eq!(code, RUNTIME_ERROR);
        assert!(
            out.contains("maxplayer account link --code <6 digits>"),
            "{out}"
        );
        assert!(fake.registration(&keys.public_key().to_hex()).is_none());
    }

    #[test]
    fn interactive_too_many_wrong_codes_stops_with_a_plain_message() {
        let fake = FakeRegistration::start();
        let keys = Keys::generate();
        fake.set_next_code("482913");
        let opts = Opts {
            email: Some("you@example.com".into()),
            label: Some("box".into()),
            ..Opts::default()
        };
        let (code, out, err) = drive(
            &fake,
            &keys,
            opts,
            "111111\n222222\n333333\n444444\n555555\n482913\n",
        );
        assert_eq!(code, RUNTIME_ERROR, "{out}");
        assert!(out.contains("1 try left."), "{out}");
        assert!(err.contains("Too many wrong tries"), "{err}");
        assert!(fake.registration(&keys.public_key().to_hex()).is_none());
    }
}
