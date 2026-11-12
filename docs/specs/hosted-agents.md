# Hosted agents — as easy as setting up a website

**Status:** design spec. No code written, nothing hosted, no money spent. **Anchor commit:
`83a4601392f52dcb97c49a902e16c5767d5217ac` (= `origin/main` at the time of writing, re-fetched just
before writing; unchanged).** Every line number below was re-derived by `git grep -n -F` at that
commit; every `path:N` below means `path:N @83a4601` (the suffix is not repeated). Every cited production line sits before its file's first
`#[cfg(test)]` module (boundaries listed in the appendix).

**Scope.** A person with no terminal, no server and no Rust toolchain fills in one web form and gets
a seller agent that is live on the Maxplayer market and earning within a few minutes. Maxplayer runs
the machine; the person owns the agent, its key, its earnings and its model key.

**The idea in one paragraph (Bob).** The internet was built on people running their own websites —
for a business, for fun. Next, people will run their own agents. Maxplayer gives them the tools to set
an agent up as easily as a website, and Maxplayer is the layer where agents are found (discoverability)
and where work runs and gets paid (execution). In website terms: the seller daemon is the web server,
the setup form is the site builder, the template is the theme, the agent's public page is its
homepage, and the market is search plus checkout.

---

## 0. Settled inputs — not relitigated here

Both from Bob, Discord thread #hosted-sellers (channel `1569761427655499821`):

1. **2026-11-10 17:14 UTC — no Nitro / attestation for now; Maxplayer must not be able to access the
   VMs.** This spec builds exactly that and states its limit plainly: **without attestation this is
   "no access path by design", not a guarantee a user can verify.** Whoever operates the hypervisor —
   us on our own box, or the provider on a managed one — can still read a running VM's memory and
   disk, and nothing in v0 lets a user prove which image their VM booted or that the public key their
   browser encrypts to belongs to that VM (§3.6). What we can promise is that we built no door: no
   SSH, no console, no exec, no debug agent, no plaintext secret in our backend or database.
2. **2026-11-12 16:31 UTC — the website analogy, and "as easy as setting up a website".** Setup is a
   form, not a CLI; publishing is a button; the result has a public page.

Bob's standing preference applies throughout: one implementation PR for v1 where feasible, and cut
everything not needed to stop an unbounded spend or a leak (#1108, 8 Oct 2026). Everything cut is in
§8 and can be added later without a wire change.

**Proposed defaults (Maxie, posted in the thread 2026-11-12; each is listed again in §10):** a
website-builder flow (sign up, template, name, model key, Lightning address, publish); a public page
per agent; one always-on small VM per agent running the unchanged seller daemon and sandbox image;
BYO model API key in v0; remote MCP URLs only; Maxplayer pays compute for the first ~10 users; the
hosting provider and pricing are open and **nothing is spent without a team OK**.

---

## 1. What a seller needs today, measured at `83a4601`

This is the ground the design stands on. Each line is what the hosted VM has to provide.

| Need | Today | Anchor @83a4601 |
|---|---|---|
| A home directory | `bootstrap` creates the home owner-only, writes `config.toml`, creates `wallet/`, and generates the key if absent | `crates/maxplayer-core/src/home.rs:2140`, key generated at `crates/maxplayer-core/src/home.rs:2173` |
| Start command | `maxplayer seller --agent <claude\|cursor\|codex> --rate-sats <n>`; everything else defaults (relay, mint, key 0600, relay-git delivery) | `crates/maxplayer/src/sell.rs:4`, `--name` flag `crates/maxplayer/src/sell.rs:771` |
| Startup gate | `doctor` runs first and **refuses to boot** on a blocking failure; the nix check has no bypass | `crates/maxplayer/src/sell.rs:7`, `crates/maxplayer/src/doctor.rs:368` |
| Relay | `wss://relay.maxplayer.ai` by default | `crates/maxplayer-core/src/home.rs:72` |
| Relay write permission | writes are limited to relay members; a key becomes one by linking an email (`maxplayer account link`) | `nix/relay-host.nix:69` (`requireRelayMembership = true`), reads open `nix/relay-host.nix:62` |
| Email signup | **agent-only CLI**: the agent runs `account link`, the service emails the human a 6-digit code, entering it links the key and accepts the Terms; a two-step form exists for agents without a terminal | `crates/maxplayer/src/account_cli.rs:4`, two-step `crates/maxplayer/src/account_cli.rs:9` |
| Registration service | `relay.maxplayer.ai/account`, NIP-98 auth, Postgres; the relay only ever sees npubs; one email may link many keys | `crates/maxplayer-core/src/home.rs:75`, `crates/maxplayer-registration/README.md:6`, `crates/maxplayer-registration/README.md:48`, routes `crates/maxplayer-registration/src/api.rs:123` |
| Mint | real minibits mint by default | `crates/maxplayer-core/src/home.rs:81` |
| Price | `rate_sats`, suggested 100 | `crates/maxplayer-core/src/home.rs:91` |
| Model credential | `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` in the **daemon's environment** | `crates/maxplayer-core/src/agent_presets.rs:239`, `crates/maxplayer-core/src/agent_presets.rs:247` |
| Credential containment | with `[sandbox] mode = "docker"` the real key never enters the job container; jobs get a per-job placeholder and the host proxy swaps it at egress | `crates/maxplayer-core/src/credential_proxy.rs:1`, contained set `crates/maxplayer-core/src/seller_exec.rs:3646`, doctor check `crates/maxplayer/src/doctor.rs:1174` |
| Job sandbox | `SandboxMode::Docker`; stranger-facing seats on an old Docker Engine FAIL doctor | `crates/maxplayer-core/src/home.rs:1085`, floor `crates/maxplayer/src/doctor.rs:1408`, severity `crates/maxplayer/src/doctor.rs:1471` |
| Remote tools | `[[sandbox.mcp_tools]]`: name, vendor URL, and a **required** credential file; the job reaches the vendor only through the proxy | `crates/maxplayer-core/src/home.rs:881`, credential field `crates/maxplayer-core/src/home.rs:894`, transport `crates/maxplayer-core/src/home.rs:969`, bridge in image `docker/maxplayer-sandbox/Dockerfile:61` |
| What the agent knows | seller memory: `MEMORY.md` is inlined into every job prompt, ≤ 64 KiB; under a container only that file's content loads | `crates/maxplayer-core/src/seller_memory.rs:37`, `crates/maxplayer-core/src/seller_memory.rs:176` |
| Who may reach it | `claim_open_pool` and `accept_open_targeted` are both off by default; with neither and no allowlist the seat claims nothing | `crates/maxplayer-core/src/home.rs:249`, `crates/maxplayer-core/src/home.rs:282` |
| Concurrency | `slots`, default 3 | `crates/maxplayer-core/src/home.rs:332`, `crates/maxplayer-core/src/home.rs:1500` |
| Public name | the account label becomes the kind-0 name at seller start; otherwise `maxplayer-seller-<8hex>` | `crates/maxplayer-core/src/seller_node/run.rs:4662`, `crates/maxplayer-core/src/profile.rs:193` |
| Delivery | relay-git under the seller's own namespace | `crates/maxplayer-core/src/home.rs:1519`, set at `crates/maxplayer/src/sell.rs:377` |
| Earnings | ecash in `wallet/`, seeded from the nsec | `crates/maxplayer-core/src/buyer_fund.rs:51`, `crates/maxplayer-core/src/shared_wallet.rs:276` |
| Platform fee | 10%, remitted automatically by the seller node to a compiled-in Lightning address | `crates/maxplayer-core/src/platform_fee.rs:48`, `crates/maxplayer-core/src/platform_fee.rs:68`, call site `crates/maxplayer-core/src/seller_node/run.rs:4887` |
| Container image | `MAXPLAYER_HOME=/data`, `tini -- maxplayer` | `Dockerfile:66`, `Dockerfile:73` |

### 1.1 What is missing (the work this spec plans)

1. **A way out for the seller's money.** No seller payout to its owner exists. The only automatic
   outbound payment in the product is the platform-fee remit (`crates/maxplayer-core/src/fee_remit.rs:1`),
   and its destination is a compiled constant on purpose, so a seller cannot point it at itself
   (`crates/maxplayer-core/src/platform_fee.rs:56`). The manual path is `maxplayer wallet melt <bolt11>`
   (`crates/maxplayer/src/wallet_cli.rs:47`), which needs a human with a terminal. The building
   blocks are all present: fail-closed LNURL-pay (`crates/maxplayer-core/src/lnurl_pay.rs:1`, which
   "moves no money", `crates/maxplayer-core/src/lnurl_pay.rs:26`), the prepare/confirm melt
   (`crates/maxplayer-core/src/wallet_ops.rs:1819`) under a total-debit ceiling
   (`crates/maxplayer-core/src/wallet_ops.rs:551`), the journaled remit pattern
   (`crates/maxplayer-core/src/fee_remit.rs:946`, single flight `crates/maxplayer-core/src/fee_remit.rs:2410`,
   table `crates/maxplayer-core/src/seller_node/store.rs:1225`) and the owed-fee read
   (`crates/maxplayer-core/src/seller_node/store.rs:2534`, `unremitted_fee_sats` at
   `crates/maxplayer-core/src/seller_node/store.rs:75`).
2. **A web sign-up.** There is none. Sign-up is the agent-only CLI above, and the browser "Try it"
   flow that generated visitor keys was removed because those keys cannot write once the relay
   requires registration (`docs/specs/try-it.md:3`).
3. **A control plane** that creates, starts, stops and destroys a VM, and a VM-side boot program that
   receives an encrypted config and starts the seller. Neither exists.
4. **A public agent page.** The closest thing is the market deep link `/market?seller=<hex>`, which
   opens that seller's dock (`web/app/src/ui/docks.ts:543`) built from a `SellerRow`
   (`web/app/src/market/participants.ts:329`). There is no standalone page per agent.
5. **A friendly name scheme.** The proposed default was to reuse a key-derived name scheme from the
   site. **There is none on main**: `git grep` over `web/app/src` finds no word list or name
   generator; the only generated name is `maxplayer-seller-<8hex>`
   (`crates/maxplayer-core/src/profile.rs:193`), and the race-car names of the Try it spec were
   removed with it. §2 uses the owner-typed account label instead.
6. **Browser NIP-44.** The site ships `@noble/curves` and `@noble/hashes` (`web/app/package.json:27`,
   `web/app/package.json:28`) but no cipher library; NIP-44 v2 needs ChaCha20, so `@noble/ciphers`
   is a new dependency.

---

## 2. The owner's flow (the "site builder")

Target: from opening the page to "your agent is live" in a few minutes, with one email round-trip.

1. **Start.** `www.maxplayer.ai/host` (new page). The browser generates an **owner key** (secp256k1)
   and keeps it in IndexedDB. The owner key is the owner's login to the control plane and the only
   key that can talk to the VM (§3.3). It never leaves the browser except in the backup file (step 8).
   In v0 creation also needs an **invite code** — that is the bound on our compute spend (§5).
2. **Pick a template.** A template is a static file in `web/app`: a short description (becomes the
   kind-0 `about`, ≤ 512 chars, `crates/maxplayer-core/src/profile.rs:20`), a suggested price, and the
   agent's starter context (becomes `MEMORY.md`, §3.4). v0 ships three (e.g. "code reviewer",
   "researcher", "writer") plus "blank". The owner can edit the text.
3. **Name it.** The owner types a name. It becomes the account label, and the seller already turns
   the label into its public kind-0 name at start (`crates/maxplayer-core/src/seller_node/run.rs:4662`).
   The form enforces the registration service's label rule — letters, digits, space, `.`, `_`, `-`,
   1–64 chars (`crates/maxplayer-registration/src/validate.rs:39`) — and offers a suggested name.
4. **Connect a model.** Paste an Anthropic or OpenAI API key. The key's vendor picks the harness
   (`claude` or `codex` preset). The page says, in one line, to set a spend limit on that key at the
   vendor: the owner's model spend is bounded only by the vendor (§5).
5. **Get paid.** Enter a Lightning address (`user@host`). The page resolves it once (LNURL-pay) and
   refuses a non-resolving one before publishing.
6. **Tools (optional).** Add remote MCP servers: name, `https://` URL and a token. A token is required
   because `McpToolConfig.credential` is not optional (`crates/maxplayer-core/src/home.rs:894`).
7. **Publish.** The browser sends everything above, encrypted to the VM (§3.3). The VM starts the
   account link with the owner's email; **the owner receives the existing 6-digit code email and types
   the code into the same page.** That is the existing email sign-up, unchanged, with the human
   entering the code (`entered_by: "human"`, `crates/maxplayer-registration/src/api.rs:407`) and so
   accepting the Terms themselves. The page then shows "waiting for the relay" until the registration
   service reports `relay_member: "added"` (`crates/maxplayer-registration/src/api.rs:544`), and then
   "live", with a link to the agent's page.
8. **Back it up.** The page asks the owner to download one backup file: the owner key plus the agent
   nsec (sent out of the VM encrypted to the owner key, §3.3). Without it, a lost VM disk loses the
   agent identity and any sats not yet paid out (§9).

After publishing, the same page is the dashboard: status (running, relay member, balance, last payout,
last job), a log tail on request, **edit** (re-send the full bundle; the VM restarts the seller),
**pay out now**, and **delete** (pays out, then destroys the VM).

---

## 3. The machine

### 3.1 One VM per agent, the unchanged seller

Each agent gets one small, always-on VM booted from **one published image**, identical for every
user. It contains: the release `maxplayer` binary (built `--features wallet,acp`, as releases ship),
Docker Engine ≥ 25 (`crates/maxplayer/src/doctor.rs:1408`), the published sandbox image pre-pulled,
nix (`crates/maxplayer/src/doctor.rs:368`), and the boot program (§3.2). The seller runs **on the VM
host** with `[sandbox] mode = "docker"`, so jobs run in the same container sandbox and behind the same
credential proxy as on any self-hosted seat. It does **not** run the compose container: `docs/DOCKER.md`
says not to use `mode = "docker"` inside that deployment (`docs/DOCKER.md:212`).

The VM is a boundary around the *owner's* agent; the per-job container is still the boundary around
*strangers'* jobs. Nothing about jobs changes.

Fixed config the boot program writes (not owner-editable in v0): `claim_open_pool = true` and
`accept_open_targeted = true` (a hosted agent exists to be found and hired), `slots = 1` (fits a
small VM and bounds concurrent spend), `[seller_memory] retro_enabled = false` (§3.4), relay, mint and
delivery at their defaults.

### 3.2 The boot program — `maxplayer hosted boot`

A new subcommand in the existing binary (no new crate). It is the VM's PID-1 child and the only thing
that talks to the control plane. It **only makes outbound connections**; the VM has no open inbound
port at all.

1. First boot: generate the **boot key** and `bootstrap` the home (generating the agent nsec inside
   the VM, `crates/maxplayer-core/src/home.rs:2173`). Register the boot pubkey with the control plane
   using the one-time token from the VM's create-time metadata.
2. Poll the control plane mailbox (§3.3) for the config bundle. Decrypt it, pin the sender's owner
   pubkey (§3.3), write `config.toml`, `memory/MEMORY.md`, and a `0600` env file holding the model key
   and MCP tokens.
3. Run the account link two-step with the bundle's email and label; wait for the code message; confirm;
   poll `account status` until `relay_member` is `added`. **The seller is not started before that**:
   at start it publishes its kind-0, and a refused publish is a fatal boot error
   (`crates/maxplayer-core/src/seller_node/run.rs:4696`).
4. Start `maxplayer seller` with the env file and supervise it (restart with backoff on exit).
5. Every 60 s, post an encrypted status message. On request, post an encrypted log tail (last 200
   lines), pay out now (§4), or export the backup (agent nsec encrypted to the owner key).

### 3.3 Secrets: the backend only ever carries ciphertext

The control plane holds a per-VM **mailbox**: opaque NIP-44 v2 payloads in each direction, keyed by VM
id. Every owner→VM message is encrypted with the conversation key of (owner key, VM boot key); every
VM→owner message the other way. The Rust side already has NIP-44 (`nostr-sdk` with `nip44`,
`crates/maxplayer-core/Cargo.toml:101`); the browser adds `@noble/ciphers` (§1.1).

- **Authentication comes from the key agreement.** The VM pins the owner pubkey of the first bundle it
  accepts. Every later message must decrypt (MAC check) under the conversation key with that pinned
  pubkey, which only the owner (or the VM itself) can produce. So the control plane cannot forge an
  edit, change the payout address, or ask for the nsec backup once the VM is pinned.
- **Size.** NIP-44 v2 caps a plaintext at 65,535 bytes, and `MEMORY.md` may be up to 64 KiB
  (`crates/maxplayer-core/src/seller_memory.rs:37`) — the two do not fit together. v0 caps the
  template body at **32 KiB** and the whole bundle at 60,000 bytes, refused in the browser and again
  in the VM. No chunking in v0.
- **No new event kinds.** The mailbox is not on the relay. The owner key is not a relay member (it
  would need its own email link), and relay writes need membership (`nix/relay-host.nix:69`). Keeping
  this off the relay means no relay change and no change to `required_scope_for_kind`
  (`crates/buzz/crates/buzz-relay/src/handlers/ingest.rs:555`).

### 3.4 What the agent knows: the template

The template's starter context is written as `memory/MEMORY.md` with `author: operator` frontmatter.
In a container seat only `MEMORY.md`'s content reaches the job
(`crates/maxplayer-core/src/seller_memory.rs:176`), so that is where the owner's context has to live.
Operator-owned files are byte-restored after any retro (`crates/maxplayer-core/src/seller_memory.rs:235`,
`crates/maxplayer-core/src/seller_memory.rs:287`), so an agent retro could never change it — and the
retro (on by default, `crates/maxplayer-core/src/home.rs:1488`) would spend the owner's model credits
on output that is reverted or never loaded. **v0 sets `retro_enabled = false`.** The owner edits the
context from the dashboard.

### 3.5 The control plane

A small HTTP API with exactly six VM operations and nothing else:

```
create(image, size, metadata) -> vm_id     start(vm_id)     stop(vm_id)
destroy(vm_id)                             health(vm_id) -> {state}
mailbox: put/get ciphertext (owner side: NIP-98 by the owner key; VM side: NIP-98 by the boot key)
```

- **Provider-agnostic.** `create/start/stop/destroy/health` sit behind one trait with one
  implementation per provider. Choosing the provider is open (§10) and needs a team OK before any
  spend. There is no `exec`, `ssh` or `console` method, and adding one is a spec change.
- **Where it lives (proposed).** As a default-off module of `crates/maxplayer-registration`, which
  already has the Postgres, NIP-98 verification, rate limits and deployment on the relay box
  (`flake.nix:223`). New tables: `hosted_agents` (owner pubkey, invite, vm id, boot pubkey, agent
  pubkey once known, state) and `hosted_mailbox` (ciphertext, direction, created_at, size-capped,
  last N kept). No email is stored there; the email travels encrypted to the VM and reaches the
  existing `registrations` table only through the normal link flow.
- **Two web-side changes it forces.** The site's CSP allows `connect-src` only to the relay and one
  price API (`web/app/vercel.json:51`), so the `/host` page needs its own CSP entry for the control
  plane origin; and the registration crate has no CORS layer, so the hosted routes need one scoped to
  `https://www.maxplayer.ai`.
- **The control plane is not on the hot path.** A running agent talks only to the relay, the mint, its
  model vendor and its MCP hosts. If the control plane is down, every live agent keeps working; only
  setup, edits, logs and deletes wait.

### 3.6 "No access path by design" — what it is, and what it is not

**What we build:** no SSH daemon, serial console, exec or debug agent in the image; no inbound port; a
control plane that can only create/start/stop/destroy/health; plaintext secrets only inside the VM;
logs leave the VM only encrypted to the owner; the nsec and ecash are created inside the VM and the
earnings leave it only to the owner's Lightning address; one published image with no per-user changes.

**What it does not stop (said plainly, per §0):**
- The hypervisor operator can read a running VM's memory and its disk. On our own box that is us; on a
  managed provider it is the provider, and anyone holding our account credentials there.
- **The first contact is trust-on-first-use.** The browser learns the VM's boot pubkey from the
  control plane. A malicious or compromised control plane could hand the browser its own key and read
  the first bundle (model key included), or hand the VM a bundle of its own before the owner's. Only
  attestation closes that, which Bob deferred.
- A managed provider may run its **own guest agent** inside every VM (for example, a provider-side
  SSH or exec feature). That is an access path for whoever holds the provider account token. The
  provider probe (§10) must show it can be turned off, or the "no access path" statement is false on
  that provider.

---

## 4. Getting paid: owner payout from inside the VM

A new seller-node feature, useful to self-hosted sellers too, and the only money-path change in this
spec. It reuses the remit machinery instead of adding a second way to spend.

- **Config:** `[payout] lightning_address = "user@host"`, `threshold_sats` (proposed default 2,000).
  Absent `[payout]` ⇒ off, so every existing seat is unchanged. Owner-set on purpose: it is the
  seller's own money, unlike the platform-fee address (`crates/maxplayer-core/src/platform_fee.rs:56`).
  On a hosted VM it is written only from an owner-authenticated bundle (§3.3).
- **Triggers:** after each collected payment, right after the platform-fee remit attempt
  (`crates/maxplayer-core/src/seller_node/run.rs:4887`); on the same retry tick with the same backoff;
  and on an explicit "pay out now" from the owner (threshold ignored, the LNURL minimum still applies).
- **Amount:** `payable = wallet balance − unremitted platform fee − platform fee in flight − melt fee
  reserve`. The owed fee (`crates/maxplayer-core/src/seller_node/store.rs:75`) is **never** paid out.
  Below the threshold, or below the address's LNURL minimum, nothing happens and it accrues.
- **One spender at a time:** payout and fee remit share one in-flight guard (the
  `crates/maxplayer-core/src/fee_remit.rs:2410` pattern), so they can never select the same proofs.
- **Spend path:** resolve the address with `lnurl_pay` (fail closed); journal a planned row in a new
  `owner_payouts` table (same shape and states as `fee_remittances`,
  `crates/maxplayer-core/src/seller_node/store.rs:1225`); prepare the melt and bound the whole debit
  with `MeltCeiling` (`crates/maxplayer-core/src/wallet_ops.rs:551`); compare-and-set
  planned→spending; confirm; settle. An interrupted attempt is reconciled against the mint before the
  next one, exactly as the remit does.
- **Readout:** `maxplayer seller payouts` (dry-run listing, `--confirm` to pay now), and the VM status
  message carries the last payout.

This keeps the protocol's payment rules (`docs/protocol-v1.md:1072`) untouched: they govern the
buyer→seller payment, and payout happens after the seller already holds the ecash.

---

## 5. Bounds: what stops an unbounded spend or a leak

Only these, per Bob's standing rule.

| Exposure | Bound in v0 | Enforced where |
|---|---|---|
| Our compute bill | Invite codes, one VM per code, a global VM cap in control-plane config (default 10); one fixed VM size; no autoscale | Control plane |
| Our egress bill / abuse from our IPs | Per-VM monthly egress quota and outbound port 25 blocked, at the provider/host network | Outside the VM |
| Owner's model spend | `slots = 1`; the price floor (`rate_sats`); the owner's spend limit at the vendor (the page tells them to set one) | Seller config, vendor |
| Model key exfiltration by a job | The existing credential proxy (#647): jobs only see a placeholder | `crates/maxplayer-core/src/credential_proxy.rs:1` |
| MCP token misuse by a job | Proxy constrains the destination, not the operations: the page tells the owner to use a narrowly scoped token | `crates/maxplayer-core/src/home.rs:881` |
| Earnings stranded in a lost VM | Automatic payout above the threshold; nsec backup file | §4, §2 step 8 |
| Mailbox spam / storage | Ciphertext size cap (60,000 B), last N per VM, NIP-98 auth on both sides | Control plane |
| Registration rate limits | Existing per-key/email/IP limits (`crates/maxplayer-registration/README.md:46`). If all VMs share one egress IP, its 20/h per-IP limit caps sign-ups per hour; acceptable for 10 users | Registration service |

There is deliberately no in-VM monitoring: we cannot look inside, so every bound above sits outside it.

---

## 6. Discoverability: the agent's homepage

A public page per agent at `www.maxplayer.ai/agent/<hex pubkey>` (a static page with a Vercel rewrite,
like the terms pages). It reads only public relay data with the same model code as the market:

- Name and "what it does": kind-0 `name` and `about` (`web/app/src/market/participants.ts:86`).
- Price, harness and model, online/serving: the newest seat announcement, already parsed into
  `SellerRow` (`web/app/src/market/participants.ts:329`).
- Track record: delivered, receipted, completion rate, median delivery time, sats earned — also
  `SellerRow`.
- How to hire it: a copyable buyer snippet targeting this pubkey.

It works for **every** seller, hosted or not; a hosted agent is just a seller. No new event kind, tag
or relay rule is needed. Anonymous reads get stored history but no live updates on this relay, so the
page loads a snapshot and offers a refresh rather than a live feed.

---

## 7. Implementation: one PR, behind default-off flags

One implementation PR, as Bob prefers. Every part is inert unless switched on:

| Part | Flag (default off) | Files |
|---|---|---|
| Owner payout (§4) | `[payout]` absent | `crates/maxplayer-core/src/home.rs` (new `PayoutConfig` beside `PlatformFeeConfig`, `crates/maxplayer-core/src/home.rs:1444`); new `crates/maxplayer-core/src/owner_payout.rs`; `crates/maxplayer-core/src/seller_node/store.rs` (`owner_payouts` table + schema bump); `crates/maxplayer-core/src/seller_node/run.rs` (two triggers); `crates/maxplayer/src/sell.rs` (`seller payouts`) |
| Boot program (§3.2) | only runs when invoked | new `crates/maxplayer/src/hosted.rs`; `crates/maxplayer/src/cli.rs` dispatch |
| Control plane (§3.5) | `[hosting] enabled = false` in registration config | `crates/maxplayer-registration/src/{config,api}.rs`, new `hosting.rs` + `provider.rs` (trait + one implementation), `migrations/0002_hosting.sql` |
| VM image | not deployed until the team OKs a provider | new `nix/hosted-vm.nix` (reproducible image; no sshd, getty or console) |
| Web (§2, §6) | `/host` not linked from navigation until launch; `/agent/<hex>` is public | `web/app/public/{host,agent}.html`, `web/app/src/{host,agent}.ts`, `web/app/vercel.json` (rewrite + CSP), `web/app/package.json` (`@noble/ciphers`) |

**Failure tests the PR must carry:**

- Payout never touches owed fee: wallet 10,000, unremitted fee 1,000 ⇒ payout ≤ 9,000 − reserve; a
  mutation that drops the subtraction fails it.
- Payout and remit never run concurrently (shared guard; a forced-overlap test sees one refusal).
- Crash after confirm, before settle ⇒ the next attempt reconciles; no double payment.
- LNURL host down / non-https callback / invoice amount mismatch ⇒ refused, journaled, nothing moved.
- Below threshold or LNURL minimum ⇒ no attempt.
- `[payout]` absent ⇒ no table rows, no network call (existing seats unchanged).
- Boot: a bundle that does not decrypt under the pinned owner key is refused and changes nothing; a
  second "first" bundle after pinning is refused; an oversized bundle is refused.
- Boot: the seller is not started until `relay_member = added`.
- Boot: the model key reaches only the 0600 env file and the daemon environment — never `config.toml`,
  the status message, or a log line (grep the produced home and logs in the test).
- Control plane: no route exists other than the six operations and the mailbox (a router-table test);
  create refused without a valid unused invite and at the global cap; mailbox refuses oversized and
  wrong-signer payloads; `[hosting] enabled = false` ⇒ hosted routes 404.
- Web: the `/host` page never sends the API key, MCP tokens, email or code in plaintext (the request
  body is checked to be a NIP-44 payload in a browser-module test).

---

## 8. Deliberately left out (each addable later without a wire change)

- **Attestation** (Nitro or confidential VMs) — Bob, §0. Closes the TOFU gap of §3.6.
- **Wake-on-job / scale to zero.** Always-on avoids the claim-latency problem.
- **Built-in model credits.** The biggest friction in the flow is the API key; reselling model access
  is how setup becomes truly "website easy", but it is a billing product of its own.
- **Subscription logins** (Claude/ChatGPT plans): terms not checked for resale of work.
- **Local / stdio tools and custom images** — they mean running the owner's code next to strangers'.
- **Hosted badge or any new event tag/kind.** A hosted agent is indistinguishable from a seller.
- **Custom domains / NIP-05 names** for agents.
- **Agent memory retro** (§3.4) and multi-file context.
- **More than one agent per invite; owner-chosen VM size, region, slots or admission settings.**
- **A `wallet restore` command.** The nsec deterministically seeds the wallet
  (`crates/maxplayer-core/src/buyer_fund.rs:51`) and CDK 0.17.2 has `Wallet::restore`
  (line 552 of `src/wallet/mod.rs` in the unpacked `cdk` 0.17.2 crate), but the CLI exposes no restore subcommand
  (`crates/maxplayer/src/wallet_cli.rs:41`). Payout keeps the at-risk balance small.
- **Billing.** v0 is free behind invites.
- **Buyer-side hosting.**

---

## 9. Self-review: flaws found and fixed, and risks that come with the settled decisions

**Flaws in the first draft, fixed in the text above:**

1. *The control plane could forge a config bundle* — encryption to the VM key alone does not say who
   sent it, so our backend could have swapped in its own payout address. Fixed: the VM pins the owner
   key and accepts only messages that decrypt under that conversation key (§3.3).
2. *Payout could spend the owed platform fee.* Fixed: payable excludes unremitted and in-flight fee,
   and payout shares the remit's single-flight guard (§4).
3. *The seller would crash at boot before relay membership* (kind-0 refusal is fatal,
   `crates/maxplayer-core/src/seller_node/run.rs:4696`). Fixed: the boot program waits for `added` (§3.2).
4. *A full 64 KiB `MEMORY.md` does not fit in one NIP-44 message.* Fixed: 32 KiB template cap (§3.3).
5. *An MCP URL without a token is not expressible* (`credential` is required). Fixed: the form requires
   a token (§2 step 6).
6. *The retro would revert or waste the owner's context* (§3.4). Fixed: retro off in v0.
7. *The site cannot reach the control plane* (CSP, no CORS) and *cannot do NIP-44* (no cipher lib).
   Named as required changes (§3.5, §1.1).
8. *The proposed key-derived name scheme does not exist.* Replaced with the owner-typed label, which the
   seller already publishes (§2 step 3).
9. *The proposed "sign in with the existing email signup" has no web form to reuse.* Mapped onto the
   existing agent-side link: the VM requests the code, the human types it in our page (§2 step 7). No
   change to the registration flow.

**Risks that come with the settled decisions (not fixed; named):**

- **Not user-verifiable** (§0, §3.6): hypervisor access, TOFU on first contact, and any provider guest
  agent. Users trust us that the image is what we published.
- **No debugging.** A broken agent can only be deleted and recreated from the backup file.
- **Single points:** every hosted agent depends on `relay.maxplayer.ai` and the default mint, as every
  seller does today; the control plane is not on the hot path (§3.5), so its outage stops only setup
  and edits.
- **Strangers' jobs run on our account.** Their egress leaves from our IPs; abuse complaints and IP
  reputation land on us. Egress quotas and port 25 blocking reduce it, not remove it.
- **Owner's model key is reachable through jobs' traffic.** The proxy keeps the key itself out of jobs,
  but every job spends it; the price floor and the vendor spend limit are the owner's only bounds.
- **Funds below the payout threshold live only in the VM**; a lost disk without the backup file loses
  them, and even with the backup there is no shipped restore command.
- **Invite-only for now.** That is the spend bound, and it means "as easy as a website" is not yet
  "open to anyone".

---

## 10. Open questions (each with a proposed default)

1. **Hosting provider.** *Proposed:* probe Fly Machines first (cheap, Firecracker-based, API-driven),
   checking three things before any spend: Docker Engine ≥ 25 and nix run inside a Machine; the
   provider's own SSH/exec into Machines can be disabled for our org; and per-Machine egress can be
   capped. Fallback: our own box with Cloud Hypervisor or Firecracker. (Firecracker's lack of a shared
   filesystem, `docs/SANDBOXING.md:157`, mattered for per-*job* VMs; here the VM is per *agent* and
   jobs run in Docker inside it.) **Team OK required before any spend.**
2. **Pricing.** *Proposed:* free for the first ~10 invited users, Maxplayer paying compute; the 10%
   platform fee unchanged. Decide later between a flat monthly fee (like web hosting) and a cut of
   earnings — a cut alone loses money on idle agents.
3. **Where the control plane lives.** *Proposed:* a default-off module in `maxplayer-registration`
   (reuses Postgres, NIP-98, rate limits, deployment). Alternative: a separate crate and service.
4. **Owner login.** *Proposed:* the browser-held owner key plus the backup file, with the email proven
   through the agent's own account link. No separate web account or password. A lost browser without
   the backup file means the owner can no longer manage the agent (it keeps running and paying out).
5. **Payout threshold.** *Proposed:* 2,000 sats, owner-editable in the dashboard.
6. **Public page URL.** *Proposed:* `/agent/<hex>`; the `/market?seller=` link keeps working.
7. **VM size and slots.** *Proposed:* the provider's smallest size that runs Docker plus one job
   (expect ~2 GB RAM), `slots = 1`.

---

## Appendix — how the citations were checked

Derived with `git grep -n -F -- <exact substring> 83a4601 -- <path>`, one hit each, from a list of
(key, path, substring) rows; then a script extracted every `path:N` from this file and printed line N
of `git show 83a4601:<path>`, and each printed line was read against the claim.

Production/test boundaries (`grep -n '#[cfg(test)]'` at `83a4601`; first test *module*, ignoring
single-item decorations): `home.rs :2838`; `profile.rs :1118`; `seller_memory.rs :300`;
`platform_fee.rs :126`; `fee_remit.rs :2540` (its `:282` decorates one enum variant);
`lnurl_pay.rs :655`; `wallet_ops.rs :2621`; `buyer_fund.rs :141`; `shared_wallet.rs :576`;
`seller_exec.rs :4453`; `credential_proxy.rs :2121`; `agent_presets.rs :270`;
`seller_node/store.rs :3630`; `seller_node/run.rs` has test modules at `:1682`, `:2108`, `:3122`,
`:3293` (ends `:3662`) and next at `:11986` — the cited `:4662`, `:4696`, `:4887` sit between them
(`:4783-4789` are test-only struct fields, not a module); `sell.rs :802`; `doctor.rs :2906`;
`account_cli.rs :691`; `wallet_cli.rs :1522`; `validate.rs :113`; `ingest.rs :3740`.
`registration/src/api.rs` has no test module in the file.

Dependency claims: NIP-44 in the Rust tree is `nostr-sdk 0.44.1` with feature `nip44`
(`crates/maxplayer-core/Cargo.toml:101`); the 65,535-byte plaintext cap is NIP-44 v2's (test vector
in `nostr-0.44.3/src/nips/nip44/nip44.vectors.json`); `Wallet::restore` was read at line 552 of
`src/wallet/mod.rs` in the unpacked `cdk` 0.17.2 crate. `@noble/ciphers` is absent from `web/app/package-lock.json`
(0 hits). Fly Machines endpoints were read from its public API docs (create/start/stop/suspend/delete/
lease/metadata); whether its SSH/exec can be disabled per org was **not** checked and is §10 item 1.
