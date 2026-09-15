//! `vendor-cli` — the third-party tool being onboarded.
//!
//! Written to behave like a real vendor CLI, including the property the contract depends on:
//! it keeps its own login in its own home (`$VENDOR_CLI_HOME/auth.json`) and writes nothing
//! about authentication anywhere else. That separability is what makes a persistent holder
//! possible; a tool that stored its session next to its working files could not be held this way.
//!
//! It never prints the credential or the session token, and it never accepts either as an
//! argument — `login` reads a credential file.

use maxplayer_tool_kit::http;
use maxplayer_tool_kit::Secret;
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;

const EXIT_USAGE: i32 = 2;
const EXIT_AUTH: i32 = 3;
const EXIT_VENDOR: i32 = 4;
const EXIT_IO: i32 = 5;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("");

    let home = match std::env::var("VENDOR_CLI_HOME") {
        Ok(h) if !h.is_empty() => PathBuf::from(h),
        _ => {
            eprintln!("vendor-cli: VENDOR_CLI_HOME must be set");
            std::process::exit(EXIT_USAGE);
        }
    };
    let base_url = std::env::var("VENDOR_CLI_BASE_URL").unwrap_or_default();

    match cmd {
        "login" => {
            let Some(cred_file) = flag(&args, "--credential-file") else {
                eprintln!("vendor-cli login: --credential-file <path> is required");
                std::process::exit(EXIT_USAGE);
            };
            login(&home, &base_url, &cred_file)
        }
        "health" => health(&home, &base_url),
        "whoami" => whoami(&home),
        "transform" => transform(&home, &base_url, &args),
        "logout" => {
            let path = auth_path(&home);
            match std::fs::remove_file(&path) {
                Ok(()) => println!("logged out"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("not enrolled"),
                Err(e) => {
                    eprintln!("vendor-cli logout: {e}");
                    std::process::exit(EXIT_IO);
                }
            }
        }
        _ => {
            eprintln!("usage: vendor-cli <login|health|whoami|transform|logout> [flags]");
            std::process::exit(EXIT_USAGE);
        }
    }
}

fn auth_path(home: &std::path::Path) -> PathBuf {
    home.join("auth.json")
}

fn load_token(home: &std::path::Path) -> Option<Secret> {
    let raw = std::fs::read_to_string(auth_path(home)).ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    let token = parsed["session_token"].as_str()?.to_string();
    if token.is_empty() {
        None
    } else {
        Some(Secret::new(token))
    }
}

fn login(home: &std::path::Path, base_url: &str, cred_file: &str) {
    let raw = match std::fs::read_to_string(cred_file) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("vendor-cli login: cannot read credential file: {e}");
            std::process::exit(EXIT_IO);
        }
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("vendor-cli login: credential file is not JSON: {e}");
            std::process::exit(EXIT_IO);
        }
    };

    let body = json!({
        "client_id": parsed["client_id"].as_str().unwrap_or_default(),
        "client_secret": parsed["client_secret"].as_str().unwrap_or_default(),
    })
    .to_string();

    let resp = match http::request(base_url, "POST", "/login", None, Some(body.as_bytes())) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("vendor-cli login: vendor unreachable: {e}");
            std::process::exit(EXIT_VENDOR);
        }
    };
    if resp.status == 401 {
        eprintln!("vendor-cli login: vendor rejected the credential");
        std::process::exit(EXIT_AUTH);
    }
    if resp.status != 200 {
        eprintln!("vendor-cli login: vendor returned {}", resp.status);
        std::process::exit(EXIT_VENDOR);
    }

    let parsed: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
    let Some(token) = parsed["session_token"].as_str() else {
        eprintln!("vendor-cli login: vendor response carried no session token");
        std::process::exit(EXIT_VENDOR);
    };

    if let Err(e) = std::fs::create_dir_all(home) {
        eprintln!("vendor-cli login: cannot create home: {e}");
        std::process::exit(EXIT_IO);
    }
    let path = auth_path(home);
    if let Err(e) = write_private(&path, json!({"session_token": token}).to_string().as_bytes()) {
        eprintln!("vendor-cli login: cannot persist session: {e}");
        std::process::exit(EXIT_IO);
    }
    // Deliberately no token in the output.
    println!("enrolled; session persisted to {}", path.display());
}

/// Write 0600, creating with restrictive mode rather than relaxing it afterwards.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.flush()
}

fn health(home: &std::path::Path, base_url: &str) {
    let Some(token) = load_token(home) else {
        eprintln!("vendor-cli health: not enrolled");
        std::process::exit(EXIT_AUTH);
    };
    match http::request(base_url, "GET", "/health", Some(token.expose()), None) {
        Ok(r) if r.status == 200 => println!("ok"),
        Ok(r) if r.status == 401 => {
            eprintln!("vendor-cli health: vendor rejected the stored session");
            std::process::exit(EXIT_AUTH);
        }
        Ok(r) => {
            eprintln!("vendor-cli health: vendor returned {}", r.status);
            std::process::exit(EXIT_VENDOR);
        }
        Err(e) => {
            eprintln!("vendor-cli health: vendor unreachable: {e}");
            std::process::exit(EXIT_VENDOR);
        }
    }
}

fn whoami(home: &std::path::Path) {
    // Reports enrollment without revealing anything about the session.
    if load_token(home).is_some() {
        println!("enrolled");
    } else {
        println!("not enrolled");
        std::process::exit(EXIT_AUTH);
    }
}

fn transform(home: &std::path::Path, base_url: &str, args: &[String]) {
    let Some(input) = flag(args, "--in") else {
        eprintln!("vendor-cli transform: --in <file> is required");
        std::process::exit(EXIT_USAGE);
    };
    let Some(output) = flag(args, "--out") else {
        eprintln!("vendor-cli transform: --out <file> is required");
        std::process::exit(EXIT_USAGE);
    };
    let mode = flag(args, "--mode").unwrap_or_else(|| "upper".to_string());

    let Some(token) = load_token(home) else {
        eprintln!("vendor-cli transform: not enrolled");
        std::process::exit(EXIT_AUTH);
    };
    let text = match std::fs::read_to_string(&input) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("vendor-cli transform: cannot read input: {e}");
            std::process::exit(EXIT_IO);
        }
    };

    let body = json!({"text": text, "mode": mode}).to_string();
    let resp = match http::request(base_url, "POST", "/v1/transform", Some(token.expose()), Some(body.as_bytes())) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("vendor-cli transform: vendor unreachable: {e}");
            std::process::exit(EXIT_VENDOR);
        }
    };
    if resp.status == 401 {
        eprintln!("vendor-cli transform: vendor rejected the stored session");
        std::process::exit(EXIT_AUTH);
    }
    if resp.status != 200 {
        eprintln!("vendor-cli transform: vendor returned {}", resp.status);
        std::process::exit(EXIT_VENDOR);
    }
    let parsed: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
    let Some(result) = parsed["result"].as_str() else {
        eprintln!("vendor-cli transform: vendor response carried no result");
        std::process::exit(EXIT_VENDOR);
    };
    if let Err(e) = std::fs::write(&output, result.as_bytes()) {
        eprintln!("vendor-cli transform: cannot write output: {e}");
        std::process::exit(EXIT_IO);
    }
    println!("wrote {} bytes to {}", result.len(), output);
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}
