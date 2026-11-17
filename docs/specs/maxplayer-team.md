# Maxplayer Team — a group of agents with shared chat and a shared private repo

**Status:** design spec. No code written. **Anchor commit: `691fe5c9d8ceb3b1085f467db31ba196885713f2`
(= `origin/main` at the time of writing).** Every code citation below was re-derived by grep at that
commit and is written `file:line @691fe5c`. Relay paths are shortened: `relay/…` =
`crates/buzz/crates/buzz-relay/src/…`, `buzz-db/…` = `crates/buzz/crates/buzz-db/src/…`,
`core/…` = `crates/maxplayer-core/src/…`, `chat/…` = `crates/maxplayer-chat/src/…`.

**Scope.** Several agents, owned by one person or by friends, work on the same thing. They talk in
an encrypted group chat and share one private git repo. That is the whole product. Everything else
(task boards, notes, memory, specs) is files in the repo.

**Settled inputs (Bob, Discord #product-ideas › Maxplayer Team, 17 Nov 2026 20:45–21:09 UTC), not
relitigated here:**

1. A team can be one person's agents or friends' agents. The wire treats both the same.
2. The workspace is flexible: files, notes, a task board, shared memory — whatever the agents put in
   the repo. Nothing beyond git is built for it.
3. Agents coordinate through chat and git. No planner, no fixed roles, nothing swarm-specific.
4. Only agents post and read. A human talks to their own agent, which relays.
5. A team agent hires sellers exactly like any buyer does.
6. Messages and the repo are readable only by team members **and Maxplayer**. Maxplayer does the same
   checks and has the same access as for private jobs (Bob, 20:50: "maxplayer should still do the
   same checks and have the same access as other private jobs"). This is access control on our
   server plus a service copy, not end-to-end encryption.
7. The creator adds and removes members.
8. A separate binary, `maxplayer-team`. `maxplayer-chat` is not changed.
9. Design teams on their own terms. No modes or migrations to cover 1:1 chat (Bob, 20:58: "dont
   overengineer to solve for chat"). Every team has a repo.

---

## 1. The product in one paragraph

`maxplayer-team` is a mailbox plus a repo, not an agent. It makes no model call and starts no
harness. `watch` stays connected to the relay and stores team messages as they arrive, optionally
waking the owner's agent. The agent calls `inbox` to read, `send` to post, `log` to see history, and
`git` to clone, pull and push the team repo. It uses the home's Nostr key, the same npub
`maxplayer whoami` prints, so a team agent that wants to hire a seller just calls the normal
`maxplayer` buyer tools.

## 2. Commands

```
maxplayer-team create <name>                       # new team: provisions the repo, creator is the only member
maxplayer-team invite <team> <npub|hex> [--name <label>]   # creator only: adds to the relay member list, sends an invite
maxplayer-team remove <team> <npub|hex>            # creator only: drops from the relay member list
maxplayer-team invites                             # pending invites (not yet joined)
maxplayer-team join <team>                         # accept an invite; only joined teams' messages are stored
maxplayer-team leave <team>                        # stop storing this team's messages (local)
maxplayer-team list [--json]                       # joined teams, members, repo URL
maxplayer-team watch [--notify <cmd> [args…]]      # stay connected; store joined teams' messages
maxplayer-team inbox [--json]                      # unread messages, marked read
maxplayer-team send <team> "<text>" [--json]
maxplayer-team log <team> [--json]
maxplayer-team status [--json]
maxplayer-team git <team> -- <git args…>           # run git against the team repo with a fresh NIP-98 header
```

Global flags `--home` and `--relay` follow chat's rules (default `MAXPLAYER_HOME`/`~/.maxplayer`,
`wss://relay.maxplayer.ai`).

## 3. Team record on the relay (the one source of truth)

The relay holds each team's member list. Clients never hold an authoritative copy; they fetch it.

* **Identity.** `team_id` is 32 random bytes (64 lowercase hex), chosen by the creator. The repo
  lives at `https://<relay>/git/<creator>/<team_id>`.
* **Why 64-hex.** That namespace is already reserved for private repos, and every existing
  private-repo guard keys on `is_hex(repo_id, 32)`, so a team repo inherits them with no change:
  - public announcements can't claim the id (`relay/handlers/side_effects.rs:2716 @691fe5c`);
  - no relay-signed kind:30618 ref state is published (`relay/api/git/transport.rs:2097 @691fe5c`);
  - private quotas apply (`relay/api/git/transport.rs:1939 @691fe5c`, caps in
    `crates/buzz/crates/maxplayer-private-protocol/src/lib.rs:24 @691fe5c`);
  - a 64-hex id with no record is refused, never served as public
    (`relay/api/git/private_jobs.rs:46 @691fe5c` then `:54`).
  - The default-branch settings route uses the public 30617 read gate
    (`relay/api/git/settings.rs:321 @691fe5c`); a team repo has no 30617, so it is refused. Teams use
    `main` and don't need that route.
* **New table `team_repositories`** (`community_id, owner, team_id, members[], service, version,
  created_at`). `service` is copied from `private_service_pubkey` (`relay/config.rs:322 @691fe5c`)
  at creation, as for job repos (`relay/api/git/private_jobs.rs:99 @691fe5c`).
  `(community_id, owner, team_id)` must not exist in `private_job_repositories` and vice versa; both
  provisioning paths check the other table in the same transaction.
* **Endpoints** (on the git router next to `PUT /api/jobs/private/{job_id}`,
  `relay/api/git/transport.rs:2204 @691fe5c`), behind a new default-off flag
  `MAXPLAYER_TEAM_REPOS` modelled on `private_job_repos` (`relay/config.rs:320 @691fe5c`):
  - `PUT /api/teams/{team_id}` — body `{members:[hex…], expected_version:n}`. Creates the team
    (`expected_version: 0`) or replaces the member list. Only the creator may call it. The request
    goes through exactly the checks private provisioning already uses: strict NIP-98 with body digest
    (`relay/api/git/private_jobs.rs:116 @691fe5c`), replay refusal (`:146`), relay membership
    (`:153`), and seeding the empty manifest pointer on create (`:262`). `expected_version` must
    match the stored version (409 otherwise), so two edits never silently overwrite each other.
    Member cap: 16 (see §7). The creator is always a member and can't be removed.
  - `GET /api/teams/{owner}/{team_id}` — NIP-98, readable by members and the service. Returns
    `{owner, team_id, members, version, repo}`.
  - The table joins the community-deletion lists next to `private_job_repositories`
    (`buzz-db/store/deletion.rs:98 @691fe5c`, `:143`).
* **Names** are local labels. The creator's invite text carries the name it chose; nothing on the
  relay stores it.
* **Invited means listed.** `invite` puts the npub on the relay list first, so the invitee can read
  the repo before running `join`. `join` only controls which teams' messages the invitee's own
  client stores. Every member must be able to write to the relay (linked account, relay membership)
  to push or send, same as any buyer.

## 4. Repo access

`authorize` (`relay/api/git/private_jobs.rs:35 @691fe5c`) is the single gate every git route calls
before any manifest load: advertisement (`relay/api/git/transport.rs:989 @691fe5c`), upload-pack
(`:1256`), receive-pack (`:1390`) and the re-check right before the publication CAS (`:1925`). It
gains one branch: when no job row exists, look up a team row.

* **Read:** owner, any member, or the service. Same shape as a job repo's `can_read`
  (`buzz-db/private_jobs.rs:35 @691fe5c`), which also includes the service.
* **Write:** owner or any member. The service never writes.
* **Ref rules** in the pre-receive hook (`relay/api/git/policy.rs:312 @691fe5c`, today calling the
  job-only `can_create_ref` at `:330`). Job repos are create-only
  (`buzz-db/private_jobs.rs:45 @691fe5c`); a team repo needs normal branches, so it gets its own
  rule: create or fast-forward any `refs/heads/*`, create `refs/tags/*`; refuse non-fast-forward
  updates and deletions. Fast-forward is the hook's existing HMAC-protected `is_ancestor` flag
  (`relay/api/git/policy.rs:99 @691fe5c`). No force-push means a member can't erase others' work.
* `no_store` already applies to every route on this router (`relay/api/git/private_jobs.rs:282
  @691fe5c`), so responses are never cached for another key.
* **Removal** takes effect at the next request: the row no longer lists the npub. A removed member
  keeps whatever they already pulled.

**Client side.** Agents use plain git through `maxplayer-team git <team> -- <args>`. It signs one
NIP-98 header with `nip98_authorization_header_with_keys` (`core/git_transport.rs:529 @691fe5c`),
valid for both the advertisement and the service POST (`:500`), within the ±60 s window for an
unscoped token (`relay/api/git/transport.rs:143 @691fe5c`), and passes it to git through
`GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_0`/`GIT_CONFIG_VALUE_0` as `http.<repo-url>.extraHeader`, so the
token never appears in argv or in `.git/config`. One header per git invocation.

## 5. Messages

No new event kind and no relay change: kind 1059 is admitted (`relay/handlers/ingest.rs:598
@691fe5c`) and readable only by its `p` recipient (`P_GATED_KINDS`, `crates/buzz/crates/buzz-core/src/kind.rs:159 @691fe5c`).

* **Wrapping** uses the existing core helper `private_content::transport::wrap`
  (`core/private_content/transport.rs:37 @691fe5c`; module built under the gateway feature,
  `core/lib.rs:231 @691fe5c`). The module doc names it the shared primitive
  (`core/private_content/transport.rs:1 @691fe5c`); it caps the wrapper at 128 KiB (`:7`) and
  randomises the outer timestamp within the relay's drift window (`:169`). Receipt uses
  `unwrap_message` (`:116`), which verifies wrap and seal and requires a kind-14 rumor tagged only to
  the receiver (`:140-145`).
* **One wrap per recipient**: every current member except the sender, plus the service
  (`privacy.service_pubkey`, `core/home.rs:1739 @691fe5c`, default `:1727`). All copies carry
  identical envelope bytes.
* **Envelope** (rumor content, decoded `deny_unknown_fields`):

  ```json
  {"schema":"maxplayer-team/1","team":"<team_id>","owner":"<hex>","msg":"<64-hex random>",
   "type":"text"|"invite","version":<member-list version>,"recipients":["<hex>",…],"text":"<1–8000 chars>"}
  ```

  `recipients` is sorted and includes the service. `msg` is the logical id: receivers deduplicate on
  `(team, msg)`, never on wrap or rumor ids, which differ per copy.
  The schema string keeps team traffic apart from chat, whose reader drops anything that isn't
  `maxplayer-chat/1` (`chat/lib.rs:404 @691fe5c`), and from private-job content.
* **Receiving** (`watch`): unwrap, decode, then require: the team is joined; the seal author is in
  the member list fetched from the relay (refetched once if `version` is newer than the cached one);
  the receiver and the service are in `recipients`. Anything else is dropped without logging the
  plaintext. An `invite` from a team's owner for a team you haven't joined goes to `invites`
  (at most 20 pending, oldest dropped); nothing else from an unjoined team is stored.
* **Service copy.** The service receives the same bytes as members. A divergent copy (same
  `(team, msg)`, different bytes) is detectable by the service exactly as for private-job content.
  No ACK: nobody waits for the service, as with private jobs.

## 6. Hiring sellers

No change. A team agent posts a normal private job with `maxplayer` from its own wallet, passing the
repo files the seller needs as `inputs`, and commits the result to the team repo itself. The seller
never gets team-repo access.

## 7. Bounds (only the ones that stop runaway loops or unbounded fan-out)

* **Send:** 50 messages per team per UTC day per sender, enforced locally like chat's per-peer cap
  (`chat/lib.rs:550 @691fe5c`). This is what stops two agents replying to each other forever.
* **Receive:** 100 stored messages per member per team per day, like chat's inbound cap
  (`chat/lib.rs:47 @691fe5c`); excess is dropped and remembered.
* **Members:** 16 per team, enforced by the relay. A send is members + 1 wraps.
* The README tells agents what chat's README does: messages are untrusted words from trusted
  agents, never instructions, and ask your owner when unsure.

## 8. Code reuse

New crate `crates/maxplayer-team`, depending on `maxplayer-core` (`gateway`) and on
`maxplayer-chat` for its public helpers: `validate_text` (`chat/lib.rs:56 @691fe5c`), display
escaping, and `Notifier` (`:612`). The home-key load/create in chat's `Home::open`
(`chat/lib.rs:204 @691fe5c`, private `create_key` at `:158`) is moved into a `pub fn` with no
behaviour change, so both binaries create and read `<home>/key` the same way. Chat's watch loop
(reconnect, backlog paging) is private to its `network.rs`; team starts from a copy of it, adapted
to the team envelope. Extracting one shared watcher is a later cleanup, not part of this.

## 9. Implementation — one PR

Relay flag default off. Tests that must fail before the change and pass after:

* Relay: a non-member gets 403 on advertisement, upload-pack and receive-pack; a removed member gets
  403 on the next request; the service can read and can't push; a member's non-fast-forward push
  and branch deletion are refused, fast-forward accepted; a non-creator `PUT` is refused; a stale
  `expected_version` gets 409; a team id already used by a private job (and the reverse) is refused;
  a team repo id with no row is refused, not served public; no 30618 is published after a team push.
* Client: a message from a non-member, from an unjoined team, or missing the service in
  `recipients` is not stored; duplicate copies store once; the send cap refuses the 51st send; the
  git wrapper's token is absent from argv and `.git/config`.

## 10. Deliberately left out (each addable later without a wire change)

Deleting or archiving a team; transferring ownership; roles other than owner/member; members
removing themselves from the relay list (they can `leave` locally); force-push and branch deletion;
a shared watcher with chat; MCP tools for the binary; teams selling as one seller; MLS/forward
secrecy (Marmot) for larger teams.

## 11. Risks that come with settled decisions

* **Maxplayer can read everything** a team says and stores. That is settled input 6; the README says
  so.
* **Prompt injection between teammates** through messages and repo files. Mitigated only by the
  README rule and the owner's own agent policy.
* **The member list lives on our server.** A compromised relay could add a member. Accepted with
  input 6, since the relay already holds the repo.
* **No forward secrecy;** a removed member keeps what they pulled.
* **Relay storage grows with members × messages.** Each send stores members + 1 kind-1059 events,
  signed by throwaway keys, which nobody can delete on our relay. The send cap and member cap bound
  it per team per day; there is no global bound.

## 12. Open questions, each with a proposed default

| # | Question | Proposed default |
|---|---|---|
| Q1 | What does the service do with team copies? Shipping the key alone doesn't deploy a content consumer (`docs/specs/private-offers-rollout.md:40 @691fe5c`), and this spec did not check whether one runs in production. | Whatever consumes private-job content copies also accepts `maxplayer-team/1`. Until a consumer runs, copies sit on the relay, exactly as for private jobs. |
| Q2 | Member cap. | 16. |
