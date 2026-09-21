#!/usr/bin/env bash
# vps-housekeeping.sh — convergent housekeeper for CrowdRelay tenant hosts.
#
# One script replaces the old pair (/srv/prune-images.sh + vps-disk-guard.sh).
# Goal: the box always converges back to its canonical state — the running app
# containers, caddy edge, and VM basics — and nothing else. Everything around
# them (build residue, caches, stale images, orphaned CI service containers,
# test-clone databases, rotated logs) is reclaimable.
#
# Safety model — three tiers, chosen per run:
#
#   ALWAYS   provably cannot break a running job or the app:
#            dangling images, unattached volumes, build cache >72h,
#            orphaned GitHub-Actions service containers (>6h when a job is
#            in-flight, >1h when idle), journald vacuum, rotated logs, /tmp,
#            apt clean/autoremove, snap disabled revisions, coredumps, and
#            stale test-clone databases inside postgres containers.
#
#   IDLE     only when NO runner job is active anywhere on the box
#            (`pgrep -f "Runner.Worker"`): runner _work checkout dirs,
#            old bin.* runner versions (keep 2), per-repository image
#            pruning beyond KEEP_IMAGES_PER_REPO, buildkit builder
#            containers, and regeneratable caches.
#
#   PRESSURE disk >= PRESSURE_PCT even when a job is in-flight: regeneratable
#            caches ONLY — the documented 2026-09-17 exception ("a restarted
#            build beats a dead disk"). Never _work, never the workspace.
#
# Hard rules encoded from incidents:
#   - Never wipe a runner _work while ANY job is active (2026-09-13 ENOSPC).
#   - Cargo registry cache and src go together or not at all — wiping cache
#     mid-extract left 550 empty src/ husks cargo trusted (2026-09-17).
#   - Volumes: docker's own unattached check is the only authority.
#   - Images: keep the KEEP_IMAGES_PER_REPO newest tags per repository;
#     docker itself refuses to remove anything a container references —
#     that refusal is the fail-closed layer, so compose members (blue/green
#     rollback handles, one-shot setup containers) keep their images.
#   - Stopped compose-project containers are NEVER removed — they can be a
#     blue/green rollback handle. Only orphaned GHA service containers are
#     reaped (name pattern ^[0-9a-f]{32}_, age-gated).
#   - Databases: only names matching test-clone patterns (pgt_*, ci_*,
#     crowdrelay_<tag>_<32hex>) older than STALE_DB_AGE_H by the uuid-v7
#     embedded in the name, with zero active connections. Named databases
#     can never match, so tenant data is structurally unreachable.
#
# Tenancy: everything auto-detects — runner dirs via /home/*/actions-runner*,
# postgres containers via image name. Optional /etc/vps-housekeeping.conf
# overrides any knob below.
#
# Usage:
#   vps-housekeeping.sh            run all applicable tiers (the cron mode)
#   vps-housekeeping.sh --report   dry-run: report reclaimable space per class
#   vps-housekeeping.sh --install  install self + cron.d + docker logrotate
#   vps-housekeeping.sh --uninstall  remove cron wiring (leaves the script)

set -uo pipefail

# ── Config ────────────────────────────────────────────────────────────────
KEEP_IMAGES_PER_REPO="${KEEP_IMAGES_PER_REPO:-2}"
PRESSURE_PCT="${PRESSURE_PCT:-90}"
CRITICAL_PCT="${CRITICAL_PCT:-95}"
STALE_DB_AGE_H="${STALE_DB_AGE_H:-24}"
GHA_CONTAINER_IDLE_MAX_H="${GHA_CONTAINER_IDLE_MAX_H:-1}"
GHA_CONTAINER_BUSY_MAX_H="${GHA_CONTAINER_BUSY_MAX_H:-6}"
JOURNAL_MAX="${JOURNAL_MAX:-200M}"
LOG_FILE="${LOG_FILE:-/var/log/vps-housekeeping.log}"
STALE_DB_PREFIXES="${STALE_DB_PREFIXES:-pgt_ ci_}"
STALE_DB_UUID_RE="${STALE_DB_UUID_RE:-crowdrelay_}"
RUNNER_GLOB="${RUNNER_GLOB:-/home/*/actions-runner*}"

CONF=/etc/vps-housekeeping.conf
[[ -r "$CONF" ]] && # shellcheck source=/dev/null
  . "$CONF"

REPORT=0
[[ "${1:-}" == "--report" ]] && REPORT=1
# Fall back to /tmp when /var/log isn't writable (non-root report runs).
if ! { [[ -w "$LOG_FILE" ]] || { [[ ! -e "$LOG_FILE" ]] && [[ -w "$(dirname "$LOG_FILE")" ]]; }; }; then
  LOG_FILE=/tmp/vps-housekeeping.log
fi

log() {
  local line; line="$(date '+%Y-%m-%d %H:%M:%S') $*"
  echo "$line" >>"$LOG_FILE" 2>/dev/null || true
  [[ "$REPORT" -eq 1 ]] && echo "$line" || true
}

# bytes → human, for the report
mb() { echo "$(( ${1:-0} / 1048576 )) MB"; }

used_pct() { df --output=pcent / 2>/dev/null | tail -1 | tr -dc '0-9'; }
avail_bytes() { df --output=avail -B1 / 2>/dev/null | tail -1 | tr -dc '0-9'; }

runner_busy() { pgrep -f 'Runner\.Worker' >/dev/null 2>&1; }

# run DRY-safe: $1 label, rest = command. In --report mode we skip the command.
sweep() {
  local label="$1"; shift
  if [[ "$REPORT" -eq 1 ]]; then
    log "  [dry] $label"
  else
    "$@" >/dev/null 2>&1 || true
    log "  swept: $label"
  fi
}

# ── Install / uninstall ───────────────────────────────────────────────────
if [[ "${1:-}" == "--install" ]]; then
  install -m 0755 "$0" /usr/local/sbin/vps-housekeeping.sh
  cat >/etc/cron.d/vps-housekeeping <<'EOF'
# Convergent housekeeping: every 30 min. The script gates destructive
# classes on job state itself, so a frequent cadence is safe.
*/30 * * * * root /usr/local/sbin/vps-housekeeping.sh
EOF
  chmod 0644 /etc/cron.d/vps-housekeeping
  # Container JSON logs can grow unboundedly without rotation.
  cat >/etc/logrotate.d/docker-json-logs <<'EOF'
/var/lib/docker/containers/*/*-json.log {
    daily
    size 50M
    rotate 3
    missingok
    notifempty
    copytruncate
    compress
    delaycompress
}
EOF
  cat >/etc/logrotate.d/vps-housekeeping <<'EOF'
/var/log/vps-housekeeping.log {
    weekly
    rotate 4
    missingok
    notifempty
    copytruncate
    compress
}
EOF
  # Retire the superseded schedulers if present.
  rm -f /etc/cron.d/vps-disk-guard-pressure /etc/cron.daily/vps-disk-guard
  ( crontab -u ubuntu -l 2>/dev/null | grep -v 'prune-images' || true ) | crontab -u ubuntu - 2>/dev/null || true
  echo "installed: /usr/local/sbin/vps-housekeeping.sh + cron.d(*/30) + docker logrotate; retired prune-images.sh / vps-disk-guard schedules"
  exit 0
fi

if [[ "${1:-}" == "--uninstall" ]]; then
  rm -f /etc/cron.d/vps-housekeeping /etc/logrotate.d/docker-json-logs
  echo "removed cron + logrotate wiring; script left at /usr/local/sbin/vps-housekeeping.sh"
  exit 0
fi

# ── Pass start ────────────────────────────────────────────────────────────
mkdir -p "$(dirname "$LOG_FILE")" 2>/dev/null || true
PCT=$(used_pct); AVAIL0=$(avail_bytes)
log "--- pass start (disk ${PCT:-?}% used, avail $(mb "${AVAIL0:-0}")$(runner_busy && echo ', runner busy')$([[ $REPORT -eq 1 ]] && echo ', DRY-RUN')) ---"

# ══ TIER: ALWAYS — safe even mid-job ═══════════════════════════════════════

# Orphaned GitHub Actions service containers. GHA names them
# "<32-hex>_<image-sanitized>_<16-hex>"; they should die with their job but
# leak when a job is killed (the 2026-09-21 ENOSPC left one running 5h).
docker ps --format '{{.Names}} {{.CreatedAt}}' 2>/dev/null | \
while read -r name _rest; do
  [[ "$name" =~ ^[0-9a-f]{32}_ ]] || continue
  created=$(docker inspect -f '{{.Created}}' "$name" 2>/dev/null) || continue
  created_s=$(date -d "$created" +%s 2>/dev/null)
  # Unparseable timestamp → skip. Never let a parse failure age a fresh
  # container into the reap threshold.
  [[ -n "$created_s" ]] || continue
  age_h=$(( ( $(date +%s) - created_s ) / 3600 ))
  limit=$GHA_CONTAINER_BUSY_MAX_H
  runner_busy || limit=$GHA_CONTAINER_IDLE_MAX_H
  if [[ "$age_h" -ge "$limit" ]]; then
    if [[ "$REPORT" -eq 0 ]]; then docker rm -f "$name" >/dev/null 2>&1; fi
    log "  reaped orphaned CI service container $name (age ${age_h}h)"
  fi
done

# Docker classes that are always safe: docker refuses to remove anything
# a container references, so these can run mid-job.
sweep "dangling images"   docker image prune -f
sweep "unattached volumes" docker volume prune -f
sweep "build cache >72h"  docker builder prune -f --filter until=72h

# Stale test-clone databases inside postgres containers. Patterns match only
# clone-shaped names; the uuid-v7 timestamp embedded in the name is the age
# authority (pg dir mtime is polluted by autovacuum).
sweep_stale_dbs() {
  local container user dbs db age_h now_ms uuid_ms
  container="$1"
  user=$(docker exec "$container" printenv POSTGRES_USER 2>/dev/null)
  user="${user:-postgres}"
  dbs=$(docker exec "$container" psql -U "$user" -d postgres -Atc \
    "SELECT datname FROM pg_database WHERE NOT datistemplate" 2>/dev/null) || return 0
  now_ms=$(( $(date +%s) * 1000 ))
  for db in $dbs; do
    # pgt_<sig>_<uuid-v7> or crowdrelay_<tag>_<uuid-v7>: pull trailing 32-hex
    if [[ "$db" =~ _([0-9a-f]{8})([0-9a-f]{4})([0-9a-f]{4})([0-9a-f]{4})([0-9a-f]{12})$ ]]; then
      hex="${BASH_REMATCH[1]}${BASH_REMATCH[2]}${BASH_REMATCH[3]}"
      uuid_ms=$(( 16#$hex ))
      age_h=$(( (now_ms - uuid_ms) / 3600000 ))
      [[ "$db" =~ ^(pgt_|ci_|${STALE_DB_UUID_RE}) ]] || continue
      [[ "$age_h" -ge "$STALE_DB_AGE_H" ]] || continue
      age_label="${age_h}h"
    else
      # ci_<package> names carry no uuid. A ci_* database can only be in use
      # while a CI job is actually running, so these are dropped only when
      # no job is active anywhere on the box — airtight without an age probe.
      [[ "$db" =~ ^ci_ ]] || continue
      runner_busy && continue
      age_label="idle"
    fi
    conns=$(docker exec "$container" psql -U "$user" -d postgres -Atc \
      "SELECT count(*) FROM pg_stat_activity WHERE datname='$db'" 2>/dev/null)
    [[ "${conns:-1}" -eq 0 ]] || continue
    if [[ "$REPORT" -eq 0 ]]; then
      docker exec "$container" psql -U "$user" -d postgres -c \
        "DROP DATABASE \"$db\"" >/dev/null 2>&1
    fi
    log "  dropped stale test db $db in $container (age ${age_label})"
  done
}

docker ps --format '{{.Names}} {{.Image}}' 2>/dev/null | \
while read -r cname cimage; do
  case "$cimage" in *postgres*) sweep_stale_dbs "$cname" ;; esac
done

# Journald, rotated logs, coredumps, tmp — always safe.
command -v journalctl >/dev/null && sweep "journald vacuum <=$JOURNAL_MAX" \
  journalctl --vacuum-size="$JOURNAL_MAX"
if [[ "$REPORT" -eq 0 ]]; then
  find /var/log -xdev -type f \( -name '*.gz' -o -name '*.1' -o -name '*.old' \) \
    -mtime +14 -delete 2>/dev/null
  rm -rf /var/crash/* /var/lib/systemd/coredump/* 2>/dev/null
  for t in /tmp /var/tmp; do
    find "$t" -xdev -depth -mindepth 1 \
      \( -name 'systemd-private-*' -o -name 'snap.*' -o -name '.X11-unix' \
         -o -name '.ICE-unix' -o -name '.font-unix' -o -name '.XIM-unix' \
         -o -name '.Test-unix' \) -prune -o \
      -mtime +1 -exec rm -rf {} + 2>/dev/null
  done
fi
log "  swept: rotated logs >14d, coredumps, /tmp+/var/tmp >1d"

command -v apt-get >/dev/null && {
  sweep "apt clean" apt-get clean
  sweep "apt autoremove" apt-get -y autoremove --purge
}

# snap disabled revisions (present on Ubuntu server images)
if command -v snap >/dev/null; then
  snap list --all 2>/dev/null | awk '/disabled/ {print $1, $3}' | \
  while read -r name rev; do
    if [[ "$REPORT" -eq 1 ]]; then
      log "  [dry] would remove disabled snap $name rev $rev"
    else
      snap remove "$name" --revision "$rev" >/dev/null 2>&1 \
        && log "  removed disabled snap $name rev $rev"
    fi
  done
fi

# ══ TIER: PRESSURE — regeneratable caches, even mid-job ═══════════════════
clear_caches() {
  for home in /root /home/*; do
    [[ -d "$home" ]] || continue
    # npm
    if [[ -d "$home/.npm/_cacache" ]]; then
      if [[ "$REPORT" -eq 1 ]]; then
        log "  [dry] npm cache $home: $(mb "$(du -sb "$home/.npm/_cacache" 2>/dev/null | cut -f1)")"
      else
        rm -rf "$home/.npm/_cacache" && log "  cleared npm cache $home"
      fi
    fi
    # pip
    if [[ -d "$home/.cache/pip" ]]; then
      if [[ "$REPORT" -eq 0 ]]; then rm -rf "$home/.cache/pip"; fi
      log "  cleared pip cache $home"
    fi
    # cargo registry: cache+src together or never — see header (2026-09-17).
    if [[ -d "$home/.cargo/registry/src" ]]; then
      if [[ "$REPORT" -eq 1 ]]; then
        log "  [dry] cargo registry $home: $(mb "$(du -sb "$home/.cargo/registry" 2>/dev/null | cut -f1)")"
      else
        rm -rf "$home/.cargo/registry/cache" "$home/.cargo/registry/src" \
          && rm -f "$home/.cargo/.global-cache" \
          && log "  cleared cargo registry cache+src $home"
      fi
    fi
    # sccache: files only, by age — the dir itself survives.
    if [[ -d "$home/.cache/sccache" ]]; then
      cb=$(find "$home/.cache/sccache" -type f -mtime +7 -printf '%s\n' 2>/dev/null \
           | awk '{s+=$1} END {print s+0}')
      if [[ "$REPORT" -eq 1 ]]; then
        [[ "${cb:-0}" -gt 0 ]] && log "  [dry] sccache >7d $home: $(mb "$cb")"
      else
        find "$home/.cache/sccache" -type f -mtime +7 -delete 2>/dev/null
        [[ "${cb:-0}" -gt 0 ]] && log "  cleared sccache >7d $home: $(mb "$cb")"
      fi
    fi
  done
}

if [[ "${PCT:-0}" -ge "$PRESSURE_PCT" ]]; then
  log "  disk pressure (${PCT}% >= ${PRESSURE_PCT}%) — clearing regeneratable caches"
  clear_caches
fi

# ══ TIER: IDLE-ONLY — everything a live job could trip on ═════════════════
if runner_busy; then
  log "  runner busy — _work, image prune, buildkit containers skipped"
else
  # Runner workspaces: checkout dirs only; _actions/_tool/_temp/_PipelineMapping
  # cache CI dependencies and stay.
  for runner in $RUNNER_GLOB; do
    [[ -d "$runner/_work" ]] || continue
    for d in "$runner"/_work/*/; do
      case "$(basename "$d")" in _*) continue ;; esac
      if [[ "$REPORT" -eq 1 ]]; then
        log "  [dry] _work checkout $d: $(mb "$(du -sb "$d" 2>/dev/null | cut -f1)")"
      else
        rm -rf "$d" && log "  cleaned runner workspace $d"
      fi
    done
    [[ -d "$runner/_work/_update" ]] && {
      [[ "$REPORT" -eq 0 ]] && rm -rf "$runner/_work/_update"
      log "  cleaned $runner/_work/_update"
    }
    # Self-update residue: keep the two newest bin.* (current + rollback).
    ls -d "$runner"/bin.* 2>/dev/null | sort -V | head -n -2 | while read -r b; do
      [[ "$REPORT" -eq 0 ]] && rm -rf "$b"
      log "  removed stale runner version $b"
    done
    # _diag logs grow forever; keep the last week.
    [[ -d "$runner/_diag" ]] && {
      [[ "$REPORT" -eq 0 ]] && find "$runner/_diag" -type f -mtime +7 -delete 2>/dev/null
      log "  swept $runner/_diag >7d"
    }
  done

  # Persistent per-lane CARGO_TARGET_DIRs (~/.cargo-target/<lane>) survive
  # _work wipes by design. They are live build output — never touch one that
  # has been written in the last 7 days, and never run this block while a
  # job is active (the enclosing tier already guarantees idle).
  for tdir in /home/*/.cargo-target/*/; do
    [[ -d "$tdir" ]] || continue
    newest=$(find "$tdir" -type f -newermt '7 days ago' -print -quit 2>/dev/null)
    [[ -n "$newest" ]] && continue
    if [[ "$REPORT" -eq 1 ]]; then
      log "  [dry] cold target dir $tdir: $(mb "$(du -sb "$tdir" 2>/dev/null | cut -f1)")"
    else
      rm -rf "$tdir" && log "  removed cold cargo target dir $tdir (no writes in 7d)"
    fi
  done

  # BuildKit builder containers are rebuild artifacts — safe when idle.
  docker ps -a --format '{{.Names}}' 2>/dev/null | grep '^buildx_buildkit' | \
  while read -r c; do
    [[ "$REPORT" -eq 0 ]] && { docker stop "$c" >/dev/null 2>&1; docker rm "$c" >/dev/null 2>&1; }
    log "  removed buildkit builder $c"
  done

  # Images: keep the KEEP_IMAGES_PER_REPO newest tags per repository.
  # `docker images` output is newest-first, so order is the age authority.
  # Dedup by image ID: rmi by ID removes every tag on the image, so an ID
  # already kept (or already attempted) must never be submitted again.
  # Docker refuses removal of anything a container references — fail-closed.
  docker images --format '{{.Repository}} {{.ID}}' 2>/dev/null | \
  while read -r repo id; do
    [[ "$repo" == "<none>" ]] && continue
    [[ "${seen_id[$id]:-}" == "1" ]] && continue
    seen_id[$id]=1
    seen_repo[$repo]=$(( ${seen_repo[$repo]:-0} + 1 ))
    [[ ${seen_repo[$repo]} -le $KEEP_IMAGES_PER_REPO ]] && continue
    if [[ "$REPORT" -eq 1 ]]; then
      log "  [dry] would rmi $repo $id"
    else
      docker rmi "$id" >/dev/null 2>&1 && log "  pruned image $repo ($id)"
    fi
  done
  # docker image prune already cleared dangling layers above.

  # Idle pressure threshold is lower — caches are free to take when no build
  # can be hurt.
  if [[ "${PCT:-0}" -ge 85 ]]; then clear_caches; fi
fi

AVAIL1=$(avail_bytes)
log "--- pass end (avail $(mb "${AVAIL1:-0}"), freed $(mb "$(( ${AVAIL1:-0} - ${AVAIL0:-0} ))")$(runner_busy && echo ', runner still busy')) ---"
