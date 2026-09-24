#!/usr/bin/env bash
# vps-disk-reclaim.sh — aggressive disk reclamation for the CI/deploy host.
#
# Housekeeping frees politely and still let the box hit 100% mid-build
# (2026-09-24: publish-images died on ENOSPC while "runner busy" guards kept
# every cache). This script is the heavy hand: it escalates through tiers of
# regeneratable content until the root filesystem is at or below the target
# occupancy, defaulting to 60%.
#
# Tiers, in escalation order — each runs only while the disk is still above
# target:
#   A  always safe      journals, tmp, logs, apt, snap, dangling docker state
#   B  idle lanes       runner _work/_diag/old bin.*, per-lane .cargo-target
#   C  shared caches    sccache, npm _cacache, cargo registry, playwright,
#                       pip, trunk — every byte re-downloads on next build
#   D  shared docker    full BuildKit prune + unused images — GATED on no
#                       Runner.Worker anywhere (a live build reads its cache);
#                       --force overrides the gate, not the consequence
#
# Never removed, under any flag:
#   - images referenced by any container, plus the two newest per repository
#     (deployed + one rollback — the retention floor everywhere)
#   - mounted volumes, postgres data, the .android SDK, toolchains pinned by
#     a rust-toolchain.toml under any runner _work or ~/dev
#
# Sustained target (operator, 2026-09-24): 50–60% used, 70% ceiling. This
# script is the enforcer of last resort — it is scheduled daily at 03:30
# UTC on the box:
#   /etc/cron.d/vps-disk-reclaim:
#     30 3 * * * root /usr/local/sbin/vps-disk-reclaim.sh --target 60 \
#       >> /var/log/vps-disk-reclaim.log 2>&1
# Reclaim alone cannot hold the number against sources that regrow every
# job, so three caps sit upstream of it:
#   - /etc/docker/daemon.json (ops/docker-daemon.json): BuildKit GC keeps
#     build cache under 3 GB and self-evicts when free space drops below
#     8 GB; container logs rotate at 20 MB x3.
#   - SCCACHE_CACHE_SIZE=2G in each runner's .env — the default is 10 GB
#     per the sccache docs, which is a third of the disk by itself.
#   - vps-housekeeping.sh PRESSURE_PCT 70 / idle floor 65 — polite clearing
#     starts before reclaim's hammer is ever needed.
#
# Usage:
#   vps-disk-reclaim.sh [--target PCT] [--force] [--report]
#     --target PCT  stop reclaiming once usage is at or below PCT (default 60)
#     --force       run tier D even while runners are busy (kills builds)
#     --report      dry-run: log what would be freed, delete nothing
#
# Install: /usr/local/sbin/vps-disk-reclaim.sh (root). Source of truth:
# crowdrelay repo, ops/vps-disk-reclaim.sh.

set -Eeuo pipefail

TARGET=60
FORCE=0
REPORT=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --target) TARGET="$2"; shift 2 ;;
    --force)  FORCE=1; shift ;;
    --report) REPORT=1; shift ;;
    *) echo "usage: $0 [--target PCT] [--force] [--report]" >&2; exit 2 ;;
  esac
done
[[ "$TARGET" =~ ^[0-9]+$ && "$TARGET" -ge 20 && "$TARGET" -le 95 ]] \
  || { echo "--target must be 20..95" >&2; exit 2; }

log() { printf '%s %s\n' "$(date '+%F %T')" "$*" >&2; }
mb()  { awk -v b="$1" 'BEGIN { printf "%d", b/1024 }'; }
pct() { df --output=pcent / 2>/dev/null | tail -1 | tr -dc '0-9'; }
avail() { df --output=avail / 2>/dev/null | tail -1 | tr -dc '0-9'; }
over()  { local p; p="$(pct)"; [[ -z "$p" || "$p" -gt "$TARGET" ]]; }

sweep() {
  local label="$1"; shift
  if [[ "$REPORT" -eq 1 ]]; then log "  [dry] $label"; else "$@" >/dev/null 2>&1 || true; log "  swept: $label"; fi
}

runner_busy() { pgrep -f 'Runner\.Worker' >/dev/null 2>&1; }
lane_busy() { pgrep -af 'Runner\.Worker' 2>/dev/null | grep -qF -- "${1%/}/bin"; }
lane_name() {
  tr -d '\357\273\277' <"$1/.runner" 2>/dev/null \
    | sed -n 's/.*"agentName" *: *"\([^"]*\)".*/\1/p'
}

START_PCT="$(pct)"; START_AVAIL="$(avail)"
log "--- reclaim start (disk ${START_PCT:-?}% used, avail $(mb "${START_AVAIL:-0}") MB, target <=${TARGET}%)$(runner_busy && echo ', runner busy')$([[ $REPORT -eq 1 ]] && echo ', DRY-RUN')$([[ $FORCE -eq 1 ]] && echo ', FORCE') ---"

# ══ TIER A — always safe ══════════════════════════════════════════════════
if over; then
  log "tier A — system junk"
  sweep "journald <=50M"           journalctl --vacuum-size=50M
  sweep "tmp/var-tmp >12h"         bash -c 'find /tmp /var/tmp -xdev -mindepth 1 -mtime +0.5 -delete 2>/dev/null'
  sweep "rotated+old logs"         bash -c 'find /var/log -xdev -type f \( -name "*.gz" -o -name "*.1" -o -name "*.old" \) -delete; find /var/log -xdev -type f -size +50M -mtime +2 -delete 2>/dev/null'
  sweep "coredumps"                bash -c 'rm -rf /var/lib/systemd/coredump/* /var/crash/* 2>/dev/null'
  sweep "apt clean+autoremove"     bash -c 'apt-get clean 2>/dev/null; apt-get -y autoremove 2>/dev/null'
  sweep "snap disabled revisions"  bash -c 'command -v snap >/dev/null && snap list --all | awk "/disabled/ {print \$1, \$3}" | while read -r n r; do snap remove "$n" --revision="$r"; done 2>/dev/null'
  sweep "dangling images"          docker image prune -f
  sweep "unattached volumes"       docker volume prune -f
  sweep "GHA service containers"   bash -c 'docker ps -a --format "{{.Names}}" | grep -E "^[0-9a-f]{32}_" | xargs -r docker rm -f'
  sweep "exited containers >24h"   bash -c 'docker ps -a --filter "status=exited" --filter "status=created" --format "{{.ID}} {{.Status}}" | grep -Ei "hours|days|weeks|months" | cut -d" " -f1 | xargs -r docker rm -f'
fi

# ══ TIER B — idle-lane local state ════════════════════════════════════════
if over; then
  log "tier B — idle runner lanes"
  for dir in /home/*/actions-runner*; do
    [[ -d "$dir" ]] || continue
    if lane_busy "$dir"; then log "  $dir busy — kept"; continue; fi
    # A dispatched job gets a few minutes between directory creation and the
    # worker spawn — the housekeeping 10-minute grace, kept here for the
    # same reason.
    recent="$(find "$dir/_work" -mindepth 1 -mmin -10 2>/dev/null | head -1)"
    if [[ -n "$recent" ]]; then log "  $dir/_work touched <10m — kept"; continue; fi
    sweep "$dir/_work"  rm -rf "$dir/_work"/*  "$dir/_work"/.[!.]*
    sweep "$dir/_diag"  bash -c "rm -rf '$dir/_diag'/* 2>/dev/null"
    # bin.* side dirs from runner self-updates — keep the live one only.
    live="$(pgrep -af 'Runner\.Listener' 2>/dev/null | grep -oF "$dir/bin" | head -1 || true)"
    for b in "$dir"/bin.*; do
      [[ -d "$b" ]] || continue
      [[ -n "$live" && "$b" == "$live".* ]] && continue
      sweep "old $b" rm -rf "$b"
    done
    agent="$(lane_name "$dir" || true)"
    [[ -n "$agent" && -d "/home/ubuntu/.cargo-target/$agent" ]] \
      && sweep ".cargo-target/$agent" rm -rf "/home/ubuntu/.cargo-target/$agent"
  done
fi

# ══ TIER C — shared regeneratable caches ══════════════════════════════════
if over; then
  log "tier C — shared caches (re-downloaded on demand)"
  sweep "sccache"            bash -c 'rm -rf /home/*/.cache/sccache/* /root/.cache/sccache/* 2>/dev/null'
  sweep "npm _cacache"       bash -c 'rm -rf /home/*/.npm/_cacache /root/.npm/_cacache 2>/dev/null'
  sweep "cargo registry+git" bash -c 'rm -rf /home/*/.cargo/registry /home/*/.cargo/git /root/.cargo/registry /root/.cargo/git 2>/dev/null'
  sweep "playwright browsers" bash -c 'rm -rf /home/*/.cache/ms-playwright /root/.cache/ms-playwright 2>/dev/null'
  sweep "pip+trunk+misc cache" bash -c 'rm -rf /home/*/.cache/pip /home/*/.cache/trunk /root/.cache/pip /root/.cache/trunk 2>/dev/null'
  # Rustup toolchains: keep the channels any rust-toolchain.toml under the
  # runners' _work or ~/dev pins, plus stable/beta/nightly. A box that just
  # wiped _work pins nothing — the three channel names are the floor.
  pins="$( { cat /home/*/actions-runner*/_work/*/*/rust-toolchain.toml 2>/dev/null; \
             for home in /home/*; do cat "$home"/dev/*/rust-toolchain.toml 2>/dev/null; done; } \
          | grep -oE 'channel *= *"[^"]+"' | cut -d'"' -f2 | sort -u || true)"
  keep="stable beta nightly $pins"
  for tc in /home/*/.rustup/toolchains/*/ /root/.rustup/toolchains/*/; do
    [[ -d "$tc" ]] || continue
    name="$(basename "$tc")"
    skip=0
    for k in $keep; do [[ "$name" == "$k"* ]] && skip=1; done
    [[ "$skip" -eq 0 ]] && sweep "rustup $name" rm -rf "$tc"
  done
fi

# ══ TIER D — shared docker state (busy-gated) ═════════════════════════════
if over; then
  if runner_busy && [[ "$FORCE" -eq 0 ]]; then
    log "tier D skipped — a Runner.Worker is live (rerun idle or pass --force)"
  else
    [[ "$FORCE" -eq 1 ]] && runner_busy && log "tier D FORCED over a live runner — builds may die"
    log "tier D — docker state"
    # Deployed + one rollback is the floor everywhere. Resolve the keep set
    # before touching anything: every image a container references, plus the
    # two newest per repository name.
    keep="$(mktemp)"; trap 'rm -f "$keep"' EXIT
    docker ps -aq 2>/dev/null | xargs -r docker inspect --format '{{.Image}}' 2>/dev/null >>"$keep" || true
    docker images --format '{{.Repository}} {{.ID}} {{.CreatedAt}}' 2>/dev/null \
      | sort -k1,1 -k3,3r | awk '{c[$1]++; if (c[$1] <= 2) print $2}' >>"$keep" || true
    # Every other unused image goes.
    docker images --format '{{.ID}}' 2>/dev/null | while read -r id; do
      grep -q "$id" "$keep" || docker rmi -f "$id" >/dev/null 2>&1 || true
    done
    log "  swept: unused images (kept in-use + 2 newest/repo)"
    sweep "ALL BuildKit cache" docker builder prune -af
    sweep "docker networks"    docker network prune -f
  fi
fi

END_PCT="$(pct)"; END_AVAIL="$(avail)"
freed=$(( ${END_AVAIL:-0} - ${START_AVAIL:-0} ))
log "--- reclaim end (disk ${END_PCT:-?}% used, avail $(mb "${END_AVAIL:-0}") MB, freed $(mb "$freed") MB) ---"
over && { log "STILL ABOVE TARGET — inspect: du -x -d1 -h / | sort -rh | head"; exit 1; } || true
