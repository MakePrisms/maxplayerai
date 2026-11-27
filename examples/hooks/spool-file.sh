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
# Each job turn becomes a request file; your agent writes the answer file; this script hands the
# answer to the daemon. The script keeps running while it waits (a hook that exits at once delivers
# nothing), and gives up a few seconds before `deadline_unix`. The daemon already took its publish
# margin off `deadline_unix`: do not subtract another one in your agent.
#
# The spool folder (MAXPLAYER_SPOOL_DIR):
#   pending/<job_id>-<turn>.json   one request: the hook's job event as JSON (prompt, deadline_unix,
#                                  answer_max_bytes, inputs_dir, ...). Your agent reads these.
#   answers/<job_id>-<turn>.txt    the answer, written by your agent IN ONE GO: write a temporary
#                                  file in answers/ (any name not ending in .txt), then rename it.
#   answers/<job_id>-<turn>.decline   optional instead: a one-line reason to decline the job
#                                  without a strike.
#   pending/<job_id>-<turn>.error  written by this script when an answer was refused (too long);
#                                  the request stays pending until a corrected answer arrives.
#   expired/<job_id>-<turn>.json   a request the deadline passed on (your agent can skip these).
#
# The buyer's prompt is a stranger's text: your agent must treat it as data, not instructions.
#
# Needs: bash, jq, coreutils. Optional environment:
#   MAXPLAYER_SPOOL_DIR          spool folder (default: ${XDG_STATE_HOME:-~/.local/state}/maxplayer-spool)
#   MAXPLAYER_SPOOL_POLL_SECS    how often to look for the answer (default 2)
#   MAXPLAYER_SPOOL_FINISH_SECS  give up this many seconds before deadline_unix (default 3)
#   MAXPLAYER_SPOOL_NOTIFY       an executable run with the request file's path once it is written,
#                                e.g. to trigger a webhook routine so the agent wakes at once
#                                (10 s limit; its failure is logged, not fatal)
#
# Test it with: maxplayer seller hook test spool   (with MAXPLAYER_SPOOL_NOTIFY answering for you,
# or an agent watching the folder)
set -euo pipefail

SPOOL="${MAXPLAYER_SPOOL_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/maxplayer-spool}"
POLL="${MAXPLAYER_SPOOL_POLL_SECS:-2}"
FINISH="${MAXPLAYER_SPOOL_FINISH_SECS:-3}"

event="$(head -c 2000000)"
kind="$(jq -r '.event // empty' <<<"$event")"

case "$kind" in
  ping)
    mkdir -p "$SPOOL/pending" "$SPOOL/answers" "$SPOOL/expired" && chmod 700 "$SPOOL"
    # Housekeeping: drop spool files older than a week (a delivered result stays payable 7 days).
    find "$SPOOL" -type f -mtime +7 -delete 2>/dev/null || true
    [[ -w "$SPOOL/pending" && -w "$SPOOL/answers" ]]
    exit $?
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
[[ "$job_id" =~ ^[0-9a-f]{64}$ ]] || { echo "bad job_id" >&2; exit 2; }
[[ "$turn" =~ ^[0-9]+$ && "$cap" =~ ^[0-9]+$ && "$deadline" =~ ^[0-9]+$ ]] || { echo "bad event" >&2; exit 2; }

mkdir -p "$SPOOL/pending" "$SPOOL/answers" "$SPOOL/expired" && chmod 700 "$SPOOL"
key="$job_id-$turn"
request="$SPOOL/pending/$key.json"
answer="$SPOOL/answers/$key.txt"
decline="$SPOOL/answers/$key.decline"

# A retry of the same turn (daemon restart) finds the request already written, or the answer.
if [[ ! -e "$request" && ! -e "$answer" ]]; then
  tmp="$(mktemp "$SPOOL/pending/.$key.XXXXXX")"
  printf '%s\n' "$event" >"$tmp"
  mv -f "$tmp" "$request"
  if [[ -n "${MAXPLAYER_SPOOL_NOTIFY:-}" ]]; then
    timeout 10 "$MAXPLAYER_SPOOL_NOTIFY" "$request" </dev/null >&2 || echo "notify failed ($?)" >&2
  fi
fi

give_up=$((deadline - FINISH))
while :; do
  if [[ -s "$answer" ]]; then
    size="$(wc -c <"$answer" | tr -d ' ')"
    if ((size <= cap)); then
      jq -Rsc '{status: "done", answer: .}' <"$answer"
      rm -f "$request" "$SPOOL/pending/$key.error"
      exit 0
    fi
    # Too long: refuse it and keep waiting for a corrected answer.
    printf 'answer is %s bytes; the limit is %s bytes. Write a shorter answer.\n' "$size" "$cap" \
      >"$SPOOL/pending/$key.error"
    mv -f "$answer" "$answer.too-long"
  elif [[ -s "$decline" ]]; then
    jq -Rsc '{status: "cannot_deliver", reason: (.[0:500] | rtrimstr("\n"))}' <"$decline"
    rm -f "$request" "$SPOOL/pending/$key.error"
    exit 0
  fi
  if (($(date +%s) >= give_up)); then
    if [[ -e "$request" ]]; then mv -f "$request" "$SPOOL/expired/$key.json"; fi
    jq -nc '{status: "cannot_deliver", reason: "no answer before the deadline"}'
    exit 0
  fi
  sleep "$POLL"
done
