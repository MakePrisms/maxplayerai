#!/usr/bin/env bash
# maxplayer hook adapter: one `claude -p` call per turn (docs/specs/hook-agents.md §5, §10).
#
#   [agents.claude-hook]
#   hook = ["/abs/path/to/examples/hooks/claude-per-turn.sh"]
#   family = "claude-code"        # optional, self-reported
#
#   [seller]
#   agents = ["claude-hook"]
#
# Turn 1 starts a Claude session; later turns of the same job resume it (`--resume <id>`). The
# session id is kept in a per-job state file, removed on `close`. Answer-only: no built-in tools and
# no MCP servers, except `Read`/`Grep`/`Glob` when the job came with input files. Claude runs
# `--restricted` with every hook and skill off, so none of YOUR Claude Code configuration (hooks in
# ~/.claude/settings.json, plugins, skills, MCP servers) sees the buyer's text. Strangers' text
# reaches Claude through this script on your machine, as you: keep it answer-only unless you
# contain it yourself.
#
# Needs: bash, jq, and a logged-in `claude` (Claude Code) on the PATH the seller daemon runs with.
# Optional environment: CLAUDE_BIN (default `claude`), MAXPLAYER_HOOK_STATE (state directory),
# CLAUDE_MODEL (passed as `--model`).
#
# Test it with: maxplayer seller hook test claude-hook
set -euo pipefail

CLAUDE_BIN="${CLAUDE_BIN:-claude}"
STATE_DIR="${MAXPLAYER_HOOK_STATE:-${XDG_STATE_HOME:-$HOME/.local/state}/maxplayer-hook-claude}"

event="$(head -c 2000000)"
kind="$(jq -r '.event // empty' <<<"$event")"

case "$kind" in
  ping)
    # Healthy = the CLI runs. A ping that should prove the model answers could make a one-token
    # call instead; that costs a request per ping.
    "$CLAUDE_BIN" --version >/dev/null 2>&1
    exit $?
    ;;
  close)
    job_id="$(jq -r '.job_id' <<<"$event")"
    if [[ "$job_id" =~ ^[0-9a-f]{64}$ ]]; then rm -f "$STATE_DIR/$job_id.session"; fi
    exit 0
    ;;
  job) ;;
  *) exit 64 ;; # an event this adapter does not know: "unsupported", never a strike
esac

job_id="$(jq -r '.job_id' <<<"$event")"
turn="$(jq -r '.turn' <<<"$event")"
cap="$(jq -r '.answer_max_bytes' <<<"$event")"
inputs_dir="$(jq -r '.inputs_dir // empty' <<<"$event")"
session="$(jq -r '.session // false' <<<"$event")" # a single-turn job has no next turn: keep no state
[[ "$job_id" =~ ^[0-9a-f]{64}$ ]] || { echo "bad job_id" >&2; exit 2; }

mkdir -p "$STATE_DIR" && chmod 700 "$STATE_DIR"
session_file="$STATE_DIR/$job_id.session"

rules="You are answering a job on the Maxplayer marketplace. Your final reply is the whole deliverable: \
it is sent to the buyer as text. Files you write are NOT delivered. Keep the reply under ${cap} bytes. \
Do not include anything you would not want the buyer to see."
if [[ -n "$inputs_dir" ]]; then rules="$rules The buyer's input files are in ${inputs_dir}; read them there."; fi

# --restricted: user/project/local settings files are ignored (no hooks, plugins or permission
# rules of yours) and the file tools stay inside the working directories; the --settings JSON
# switches hooks off again in case managed settings define some; no skills, no MCP servers.
args=(-p --output-format json --restricted --settings '{"disableAllHooks":true}'
  --disable-slash-commands --strict-mcp-config --append-system-prompt "$rules")
if [[ -n "$inputs_dir" ]]; then
  args+=(--tools "Read,Grep,Glob" --add-dir "$inputs_dir")
else
  args+=(--tools "")
fi
if [[ -n "${CLAUDE_MODEL:-}" ]]; then args+=(--model "$CLAUDE_MODEL"); fi
if [[ "$session" == "true" && "$turn" != "1" && -s "$session_file" ]]; then
  args+=(--resume "$(cat "$session_file")")
fi

# The prompt goes on stdin, never through a shell: it is the buyer's text, verbatim.
if ! out="$(jq -r '.prompt' <<<"$event" | "$CLAUDE_BIN" "${args[@]}")"; then
  echo "claude exited non-zero" >&2
  exit 1 # a broken agent: the daemon fails the job and strikes the entry
fi
# Structured output only: a CLI error is never wrapped as an answer (§5.0). Exactly one successful
# result envelope with a string reply, or the call failed: nothing is saved, nothing is printed.
if ! jq -se 'length == 1 and (.[0] | type == "object" and .type == "result"
      and .subtype == "success" and .is_error == false and (.result | type) == "string")' \
      <<<"$out" >/dev/null 2>&1; then
  echo "claude did not return a successful result: $(jq -r '.result // .subtype // "unparseable output"' <<<"$out" 2>/dev/null | head -c 500)" >&2
  exit 1
fi
session_id="$(jq -r '.session_id // empty' <<<"$out")"
if [[ "$session" == "true" && -n "$session_id" ]]; then printf '%s' "$session_id" >"$session_file"; fi

answer="$(jq -r '.result' <<<"$out")"
if [[ -z "${answer//[[:space:]]/}" ]]; then
  jq -nc '{status: "cannot_deliver", reason: "claude returned an empty reply"}'
  exit 0
fi
if (( $(printf '%s' "$answer" | wc -c) > cap )); then
  jq -nc --arg cap "$cap" '{status: "cannot_deliver", reason: ("reply over the " + $cap + "-byte cap")}'
  exit 0
fi
jq -nc --arg a "$answer" '{status: "done", answer: $a}'
