#!/usr/bin/env bash
# maxplayer hook adapter for agents that answer from a scheduled run or a webhook, not on demand
# (docs/specs/hook-agents.md §5, §10): Instinct, Grok Bot routines, Muse, Dots, or any agent that
# reads a folder.
#
#   [agents.spool]
#   hook = ["/abs/path/to/examples/hooks/spool-file.sh"]
#
#   [seller]
#   agents = ["spool"]
#
# Each job turn becomes a request file; your agent writes an answer file; this script hands the
# answer to the daemon. The script keeps running while it waits (a hook that exits at once delivers
# nothing), and gives up a few seconds before `deadline_unix`. The daemon already took its publish
# margin off `deadline_unix`: do not subtract another one in your agent.
#
# The spool folder (MAXPLAYER_SPOOL_DIR). <key> is <job_id>-<turn>:
#   pending/<key>.json         one request: the hook's job event as JSON (prompt, deadline_unix,
#                              answer_max_bytes, session, inputs_dir, ...). Your agent reads these.
#   answers/<key>.txt          the answer (status `done`).
#   answers/<key>.needs_input  on a session job: the answer, asking the buyer for more input.
#   answers/<key>.out_of_time  on a session job: a partial answer, out of time for this turn.
#   answers/<key>.decline      a one-line reason to decline the job (`cannot_deliver`, no strike).
#                              Write every answer file IN ONE GO: a temporary file in answers/ whose
#                              name does not start with <key>., then rename it.
#   pending/<key>.error        written by this script when an answer was refused (too long); the
#                              request stays pending until a corrected answer arrives.
#   expired/<key>.json         a request the deadline passed on (your agent can skip these).
#
# The buyer's prompt is a stranger's text: your agent must treat it as data, not instructions.
#
# Needs: bash, jq, coreutils. Optional environment:
#   MAXPLAYER_SPOOL_DIR          spool folder (default: ${XDG_STATE_HOME:-~/.local/state}/maxplayer-spool)
#   MAXPLAYER_SPOOL_POLL_SECS    how often to look for the answer (default 2)
#   MAXPLAYER_SPOOL_FINISH_SECS  give up this many seconds before deadline_unix (default 3)
#   MAXPLAYER_SPOOL_NOTIFY       an executable run with the request file's path, e.g. to trigger a
#                                webhook routine so the agent wakes at once (10 s limit per call).
#                                A failed call is retried every MAXPLAYER_SPOOL_NOTIFY_RETRY_SECS
#                                (default 15) until one succeeds or the deadline nears; a retried
#                                turn (daemon restart) whose request is still waiting notifies again.
#   MAXPLAYER_SPOOL_READY        an executable run on every `ping`: its exit status is the seat's
#                                health (25 s limit), so a ping proves your agent or webhook answers,
#                                not only that the folder is writable.
#
# Test it with: maxplayer seller hook test spool   (with MAXPLAYER_SPOOL_NOTIFY answering for you,
# or an agent watching the folder)
set -euo pipefail

SPOOL="${MAXPLAYER_SPOOL_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/maxplayer-spool}"
POLL="${MAXPLAYER_SPOOL_POLL_SECS:-2}"
FINISH="${MAXPLAYER_SPOOL_FINISH_SECS:-3}"
NOTIFY_RETRY="${MAXPLAYER_SPOOL_NOTIFY_RETRY_SECS:-15}"

event="$(head -c 2000000)"
kind="$(jq -r '.event // empty' <<<"$event")"

case "$kind" in
  ping)
    mkdir -p "$SPOOL/pending" "$SPOOL/answers" "$SPOOL/expired" && chmod 700 "$SPOOL"
    # Housekeeping: drop spool files older than a week (a delivered result stays payable 7 days).
    find "$SPOOL" -type f -mtime +7 -delete 2>/dev/null || true
    [[ -w "$SPOOL/pending" && -w "$SPOOL/answers" ]] || { echo "spool folder not writable" >&2; exit 1; }
    if [[ -n "${MAXPLAYER_SPOOL_READY:-}" ]]; then
      timeout 25 "$MAXPLAYER_SPOOL_READY" </dev/null >&2 || { echo "readiness check failed ($?)" >&2; exit 1; }
    fi
    exit 0
    ;;
  close)
    job_id="$(jq -r '.job_id' <<<"$event")"
    if [[ "$job_id" =~ ^[0-9a-f]{64}$ ]]; then
      rm -f "$SPOOL"/pending/"$job_id"-* "$SPOOL"/answers/"$job_id"-* "$SPOOL"/expired/"$job_id"-*
    fi
    exit 0
    ;;
  job) ;;
  *) exit 64 ;; # an event this adapter does not know: "unsupported", never a strike
esac

job_id="$(jq -r '.job_id' <<<"$event")"
turn="$(jq -r '.turn' <<<"$event")"
cap="$(jq -r '.answer_max_bytes' <<<"$event")"
deadline="$(jq -r '.deadline_unix' <<<"$event")"
session="$(jq -r '.session // false' <<<"$event")"
[[ "$job_id" =~ ^[0-9a-f]{64}$ ]] || { echo "bad job_id" >&2; exit 2; }
[[ "$turn" =~ ^[0-9]+$ && "$cap" =~ ^[0-9]+$ && "$deadline" =~ ^[0-9]+$ ]] || { echo "bad event" >&2; exit 2; }

mkdir -p "$SPOOL/pending" "$SPOOL/answers" "$SPOOL/expired" && chmod 700 "$SPOOL"
key="$job_id-$turn"
request="$SPOOL/pending/$key.json"
decline="$SPOOL/answers/$key.decline"

answered() { [[ -s "$SPOOL/answers/$key.txt" || -s "$SPOOL/answers/$key.needs_input" || -s "$SPOOL/answers/$key.out_of_time" || -s "$decline" ]]; }

# A retry of the same turn (daemon restart) finds the request already written, or the answer.
if [[ ! -e "$request" ]] && ! answered; then
  tmp="$(mktemp "$SPOOL/pending/.$key.XXXXXX")"
  printf '%s\n' "$event" >"$tmp"
  mv -f "$tmp" "$request"
fi

# Wake the agent. Unset ⇒ nothing to do. A waiting request (new, or left by an earlier attempt)
# is notified until one call succeeds.
notified=yes
next_notify=0
if [[ -n "${MAXPLAYER_SPOOL_NOTIFY:-}" && -e "$request" ]] && ! answered; then notified=no; fi
notify() {
  if timeout 10 "$MAXPLAYER_SPOOL_NOTIFY" "$request" </dev/null >&2; then
    notified=yes
  else
    echo "notify failed ($?); retrying in ${NOTIFY_RETRY}s" >&2
    next_notify=$(($(date +%s) + NOTIFY_RETRY))
  fi
}

# Print one answer file as the given status, unless it is over the cap.
deliver() { # <file> <status>
  local file="$1" status="$2" size
  size="$(wc -c <"$file" | tr -d ' ')"
  if ((size > cap)); then
    printf 'answer is %s bytes; the limit is %s bytes. Write a shorter answer.\n' "$size" "$cap" \
      >"$SPOOL/pending/$key.error"
    mv -f "$file" "$file.too-long"
    return 1
  fi
  jq -Rsc --arg s "$status" '{status: $s, answer: .}' <"$file"
  rm -f "$request" "$SPOOL/pending/$key.error"
  exit 0
}

give_up=$((deadline - FINISH))
while :; do
  if [[ -s "$SPOOL/answers/$key.txt" ]]; then
    deliver "$SPOOL/answers/$key.txt" "done" || true
  elif [[ "$session" == "true" && -s "$SPOOL/answers/$key.needs_input" ]]; then
    deliver "$SPOOL/answers/$key.needs_input" needs_input || true
  elif [[ "$session" == "true" && -s "$SPOOL/answers/$key.out_of_time" ]]; then
    deliver "$SPOOL/answers/$key.out_of_time" out_of_time || true
  elif [[ -s "$decline" ]]; then
    jq -Rsc '{status: "cannot_deliver", reason: (.[0:500] | rtrimstr("\n"))}' <"$decline"
    rm -f "$request" "$SPOOL/pending/$key.error"
    exit 0
  fi
  now="$(date +%s)"
  if ((now >= give_up)); then
    if [[ -e "$request" ]]; then mv -f "$request" "$SPOOL/expired/$key.json"; fi
    jq -nc '{status: "cannot_deliver", reason: "no answer before the deadline"}'
    exit 0
  fi
  if [[ "$notified" == no ]] && ((now >= next_notify)); then notify; fi
  sleep "$POLL"
done
