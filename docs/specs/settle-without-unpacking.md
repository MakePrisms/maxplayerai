# Settle without unpacking: deliveries stay in the buyer store

**Status:** design spec. No code written. **Anchor commit: `4d67a21c5c8f6032f1602b50008b4ec6f42a39b1`
(= `origin/main` at the time of writing).** Line numbers are written `file:line @4d67a21`.

**Scope.** Close P1 from the v0.7.3-rc8 security review (seller-authored files such as `CLAUDE.md`,
`AGENTS.md`, `.claude/settings.json`, `.mcp.json` or a git `HEAD`+`config` layout land in
`~/.maxplayer/results/<job_id>/` under their own names, where agent harnesses and git load them on
their own) without a list of dangerous names. A new buyer binary settles git deliveries without
writing seller-named files to disk.

**Settled inputs, not relitigated here** (#v0-7-3-rc8-security-review, 12 Nov 2026):

1. No deny-list. No list of harness file names decides what is safe (Petar, 18:28).
2. The new binary does not unpack git deliveries. Old binaries keep today's behaviour. No protocol
   or wire change (Petar 18:28, Bob 18:37).
3. The agent reviews a delivery in place and extracts only on purpose, into a throwaway folder.
   Instructions carry that rule (Petar 18:28).
4. A buyer may hire another Maxplayer job as a security reviewer of a delivery. A paid
   "verify every job" service is later work, out of scope (Petar 18:08).
5. #1307 (rename agent-control names to `*.delivered`) is not merged (Petar 18:35, Bob 18:37).

---

## 1. Today

- Before any payment the delivered commit is fetched into the buyer's bare store and its tree is
  checked (`authorize_pay.rs:527 @4d67a21`). The commit is retained under
  `refs/maxplayer/deliveries/<oid>` (`delivery_git.rs:567 @4d67a21`), which store maintenance keeps
  as the buyer's evidence of what it paid for (`store_maint.rs:41 @4d67a21`).
- After payment, `materialize_delivery` (`collect.rs:630 @4d67a21`) writes every blob under its
  seller-chosen path into `results/<job_id>`: paid jobs at `collect.rs:253`, free jobs at
  `collect.rs:521`, session turns at `collect.rs:328` (into `results/<job_id>-t<n>`,
  `session.rs:672`). That write is the whole exposure: in the store, no delivered file exists
  under its own name.
- Inline answers are written as the fixed name `answer.txt` (`collect.rs:618 @4d67a21`).

## 2. Design

### 2.1 Settle stops at the store

`collect_async` (`collect.rs:155`), `collect_turn_async` (`collect.rs:275`) and `collect_free`
(`collect.rs:424`) no longer call `materialize_delivery` for a git delivery and create no
`results/` folder for it. Everything before that is unchanged: accept, tip-match, pre-pay tree
checks, payment, binds, the free-collect record (`collect.rs:542`, its `path` field becomes the
store ref) and the daemon's auto-settle. Inline answers keep writing `answer.txt`; it is a fixed
name the seller cannot choose.

### 2.2 What `get_job` and `collect` return

For a settled git delivery, `settlement` carries `commit_oid`, `files` (already read from the store
tree without writing anything, `settled_view.rs:99 @4d67a21`) and `delivery: "store"`, and no
`results_path` (`settlement.rs:508` stops finding a folder because none is written).
`CollectOutcome.path` is absent for a git delivery. The settled hints (`settlement.rs:254`, `:290`)
say: the delivery is in the buyer store; read files with `read_delivery`, or extract it into a
throwaway folder.

### 2.3 Reading in place

- MCP tool `read_delivery {job_id, turn?, path?}`: no `path` lists the files; with `path` it
  returns that file's text (UTF-8; a binary file returns its size and "extract to read"), capped
  per call with an offset for long files. Paths are looked up in the delivered tree only; `..`,
  absolute paths and unknown paths are refused.
- CLI `maxplayer delivery show <job_id> [--turn n] [<path>]`, same behaviour.
- Both read the store through libgit2, so the no-system-git property holds
  (`tests/no_system_git.rs`). Content read this way is data in the transcript: no harness treats
  it as its own instructions or config. (Prompt injection inside the text is the same as for any
  untrusted document and is what decision 4's reviewer job is for.)

### 2.4 Extracting on purpose

`maxplayer delivery extract <job_id> [--turn n] [--to <dir>]`, and MCP `extract_delivery`.

- Default destination: a new directory under the system temp dir
  (`$TMPDIR/maxplayer-<job8>-XXXXXX`), printed back. It is outside the home directory, so an agent
  started from `~` never sits above it.
- `--to` must not exist or be empty, and must not be inside `$MAXPLAYER_HOME`.
- It reuses today's writer unchanged and without renaming: the pre-pay path checks
  (`validate_tree_paths`, `collect.rs:884`), staging, `O_NOFOLLOW` writes and destination checks
  (`check_dest`, `collect.rs:753`). This is the only path in the new binary that writes delivered
  files.
- The output and tool description say: this folder is a stranger's code; do not start an agent
  session in it or run git in it until you have reviewed it.

### 2.5 Contribution jobs

The buyer already has its own clone and system git. Documented flow (replaces the mode B `rsync`
recipe in multi-turn-buying, `skill.md:109`):

```bash
git -C <clone> fetch "$MAXPLAYER_HOME/store" \
  "refs/maxplayer/deliveries/<commit_oid>:refs/heads/review/<job8>"
git -C <clone> diff <base>...review/<job8>      # review, or hand to a reviewer job
git -C <clone> merge review/<job8>              # only when satisfied
```

`merge_retained_commit` (`delivery_git.rs:609`) stays unused; a `maxplayer apply` command is later
work.

### 2.6 Docs

buyer-operate (`skill.md:174` and the settlement section), multi-turn-buying (mode A reads
`QUESTIONS.md` with `read_delivery`; mode B uses §2.5), grok-bot-operate, muse-buyer
`references/settlement.md`, `docs/BUYER-QUICKSTART.md`, the collect CLI usage
(`collect_cli.rs:69`) and the MCP `collect` description (`mcp.rs:450`). The skills are served from
the website to old and new binaries alike, so the wording branches on the response: `results_path`
present means an older binary wrote the files there; `delivery: "store"` means read or extract.
Add one line to both: folders under `results/` written by older binaries hold seller-named files;
delete them, and never start an agent session from `~` or inside `results/`.

## 3. Compatibility

- No wire change: sellers, the relay and the reviewer are untouched.
- Old binaries behave exactly as today.
- Ship the branching skill text (§2.6) before or with the binary, so an agent on either binary
  finds its files.
- `collect --out` (`collect_cli.rs:29`) only named a `results/` subfolder; see open question 1.

## 4. Implementation: one PR

Failure tests, each red on revert:

1. A paid, a free and a session-turn git delivery settle with nothing under `results/`
   (`results/<job_id>` and `results/<job_id>-t2` absent), while `get_job` reports `settled`,
   `commit_oid`, `files` and no `results_path`.
2. An inline delivery still writes `results/<job_id>/answer.txt`.
3. `read_delivery` returns a delivered file's bytes, lists files without `path`, refuses `..`,
   absolute and unknown paths, caps the size, and passes with system git unreachable.
4. `extract` with no `--to` writes under the system temp dir; refuses a non-empty `--to` and one
   inside `$MAXPLAYER_HOME`; every existing materialize safety test (traversal, symlinks, case
   collisions, DAG bombs) moves to `extract` and stays green.
5. A node skill test: no buyer skill tells the agent to open `results/<job_id>` for a git delivery
   without the `results_path` branch.

## 5. Deliberately left out

Each can be added later without a wire change: the paid verification service; a `maxplayer apply`
into a project; automatic clean-up of old `results/` folders; automating the reviewer job.

## 6. Open questions (proposed defaults)

1. **`collect --out` for a git delivery.** Default: refuse with a pointer to
   `delivery extract --to`; keep it for inline answers.
2. **An opt-in to the old unpacking.** Default: none. Without #1307's renaming it would bring the
   exposure back.
3. **A git repository layout in an extracted folder** (`HEAD` + `objects/` + `refs/` + `config` in
   any directory: `git status` there can run a seller command on some git versions). Default:
   `extract` refuses such a tree. This is one structural rule about git, not a list of harness
   names.
4. **Release.** Default: rc9, skills updated first.
