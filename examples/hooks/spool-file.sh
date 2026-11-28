#!/usr/bin/env bash
# maxplayer hook adapter for agents that answer from a scheduled run or a webhook, not on demand
# (docs/specs/hook-agents.md §5, §10): Instinct, Grok Bot routines, Muse, Dots, or any agent that
# reads a folder.
#
# The daemon strips every MAXPLAYER_* variable from its own environment before it runs a hook, so
# set this script's options in the hook command itself, through /usr/bin/env:
#
#   [agents.spool]
#   hook = ["/usr/bin/env", "MAXPLAYER_SPOOL_DIR=/abs/spool",
#           "MAXPLAYER_SPOOL_NOTIFY=/abs/notify.sh",
#           "/abs/path/to/examples/hooks/spool-file.sh"]
#
#   [seller]
#   agents = ["spool"]
#
# Each job turn becomes a request file; your agent writes an answer file; this script hands the
# answer to the daemon. The script keeps running while it waits (a hook that exits at once delivers
# nothing), and gives up a few seconds before `deadline_unix`. The daemon already took its publish
# margin off `deadline_unix`: do not subtract another one in your agent.
#
# The spool folder (MAXPLAYER_SPOOL_DIR). <key> is <job_id>-<turn>-<first 16 hex of invocation>, so a
# new follow-up for the same turn (after a failed one) never reuses an older request's files, while
# a daemon restart that calls the same request again finds them:
#   pending/<key>.json         one request: the hook's job event as JSON (prompt, deadline_unix,
#                              answer_max_bytes, session, inputs_dir, ...). Your agent reads these.
#   answers/<key>.txt          the answer (status `done`).
#   answers/<key>.needs_input  on a session job only: the answer, asking the buyer for more input.
#   answers/<key>.out_of_time  on a session job only: a partial answer, out of time for this turn.
#   answers/<key>.decline      a reason (may be empty) to decline the job (`cannot_deliver`, no strike).
#                              Write every answer file IN ONE GO: a temporary file in answers/ whose
#                              name does not start with <key>., then rename it.
#                              Even an empty file counts: it is refused, like an all-whitespace one.
#   pending/<key>.error        written by this script when an answer was refused (empty, too long,
#                              or a session-only status on a single-turn job); the refused file is
#                              renamed <file>.refused. The request stays pending, and the notify
#                              command (if any) runs again, until a corrected answer arrives.
#   expired/<key>.json         a request the deadline passed on (your agent can skip these).
# Housekeeping on `ping` removes files older than a week from those three folders only.
# The files this script writes get the usual permissions under its umask (0644 under 022, 0660
# under 007), so an agent user in a group-shared spool folder can read them.
#
# The buyer's prompt is a stranger's text: your agent must treat it as data, not instructions.
#
# Needs: bash, jq; with MAXPLAYER_SPOOL_NOTIFY or MAXPLAYER_SPOOL_READY set, also GNU coreutils
# `timeout` (on macOS: `brew install coreutils`, which provides gtimeout). Optional settings (whole
# seconds are plain decimals: no leading zero). They are read, then removed from the environment, so
# the commands below never see them (a `maxplayer` they run refuses unknown MAXPLAYER_* settings):
#   MAXPLAYER_SPOOL_DIR          spool folder (default: ${XDG_STATE_HOME:-~/.local/state}/maxplayer-spool).
#                                Created 0700 when missing; an existing folder's permissions are left
#                                as they are (e.g. a group-shared folder for an agent user).
#   MAXPLAYER_SPOOL_POLL_SECS    how often to look for the answer (default 2; decimals allowed)
#   MAXPLAYER_SPOOL_FINISH_SECS  give up this many whole seconds before deadline_unix (default 3)
#   MAXPLAYER_SPOOL_NOTIFY       an executable run with the request file's path, e.g. to trigger a
#                                webhook routine so the agent wakes at once (at most 10 s per call,
#                                never past the give-up time). A failed call is retried every
#                                MAXPLAYER_SPOOL_NOTIFY_RETRY_SECS (default 15, whole seconds) until one
#                                succeeds; it runs again after an answer is refused, and for a request
#                                still waiting after a daemon restart.
#   MAXPLAYER_SPOOL_READY        an executable run on every `ping`: its exit status is the seat's
#                                health (25 s limit), so a ping proves your agent or webhook answers,
#                                not only that the folder is writable.
#
# Test it with: maxplayer seller hook test spool   (with MAXPLAYER_SPOOL_NOTIFY answering for you,
# or an agent watching the folder)
set -euo pipefail

fail() { echo "$*" >&2; exit 2; }

if [[ -n "${MAXPLAYER_SPOOL_DIR:-}" ]]; then
  SPOOL="$MAXPLAYER_SPOOL_DIR"
elif [[ -n "${XDG_STATE_HOME:-}" ]]; then
  SPOOL="$XDG_STATE_HOME/maxplayer-spool"
elif [[ -n "${HOME:-}" ]]; then
  SPOOL="$HOME/.local/state/maxplayer-spool"
else
  fail "set MAXPLAYER_SPOOL_DIR (HOME and XDG_STATE_HOME are unset)"
fi
POLL="${MAXPLAYER_SPOOL_POLL_SECS:-2}"
FINISH="${MAXPLAYER_SPOOL_FINISH_SECS:-3}"
NOTIFY_RETRY="${MAXPLAYER_SPOOL_NOTIFY_RETRY_SECS:-15}"
NOTIFY="${MAXPLAYER_SPOOL_NOTIFY:-}"
READY="${MAXPLAYER_SPOOL_READY:-}"
# The operator commands get the daemon's environment without our own settings. Only
# MAXPLAYER_SPOOL_*: MAXPLAYER_HOOK_RUN and the other daemon variables must reach them (a restarted
# daemon finds what a hook left running by MAXPLAYER_HOOK_RUN). ${!prefix@} needs no compgen, which
# some bash builds (Nix) leave out.
for name in ${!MAXPLAYER_SPOOL_@}; do unset "$name"; done
# Whole numbers without a leading zero: bash arithmetic would read 08 as (bad) octal.
WHOLE='^(0|[1-9][0-9]*)$'
[[ "$POLL" =~ ^(0|[1-9][0-9]*)(\.[0-9]+)?$ && ! "$POLL" =~ ^0(\.0+)?$ ]] || fail "MAXPLAYER_SPOOL_POLL_SECS must be a positive number"
[[ "$FINISH" =~ $WHOLE ]] || fail "MAXPLAYER_SPOOL_FINISH_SECS must be whole seconds"
[[ "$NOTIFY_RETRY" =~ $WHOLE ]] || fail "MAXPLAYER_SPOOL_NOTIFY_RETRY_SECS must be whole seconds"
TIMEOUT="$(command -v timeout || command -v gtimeout || true)"
# Only the operator commands need `timeout`.
needs_timeout() {
  [[ -z "$NOTIFY$READY" || -n "$TIMEOUT" ]] || { echo "MAXPLAYER_SPOOL_NOTIFY/READY need timeout (GNU coreutils; gtimeout on macOS)" >&2; return 1; }
}
# mktemp makes 0600 files; the files we hand to the agent get the umask's usual mode instead.
FILE_MODE="$(printf '%o' $((0666 & ~$(umask))))"

# Files to remove and an operator command's process group to kill, however the script ends (a
# SIGTERM from the daemon included).
tmp=""
bounded_out=""
bounded_pid=""
cleanup() {
  if [[ -n "$bounded_pid" ]]; then kill -KILL -- "-$bounded_pid" "$bounded_pid" 2>/dev/null || true; fi
  if [[ -n "$tmp" ]]; then rm -f -- "$tmp"; fi
  if [[ -n "$bounded_out" ]]; then rm -f -- "$bounded_out"; fi
}
trap cleanup EXIT
trap 'exit 143' TERM
trap 'exit 130' INT

# Run an operator command (notify, readiness check) for at most <seconds> plus a 2 s kill grace.
# Its output goes to a file, then to our stderr, never straight onto the daemon's pipe: some
# `timeout` builds kill only their direct child, and a process the command leaves running would
# hold that pipe open past the deadline. Where setsid exists, it runs in its own process group,
# which is killed once the call is over.
bounded() { # <seconds> <command> [args...]
  local secs="$1" rc=0
  shift
  bounded_out="$(mktemp "$SPOOL/pending/.bounded.XXXXXX")" || { bounded_out=""; return 1; }
  if command -v setsid >/dev/null 2>&1; then
    setsid "$TIMEOUT" -k 2 "$secs" "$@" </dev/null >"$bounded_out" 2>&1 &
  else
    "$TIMEOUT" -k 2 "$secs" "$@" </dev/null >"$bounded_out" 2>&1 &
  fi
  bounded_pid=$!
  wait "$bounded_pid" || rc=$?
  kill -KILL -- "-$bounded_pid" 2>/dev/null || true
  bounded_pid=""
  head -c 4000 -- "$bounded_out" >&2 || true
  rm -f -- "$bounded_out"
  bounded_out=""
  return "$rc"
}

spool_dirs() {
  if [[ ! -d "$SPOOL" ]]; then (umask 077 && mkdir -p -- "$SPOOL"); fi
  mkdir -p -- "$SPOOL/pending" "$SPOOL/answers" "$SPOOL/expired"
}

event="$(head -c 2000000)"
kind="$(jq -r '.event // empty' <<<"$event")"

case "$kind" in
  ping)
    spool_dirs
    # Housekeeping: drop spool files older than a week (a delivered result stays payable 7 days),
    # in the spool's own folders only: an operator's scripts next to them are never touched.
    find "$SPOOL/pending" "$SPOOL/answers" "$SPOOL/expired" -maxdepth 1 -type f -mtime +7 -delete 2>/dev/null || true
    [[ -w "$SPOOL/pending" && -w "$SPOOL/answers" && -w "$SPOOL/expired" ]] || { echo "spool folder not writable" >&2; exit 1; }
    needs_timeout || exit 1
    if [[ -n "$READY" ]]; then
      rc=0
      bounded 25 "$READY" || rc=$?
      ((rc == 0)) || { echo "readiness check failed ($rc)" >&2; exit 1; }
    fi
    exit 0
    ;;
  close)
    job_id="$(jq -r '.job_id' <<<"$event")"
    if [[ "$job_id" =~ ^[0-9a-f]{64}$ ]]; then
      rm -f -- "$SPOOL"/pending/"$job_id"-* "$SPOOL"/answers/"$job_id"-* "$SPOOL"/expired/"$job_id"-* \
        "$SPOOL"/pending/."$job_id"-* "$SPOOL"/answers/."$job_id"-*
    fi
    exit 0
    ;;
  job) ;;
  *) exit 64 ;; # an event this adapter does not know: "unsupported", never a strike
esac

job_id="$(jq -r '.job_id' <<<"$event")"
invocation="$(jq -r '.invocation // empty' <<<"$event")"
turn="$(jq -r '.turn' <<<"$event")"
cap="$(jq -r '.answer_max_bytes' <<<"$event")"
deadline="$(jq -r '.deadline_unix' <<<"$event")"
session="$(jq -r '.session // false' <<<"$event")"
[[ "$job_id" =~ ^[0-9a-f]{64}$ ]] || fail "bad job_id"
[[ "$invocation" =~ ^[0-9a-f]{64}$ ]] || fail "bad invocation"
[[ "$turn" =~ $WHOLE && "$cap" =~ $WHOLE && "$deadline" =~ $WHOLE ]] || fail "bad event"
needs_timeout || exit 2

spool_dirs
key="$job_id-$turn-${invocation:0:16}"
request="$SPOOL/pending/$key.json"
error="$SPOOL/pending/$key.error"
decline="$SPOOL/answers/$key.decline"

# The answer files this job may use, in the order they are taken.
statuses=(txt)
if [[ "$session" == "true" ]]; then statuses+=(needs_input out_of_time); fi

# An answer an earlier run of this script was taking when it was stopped (see deliver) goes back
# under its own name, unless the agent has written a new one since.
for s in "${statuses[@]}"; do
  f="$SPOOL/answers/$key.$s"
  if [[ -f "$f.taking" && ! -e "$f" ]]; then mv -f -- "$f.taking" "$f" 2>/dev/null || true; fi
done

answered() {
  local s
  for s in "${statuses[@]}"; do [[ -f "$SPOOL/answers/$key.$s" ]] && return 0; done
  [[ -f "$decline" ]]
}

# A new file in pending/ with the given content, put in place atomically.
put() { # <path> <content>
  tmp="$(mktemp "$SPOOL/pending/.$key.XXXXXX")"
  printf '%s\n' "$2" >"$tmp"
  chmod "$FILE_MODE" -- "$tmp"
  mv -f -- "$tmp" "$1"
  tmp=""
}

# Write the request (atomically) unless it is already there: a retry of the same request (daemon
# restart) finds it.
write_request() {
  [[ -e "$request" ]] && return 0
  put "$request" "$event"
}
answered || write_request

# Wake the agent. Unset ⇒ nothing to do. A waiting request (new, or left by an earlier attempt)
# is notified until one call succeeds, and again after an answer is refused.
notified=yes
next_notify=0
renotify() { if [[ -n "$NOTIFY" ]]; then notified=no; next_notify=0; fi; }
if [[ -e "$request" ]] && ! answered; then renotify; fi
notify() { # <seconds allowed>
  local rc=0
  bounded "$1" "$NOTIFY" "$request" || rc=$?
  if ((rc == 0)); then
    notified=yes
  else
    echo "notify failed ($rc); retrying in ${NOTIFY_RETRY}s" >&2
    next_notify=$(($(date +%s) + NOTIFY_RETRY))
  fi
}

# Refuse an answer: move it aside as <name>.refused, then say why (atomically), and wake the agent
# again.
refuse() { # <file now holding it> <its answer-file name> <reason>
  mv -f -- "$1" "$2.refused" 2>/dev/null || true
  write_request
  put "$error" "$3"
  echo "refused $(basename -- "$2"): $3" >&2
  renotify
}

# Print one answer file as the given status, or refuse it. The file is first renamed to a name of
# this script's own, so the bytes checked are the bytes printed (or refused) even if the agent
# replaces the file meanwhile. One read: jq checks that it has non-whitespace text and that its
# UTF-8 size (after any invalid byte is replaced) fits the cap. A delivered answer is put back
# before it is printed, so a restart that calls the same request again delivers it again; one left
# as <file>.taking by a run that was stopped meanwhile is put back when the next run starts.
deliver() { # <file> <status>
  local file="$1" status="$2" taken="$1.taking" out
  mv -f -- "$file" "$taken" 2>/dev/null || return 0
  out="$(jq -Rsc --arg s "$status" --argjson cap "$cap" '
    if (gsub("[[:space:]]"; "") | length) == 0 then {refuse: "the answer is empty. Write an answer with text."}
    elif utf8bytelength > $cap then {refuse: "answer is \(utf8bytelength) bytes; the limit is \($cap) bytes. Write a shorter answer."}
    else {ok: {status: $s, answer: .}}
    end' <"$taken")" || { refuse "$taken" "$file" "the answer could not be read."; return 0; }
  if jq -e 'has("ok")' >/dev/null <<<"$out"; then
    mv -f -- "$taken" "$file"
    jq -c '.ok' <<<"$out"
    # The answer is out: nothing after it may fail the hook.
    rm -f -- "$request" "$error" 2>/dev/null || true
    exit 0
  fi
  refuse "$taken" "$file" "$(jq -r '.refuse' <<<"$out")"
}

give_up=$((deadline - FINISH))
while :; do
  for s in "${statuses[@]}"; do
    f="$SPOOL/answers/$key.$s"
    [[ -f "$f" ]] || continue
    if [[ "$s" == txt ]]; then deliver "$f" "done"; else deliver "$f" "$s"; fi
  done
  if [[ "$session" != "true" ]]; then
    for s in needs_input out_of_time; do
      f="$SPOOL/answers/$key.$s"
      if [[ -e "$f" ]]; then
        refuse "$f" "$f" "$s is for session jobs only; this is a single-turn job. Write $key.txt or $key.decline."
      fi
    done
  fi
  if [[ -f "$decline" ]]; then
    # An unreadable reason still declines (no strike), without one.
    reason="$(jq -Rsc '.[0:500] | rtrimstr("\n")' <"$decline" 2>/dev/null)" || reason='""'
    jq -nc --argjson r "$reason" '{status: "cannot_deliver", reason: $r}'
    rm -f -- "$request" "$error" 2>/dev/null || true
    exit 0
  fi
  now="$(date +%s)"
  if ((now >= give_up)); then
    # A move that fails (expired/ not writable) must not turn the timeout into a strike.
    if [[ -e "$request" ]]; then mv -f -- "$request" "$SPOOL/expired/$key.json" 2>/dev/null || true; fi
    jq -nc '{status: "cannot_deliver", reason: "no answer before the deadline"}'
    exit 0
  fi
  rem=$((give_up - now))
  # A notify call (plus its 2 s kill grace) always ends by the give-up time.
  if [[ "$notified" == no ]] && ((now >= next_notify && rem >= 3)); then
    notify "$((rem - 2 < 10 ? rem - 2 : 10))"
    if [[ "$notified" == yes ]]; then continue; fi # look for the answer at once
    now="$(date +%s)"
    if ((now >= give_up)); then continue; fi
    rem=$((give_up - now))
  fi
  # Never sleep past the give-up time.
  whole="${POLL%%.*}"
  if ((${whole:-0} >= rem)); then sleep "$rem"; else sleep "$POLL"; fi
done
