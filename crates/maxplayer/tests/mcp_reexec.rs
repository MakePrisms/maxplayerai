//! An upgrade that replaces the binary under a running `maxplayer mcp` is picked up by the next
//! request, with nobody restarting the client: the server execs the new file in place (same pid,
//! same stdio) and the request it had just read, plus one pipelined behind it, are answered there.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

fn read_response(lines: &mut impl BufRead) -> Value {
    let mut line = String::new();
    let read = lines.read_line(&mut line).expect("read MCP response");
    assert!(read > 0, "the MCP server closed stdout");
    serde_json::from_str(line.trim())
        .unwrap_or_else(|error| panic!("bad response {line:?}: {error}"))
}

fn install(path: &Path, body: &[u8]) {
    let next = path.with_extension("next");
    std::fs::write(&next, body).unwrap();
    std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&next, path).unwrap(); // what npm, install.sh and package managers do
}

/// RED ON REVERT: without the re-exec, the swapped-in binary is never run with `mcp --resume-fd`
/// and the requests after the swap are answered by the old process.
#[test]
fn a_replaced_binary_serves_the_next_request_without_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let real = bin.join("maxplayer-real");
    std::fs::copy(env!("CARGO_BIN_EXE_maxplayer"), &real).unwrap();
    let exe = bin.join("maxplayer");
    std::fs::copy(&real, &exe).unwrap();
    let invoked = dir.path().join("invoked");

    let mut command = Command::new(&exe);
    command.arg("mcp");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("MAXPLAYER_") {
            command.env_remove(key);
        }
    }
    let stderr = std::fs::File::create(dir.path().join("stderr")).unwrap();
    let mut child = command
        .env("MAXPLAYER_HOME", dir.path().join("home"))
        .env("MAXPLAYER_RELAY_URL", "ws://127.0.0.1:1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .expect("spawn maxplayer mcp");
    let pid = child.id();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"t","version":"0"}}}}}}"#).unwrap();
    let first = read_response(&mut stdout);
    assert_eq!(first["id"], 1, "{first}");
    assert!(!invoked.exists());

    // The upgrade: a new file at the same path. It records how it is run, then is the real binary.
    let script = format!(
        "#!/bin/sh\necho \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
        invoked.display(),
        real.display()
    );
    install(&exe, script.as_bytes());

    // Two requests in one write: the second is buffered behind the first when the swap is seen.
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n\
              {\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/list\"}\n",
        )
        .unwrap();
    stdin.flush().unwrap();
    for id in [2, 3] {
        let response = read_response(&mut stdout);
        assert_eq!(response["id"], id, "{response}");
        assert!(
            response["result"]["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty()),
            "{response}"
        );
    }

    let runs = std::fs::read_to_string(&invoked).unwrap_or_default();
    assert!(
        runs.lines().any(|line| line == "--version"),
        "the new file was probed first: {runs:?}"
    );
    assert!(
        runs.lines()
            .any(|line| line.starts_with("mcp --resume-fd ")),
        "the server re-exec'd the replaced binary: {runs:?}"
    );
    assert_eq!(child.id(), pid, "same process, same stdio");

    // Still one continuous server: a later request is answered, and EOF ends it cleanly.
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/list\"}\n")
        .unwrap();
    assert_eq!(read_response(&mut stdout)["id"], 4);
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "mcp did not exit on EOF");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{status}");
    let log = std::fs::read_to_string(dir.path().join("stderr")).unwrap();
    assert!(log.contains("binary replaced"), "{log}");
    assert_eq!(
        runs.lines().filter(|line| line.starts_with("mcp ")).count(),
        1,
        "exactly one re-exec: {runs:?}"
    );
}
