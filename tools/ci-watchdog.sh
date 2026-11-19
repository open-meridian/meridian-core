#!/usr/bin/env bash
# What a cluster job's runner had left, every five seconds, in a file a small
# upload keeps (the product owner, 2026-11-19).
#
# Three times on 2026-11-19 a cluster job died about six minutes into
# `make e2e-cluster-k3d`: the step had no log text, and the job then hung in
# its log upload past that step's limit until the run was cancelled. GitHub
# showed no incident. The runner running out of memory or disk while k3d
# loaded images or installed was inferred, not shown. This samples what would
# show it, to a file outside the job's log, which the next steps print and
# upload before anything else is uploaded:
#
#   ci-watchdog.sh start <file>   start the sampler, detached, and return
#   ci-watchdog.sh tail <file>    the file's last 40 lines
#   ci-watchdog.sh make-tail <make-log> <out>
#                                 the make's last 200 lines and the e2e's
#                                 FAILED lines (its own .e2e-*.log files in
#                                 the working directory too) into <out>, and
#                                 into the step summary: a fourth run on
#                                 2026-11-19 failed its make at 5.7 minutes
#                                 and hung in the logs' upload after it, so
#                                 why it failed is kept before that upload
#
# Each sample: the time and load; `free -m`; `df -h` of / and of Docker's
# root; the five processes holding the most memory; `docker stats
# --no-stream`; and any kernel OOM-killer lines not already written. As root
# (sudo), because dmesg is restricted to root on GitHub's runners, and so a
# sample can still be appended from the blocks ext4 keeps for root once / is
# full to everyone else. Every command is bounded, so a wedged Docker daemon
# delays a sample by seconds and never stops the next. The sampler ends when
# the file's directory goes (the runner empties its temp directory after the
# job) or after WATCHDOG_HOURS (default 3), whichever is first: it never
# outlives a job on a runner that is not thrown away after it.

set -euo pipefail

usage() { echo "usage: $0 start|tail <file> | make-tail <make-log> <out>" >&2; exit 2; }
[ $# -eq 2 ] || { [ $# -eq 3 ] && [ "$1" = make-tail ]; } || usage
cmd=$1 file=$2
every=${WATCHDOG_EVERY:-5}

# One command, bounded: its output, or why there was none.
bounded() {
  local secs=$1 out rc=0
  shift
  out=$(timeout "$secs" "$@" 2>&1) || rc=$?
  [ -z "$out" ] || printf '%s\n' "$out"
  case $rc in
    0) ;;
    124) echo "$1: no answer in $secs s" ;;
    *) [ -n "$out" ] || echo "$1: exit $rc" ;;
  esac
}

sample() {
  local root=$1 seen=$2 oom
  echo "=== $(date -u +%Y-%m-%dT%H:%M:%SZ) load $(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || echo '?')"
  bounded 3 free -m
  bounded 3 df -h / "$root" | awk '!seen[$0]++'
  bounded 3 ps -eo pid,rss,pcpu,comm --sort=-rss | head -n 6
  bounded 4 docker stats --no-stream --format 'table {{.Name}}\t{{.MemUsage}}\t{{.CPUPerc}}'
  # Only the OOM-killer's lines not written before, so a kill is written once.
  oom=$(timeout 3 dmesg 2>/dev/null | grep -iE 'out of memory|oom-kill|killed process' || true)
  [ -z "$oom" ] || printf '%s\n' "$oom" | tail -n +"$((seen + 1))"
  printf '%s' "$oom" | grep -c . >"$file.oom-seen" 2>/dev/null || true
}

loop() {
  local dir root deadline seen
  dir=$(dirname "$file")
  root=$(timeout 5 docker info --format '{{.DockerRootDir}}' 2>/dev/null || true)
  [ -n "$root" ] || root=/var/lib/docker
  deadline=$((SECONDS + ${WATCHDOG_HOURS:-3} * 3600))
  while [ -d "$dir" ] && [ "$SECONDS" -lt "$deadline" ]; do
    seen=$(cat "$file.oom-seen" 2>/dev/null || echo 0)
    sample "$root" "${seen:-0}" >>"$file" 2>&1 || true
    sleep "$every"
  done
}

case $cmd in
  start)
    mkdir -p "$(dirname "$file")"
    # Made by this user, so the upload after it can read what root appends.
    : >"$file"
    sudo_=""
    if [ "$(id -u)" != 0 ] && sudo -n true 2>/dev/null; then sudo_="sudo -n --preserve-env=WATCHDOG_EVERY,WATCHDOG_HOURS"; fi
    # Detached from the step: its own session, and no stdout or stderr the
    # runner would wait on before ending the step.
    self="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
    $sudo_ setsid nohup "$self" loop "$file" </dev/null >/dev/null 2>&1 &
    sleep 1
    [ -s "$file" ] || sleep "$every"
    if [ -s "$file" ]; then
      echo "ci-watchdog: sampling every ${every} s to $file"
    else
      echo "::warning::ci-watchdog: the sampler wrote nothing to $file"
    fi
    ;;
  loop) loop ;;
  tail)
    if [ -s "$file" ]; then
      echo "ci-watchdog: the last 40 lines of $file ($(grep -c '^=== ' "$file") samples)"
      tail -n 40 "$file"
    else
      echo "ci-watchdog: $file is missing or empty; the sampler never started or wrote nothing"
    fi
    ;;
  make-tail)
    out=$3
    {
      echo "== FAILED lines: the make's, then the e2e's own logs'"
      grep -h -E 'FAILED' "$file" .e2e-*.log 2>/dev/null | tail -n 100 || true
      echo "== the make's last 200 lines"
      if [ -s "$file" ]; then tail -n 200 "$file"; else echo "(no make output at $file)"; fi
    } >"$out"
    cat "$out"
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
      { echo "### What the make said, before any upload"; echo '```'; cat "$out"; echo '```'; } >>"$GITHUB_STEP_SUMMARY"
    fi
    ;;
  *) usage ;;
esac
