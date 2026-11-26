#!/usr/bin/env bash
# Powers the machine off once the runner has been idle for a while, so the
# EC2 instance (InstanceInitiatedShutdownBehavior=stop) costs nothing between
# questions. The Granite API starts it again when someone asks the agent
# something (AGENT_RUNNER_INSTANCE_ID on the API). Run as root by
# claude-runner-idle.timer every minute.
#
# IDLE_STOP_SECS   idle time before stopping (default 600)
# MIN_UPTIME_SECS  never stop a machine that booted less than this ago (default 600)
# RUNNER_URL       the runner's HTTP API (default http://127.0.0.1:8080)
set -euo pipefail

IDLE_STOP_SECS="${IDLE_STOP_SECS:-600}"
MIN_UPTIME_SECS="${MIN_UPTIME_SECS:-600}"
RUNNER_URL="${RUNNER_URL:-http://127.0.0.1:8080}"

uptime_secs=$(cut -d. -f1 /proc/uptime)
if (( uptime_secs < MIN_UPTIME_SECS )); then
  exit 0
fi

# No answer: the runner is down, restarting or not set up. Give it the boot
# grace plus the idle time, then stop anyway: a machine that cannot answer
# must not run (and cost) for nothing. The next question starts it again.
if ! idle=$(curl -fsS -m 5 "$RUNNER_URL/idle"); then
  if (( uptime_secs >= MIN_UPTIME_SECS + IDLE_STOP_SECS )); then
    logger -t claude-runner-idle "runner has not answered /idle for a while; stopping the machine"
    systemctl poweroff
  else
    logger -t claude-runner-idle "runner did not answer /idle; not stopping yet"
  fi
  exit 0
fi

running=$(printf '%s' "$idle" | sed -E 's/.*"running":([0-9]+).*/\1/')
idle_secs=$(printf '%s' "$idle" | sed -E 's/.*"idle_secs":([0-9]+).*/\1/')
if [[ "$running" == "0" ]] && (( idle_secs >= IDLE_STOP_SECS )); then
  logger -t claude-runner-idle "idle for ${idle_secs}s; stopping the machine"
  systemctl poweroff
fi
