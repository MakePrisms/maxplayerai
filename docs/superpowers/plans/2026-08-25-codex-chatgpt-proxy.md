# Codex ChatGPT Proxy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let Docker Codex use a host ChatGPT session without access or refresh tokens in the container.

**Architecture:** Read the host session for each run. Route placeholder headers through a typed per-job proxy route.

**Tech Stack:** Rust, Tokio, Hyper, Reqwest, Serde, Docker, and `codex-acp`.

**Spec:** `docs/superpowers/specs/2026-08-25-codex-chatgpt-proxy.md`

## Global Constraints

- Keep the existing Claude seller unchanged.

- Keep all current credential paths unchanged when the new table is absent.

- Never print, log, commit, or pass a real token on a command line.

- Never mount `auth.json` into Docker.

- Never replace data in a request body or URL.

- Read only `tokens.access_token` and `tokens.account_id` into typed Rust fields.

- Do not implement token refresh in this version.

- Use a failing test before each production change.

- Use the feature union for tests that cover the Docker run path.

---

## Task 1: Add the Configuration and Session Reader

**Files:**

- Create: `crates/maxplayer-core/src/codex_subscription.rs`

- Modify: `crates/maxplayer-core/src/lib.rs`

- Modify: `crates/maxplayer-core/src/home.rs`

- Modify: `crates/maxplayer-core/tests/sandbox_netns_live.rs`

- Test: `crates/maxplayer-core/src/codex_subscription.rs`

- Test: `crates/maxplayer-core/src/home.rs`

**Interfaces:**

```rust
pub struct CodexChatgptConfig {
    pub auth_file: PathBuf,
}

pub struct ChatgptSession;

pub fn read_chatgpt_session(
    auth_file: &Path,
    required_lifetime: Duration,
    now: SystemTime,
) -> Result<ChatgptSession, SessionError>;

pub fn gateway_auth_request_json(
    proxy_url: &str,
    access_placeholder: &str,
    account_placeholder: &str,
) -> String;
```

### Steps

- [x] Add a config parse test for `[sandbox.codex_chatgpt]`.

- [x] Add tests for a valid synthetic JWT, a short lifetime, and malformed data.

- [x] Add a Unix test that refuses group or world access.

- [x] Add a test that confirms the gateway auth JSON contains placeholders only.

- [x] Run the new tests before implementation.

Use this command:

```bash
cargo test -p maxplayer-core --features wallet,acp codex_subscription
```

Expected result: compilation fails because the module and fields do not exist.

- [x] Add `CodexChatgptConfig` to `SandboxConfig` as an optional field.

- [x] Add the gated `codex_subscription` module.

- [x] Parse the two required token fields with typed Serde structures.

- [x] Decode the JWT `exp` claim with URL-safe Base64.

- [x] Require the job lifetime plus the 15-minute margin.

- [x] Check the auth file mode on Unix.

- [x] Build the default gateway auth JSON with `serde_json`.

- [x] Run the new tests again.

Use this command:

```bash
cargo test -p maxplayer-core --features wallet,acp codex_subscription
```

Expected result: all matching tests pass.

- [x] Commit the task.

```bash
git add crates/maxplayer-core/src/codex_subscription.rs crates/maxplayer-core/src/lib.rs crates/maxplayer-core/src/home.rs crates/maxplayer-core/tests/sandbox_netns_live.rs
git commit -m "feat: read Codex ChatGPT sessions on the host"
```

---

## Task 2: Add the Typed Proxy Route

**Files:**

- Modify: `crates/maxplayer-core/src/credential_proxy.rs`

- Test: `crates/maxplayer-core/src/credential_proxy.rs`

**Interfaces:**

```rust
pub struct CodexSessionCredential {
    pub access_placeholder: String,
    pub access_token: String,
    pub account_placeholder: String,
    pub account_id: String,
    pub upstream: String,
}

impl ProxyEngine {
    pub fn register_codex_session(
        &self,
        credential: CodexSessionCredential,
    ) -> Result<(), Refusal>;

    pub fn authorize_request(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
    ) -> Decision;
}
```

### Steps

- [x] Add a unit test that requires both exact Codex placeholder headers.

- [x] Add tests that refuse a wrong method, path, header, or destination.

- [x] Add a live proxy test with a local upstream.

- [x] Assert that the upstream gets both real headers.

- [x] Assert that the upstream gets the original body byte for byte.

- [x] Assert that each response replaces both host values with placeholders.

- [x] Run the new proxy tests before implementation.

Use this command:

```bash
cargo test -p maxplayer-core --features wallet codex_session
```

Expected result: compilation fails because the typed route does not exist.

- [x] Add the typed session registry beside the current registry.

- [x] Keep the generic `authorize` behavior unchanged.

- [x] Add `authorize_request` for the transport path.

- [x] Restrict the typed route to the three approved requests.

- [x] Replace only the two exact request headers.

- [x] Carry all response scrub pairs in the forward decision.

- [x] Extend the stream scrub to support both values across chunk boundaries.

- [x] Keep request bodies and URLs outside the decision.

- [x] Run the new proxy tests again.

Use this command:

```bash
cargo test -p maxplayer-core --features wallet codex_session
```

Expected result: all matching tests pass.

- [x] Run the current proxy tests.

Use this command:

```bash
cargo test -p maxplayer-core --features wallet credential_proxy
```

Expected result: all matching tests pass.

- [x] Commit the task.

```bash
git add crates/maxplayer-core/src/credential_proxy.rs
git commit -m "feat: add a typed Codex session proxy route"
```

---

## Task 3: Connect the Host Session to Docker Codex

**Files:**

- Modify: `crates/maxplayer-core/src/seller_exec.rs`

- Test: `crates/maxplayer-core/src/seller_exec.rs`

**Interfaces:**

```rust
impl SandboxPolicy {
    pub fn codex_chatgpt(&self) -> Option<&CodexChatgptConfig>;
}
```

### Steps

- [x] Add a policy test that refuses a relative auth path.

- [x] Add a launch test for the built-in `codex-acp` command.

- [x] Assert that Docker gets `DEFAULT_AUTH_REQUEST`.

- [x] Assert that Docker gets no `CODEX_CONFIG` or `MODEL_PROVIDER`.

- [x] Assert that Docker gets no host access token.

- [x] Assert that Docker gets no host account ID.

- [x] Assert that Docker gets no `OPENAI_API_KEY` or `OPENAI_BASE_URL`.

- [x] Add a regression test that the table does not activate for a Docker Claude command.

- [x] Run the new launch tests before implementation.

Use this command:

```bash
cargo test -p maxplayer-core --features acp,gateway,git-delivery,wallet codex_chatgpt
```

Expected result: compilation fails because the policy field and launch path do not exist.

- [x] Carry the optional config on the Docker-only sandbox policy.

- [x] Activate the mode only for a `codex-acp` command.

- [x] Remove ambient OpenAI auth variables from the Codex container view.

- [x] Read and validate the session before the proxy starts.

- [x] Pass the active job timeout to the lifetime check.

- [x] Register the typed session route.

- [x] Add the placeholder gateway request to the container environment.

- [x] Keep the proxy guard alive until the run ends.

- [x] Run the new launch tests again.

Use this command:

```bash
cargo test -p maxplayer-core --features acp,gateway,git-delivery,wallet codex_chatgpt
```

Expected result: all matching tests pass.

- [x] Run the existing seller execution tests.

Use this command:

```bash
cargo test -p maxplayer-core --features acp,gateway,git-delivery,wallet seller_exec
```

Expected result: all matching tests pass.

- [x] Commit the task.

```bash
git add crates/maxplayer-core/src/seller_exec.rs
git commit -m "feat: route Docker Codex through ChatGPT auth"
```

---

## Task 4: Document the Controlled Seller

**Files:**

- Modify: `docs/DOCKER.md`

- Modify: `docs/SELLER-QUICKSTART.md`

- Modify: `docs/superpowers/specs/2026-08-25-codex-chatgpt-proxy.md`

- Modify: `docs/superpowers/plans/2026-08-25-codex-chatgpt-proxy.md`

### Steps

- [x] Add the ChatGPT session table to the Docker guide.

- [x] State that the host auth file never enters Docker.

- [x] State the access token lifetime rule.

- [x] State that version one does not refresh a token.

- [x] Add a targeted seller example with a separate seller home.

- [x] Add a dedicated Codex auth directory to the example.

- [x] Add one slot and a buyer public key allowlist to the example.

- [x] Add a dedicated Docker network and proxy port range.

- [x] Record the later host refresh phase as a fixed design item.

- [x] Review each command for secret values on the command line.

- [x] Commit the task.

```bash
git add docs/DOCKER.md docs/SELLER-QUICKSTART.md docs/superpowers
git commit -m "docs: add the Codex ChatGPT Docker setup"
```

---

## Task 5: Verify the Complete Change

**Files:**

- Verify all changed files.

### Steps

- [x] Run the formatter check.

```bash
cargo fmt --all --check
```

Actual result: the current formatter reports repository-wide baseline differences. It changes no files.

The standalone new module passes `rustfmt --edition 2024 --check`.

- [x] Run the default core tests.

```bash
cargo test -p maxplayer-core
```

Expected result: all tests pass.

- [x] Run the offline money tests.

```bash
cargo test -p maxplayer-core --features wallet
```

Expected result: all tests pass.

- [x] Run the feature-union tests.

```bash
cargo test -p maxplayer-core --features acp,gateway,git-delivery,wallet
```

Expected result: all tests pass and the Codex tests run.

- [x] Build the release feature set.

```bash
cargo build -p maxplayer --release --no-default-features --features wallet,acp
```

Expected result: exit code zero.

- [x] Inspect the diff for a token, auth file, seller key, or wallet file.

```bash
git status --short
git diff --check HEAD~4..HEAD
git diff --stat HEAD~4..HEAD
```

Expected result: only source and documentation files appear.

- [x] Confirm the source worktree still has no new tracked change.

```bash
git -C "$SOURCE_WORKTREE" status --short
```

Expected result: only the user files that existed before this work appear.

## Plan Review

- [x] Confirm each production behavior has a failing test step.

- [x] Confirm every task has complete steps and exact values.

- [x] Confirm all interface names agree across tasks.

- [x] Confirm the later host refresh stays outside this version.

- [x] Confirm the existing Claude path stays outside all new branches.

---

## Task 6: Correct the `codex-acp` Auth Gate After the Live Probe

### Steps

- [x] Run the controlled seller pre-advertise probe against the real ChatGPT backend.

- [x] Confirm that `CODEX_CONFIG` and `MODEL_PROVIDER` reach the adapter auth gate too late.

- [x] Reproduce the adapter path with a placeholder-only `DEFAULT_AUTH_REQUEST`.

- [x] Add a failing regression test for the adapter gateway request.

- [x] Replace the old provider variables with `DEFAULT_AUTH_REQUEST`.

- [x] Rebuild the release binary.

- [x] Run the controlled seller pre-advertise probe again.

Actual result: the capability probe passed, and the targeted seller advertised one Codex slot.
