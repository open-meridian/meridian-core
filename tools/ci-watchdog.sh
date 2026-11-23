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
#   ci-watchdog.sh start <file>   start the sampler, detached, and return;
#                                 a notice names the runner's image, kernel
#                                 and host, and GitHub's API's address is
#                                 kept in <file>.api, which the evidence
#                                 and the beacon give curl (--resolve)
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
#   ci-watchdog.sh proof <which> <make-log>
#                                 after a failed make and before anything
#                                 touches the network: one fact from file
#                                 reads and local commands alone, as an
#                                 ::error:: annotation where it does not
#                                 hold and a ::notice:: where it does, and
#                                 as the step's result. gave-up: which of
#                                 the make's `ready` waits gave up; kernel:
#                                 the kernel's blocked-task, hung-task,
#                                 netlink and unregister_netdevice lines;
#                                 answers: whether `ip -o link` and `docker
#                                 ps` each answer within 3 s, said to the
#                                 step's outputs as stalled=true|false.
#                                 Each is also appended to proof.log beside
#                                 the make's log, the annotations' source,
#                                 and to the step summary. A command is
#                                 asked in a detached child that is never
#                                 waited for: a process stuck in the kernel
#                                 ignores every signal, and `timeout` waits
#                                 for its child, so on the runs that died
#                                 `timeout 3 ip ...` would hang itself
#   ci-watchdog.sh probe <which> <make-log>
#                                 one fact as a step's result, which arrives
#                                 when nothing else does: names, hub, blob,
#                                 subnet. Each writes ok or
#                                 failed, with its output's last line, to
#                                 the step summary too. The steps are not
#                                 continue-on-error, so the API shows each
#                                 as passed, failed or timed out
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

usage() { echo "usage: $0 start|tail <file> | make-tail <make-log> <out> | evidence <make-log> <file> | proof|probe <which> <make-log>" >&2; exit 2; }
case ${1:-}:$# in
  start:2|loop:2|tail:2|make-tail:3|evidence:3|beacon:3|proof:3|probe:3|probe-one:3) ;;
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

# Whether a command answers within $1 s: asked in a detached child, in its
# own session, that is never waited for, so one stuck in the kernel is left
# behind rather than holding this script. Its output is then in $within_out
# and its exit in $within_rc; 1 when it did not answer in time.
within() {
  local secs=$1 dir i=0 detach=""
  shift
  dir=$(mktemp -d)
  if command -v setsid >/dev/null 2>&1; then detach=setsid; fi
  $detach sh -c 'd=$1; shift; "$@" >"$d/out" 2>&1; echo $? >"$d/rc.part"; mv "$d/rc.part" "$d/rc"' sh "$dir" "$@" \
    </dev/null >/dev/null 2>&1 &
  while [ ! -f "$dir/rc" ] && [ "$i" -lt "$((secs * 10))" ]; do sleep 0.1; i=$((i + 1)); done
  if [ -f "$dir/rc" ]; then
    within_rc=$(cat "$dir/rc") within_out=$(cat "$dir/out" 2>/dev/null || true)
    rm -rf "$dir"
    return 0
  fi
  within_rc="" within_out=""
  return 1
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
  # The API's address as the watchdog's start found it, where it did: no
  # name is looked up after a failure.
  local resolve=()
  if [ -s "${watch:-}.api" ]; then resolve=(--resolve "$(api_host):443:$(head -n 1 "$watch.api")"); fi
  curl -sS -m 10 ${resolve[@]+"${resolve[@]}"} -X "$method" "$url" -H "Authorization: Bearer $GH_TOKEN" -H 'Accept: application/vnd.github+json' \
    -H 'X-GitHub-Api-Version: 2022-11-28' -d "$body"
}
check_api() { echo "${GITHUB_API_URL:-https://api.github.com}/repos/$GITHUB_REPOSITORY/check-runs"; }
api_host() { local h=${GITHUB_API_URL:-https://api.github.com}; h=${h#*://}; echo "${h%%/*}"; }

case $cmd in
  start)
    # Which runner this is, as an annotation, which arrives when the job's
    # log does not: the runs that died since 2026-11-17 came from two image
    # versions, and their kernel and host were never known.
    echo "::notice title=the runner::image ${ImageOS:-?} ${ImageVersion:-?}, kernel $(uname -r 2>/dev/null || echo '?'), host $(hostname 2>/dev/null || echo '?'), runner ${RUNNER_NAME:-?}"
    mkdir -p "$(dirname "$file")"
    # The API's address, looked up now while names resolve, kept beside the
    # sampler's file for the failure's check run (curl --resolve): on the
    # runs that died every new process touching the network hung after the
    # make, and a lookup then is one more chance to.
    addr=$(timeout 4 getent ahostsv4 "$(api_host)" 2>/dev/null | awk 'NR == 1 { print $1 }' || true)
    if [ -n "$addr" ]; then
      echo "$addr" >"$file.api"
      echo "ci-watchdog: $(api_host) is $addr, kept in $file.api for the failure's check run"
    else
      echo "::warning::ci-watchdog: $(api_host) did not resolve in 4 s; the failure's check run will look it up"
    fi
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
  proof)
    # $file names the fact, $3 is the make's log. Nothing here touches the
    # network, and no fact waits on its children for more than 8 s in all.
    make=$3 proof="$(dirname "$3")/proof.log"
    say() {
      # $1 error or notice, $2 the title, the text on stdin: the annotation,
      # the proof file and the step summary.
      local level=$1 title=$2 text
      text=$(cat)
      { echo "== $(date -u +%H:%M:%SZ) $level: $title"; printf '%s\n' "$text"; } >>"$proof" 2>/dev/null || true
      echo "::$level title=$title::$(printf '%s' "$text" | tail -c 3500 | sed -e 's/%/%25/g' -e 's/\r/%0D/g' | awk 'BEGIN{ORS="%0A"} {print}')"
      if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        printf -- '- proof `%s`: **%s**, %s\n' "$file" "$level" "$title" >>"$GITHUB_STEP_SUMMARY" || true
      fi
    }
    case $file in
      gave-up)
        # The Makefile's ready(): "waiting for <what>, at most <n>s", then
        # "gave up on <what> after <n>s" where it gives up.
        gave=$(grep -h -E 'gave up on |could not be made' "$make" 2>/dev/null | head -n 3 || true)
        last=$(grep -h -E 'waiting for .*, at most ' "$make" 2>/dev/null | tail -n 1 || true)
        if [ -n "$gave" ]; then
          printf '%s\n' "$gave" | say error "the make gave up: $(printf '%s' "$gave" | head -n 1 | sed -e 's/^e2e-cluster-k3d: //' -e 's/\. It said:$//' | cut -c1-120)"
          exit 1
        fi
        echo "the last wait the make began: ${last:-none}" | say notice "no ready wait gave up"
        ;;
      kernel)
        # As root where it can be, since dmesg is root's on GitHub's runners;
        # the journal's kernel lines where dmesg says nothing.
        pattern='blocked for more than|hung_task|INFO: task |netlink|unregister_netdevice'
        read_by=""
        # Each try's seconds first, 8 in all, inside the step's 10: sudo can
        # itself wait on a name lookup.
        for how in "4 sudo -n dmesg" "2 dmesg" "2 journalctl -k -q --no-pager"; do
          # shellcheck disable=SC2086 # the words are the seconds and the command
          if within $how && [ "$within_rc" = 0 ] && [ -n "$within_out" ]; then read_by=${how#* }; break; fi
        done
        if [ -z "$read_by" ]; then
          echo "sudo -n dmesg (4 s), dmesg (2 s) and journalctl -k (2 s) gave nothing" | say error "the kernel's log could not be read"
          exit 1
        fi
        found=$(printf '%s\n' "$within_out" | grep -i -E "$pattern" | tail -n 30 || true)
        if [ -n "$found" ]; then
          printf '%s\n' "$found" | say error "the kernel logged $(printf '%s\n' "$found" | grep -c .) blocked-task, hung-task, netlink or unregister_netdevice lines"
          exit 1
        fi
        echo "$(printf '%s\n' "$within_out" | grep -c .) lines read by $read_by" | say notice "the kernel logged no blocked task, hung task, netlink or unregister_netdevice line"
        ;;
      answers)
        stalled=false
        report=""
        for how in "ip -o link" "docker ps"; do
          # shellcheck disable=SC2086 # the words are the command
          if within 3 $how; then
            report="$report$how: answered in 3 s (exit $within_rc, $(printf '%s\n' "$within_out" | grep -c .) lines)
"
          else
            stalled=true
            report="$report$how: no answer in 3 s, left running
"
          fi
        done
        [ -z "${GITHUB_OUTPUT:-}" ] || echo "stalled=$stalled" >>"$GITHUB_OUTPUT"
        if [ "$stalled" = true ]; then
          printf '%s' "$report" | say error "the runner is stalled: $(printf '%s' "$report" | grep 'no answer' | cut -d: -f1 | paste -sd, - | sed 's/,/ and /') did not answer in 3 s"
          exit 1
        fi
        printf '%s' "$report" | say notice "ip and docker each answered within 3 s"
        ;;
      *) usage ;;
    esac
    ;;
  probe)
    # $file names the fact, $3 is the make's log. A step that fails says the
    # fact does not hold. The fact is asked in a bounded child, so its
    # result reaches the step summary even when the asking hangs.
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
      names) if names; then echo "registry-1.docker.io resolves"; else echo "registry-1.docker.io does not resolve"; false; fi ;;
      hub) r=$(answers "$HUB"); echo "$HUB: $r"; [ "${r%%/*}" != 000 ] ;;
      blob) r=$(answers "$BLOB"); echo "$BLOB: $r"; [ "${r%%/*}" != 000 ] ;;
      subnet) rc=0; overlaps || rc=$?; [ "$rc" -eq 1 ] ;;
      *) usage ;;
    esac
    ;;
  *) usage ;;
esac
