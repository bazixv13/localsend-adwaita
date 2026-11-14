#!/usr/bin/env bash
# Automated runtime checks for the LocalSend Flatpak.
#
# Drives the running app over the loopback LocalSend protocol and inspects the
# org.gtk.Notifications bus traffic, so the behaviour that used to be checked by
# hand (one reused notification, low-priority progress, abandoned transfers
# being cleaned up, text reception) runs unattended.
#
# Usage: scripts/check-runtime.sh [--fast]
#   --fast skips the 75 s abandoned-transfer wait
#
# Not covered here (needs a peer or UI automation):
#   * receiver-cancels-mid-send (needs a second real LocalSend instance)
#   * the accept/decline decision screen timing out (needs quick_save=false)
# Compile/static checks are separate:
#   cargo check --offline --locked
#   appstreamcli compose --no-net --prefix=/ --result-root=/tmp/as-root \
#       --data-dir=/tmp/as-data <build>/files
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROBE="$ROOT/scripts/probe.py"
APP_ID="io.github.localsend_adwaita.LocalSend"
CONFIG="$HOME/.var/app/$APP_ID/config/localsend-adwaita/config.json"
PORT=53317
[[ -f "$CONFIG" ]] && PORT=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("port", 53317))' "$CONFIG" 2>/dev/null || echo 53317)

failed=0
pass() { printf 'PASS  %s\n' "$1"; }
bad()  { printf 'FAIL  %s\n' "$1"; failed=1; }
skip() { printf 'SKIP  %s\n' "$1"; }

port_open() {
  python3 - "$1" <<'PY'
import socket, sys
s = socket.socket()
s.settimeout(1)
try:
    s.connect(("127.0.0.1", int(sys.argv[1])))
except OSError:
    sys.exit(1)
finally:
    s.close()
PY
}

ensure_running() {
  if pgrep -f "localsend-adwaita$" >/dev/null; then
    return 0
  fi
  echo "starting $APP_ID…"
  nohup flatpak run --user "$APP_ID" >/tmp/localsend-check-app.log 2>&1 &
  for _ in $(seq 1 40); do
    port_open "$PORT" && sleep 2 && return 0
    sleep 1
  done
  echo "app did not start (see /tmp/localsend-check-app.log)" >&2
  return 1
}

# The checks assert notification behaviour, which only happens while the window
# is hidden; hide it through the tray item if a StatusNotifier host is around.
hide_window() {
  local item label
  item=$(gdbus call --session --dest org.kde.StatusNotifierWatcher \
          --object-path /StatusNotifierWatcher \
          --method org.freedesktop.DBus.Properties.Get \
          org.kde.StatusNotifierWatcher RegisteredStatusNotifierItems 2>/dev/null \
        | grep -o "':[0-9.]*'" | tail -n1 | tr -d "'")
  [[ -z "$item" ]] && return 1
  label=$(gdbus call --session --dest "$item" --object-path /MenuBar \
            --method com.canonical.dbusmenu.GetLayout 0 2 "[]" 2>/dev/null)
  if [[ "$label" == *"Hide LocalSend"* ]]; then
    gdbus call --session --dest "$item" --object-path /MenuBar \
      --method com.canonical.dbusmenu.Event 1 clicked "<int32 0>" 0 >/dev/null 2>&1
    sleep 1
  fi
  return 0
}

MON_PID=""
start_monitor() {
  : >"$1"
  dbus-monitor --session "interface='org.gtk.Notifications'" >"$1" 2>&1 &
  MON_PID=$!
  sleep 1
}
stop_monitor() {
  [[ -n "$MON_PID" ]] || return 0
  kill "$MON_PID" 2>/dev/null
  wait "$MON_PID" 2>/dev/null
  MON_PID=""
}

downloads() { ls -1 "$HOME/Downloads/LocalSend" 2>/dev/null | sort; }

main() {
  local fast=0
  [[ "${1:-}" == "--fast" ]] && fast=1

  ensure_running || { echo "cannot run the checks without the app" >&2; exit 2; }
  if ! hide_window; then
    skip "window hidden (no StatusNotifier host; notifications may be suppressed)"
  fi

  local before after log

  # 1. One reused notification id: progress updates at low priority, result at normal.
  log=$(mktemp)
  start_monitor "$log"
  python3 "$PROBE" progress "$PORT" probe-check.bin >/dev/null
  sleep 2
  stop_monitor
  rm -f "$HOME"/Downloads/LocalSend/probe-check*.bin
  if python3 "$PROBE" verify-progress "$log"; then
    pass "progress updates reuse one low-priority notification"
  else
    bad "notification reuse/priority"
    python3 "$PROBE" summary "$log" | sed 's/^/      /'
  fi
  rm -f "$log"

  # 2. A sender that asks for a session and then vanishes must not leave a
  #    stuck transfer screen; the idle prune reports the failure.
  if [[ $fast -eq 1 ]]; then
    skip "abandoned transfer cleanup (--fast)"
  else
    log=$(mktemp)
    start_monitor "$log"
    before=$(downloads)
    python3 "$PROBE" abandon "$PORT" >/dev/null
    echo "waiting 75s for the idle cleanup…"
    sleep 75
    stop_monitor
    if python3 "$PROBE" verify-abandon "$log"; then
      pass "abandoned transfer is cleaned up"
    else
      bad "abandoned transfer cleanup"
      python3 "$PROBE" summary "$log" | sed 's/^/      /'
    fi
    after=$(downloads)
    comm -13 <(printf '%s\n' "$before") <(printf '%s\n' "$after") | sed 's/^/      new file: /'
    rm -f "$log"
  fi

  # 3. Text stays text: 204 from the upload and the payload lands on the clipboard.
  local payload="runtime-check-$(date +%s)" out
  if out=$(python3 "$PROBE" text "$PORT" "$payload"); then
    pass "text upload accepted with 204 ($out)"
  else
    bad "text upload ($out)"
  fi
  if command -v wl-paste >/dev/null; then
    if [[ "$(timeout 5 wl-paste -n 2>/dev/null)" == "$payload" ]]; then
      pass "text payload reached the clipboard"
    else
      bad "clipboard does not hold the text payload"
    fi
  else
    skip "clipboard verification (wl-paste missing)"
  fi

  # 4. Memory, for tracking rather than asserting.
  local pid
  pid=$(ps -eo pid,rss,args | grep " localsend-adwaita$" | sort -k2 -n | tail -n1 | awk '{print $1}')
  if [[ -n "$pid" ]]; then
    printf 'INFO  RSS %s kB' "$(ps -o rss= -p "$pid" | tr -d ' ')"
    [[ -r "/proc/$pid/smaps_rollup" ]] && \
      printf ', %s' "$(grep -E '^(Pss_Anon|Pss_File):' "/proc/$pid/smaps_rollup" | tr -s ' ' | paste -sd' ' -)"
    printf '\n'
  fi

  if [[ $failed -eq 0 ]]; then
    echo "all runtime checks passed"
  else
    echo "some runtime checks failed"
  fi
  return $failed
}

main "$@"
