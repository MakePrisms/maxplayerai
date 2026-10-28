# maxplayer-registration

The email registration service from the email-signup spec (v2.12). It links an agent's Nostr
key (npub) to a human's email with a 6-digit code, records the terms and privacy versions
accepted and the product-news setting, and tells the relay (NIP-43 kind 9030) that the key
may write. Emails stay in this service's own Postgres database. The relay only ever sees npubs.

It is its own cargo workspace (like `maxplayer-mint` and `maxplayer-challenges`), so axum and
sqlx never reach the root build or the `maxplayer` binary. On the relay box it is part of
`nixosConfigurations.relay` (see [Deployment](#deployment)).

```sh
cargo build --release --locked --manifest-path crates/maxplayer-registration/Cargo.toml
```

## Endpoints

All live under `/account/`, behind the relay box's nginx. NIP-98 requests must sign the
service's **public** URL, e.g. `https://relay.maxplayer.ai/account/link/request`. A request with
a body must also carry a `payload` tag (SHA-256 of the exact body), and each NIP-98 event is
accepted once.

| Method | Path | Auth | Body → answer |
|---|---|---|---|
| POST | `/account/link/request` | NIP-98 | `{email, label, terms_version, privacy_version, notice_version?}` → `202 {status:"code_sent", expires_in_secs, max_attempts, terms_url, privacy_url}` |
| POST | `/account/link/confirm` | NIP-98 | `{code, entered_by?: "human"\|"agent"}` → `200 {status:"linked", first_link, email, label, terms_version, privacy_version, relay_member:"added"\|"pending", marketing}` |
| GET | `/account/status` | NIP-98 | → `{registered:false}` or `{registered:true, email, label, terms_version, privacy_version, accepted_at, code_entered_by, linked_at, marketing, relay_member}` |
| POST | `/account/marketing` | NIP-98 | `{enabled: bool, notice_version?}` → `{marketing:"subscribed"\|"unsubscribed"}` (for `maxplayer account marketing on\|off`) |
| POST | `/account/webhooks/resend` | Svix signature | Resend events: `email.bounced`, `email.complained`, `contact.updated` (unsubscribed) |
| GET | `/account/health` | none | `{ok:true}` after a database ping |

Errors are `{"error": <stable code>, "message": <text>}` and never contain an email address.
The codes are: `missing_auth`, `malformed_auth`, `wrong_kind`, `bad_signature`, `stale_auth`,
`url_mismatch`, `method_mismatch`, `payload_missing`, `payload_mismatch`, `replayed_auth`,
`invalid_body`, `invalid_email`, `invalid_label`, `unknown_terms_version`,
`unknown_privacy_version`, `unknown_notice_version`, `invalid_entered_by`, `resend_cooldown`,
`rate_limited` (both with `Retry-After`), `email_send_failed`, `no_pending_code`, `wrong_code`
(with `attempts_left`), `code_expired`, `too_many_attempts`, `key_linked_to_other_email`,
`not_registered`, `bad_webhook_signature`, `webhook_not_configured`.

### Rules

- Codes have 6 digits, expire after 10 minutes, and allow 5 tries. Only an HMAC-SHA256 of the
  code is stored, keyed with a pepper derived from the relay admin key and bound to the npub, so
  a database dump does not reveal codes. A new request replaces any live code for that key.
- Sends: one code email per key every 60 s, and per hour at most 5 per key, 5 per email and
  20 per IP. Confirms: 30 per key and 60 per IP per hour. All of these are configurable.
- Entering an email never reveals whether it is already registered. One email can link many
  keys, one row per key. A key that is already linked to a different email gets
  `409 key_linked_to_other_email`, because v1 has no "change email" (spec §4).
- Re-linking the same key to the same email is idempotent. The label, versions and
  `accepted_at` are updated and `created_at` is kept. A 9030 is sent again (the relay treats it
  as a no-op), and no second welcome email goes out.
- On the first link, a `marketing_consents` row is created (`signup_default`, automatic
  opt-in, spec §8.1) unless that email already has one, so an earlier opt-out survives.

## Relay membership (NIP-43)

On a successful confirm, the service queues and sends a kind **9030**, signed by the relay
admin key with a fresh `created_at` (the relay accepts ±120 s):

```json
{"kind": 9030, "content": "", "tags": [["-"], ["p", "<agent pubkey hex>"]]}
```

It goes to `POST <relay.base_url>/events`, NIP-98 signed for `<relay.auth_base_url>/events`.
The relay must already list the admin key as `admin` or `owner`. Failed sends stay in
`member_sync_outbox` and are retried every 30 s with backoff up to an hour, re-signed each time.
A reconcile every 5 minutes reads the relay's kind-13534 member list. It queues adds for
registered keys that are missing from the list, and removes (**9031**) for `member`-role keys
that this service added but that have no registration row any more.

**Operator removal (ban):** either

```sh
maxplayer-registration remove <npub|hex> --config /etc/maxplayer-registration/config.toml
```

which deletes the row and sends the 9031 at once, or delete the row with SQL
(`DELETE FROM registrations WHERE pubkey = '<hex>';`) and the next reconcile sends the 9031.
The reconcile never removes admins, owners, the admin key, the relay key or
`sync.keep_members`. It removes keys it didn't add only when `sync.reconcile_removals = true`
(off by default, so keys added by hand before signup existed are kept).

`maxplayer-registration sync` runs one outbox pass and one reconcile, then exits.

## Configuration

One TOML file (default `/etc/maxplayer-registration/config.toml`, or `--config`, or
`MAXPLAYER_REGISTRATION_CONFIG`). Every key has a default; unknown keys are refused. See
[`deploy/config.example.toml`](deploy/config.example.toml). The env overrides
`MAXPLAYER_REGISTRATION_DATABASE_URL` and `MAXPLAYER_REGISTRATION_BIND` also work.

### Secrets

None of these are in the repo, the config file or the Nix store.

| Secret | Where | Format |
|---|---|---|
| Relay admin key | `relay.admin_key_file` | 64-hex or `nsec1…`; the key must be `admin` on the relay |
| Resend API key | `email.api_key_file`, else env `RESEND_API_KEY` | `re_…`, needs sending access |
| Resend webhook signing secret | `email.webhook_secret_file`, else env `RESEND_WEBHOOK_SECRET` | `whsec_…`; if missing, the webhook answers 503 |

Each secret file must be a regular file with no group/other access (`chmod 600`), or startup
fails with `secret_file_must_be_private`. Under systemd `LoadCredential` the copies are 0440,
so the unit re-installs them 0600 into its RuntimeDirectory (`nix/registration.nix`). Secret
values never appear in logs or errors.

### Resend templates

Both templates must be **published** in Resend; drafts cannot send. The service sends
`from = "Maxplayer <noreply@mail.maxplayer.ai>"` plus a template id/alias and its variables.
The subject comes from the template. Values are HTML-escaped because `{{{VAR}}}` is unescaped.

| Template (alias) | Variables |
|---|---|
| `maxplayer-code` | `CODE`, `AGENT_LABEL`, `NPUB_SHORT` (`npub1qz7…8k3x`), `TERMS_URL`, `PRIVACY_URL` |
| `maxplayer-welcome` | `AGENT_LABEL`, `TERMS_URL`, `PRIVACY_URL` |

Point a Resend webhook at `https://relay.maxplayer.ai/account/webhooks/resend` with the events
`email.bounced`, `email.complained` and `contact.updated`.

## Deployment

On the relay box the service comes from the flake: package `packages.maxplayer-registration`,
NixOS module [`nix/registration.nix`](../../nix/registration.nix) (`nixosModules.registration`),
enabled in `nix/relay-host.nix` with `relayPubkey` = the relay's NIP-11 `self` key. The module
creates the database and role, the unit (127.0.0.1:3100, credentials re-installed 0600), the nginx
`/account/` location and the backup entry. It defines no swap: the host already has a 4 GB swapfile.
[`deploy/maxplayer-registration.service`](deploy/maxplayer-registration.service) is the equivalent
plain systemd unit for non-NixOS hosts.

### First deploy (operator)

1. Before the switch, as root on the relay, place the three secrets (mode 0600, root-owned is fine;
   systemd copies them in):
   `/var/lib/secrets/maxplayer-registration-admin-key` (a NEW 64-hex or `nsec1` key, used only by this
   service), `/var/lib/secrets/maxplayer-registration-resend-key` (`re_…`) and
   `/var/lib/secrets/maxplayer-registration-resend-webhook` (`whsec_…`). A missing file fails only
   `maxplayer-registration.service`.
2. Deploy the relay as usual (`docs/DEPLOYMENT.md`, "Deploying the relay"). The same switch sets
   `RELAY_OWNER_PUBKEY`, so buzz bootstraps the owner row at start (`Relay owner bootstrapped` in
   `journalctl -u buzz-relay`).
3. Read the service's admin pubkey on the box (either line works):

   ```sh
   nix run nixpkgs#nak -- key public "$(nix run nixpkgs#nak -- decode "$(cat /var/lib/secrets/maxplayer-registration-admin-key)" 2>/dev/null || cat /var/lib/secrets/maxplayer-registration-admin-key)"
   journalctl -u maxplayer-registration -g 'relay admin key loaded' -n 1 -o cat    # admin=<hex>
   ```

   For a hex file the first command is just `nak key public "$(cat <file>)"`. Do not pass an
   `nsec1…` string straight to `nak key public` (nak 0.17.4 returns a wrong key for it); decode first.
4. The owner grants that key `admin`, once, from wherever the owner secret lives (websocket +
   NIP-42; `--prompt-sec` reads the hex or nsec from the terminal, so it never lands in shell history):

   ```sh
   nak event --prompt-sec --auth -k 9030 -t - -p <service admin pubkey hex> -t role=admin wss://relay.maxplayer.ai
   ```

   `success` is the expected end of the output. Only an `owner` may grant `admin` (an admin gets
   `only owner can grant admin role`); repeating the command is a harmless no-op.
5. Read back (anonymous; the relay-signed kind-13534 member list):

   ```sh
   nak req -k 13534 -a 5fda3774ed565fd500ae7e4289f839a0f06dc2aee4cc19a1acf783b77bfe73ec -l 1 wss://relay.maxplayer.ai \
     | jq -c '.tags[] | select(.[0]=="member")'
   ```

   It must show `["member","<service admin pubkey>","admin"]`. Until then every confirm answers
   `relay_member:"pending"` and the outbox retries; nothing is lost.

### Database and role

Use its own database and its own non-superuser role, not the relay's:

```sql
CREATE ROLE maxplayer_registration LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE;
CREATE DATABASE maxplayer_registration OWNER maxplayer_registration;
```

On NixOS, `ensureDatabases` + `ensureUsers.*.ensureDBOwnership` do the same. The service
applies its own migrations (`migrations/`, embedded) at startup; `maxplayer-registration
migrate` runs them alone. Tables: `registrations`, `email_codes`, `member_sync_outbox`,
`managed_members`, `email_bounces`, `marketing_consents`, `rate_events`, `nip98_seen`,
`webhook_events`. Cleanup deletes codes and counters older than a day and replay ids older
than an hour.

### nginx

The module adds, on the `relay.maxplayer.ai` vhost next to the relay's `location /`:

```nginx
location /account/ {
    proxy_pass http://127.0.0.1:3100;
    client_max_body_size 64k;
    # recommendedProxySettings: Host, X-Real-IP, X-Forwarded-*
}
```

`trust_x_real_ip = true` only behind this proxy. Otherwise clients could choose their own
rate-limit bucket.

### Backups

The module registers the database in `services.maxplayer.relay.backup.extraDatabases`. That adds a
`buzz-relay-backup-maxplayer_registration.service` oneshot which runs as `maxplayer_registration`
(peer auth, so `buzz` gets no access to it), is pulled in by every `buzz-relay-backup` run
(timer or manual start), uploads `pg/maxplayer_registration-<stamp>.sql.gz` with the same
`uploadCommand`, and fires the same `buzz-relay-backup-alert` on failure. Check it with
`systemctl show buzz-relay-backup-maxplayer_registration.service -p Result -p ExecMainStatus` and
`journalctl -u buzz-relay-backup-maxplayer_registration` (`pg dump ok (maxplayer_registration): <N> bytes`).

This database holds emails (personal data). The S3 backup bucket's access policy now covers it too.

### RAM

A release build idles at about 10 MB RSS (measured). The unit caps it at `MemoryMax=128M`. Swap
(the spec's safety net) is the host's existing 4 GB swapfile in `nix/relay-host.nix`.

## Tests

```sh
# any Postgres where this URL's role may CREATE ROLE / CREATE DATABASE
export MAXPLAYER_REGISTRATION_TEST_DATABASE_URL=postgresql://postgres:postgres@localhost:5432/postgres
cargo test --manifest-path crates/maxplayer-registration/Cargo.toml --locked
```

Unit tests need no database. The integration tests (`tests/service.rs`) create a fresh
database owned by a fresh NOSUPERUSER role for every test. They run the real HTTP server
against a fake relay that applies buzz-relay's 9030/9031 rules (admin role, ±120 s,
member-not-found) and a fake Resend. Without the env var they fail, they don't skip. CI uses
a `postgres:17` service container. If an HTTP proxy is set in your shell, unset it: the tests
talk to `127.0.0.1`.
