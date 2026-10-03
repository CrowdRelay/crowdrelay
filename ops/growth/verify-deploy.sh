#!/usr/bin/env bash
# Post-deploy proofs for FAN_100, read-only, in one run.
#
#   ops/growth/verify-deploy.sh [ssh-host]      (default: virya-crowdrelay)
#
# Every line is one of:
#   [ok]    the expected thing is true
#   [zero]  a measured zero — a result, not a failure (nothing happened yet)
#   [warn]  something that should have changed after the deploy did not
#   [info]  context
#
# It reads the database and worker logs over ssh and the public /v1/meta. It
# never writes, never calls an authenticated API, and never touches a tracked
# link (a GET on /l/{slug} would record a real click). Exit status is always 0:
# this reports, it does not gate.
#
# A null is a result; an invented success is not. Nothing here marks a check
# "ok" because a number is non-zero by accident — each states what it expects.
set -uo pipefail

HOST="${1:-virya-crowdrelay}"
META_URL="${META_URL:-https://signal-api.virya.music/v1/meta}"
# One string, no shell metacharacters: it is re-parsed by the remote shell.
DB='docker exec -i crowdrelay-db psql -U postgres -d crowdrelay -At -v ON_ERROR_STOP=1'

ok()   { printf '[ok]   %s\n' "$*"; }
zero() { printf '[zero] %s\n' "$*"; }
warn() { printf '[warn] %s\n' "$*"; }
info() { printf '[info] %s\n' "$*"; }

# A failed query is a loud [warn], never an empty string a check could read as zero.
sql() {
  local out
  if ! out="$(ssh -o BatchMode=yes -o ConnectTimeout=10 "$HOST" "$DB" <<<"$1" 2>&1)"; then
    warn "query failed: ${1:0:80}... (${out:0:120})"
    printf '?'
    return 0
  fi
  printf '%s' "$out"
}
logs() { ssh -o BatchMode=yes -o ConnectTimeout=10 "$HOST" "docker logs --since ${1} crowdrelay-worker-1 2>&1" 2>/dev/null; }

echo "== build"
meta="$(curl -s -m 15 "$META_URL" || true)"
sha="$(grep -o '"gitSha":"[a-f0-9]*"' <<<"$meta" | cut -d'"' -f4)"
schema="$(grep -o '"schemaVersion":[0-9]*' <<<"$meta" | cut -d: -f2)"
if [ -z "$sha" ]; then
  warn "could not read $META_URL"
else
  info "prod gitSha ${sha:0:8}, schema ${schema:-?}"
  if git rev-parse --git-dir >/dev/null 2>&1 && git fetch -q origin main 2>/dev/null; then
    behind="$(git rev-list --count "$sha"..origin/main 2>/dev/null || echo '?')"
    if [ "$behind" = "0" ]; then ok "prod is at origin/main"; else warn "prod is $behind commit(s) behind origin/main"; fi
  fi
fi

echo "== reply lane (crowdrelay#513 + the breach acknowledgement)"
halts="$(logs 10m | grep -c 'scout lane halted' || true)"
if [ "${halts:-0}" = "0" ]; then ok "no 'scout lane halted' in the last 10 minutes"; else warn "$halts 'scout lane halted' in the last 10 minutes"; fi
own="$(sql "select value from tenant_settings where key='scout_own_handles'")"
if [ -n "$own" ]; then ok "scout_own_handles = $own"; else warn "scout_own_handles is not set (own accounts are still collected as prospects)"; fi
own_prospects="$(sql "select count(*) from fan_prospects p join person_identities i on i.person_id=p.person_id and i.kind='platform_handle' where i.value in ('wojciech_bator')")"
if [ "${own_prospects:-?}" = "0" ]; then ok "the owner's own account is not a prospect"; else info "owner's account prospects: ${own_prospects:-?} (the next hourly sweep retracts them after #513 deploys)"; fi

echo "== brain (crowdrelay#521: the prior comes from the tenant's record)"
seed="$(logs 6h | grep 'prior seeded from the tenant' | tail -1 | sed -E 's/.*"delivered":([0-9]+).*"attributed_fans":([0-9]+).*"prior_mean_fans":([0-9.]+).*/delivered=\1 attributed=\2 prior_mean_fans=\3/')"
if [ -n "$seed" ]; then ok "prior seeded: $seed (compiled-in default was 2.0)"; else info "no 'prior seeded' line in the last 6h (it logs when the checkpoint rebuilds, once per deploy)"; fi
obs="$(sql "select coalesce((state->'fans'->'global'->>'n'),'?') from brain_state where module='causal_model'")"
info "causal model fan observations: ${obs:-?}"
# Counted from the outcome rows, and only by what they say: a measurement that
# matured is not a fan. Rows measured before the attribution fix are history.
matured="$(sql "select count(*)||' matured, '||count(*) filter (where o.observed_value > 0)||' with fans > 0' from autopilot_measurements m join autopilot_outcomes o on o.measurement_id = m.id where m.status='succeeded' and m.measurement_kind in ('incremental_fan_growth_3d','incremental_fan_growth_14d') and m.finished_at > now() - interval '3 days'")"
case "$matured" in
  "0 matured"*) zero "no fan-growth measurement matured in the last 3 days (windows opened after 2026-09-30 mature from 10-03)" ;;
  "?") ;;
  *) info "fan-growth measurements in the last 3 days: $matured (a matured zero is learning; fans > 0 is the claim to check against provenance)" ;;
esac

echo "== lanes (crowdrelay#518: a down artifact lane is held and probed)"
fails="$(sql "select count(*) from autopilot_actions where action_kind='content.artifact.request' and status='failed' and created_at > now() - interval '2 hours'")"
if [ "${fails:-0}" -le 1 ] 2>/dev/null; then ok "content.artifact.request failures in 2h: ${fails:-0} (a probe per backoff at most)"; else warn "content.artifact.request failures in 2h: $fails (expected <=1 while the lane is held)"; fi
info "artifact failures by kind (2h): $(sql "select coalesce(string_agg(k||'='||c, ', '), 'none') from (select coalesce(last_error_kind,'?') k, count(*) c from autopilot_actions where action_kind='content.artifact.request' and status='failed' and created_at > now() - interval '2 hours' group by 1) x")"

echo "== rail (crowdrelay#536/#541: readiness and the publish token)"
auto="$(sql "select coalesce((select value from tenant_settings where key='social_auto_post'),'unset')||' / platforms='||coalesce((select value from tenant_settings where key='social_autopost_platforms'),'unset')")"
info "social_auto_post = $auto"
scopes="$(sql "select coalesce(value,'') from tenant_settings where key='meta_publish_scopes'")"
checked="$(sql "select value from tenant_settings where key='meta_publish_scopes_checked_at'")"
if [ -z "$checked" ]; then
  last="$(logs 24h | grep 'publish token scopes' | tail -1 | sed -E 's/.*"message":"([^"]*)".*/\1/' | head -c 120)"
  info "publish token not verified yet (${last:-no check logged}); readiness shows a caveat, it does not block"
elif grep -q 'pages_manage_posts' <<<"$scopes"; then ok "publish token verified at $checked: pages_manage_posts present"
else warn "publish token checked at $checked and lacks pages_manage_posts: $scopes"; fi
gate="$(sql "select enabled from growth_component_state where component='social_post_executor'")"
info "worker reports social_post_executor enabled: ${gate:-not reported}"

echo "== capture (crowdrelay#533, #535)"
owned="$(sql "select count(*) from fan_ad_attribution where utm_source='owned'")"
if [ "${owned:-0}" = "0" ]; then zero "no signup has come through an owned-link landing yet (utm_source=owned)"; else ok "$owned signup(s) via owned-link landing"; fi
drafts="$(sql "select count(*) from content_sources cs where cs.source_kind='video' and cs.metadata ? 'fan_capture_draft_at' and not (cs.metadata ? 'fan_capture_comment_posted_unix') and cs.occurred_at > now() - interval '30 days'")"
if [ "${drafts:-0}" -gt 0 ] 2>/dev/null; then info "$drafts YouTube capture comment(s) prepared for a person to paste (see ops/attention unpublished_drafts.youtube)"; else info "no YouTube capture comment prepared yet"; fi

echo "== the funnel, as it stands"
info "clicks 24h / visitors: $(sql "select count(*)||' / '||count(distinct anonymous_visitor_id) from click_events where occurred_at > now() - interval '24 hours'")"
newest="$(sql "select coalesce(max(created_at)::date::text,'never') from fans where deleted_at is null and merged_into_fan_id is null")"
fans="$(sql "select count(*) from fans where deleted_at is null and merged_into_fan_id is null")"
since="$(sql "select count(*) from fans where deleted_at is null and merged_into_fan_id is null and created_at > now() - interval '7 days'")"
if [ "${since:-0}" = "0" ]; then zero "no new fan in 7 days (fans: $fans, newest signup: $newest)"; else ok "$since new fan(s) in 7 days (fans: $fans)"; fi
info "joins by campaign tag (90d; '(untagged)' is a join with no tag, never a guessed channel): $(sql "select coalesce(string_agg(src||'/'||med||'='||n, ', ' order by n desc), 'none') from (select coalesce(nullif(btrim(a.utm_source),''),'(untagged)') src, coalesce(nullif(btrim(a.utm_medium),''),'-') med, count(*) n from fans f left join fan_ad_attribution a on a.workspace_id=f.workspace_id and a.fan_id=f.id where f.status='active' and f.deleted_at is null and f.merged_into_fan_id is null and f.created_at > now() - interval '90 days' group by 1,2) x")"
info "system-attributed active fans: $(sql "select count(distinct f.id) from fan_provenance_events e join fans f on f.id=e.fan_id where e.event_kind='conversion' and e.action_id is not null and f.status='active' and f.deleted_at is null and f.merged_into_fan_id is null")"
info "push endpoints active/total: $(sql "select count(*) filter (where active)||' / '||count(*) from fan_push_endpoints")  (virya#56 should raise active)"
info "fans who opened Signal in 7d / 30d: $(sql "select count(distinct fan_id) filter (where last_seen_at > now() - interval '7 days')||' / '||count(distinct fan_id) filter (where last_seen_at > now() - interval '30 days') from fan_sessions")"
info "latarnik roles / missions: $(sql "select (select count(*) from latarnik_roles)||' / '||(select count(*) from latarnik_missions)")"
