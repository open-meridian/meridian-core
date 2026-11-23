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
#   ci-watchdog.sh evidence <make-log> <file>
#                                 after a failed make: its FAILED lines, its
#                                 last lines, the runner's network as it is
#                                 now and the sampler's last samples, as
#                                 annotations and as a check run of its own
#                                 on the commit (GH_TOKEN with checks: write,
#                                 CHECK_SHA, CHECK_NAME), which a detached
#                                 beacon then keeps adding to. A fifth run
#                                 (PR 125's own) lost its log and every
#                                 upload again, the sampler's among them: its
#                                 steps' results did arrive, so what is known
#                                 leaves by the API and never by an upload.
#                                 The check run is made first, from file
#                                 reads alone (the make's FAILED and last
#                                 lines, the sampler's last load, memory
#                                 and disk lines), before any network or
#                                 Docker probe: on runs 45130110944 and
#                                 45139152927 every probe ran to its limit
#                                 and nothing reached GitHub. The probes'
#                                 results are added to it after
#   ci-watchdog.sh probe <which> <make-log>
#                                 one fact as a step's result, which arrives
#                                 when nothing else does: gave-up, names,
#                                 hub, blob, subnet. Each writes ok or
#                                 failed, with its output's last line, to
#                                 the step summary too: the steps are
#                                 continue-on-error, so the API shows each
#                                 as passed whatever it found
#
# Each sample: the time and load; `free -m`; `df -h` of / and of Docker's
# root; the five processes holding the most memory; `docker stats
# --no-stream`; whether names resolve past the cache and whether Docker Hub,
# the results' blob store and GitHub's API answer; and any kernel OOM-killer
# lines not already written. The file opens with the runner's addresses,
# routes, resolvers and Docker networks, for the green runs' baseline. As root
# (sudo), because dmesg is restricted to root on GitHub's runners, and so a
# sample can still be appended from the blocks ext4 keeps for root once / is
# full to everyone else. Every command is bounded, so a wedged Docker daemon
# delays a sample by seconds and never stops the next. The sampler ends when
# the file's directory goes (the runner empties its temp directory after the
# job) or after WATCHDOG_HOURS (default 3), whichever is first: it never
# outlives a job on a runner that is not thrown away after it.

set -euo pipefail

usage() { echo "usage: $0 start|tail <file> | make-tail <make-log> <out> | evidence <make-log> <file> | probe <which> <make-log>" >&2; exit 2; }
case ${1:-}:$# in
  start:2|loop:2|tail:2|make-tail:3|evidence:3|beacon:3|probe:3|probe-one:3) ;;
  *) usage ;;
esac
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

# The hosts a cluster job needs after its start: the images' registry, the
# store the job's log and artifacts are uploaded to, GitHub's API, and the
# service an upload asks first.
HUB=https://registry-1.docker.io/v2/
BLOB=https://productionresultssa0.blob.core.windows.net/
API=https://api.github.com/zen
RESULTS=https://results-receiver.actions.githubusercontent.com/
SUBNET=${E2E_K3D_SUBNET:-172.28.0.0/16}

# One URL: its status (000 when nothing answered), curl's exit, and the
# seconds its name and its connection took.
answers() { curl -s -o /dev/null -m 4 -w '%{http_code}/%{exitcode}/%{time_namelookup}/%{time_connect}' "$1" 2>/dev/null || true; }

# Whether a name resolves now, the resolver's cache left out where it can be.
names() {
  local nocache=""
  if resolvectl --help 2>/dev/null | grep -q -- '--cache'; then nocache=--cache=no; fi
  if command -v resolvectl >/dev/null 2>&1; then
    timeout 4 resolvectl query $nocache -4 registry-1.docker.io >/dev/null 2>&1
  else
    timeout 4 getent ahostsv4 registry-1.docker.io >/dev/null 2>&1
  fi
}

# One line: names, and each host's answer, asked together.
net_line() {
  local dir n
  dir=$(mktemp -d)
  ( names && echo ok || echo FAIL ) >"$dir/n" &
  answers "$HUB" >"$dir/h" &
  answers "$BLOB" >"$dir/b" &
  answers "$API" >"$dir/a" &
  answers "$RESULTS" >"$dir/r" &
  wait
  n=$(cat "$dir/n")
  echo "net: names ${n:-?} hub $(cat "$dir/h") blob $(cat "$dir/b") api $(cat "$dir/a") results $(cat "$dir/r") (status/curl exit/lookup s/connect s)"
  rm -rf "$dir"
}

# The runner's addresses, routes, resolvers and Docker networks.
net_state() {
  echo "-- addresses"; bounded 3 ip -br addr
  echo "-- routes"; bounded 3 ip route
  echo "-- resolvers"; { bounded 3 resolvectl dns; grep -h -E '^nameserver' /etc/resolv.conf /run/systemd/resolve/resolv.conf 2>/dev/null | sort -u; } || true
  echo "-- docker networks"
  for n in $(timeout 4 docker network ls -q 2>/dev/null); do
    bounded 3 docker network inspect --format '{{.Name}} {{range .IPAM.Config}}{{.Subnet}} {{end}}' "$n"
  done
}

# Whether an address of the runner's own, a route of its or a resolver lies
# in the subnet the cluster's Docker network takes (the Makefile's
# E2E_K3D_SUBNET), other than on that network's own bridge: 0 when one does,
# 1 when none does.
overlaps() {
  python3 - "$SUBNET" <<'PY'
import ipaddress, re, subprocess, sys
try: net = ipaddress.ip_network(sys.argv[1], strict=False)
except ValueError as e: print(e); sys.exit(2)
def run(*a):
    try: return subprocess.run(a, capture_output=True, text=True, timeout=4).stdout
    except Exception: return ""
found = []
for line in run("ip", "-o", "-4", "addr").splitlines():
    dev = line.split()[1]
    if dev.startswith(("br-", "veth", "docker")): continue
    for a in re.findall(r"inet (\S+)", line):
        if ipaddress.ip_interface(a).network.overlaps(net): found.append(f"address {a} on {dev}")
for line in run("ip", "-4", "route").splitlines():
    if re.search(r"dev (br-|veth|docker)", line): continue
    for a in re.findall(r"(\d+\.\d+\.\d+\.\d+(?:/\d+)?)", line):
        try:
            if ipaddress.ip_network(a, strict=False).overlaps(net): found.append(f"route: {line.strip()}"); break
        except ValueError: pass
text = run("resolvectl", "dns")
for f in ("/etc/resolv.conf", "/run/systemd/resolve/resolv.conf"):
    try: text += open(f).read()
    except OSError: pass
for a in set(re.findall(r"\b(\d+\.\d+\.\d+\.\d+)\b", text)):
    if ipaddress.ip_address(a) in net: found.append(f"resolver {a}")
print("\n".join(found) if found else f"nothing of the runner's own lies in {net}")
sys.exit(0 if found else 1)
PY
}

sample() {
  local root=$1 seen=$2 oom
  echo "=== $(date -u +%Y-%m-%dT%H:%M:%SZ) load $(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || echo '?')"
  bounded 3 free -m
  bounded 3 df -h / "$root" | awk '!seen[$0]++'
  bounded 3 sh -c 'ps -eo pid,rss,pcpu,comm --sort=-rss | head -n 6'
  bounded 4 docker stats --no-stream --format 'table {{.Name}}\t{{.MemUsage}}\t{{.CPUPerc}}'
  net_line
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
  { echo "=== the runner's network at the start"; net_state; } >>"$file" 2>&1 || true
  while [ -d "$dir" ] && [ "$SECONDS" -lt "$deadline" ]; do
    seen=$(cat "$file.oom-seen" 2>/dev/null || echo 0)
    sample "$root" "${seen:-0}" >>"$file" 2>&1 || true
    sleep "$every"
  done
}

# What a failed make left on disk, from file reads alone: no network and no
# Docker, so it is ready to send before anything is probed. $1 is the make's
# log, $2 the sampler's file.
known() {
  local make=$1 watch=$2
  echo "== $(date -u +%Y-%m-%dT%H:%M:%SZ) ${CHECK_NAME:-a cluster job} on $(hostname 2>/dev/null || echo '?'), runner ${RUNNER_NAME:-?}"
  echo "== FAILED and gave-up lines: the make's, then the e2e's own logs'"
  grep -h -E 'FAILED|gave up|could not be made' "$make" .e2e-*.log 2>/dev/null | tail -n 40 || true
  echo "== the make's last 60 lines"
  if [ -s "$make" ]; then tail -n 60 "$make"; else echo "(no make output at $make)"; fi
  echo "== the sampler's last load, memory and disk lines"
  if [ -s "$watch" ]; then
    # Its last two samples, the last perhaps cut short by the make's end.
    awk '/^=== 20/{n++} {l[NR]=$0; s[NR]=n} END{for(i=1;i<=NR;i++) if(n && s[i]>=n-1) print l[i]}' "$watch" \
      | grep -E '^=== 20|^Mem:|^Swap:|^Filesystem|^/dev/|^overlay' || echo "(no sample in $watch yet)"
  else
    echo "(no sampler file at $watch)"
  fi
}

# The evidence check run, made (POST) or brought up to date (PATCH): $3 is
# the evidence so far, $4 what the beacon added after the step. Bounded by
# curl's own limit; its answer on stdout.
check_send() {
  local method=$1 url=$2 ev=$3 after=$4 body
  body=$( { head -c 40000 "$ev"; echo; echo "== after the step, every ${BEACON_EVERY:-15} s"; tail -c 20000 "$after" 2>/dev/null || true; } \
    | jq -Rs --arg name "watchdog: ${CHECK_NAME:-a cluster job}" --arg sha "$CHECK_SHA" \
        '{name: $name, head_sha: $sha, status: "completed", conclusion: "neutral",
          output: {title: "What a failed cluster job knew", summary: "tools/ci-watchdog.sh evidence: the make and the sampler from its files first, then the network and the cluster, then a beacon.", text: .}}')
  curl -sS -m 10 -X "$method" "$url" -H "Authorization: Bearer $GH_TOKEN" -H 'Accept: application/vnd.github+json' \
    -H 'X-GitHub-Api-Version: 2022-11-28' -d "$body"
}
check_api() { echo "${GITHUB_API_URL:-https://api.github.com}/repos/$GITHUB_REPOSITORY/check-runs"; }

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
  evidence)
    # $file is the make's log, $3 the sampler's file; what is gathered goes
    # beside the sampler's file, where the beacon reads it.
    watch=$3 ev="$3.evidence"
    # What the files hold, first and alone: runs 45130110944 and 45139152927
    # probed the network before their first POST, every probe ran to its
    # limit, and nothing reached GitHub.
    known "$file" "$watch" 2>&1 | sed -u -e 's/\x1b\[[0-9;]*[A-Za-z]//g' >"$ev"
    # By the API next, before any probe: the check run made from those
    # reads, then a beacon that outlives this step and sends what has been
    # gathered so far each time, so a command below that hangs loses
    # nothing, and whether it goes on saying anything is evidence too.
    posting=""
    if [ -n "${GH_TOKEN:-}" ] && [ -n "${CHECK_SHA:-}" ] && [ -n "${GITHUB_REPOSITORY:-}" ]; then
      posting=yes
      id=$(check_send POST "$(check_api)" "$ev" "$watch.after" 2>"$watch.post.err" | jq -r '.id // empty' 2>/dev/null || true)
      if [ -n "$id" ]; then echo "$id" >"$watch.check"; fi
      self="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
      setsid nohup "$self" beacon "$file" "$watch" </dev/null >"$watch.beacon.out" 2>&1 &
    fi
    # Then the probes, added to what the check run holds.
    {
      echo "== the network now"
      net_line
      echo "-- the cluster's subnet $SUBNET against the runner's own"
      overlaps || true
      net_state
      echo "-- containers"; bounded 4 docker ps -a --format '{{.Names}} {{.Status}}'
      echo "-- the kernel's last lines"; { timeout 3 sudo -n dmesg 2>/dev/null || timeout 3 dmesg 2>&1 || true; } | tail -n 25
      # A cluster the make gave up on is left running: why its pods are not
      # ready, and what its node can resolve and holds.
      echo "== the cluster, if it was left"
      if [ -s .e2e-k3d.kubeconfig ]; then
        KUBECONFIG=.e2e-k3d.kubeconfig bounded 8 kubectl get pods -A -o wide
        echo "-- its events"; KUBECONFIG=.e2e-k3d.kubeconfig bounded 8 kubectl get events -A --sort-by=.lastTimestamp | tail -n 40
      else
        echo "(no .e2e-k3d.kubeconfig here: the cluster was removed or never made)"
      fi
      for node in $(timeout 4 docker ps --format '{{.Names}}' 2>/dev/null | grep -E '^k3d-.*-server-0$'); do
        echo "-- $node: its resolver, a lookup, its images"
        bounded 8 docker exec "$node" sh -c 'cat /etc/resolv.conf; nslookup registry-1.docker.io 2>&1 | tail -n 6; crictl images 2>&1 | head -n 20'
      done
      echo "== the sampler's file: its start, then its last three samples"
      sed -n '1,/^=== 20/p' "$watch" 2>/dev/null | head -n 40
      awk '/^=== /{n++} {l[NR]=$0; s[NR]=n} END{for(i=1;i<=NR;i++) if(s[i]>n-3) print l[i]}' "$watch" 2>/dev/null | tail -n 80
    } 2>&1 | sed -u -e 's/\x1b\[[0-9;]*[A-Za-z]//g' >>"$ev"
    # Annotations: at most ten of a level a step, each cut to what one keeps.
    annotate() {
      local level=$1 title=$2 text
      text=$(tail -c 3500 | sed -e 's/%/%25/g' -e 's/\r/%0D/g' | awk 'BEGIN{ORS="%0A"} {print}')
      [ -z "$text" ] || echo "::$level title=$title::$text"
    }
    section() { awk -v want="$1" '/^== /{on = index($0, want) > 0; next} on' "$ev"; }
    section 'FAILED and gave-up' | annotate error "the make's FAILED and gave-up lines"
    section "the make's last 60" | tail -n 25 | annotate error "the make's last lines"
    section 'the network now' | annotate warning "the runner's network after the make"
    section 'the cluster, if it was left' | annotate warning "the cluster the make left"
    section "the sampler's file" | grep -E '^=== |^net: |^Mem:|^/dev/|[Oo]ut of memory|oom' | tail -n 30 | annotate warning "the watchdog's last samples"
    if [ -n "$posting" ]; then
      if [ -s "$watch.check" ]; then
        echo "ci-watchdog: the evidence is check run $(cat "$watch.check") on $CHECK_SHA, made before any probe; a beacon adds the probes' results to it"
      else
        echo "::warning::ci-watchdog: the first check run was not made; the beacon keeps trying. It said: $(tail -c 300 "$watch.post.err" 2>/dev/null | tr '\n' ' ')"
      fi
    else
      echo "ci-watchdog: no GH_TOKEN, CHECK_SHA or GITHUB_REPOSITORY; annotations only"
    fi
    ;;
  beacon)
    # Every 15 s for BEACON_MINUTES (default 30) or until the sampler's
    # directory goes: the check run made, if the evidence step's POST did
    # not make it, or brought up to date, and only then a line of what the
    # runner can reach and what its worker is doing.
    watch=$3 ev="$3.evidence" after="$3.after"
    id=$(cat "$watch.check" 2>/dev/null || true)
    api=$(check_api)
    deadline=$((SECONDS + ${BEACON_MINUTES:-30} * 60))
    : >>"$after"
    sleep "${BEACON_FIRST:-3}"
    while [ -d "$(dirname "$watch")" ] && [ "$SECONDS" -lt "$deadline" ]; do
      if [ -z "$id" ]; then
        id=$(check_send POST "$api" "$ev" "$after" | jq -r '.id // empty' 2>/dev/null || true)
        if [ -n "$id" ]; then echo "$id" >"$watch.check"; else echo "--- no check run yet" >>"$after"; fi
      else
        check_send PATCH "$api/$id" "$ev" "$after" >/dev/null 2>&1 || echo "--- the update did not go" >>"$after"
      fi
      {
        echo "--- $(date -u +%H:%M:%SZ) load $(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || echo '?') $(net_line)"
        # The worker, the listener and what the worker runs: age and state.
        bounded 3 ps -eo pid,ppid,etimes,stat,pcpu,args --sort=pid | grep -E 'Runner\.(Worker|Listener)|externals/node|upload' | grep -v grep | cut -c1-160 || true
      } >>"$after" 2>&1
      sleep "${BEACON_EVERY:-15}"
    done
    ;;
  probe)
    # $file names the fact, $3 is the make's log. A step that fails says the
    # fact does not hold. The fact is asked in a bounded child, so its
    # result reaches the step summary even when the asking hangs: the steps
    # are continue-on-error, and the API shows each as passed whatever it
    # found.
    self="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
    rc=0
    said=$(timeout "${PROBE_SECONDS:-15}" "$self" probe-one "$file" "$3" 2>&1) || rc=$?
    case $rc in
      0) result=ok ;;
      124) result=failed said=$(printf '%s\n%s' "$said" "no answer in ${PROBE_SECONDS:-15} s") ;;
      *) result=failed ;;
    esac
    [ -z "$said" ] || printf '%s\n' "$said"
    line=$(printf '%s\n' "$said" | grep -v '^[[:space:]]*$' | tail -n 1 | cut -c1-300 || true)
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
      printf -- '- probe `%s`: **%s**, %s\n' "$file" "$result" "${line:-no output}" >>"$GITHUB_STEP_SUMMARY" || true
    fi
    exit "$rc"
    ;;
  probe-one)
    case $file in
      gave-up) if grep -E 'gave up on|could not be made' "$3"; then false; else echo "no 'gave up on' or 'could not be made' line in $3"; fi ;;
      names) if names; then echo "registry-1.docker.io resolves"; else echo "registry-1.docker.io does not resolve"; false; fi ;;
      hub) r=$(answers "$HUB"); echo "$HUB: $r"; [ "${r%%/*}" != 000 ] ;;
      blob) r=$(answers "$BLOB"); echo "$BLOB: $r"; [ "${r%%/*}" != 000 ] ;;
      subnet) rc=0; overlaps || rc=$?; [ "$rc" -eq 1 ] ;;
      *) usage ;;
    esac
    ;;
  *) usage ;;
esac
