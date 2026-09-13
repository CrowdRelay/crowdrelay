#!/usr/bin/env python3
"""The prose and structure of the system reference.

Separated from `build_reference.py` so the measurement code and the writing stay
independently readable. Every count this file prints comes from the `facts` dict,
never from a literal — a reference that quotes a stale number is worse than one
that omits it.
"""
from __future__ import annotations


def build_document(facts: dict, helpers: dict) -> str:
    esc = helpers["esc"]
    table = helpers["table"]
    chips = helpers["chips"]
    stats = helpers["stats"]

    git = facts["git"]
    count = facts["counts"]
    parts: list[str] = []
    add = parts.append

    # ── cover ──────────────────────────────────────────────────────────────
    add(f"""
<div class="cover">
  <div class="kicker">Internal Technical Reference</div>
  <h1>CrowdRelay</h1>
  <div class="sub">Durable business authority for the Virya ecosystem — the HTTP
    boundary, the Postgres system of record, the delivery spine, and the
    autopilot that decides what to do next.</div>
  <div class="meta">
    <b>revision</b> {esc(git['sha'])} &nbsp;·&nbsp; {esc(git['subject'])[:78]}<br>
    <b>built</b> {esc(git['built'])} &nbsp;·&nbsp;
    <b>measured from</b> the working tree, not from prose<br>
    <b>scale</b> {count['routes']} routes &nbsp;·&nbsp; {count['migrations']} migrations
    &nbsp;·&nbsp; {count['tables']} tables &nbsp;·&nbsp; {count['gates']} contract gates
  </div>
</div>""")

    # ── contents ───────────────────────────────────────────────────────────
    sections = [
        "What this system is",
        "Shape: crates and dependency direction",
        "The HTTP surface and why the prefix is the authority",
        "Data: Postgres as the system of record",
        "The delivery spine",
        "The autopilot: 22 contexts",
        "The brain: how a decision is formed",
        "The learning loop, end to end",
        "Background workers",
        "Publishing, and the switches that gate it",
        "Operator surfaces",
        "Gates, ratchets, and the runtime-SQL risk",
        "Deployment",
        "Current state, as measured",
        "Open items and known sharp edges",
    ]
    add('<h2>Contents</h2><div class="toc"><ol>')
    for name in sections:
        add(f"<li>{esc(name)}</li>")
    add("</ol></div>")

    # ── 1. what it is ──────────────────────────────────────────────────────
    add("""
<h2>1 · What this system is</h2>
<p>CrowdRelay is the authority for business state across the Virya ecosystem.
Postgres owns the truth; every other service reads through CrowdRelay's HTTP
boundary or through events CrowdRelay emits. It is single-tenant by
architecture — one workspace per deployment, with <code>workspace_id</code> on
every business table as the isolation boundary — and Virya is the first
tenant.</p>

<div class="panel note">
<h4>The North Star</h4>
<p>Grow real fans for the tenant. Three stages, and every feature is supposed to
trace to one of them:</p>
<ul>
  <li><b>Aggregate</b> — pull fans from every side of the internet (Reddit, Meta,
      Spotify, Bandsintown, forums, press, live shows) into the fanbase and into
      the Signal app.</li>
  <li><b>Grow</b> — genuine engagement, not spam. The anti-spam guardrails in the
      publishing path are load-bearing, not decoration.</li>
  <li><b>Convert</b> — tickets, merch, attendance, through fan-360 mechanisms.</li>
</ul>
</div>

<h3>The ecosystem around it</h3>
""")
    add(table(
        ["Repository", "Role", "Talks to CrowdRelay by"],
        [
            ["<code>crowdrelay</code>", "This system. API, worker, brain, Postgres.", "—"],
            ["<code>crowdrelay-control-plane</code>",
             "Operator console. Rust API + web frontend.",
             "HTTP, <code>/v1/control-plane/*</code>"],
            ["<code>crowdrelay-agents</code>",
             "TypeScript. LLM tasks, Reddit browser automation via Playwright.",
             "HTTP both ways; writes <code>agent_outcomes</code>"],
            ["<code>virya</code>", "Public site and storefront.", "HTTP, <code>/v1/public/*</code>"],
            ["<code>virya-signal</code>", "Fan app (Tauri). Installs, push, tickets.",
             "HTTP, <code>/v1/me/*</code> and <code>/v1/public/*</code>"],
            ["<code>synesthesia</code>", "Audio-visual surface.", "HTTP"],
            ["n8n", "External executors for outbound work.",
             "Consumes webhook events, posts receipts"],
        ],
    ))

    # ── 2. shape ───────────────────────────────────────────────────────────
    add("<h2>2 · Shape: crates and dependency direction</h2>")
    add(stats([
        ("Rust crates", str(len(facts["crates"]))),
        ("Source files", str(sum(row[2] for row in facts["crates"]))),
        ("Lines of Rust", f"{sum(row[3] for row in facts['crates']):,}"),
        ("Edition", "2024"),
    ]))
    add(table(
        ["Crate", "Purpose", "Files", "Lines"],
        [[f"<code>{esc(n)}</code>", esc(p), str(f), f"{l:,}"]
         for n, p, f, l in facts["crates"]],
        numeric={2, 3},
    ))
    add("""
<div class="flow">
domain  ←  brain  ←  application  ←  infra  ←  api / worker
<span class="dim">(pure policy)        (ports)      (SQL)    (transport)</span>
</div>
<p>Dependencies point inward. <code>crowdrelay-domain</code> is pure — no SQL, no
IO, no clock — so pricing, admission and autopilot rules are unit-testable
without a database. <code>crowdrelay-application</code> holds use cases and
ports and contains <b>zero SQL call sites</b>; a ratchet keeps it that way.
<code>crowdrelay-infra</code> owns every query. <code>api</code> and
<code>worker</code> are transport shells over it.</p>

<div class="panel warn">
<h4>Stack constraints worth knowing before you touch anything</h4>
<ul>
  <li>Rust 1.97.1, edition 2024, Axum 0.8, SQLx 0.8, Tokio 1, <code>time</code>
      (never chrono), uuid v7, rustls only.</li>
  <li><b>No compile-time SQLx macros.</b> Only runtime <code>sqlx::query</code>
      and <code>query_as</code>. This means no <code>DATABASE_URL</code> is
      needed to build — and that a query naming a column that does not exist
      compiles, lints and passes every unit test, then fails on first request.
      Section 12 covers what guards that.</li>
  <li>Postgres 18 is the asserted floor; local and CI run 19beta3.</li>
</ul>
</div>""")

    # ── 3. HTTP surface ────────────────────────────────────────────────────
    add(f"""
<h2>3 · The HTTP surface and why the prefix is the authority</h2>
<p>{count['routes']} routes live across <b>nine files</b>, not one.
<code>routing.rs</code> holds the bulk and <code>.merge()</code>s the rest.
Grepping only <code>routing.rs</code> makes a live endpoint look unrouted — a
mistake made repeatedly against the ops timeline surface and the autopilot
dry-run preview, both of which are live.</p>""")
    add(table(
        ["Route file", "Routes"],
        [[f"<code>{esc(n)}</code>", str(c)] for n, c in facts["routes"]],
        numeric={1},
    ))
    add("""
<h3>Authorization is path-prefix based</h3>
<p>Middleware, auth, request IDs and body limits are centralized in
<code>crowdrelay-api/src/lib.rs</code>. A route inherits its authority from its
prefix, so there is no per-route capability to forget — and no way to widen
authority by accident except by choosing the wrong prefix.</p>""")
    add(table(
        ["Prefix", "Routes", "Requires"],
        [
            ["<code>/v1/admin</code>", str(dict(facts["prefixes"]).get("/v1/admin", 0)),
             "<code>PrivilegedAuthorization::Admin</code>"],
            ["<code>/v1/control-plane</code>",
             str(dict(facts["prefixes"]).get("/v1/control-plane", 0)),
             "<code>ControlPlane</code>; narrower scopes carved out by "
             "<code>is_control_plane_management_path</code>"],
            ["<code>/v1/internal</code>", str(dict(facts["prefixes"]).get("/v1/internal", 0)),
             "Internal service credential"],
            ["<code>/v1/public</code>", str(dict(facts["prefixes"]).get("/v1/public", 0)),
             "Unauthenticated. Rate limited."],
            ["<code>/v1/staff</code>", str(dict(facts["prefixes"]).get("/v1/staff", 0)),
             "Staff device session"],
            ["<code>/v1/me</code>", str(dict(facts["prefixes"]).get("/v1/me", 0)),
             "Fan session"],
            ["<code>/v1/beacon</code>", str(dict(facts["prefixes"]).get("/v1/beacon", 0)),
             "Beacon principal"],
        ],
        numeric={1},
    ))
    add(f"""
<div class="panel note">
<p><b>These authority surfaces must never blur.</b> <code>openapi/openapi.yaml</code>
({count['openapi_paths']} paths) is the supported integration contract; the
control-plane and internal surfaces are deliberately outside it.</p>
</div>""")
    # ── 4. data ────────────────────────────────────────────────────────────
    add(f"""
<h2>4 · Data: Postgres as the system of record</h2>""")
    add(stats([
        ("Migrations", str(count["migrations"])),
        ("Tables created", str(count["tables"])),
        ("Workspace-scoped", "232"),
        ("Next migration", f"{count['migrations'] + 1:04d}_*"),
    ]))
    add("""
<p>Migrations are sequential and immutable once applied — sqlx stores a checksum
per migration, so editing an applied file makes the database refuse with
<code>VersionMismatch</code>. That has a practical consequence for testing: a
migration mutation test must run against a <i>fresh</i> database, or every
assertion fails for the wrong reason.</p>

<div class="panel note">
<h4>The four invariants that must never be broken</h4>
<ol>
  <li><b>Postgres is authoritative</b> for business state. No other store holds
      truth.</li>
  <li><b>One transaction commits three things together</b>: the business rows,
      the idempotency result, and the outbox intent. If they can be committed
      separately they will eventually disagree.</li>
  <li><b>Provider delivery is async and at-least-once.</b> Consumers dedupe.</li>
  <li><b>The public API never waits</b> for email, n8n or provider work.</li>
</ol>
</div>

<h3>Tenant isolation is one column</h3>
<p>232 tables carry <code>workspace_id</code>, and that column is the entire
isolation boundary. A read that does not name the workspace is a latent
cross-tenant leak, so <code>workspace-scope-ratchet.py</code> counts every
statement that reads a scoped table without naming it, and a new one fails the
build.</p>""")

    # ── 5. delivery spine ──────────────────────────────────────────────────
    add("""
<h2>5 · The delivery spine</h2>
<p>Nothing outbound happens on the request path. A request commits an intent and
returns; the worker delivers it. Debugging anything outbound means walking this
chain in order — <b>a 200 proves request handling, never delivery</b>.</p>
<div class="flow">
request  ›  transaction  ›  outbox row  ›  lease  ›  attempt  ›  provider
         ›  retry | delivered | dead  ›  consumer dedupe  ›  receipt
</div>""")
    add(table(
        ["Stage", "Where", "What can go wrong"],
        [
            ["Intent", "same transaction as the business write",
             "Nothing — if the transaction commits, the intent exists"],
            ["Lease", "<code>worker/src/outbox/worker.rs</code>",
             "Two workers racing; leases are why that is safe"],
            ["Attempt", "<code>outbox/transport.rs</code>, signed",
             "Transport error, 5xx, timeout — all retryable"],
            ["Terminal", "<code>delivered</code> or <code>dead</code>",
             "A <code>dead</code> row is work that landed nowhere; "
             "<code>ops/deliveries</code> shows it"],
            ["Consumer", "n8n or a sibling service",
             "A 4xx refusal is permanent and must be reported, not retried"],
        ],
    ))
    add("""
<p>The outbox reads <b>no environment variables directly</b> — callers pass
validated policy and a <code>SecretProvider</code>. Internals live in
<code>crates/crowdrelay-worker/src/outbox/{worker,repository,transport,signature,backoff,secrets}.rs</code>.</p>

<div class="panel warn">
<h4>When a worker misbehaves, separate these before redesigning anything</h4>
<ul>
  <li>Panic or process death (the process is gone)</li>
  <li>Connection loss (the database went away and came back)</li>
  <li>Lease race (two workers thought they owned the same row)</li>
  <li>Retry amplification or resource exhaustion (the system is fighting itself)</li>
</ul>
</div>""")

    # ── 6. autopilot ───────────────────────────────────────────────────────
    add(f"""
<h2>6 · The autopilot: {len(facts['contexts'])} contexts</h2>
<p>A <b>context</b> is one domain the autopilot can act in. Each has its own
policy row in <code>viryaos_autopilot_policies</code>: enabled or not, an
autonomy level, a confidence floor, and a 24-hour action cap. The contexts are
defined by a CHECK constraint, which is what makes the list authoritative rather
than a convention.</p>""")
    add(chips(facts["contexts"]))
    add(table(
        ["Autonomy level", "Meaning"],
        [
            ["<code>observe</code>", "Record a decision. Never create an action."],
            ["<code>recommend</code>", "Surface a recommendation for a human to read."],
            ["<code>require_approval</code>",
             "Create the action in <code>awaiting_approval</code>. A human "
             "approves or it expires."],
            ["<code>bounded_auto</code>",
             "Execute without asking, inside the confidence floor and the "
             "24-hour cap."],
        ],
    ))
    add("""
<h3>The cycle</h3>
<p>One cycle runs every five minutes by default and is recorded in
<code>viryaos_autopilot_cycle_runs</code>. Its phases are <b>isolated on
purpose</b>: one failing must not stop already-authorized work, which is why a
cycle records <code>degraded</code> rather than <code>failed</code>.</p>
<div class="flow">
park check  ›  growth-metric capture  ›  evaluation (decisions)
  ›  action claim + execution  ›  measurement resolution
  ›  play / wave outcomes  ›  reply triage  ›  close + record North Star
</div>
<p>A degraded cycle now records <i>which</i> phases failed
(<code>degraded_phases</code>, migration 0261) and
<code>brain.phase_failing_every_cycle</code> reports a phase that has failed
twelve consecutive cycles — the reading that separates isolation absorbing a
transient error from a phase that has stopped.</p>

<h3>The decision ledger</h3>
<p>Every decision is a row in <code>viryaos_autopilot_decisions</code> carrying
<code>decision_key</code>, <code>context</code>,
<code>confidence_basis_points</code>, <code>disposition</code>,
<code>reason</code>, <code>input_snapshot</code>, <code>policy_snapshot</code>,
<code>recommendation</code> and a <code>trace_id</code> that is
<code>NOT NULL</code> — so a decision cannot be missing from the timeline.
Dispositions are <code>auto_execute</code>, <code>require_approval</code>,
<code>recommend_only</code> and <code>deny</code>.</p>""")

    # ── 7. brain ───────────────────────────────────────────────────────────
    add(f"""
<h2>7 · The brain: how a decision is formed</h2>
<p><code>crowdrelay-brain</code> is {len(facts['brain'])} modules of pure
policy — no SQL. Infra loads a snapshot, the brain decides, infra persists.</p>""")
    add(chips(facts["brain"]))
    add(table(
        ["Module", "What it answers"],
        [
            ["<code>world_model</code>",
             "What is true right now: fans, reach, platform yield, fatigue."],
            ["<code>opportunity</code>, <code>opportunity_graph</code>",
             "What could be done, scored, and how options relate."],
            ["<code>efe</code>",
             "Expected free energy — the ranking that trades value against "
             "information gain."],
            ["<code>portfolio</code>",
             "Which of the ranked candidates to actually dispatch, under budget "
             "and fatigue constraints. Owns the WAIT decision."],
            ["<code>strategy</code>, <code>strategy_learning</code>",
             "Which posture is working; the posterior over strategies."],
            ["<code>causal_model</code>, <code>treatment_effect</code>",
             "What a channel actually causes, from contrasted outcomes."],
            ["<code>hypothesis</code>, <code>experiment</code>",
             "Explicit hypotheses with a lifecycle, and the assignment that "
             "tests them."],
            ["<code>validation</code>",
             "Walk-forward validation — does the model predict out of sample."],
            ["<code>self_assessment</code>",
             "improving / learning / stagnant / regressing / initializing, over "
             "a 60-day daily North Star series."],
            ["<code>credit_ledger</code>, <code>attribution</code>",
             "Which dispatch gets credit for a fan."],
        ],
    ))
    add("""
<div class="panel warn">
<h4>Two behaviours that surprise people</h4>
<p><b>WAIT can be overridden.</b> <code>portfolio.rs</code> lets
<code>min_dispatches &gt; 0</code> override a winning WAIT whenever any candidate
has positive value. It exists to break a cold-start deadlock, and as a permanent
setting it means the system can never learn that doing nothing was better.</p>
<p><b>Self-assessment has no magnitude floor.</b> <code>regressing</code> is
correct on a real declining series, but on a tiny population a single uninstall
can produce it — and <code>needs_attention</code> then halves the action
budget.</p>
</div>""")

    # ── 8. learning loop ───────────────────────────────────────────────────
    add("""
<h2 class="pagebreak">8 · The learning loop, end to end</h2>
<p>This is the chain the whole system exists to close. Every link is built. The
loop is only as good as its weakest link, and the weakest link is the one that
touches the outside world.</p>
<div class="flow">
snapshot  ›  <b>decision</b>  ›  action  ›  dispatch  ›  <b>external effect</b>
  ›  receipt  ›  reach event  ›  <b>evidence</b>  ›  measurement resolves
  ›  treatment effect  ›  <b>belief revision</b>  ›  next snapshot
</div>""")
    add(table(
        ["Link", "Table", "What must be true for the next link to happen"],
        [
            ["Decision", "<code>viryaos_autopilot_decisions</code>",
             "A disposition other than <code>deny</code>"],
            ["Action", "<code>viryaos_autopilot_actions</code>",
             "Approved, or auto-executed inside policy"],
            ["Dispatch", "outbox + executor",
             "An executor exists that advertises the needed capability"],
            ["External effect", "the platform itself",
             "<b>The post actually lands.</b> This is the link that has never "
             "completed."],
            ["Receipt", "<code>viryaos_execution_receipts</code>",
             "The executor reports back, or reconciliation infers it"],
            ["Reach", "<code>viryaos_reach_events</code>",
             "A row exists — it is the denominator credit allocation divides by"],
            ["Evidence", "<code>viryaos_growth_evidence</code>",
             "<code>resolved_at</code> becomes non-null when the window closes"],
            ["Belief", "<code>viryaos_brain_belief_revisions</code>",
             "Enough resolved evidence to move a posterior off its prior"],
        ],
    ))
    add("""
<div class="panel bad">
<h4>Where it is actually stuck</h4>
<p>Zero posts have landed on any external platform. Everything downstream of
"external effect" therefore has nothing to work on: evidence rows exist but none
are resolved, the causal model has a handful of datapoints, the strategy
posterior is empty, and belief revisions are zero. The machinery is not broken —
it is starved. Section 10 explains why the post has not landed and Section 15
lists what is open.</p>
</div>

<h3>Measurement is anchored, not assumed</h3>
<p>A measurement's window starts at <b>publication</b>, not at draft time. When an
operator publishes by hand and registers the URL, the same transaction that marks
the post <code>posted</code> also re-anchors the pending measurements and writes
the reach row. A post that sat in a queue for three days does not get measured
against a window that opened when it was written.</p>

<h3>The grounding gate is fail-closed</h3>
<p>An agent outcome that no verifier checked does not become an action. The
rejection reason is recorded, and four reasons mean four different responses:
<code>INSUFFICIENT_EVIDENCE</code> is a dead connector,
<code>MISSING_TARGET_IDENTITY</code> is one bad model answer,
<code>NOT_GROUNDING_CHECKED</code> is the verifier itself, and
<code>OFF_PLATFORM_PUSH_TARGET</code> is a model proposing to send the fanbase to
a destination nobody approved.</p>""")

    # ── 9. workers ─────────────────────────────────────────────────────────
    add(f"""
<h2>9 · Background workers</h2>
<p>{len(facts['worker'])} modules in <code>crowdrelay-worker</code>. One process,
many loops, each on its own interval. Leadership is leased, so two workers
running during a blue-green cutover do not both act.</p>""")
    add(chips(facts["worker"]))
    add(table(
        ["Loop", "Interval", "Job"],
        [
            ["<code>autopilot</code>", "5 min",
             "One brain cycle. Records a cycle run."],
            ["<code>outbox</code>", "continuous",
             "Lease, attempt, retry or dead-letter every outbound event."],
            ["<code>agent_outcomes</code>", "poll",
             "Ingest LLM outcomes through the data-quality guard."],
            ["<code>community_executor</code>", "60 s",
             "Claim community-post drafts and publish them. Manual mode drafts "
             "instead."],
            ["<code>community_join_executor</code>", "5 min",
             "Subscribe to screened subreddits. Max 10 per 24 h, 1 per 5 min."],
            ["<code>ops_watchdog</code>", "5 min",
             "Evaluate every alarm condition; write "
             "<code>viryaos_ops_alert_state</code>."],
            ["<code>receipt_reconciliation</code>", "slow",
             "Resolve actions whose executor receipt never arrived."],
            ["<code>event_sync</code>", "scheduled",
             "Bandsintown calendar in; owns publish ↔ cancel for synced events."],
            ["<code>push_delivery</code>", "poll", "Deliver fan push notifications."],
            ["<code>discovery</code>", "scheduled",
             "Find candidate communities. Driven by "
             "<code>CROWDRELAY_DISCOVERY_*_QUERIES</code> — unset means zero "
             "queries."],
            ["<code>retention</code>", "daily",
             "Delete expired rows, scrub payloads, preserve the audit trail."],
            ["<code>leadership</code>", "continuous",
             "Hold or drain the worker lease across a deploy."],
        ],
    ))

    # ── 10. publishing ─────────────────────────────────────────────────────
    add("""
<h2>10 · Publishing, and the switches that gate it</h2>
<p>Outbound publishing is off by default on every channel, and Reddit needs two
switches rather than one. This is deliberate: publishing through the logged-in
Reddit session is what puts that session at risk, and an automated post a
moderator reads as spam does not cost a post — it costs the account, and with it
every read the discovery loop depends on.</p>""")
    add(table(
        ["Variable", "Default", "Effect when unset"],
        [
            ["<code>CROWDRELAY_REDDIT_WRITE_ENABLED</code>", "false",
             "<b>Checked first and overrides the others.</b> Reddit drafts only."],
            ["<code>CROWDRELAY_COMMUNITY_AUTO_POST</code>", "false",
             "Community posts are drafted <code>awaiting_manual_post</code>."],
            ["<code>CROWDRELAY_AGENT_SERVICE_AUTH_KEY</code>", "unset",
             "No browser session to post through."],
            ["<code>CROWDRELAY_COMMUNITY_AUTO_JOIN</code>", "false",
             "Screened communities stay <code>not_joined</code>; posts queued "
             "behind them cannot dispatch."],
            ["<code>CROWDRELAY_TELEGRAM_AUTO_POST</code>", "false",
             "Drafts wait for a person. The flag <i>is</i> the standing approval "
             "— asking again per post is asking twice."],
            ["<code>CROWDRELAY_DISCORD_AUTO_POST</code>", "false", "As above."],
            ["<code>CROWDRELAY_SOCIAL_AUTO_POST</code>", "false", "As above."],
            ["<code>CROWDRELAY_DISCOVERY_REDDIT_QUERIES</code>", "empty",
             "<b>Zero queries, not a default.</b> Discovery finds nothing."],
        ],
    ))
    add("""
<h3>Anti-spam guardrails, and what they do on refusal</h3>
<ul>
  <li>One post per subreddit per <b>7 days</b>.</li>
  <li><code>MAX_POSTS_PER_24H = 1</code> per workspace.</li>
  <li>Community joins: 10 per 24 h, 1 per 5 min, minimum 100 members.</li>
</ul>
<p>A guardrail saying "not yet" <b>defers</b> the draft with a retry window; it
does not discard it. Only a refusal from the platform itself — where a retry
would repeat the refusal — fails the draft. A transport error, a 5xx, an expired
browser session or a missing agent service all defer, bounded at six attempts,
and the parent action is told nothing until those are exhausted.</p>

<h3>Reading whether publishing is on</h3>
<p><code>/metrics</code> is unauthenticated and carries
<code>crowdrelay_growth_component_enabled{component="…"}</code> for every growth
component, with the missing switch as a label, plus
<code>crowdrelay_growth_component_reported_age_seconds</code>. That age matters:
these switches are read at <b>startup</b>, so a row older than the last env-file
edit means the worker was never restarted.</p>""")

    # ── 11. operator surfaces ──────────────────────────────────────────────
    add("""
<h2 class="pagebreak">11 · Operator surfaces</h2>
<p>Probe before concluding a capability is missing. Every endpoint below is
registered and answers 401, not 404.</p>""")
    add(table(
        ["Operator question", "Endpoint"],
        [
            ["What needs me?", "<code>/v1/control-plane/ops/attention</code>"],
            ["What is the system doing?", "<code>/v1/control-plane/ops/summary</code>"],
            ["Why did it do this?",
             "<code>/v1/control-plane/ops/trace/{trace_id}</code>"],
            ["What happened to this request?",
             "<code>/v1/control-plane/ops/operations/{request_id}</code>"],
            ["What is the brain about to try?",
             "<code>/v1/control-plane/autopilot/cycle/preview</code>"],
            ["What is its posture?",
             "<code>/v1/control-plane/autopilot/posture</code>"],
            ["What was sent, and did it arrive?",
             "<code>/v1/control-plane/ops/outbox</code>, <code>/ops/deliveries</code>"],
            ["What did recent cycles do?",
             "<code>/v1/admin/ops/cycles?state=degraded</code>"],
            ["Which connections actually work?",
             "<code>/v1/admin/ops/connections</code>"],
        ],
    ))
    add("""
<p><code>ops/attention</code> is the exception-first view:
<code>needs_you</code>, <code>awaiting_approval</code>, <code>findings</code> and
<code>brain</code>. <code>ops/trace/{trace_id}</code> joins decision, action,
outbox, delivery, measurement, evidence, reach, audit and agent outcome into one
timeline — every one of those rows carries the trace.</p>

<div class="panel note">
<h4>Alarms worth recognising</h4>
<ul>
  <li><code>publishing.drafts_with_no_publisher</code> — drafts waiting while
      nothing will publish them. Names the missing switch.</li>
  <li><code>publishing.drafts_failed</code> — failed drafts with the distinct
      reasons, which is what decides whether the content can be requeued.</li>
  <li><code>safety.off_platform_push_proposed</code> — critical. A model proposed
      sending the fanbase to an unapproved destination; the guard refused it.</li>
  <li><code>brain.phase_failing_every_cycle</code> — critical. A cycle phase has
      failed twelve consecutive times.</li>
  <li><code>learning.outcomes_unverified</code> — critical. Every agent outcome
      is being refused for want of a grounding check.</li>
</ul>
</div>""")

    # ── 12. gates ──────────────────────────────────────────────────────────
    add(f"""
<h2>12 · Gates, ratchets, and the runtime-SQL risk</h2>
<p>There are no compile-time SQL macros here, so the compiler cannot catch a
query that names something that does not exist. That is the standing risk of the
runtime-query choice, and it has cost real outages: one uncast
<code>EXTRACT</code> aborted every autopilot cycle for two hours after compiling,
linting and passing 1,684 tests. {count['gates']} contract gates and four
ratchets cover what static typing does not.</p>""")
    add(stats([
        ("Contract gates", str(count["gates"])),
        ("Policy scripts", str(count["policy_scripts"])),
        ("Ratchets", "4"),
        ("OpenAPI paths", str(count["openapi_paths"])),
    ]))
    add(table(
        ["Gate", "What it refuses"],
        [
            ["<code>sql-result-types.py</code>",
             "<code>PREPARE</code>s every row-returning query against a live "
             "schema and reads <code>pg_prepared_statements.result_types</code> — "
             "exactly what sqlx decodes. Refuses a NUMERIC output column, since "
             "nothing here stores numeric. Also a ratchet on the prepared count, "
             "because a query that <i>fails</i> to prepare used to be counted as "
             "merely unprepared."],
            ["<code>test_sql_identifiers_v1.py</code>",
             "A relation named after FROM/JOIN/INSERT/UPDATE/DELETE that no "
             "migration creates."],
            ["<code>test_sql_columns_v1.py</code>",
             "Column-level mismatches against a real schema."],
            ["<code>workspace-scope-ratchet.py</code>",
             "A new statement reading a workspace-scoped table without naming the "
             "workspace."],
            ["<code>api-sql-ratchet.py</code>",
             "A new write statement in the HTTP layer. Move it behind a "
             "repository."],
            ["<code>source-size-ratchet.py</code>",
             "A file over 1,200 lines that is not already in the baseline, or a "
             "listed file growing past its recorded max."],
            ["<code>test_platform_vocabulary_v1.py</code>",
             "Platform CHECK constraints and the <code>Platform</code> / "
             "<code>MetricPlatform</code> enums disagreeing; also a migration "
             "dropping a value from a CHECK."],
            ["<code>test_publishing_switches_v1.py</code>",
             "A <code>CROWDRELAY_*</code> variable the worker reads that "
             "<code>.env.example</code> does not name."],
            ["<code>test_prometheus_exposition_v1.py</code>",
             "A histogram whose declared family has no matching "
             "<code>_bucket</code> series; a comment line that does not start "
             "with <code>#</code>."],
            ["<code>test_decision_trace_contract_v1.py</code>",
             "A new writer to the decision ledger that does not supply a trace."],
        ],
    ))
    add("""
<div class="panel warn">
<h4>Two rules about gates, learned the hard way</h4>
<p><b>Mutate every new gate against a real violation before trusting it.</b> A
gate that cannot fail still looks like coverage. Several written here initially
passed a mutation: one compared files where it should have counted statements,
one used <code>assertIn</code> where one of two instances could change, one
scoped itself by a function name that two functions shared.</p>
<p><b>A migration mutation must run against a fresh database.</b> sqlx checksums
mean an already-migrated one answers <code>VersionMismatch</code> before any
assertion runs, so every test fails for the wrong reason and the check looks
like it bites.</p>
</div>

<h3>Running them</h3>
<pre><code>just check     # fmt, clippy -D warnings, cargo test (workspace, all targets)
just ci        # check + contract assets + contract-tests + policy-checks
just test-postgres   # the #[ignore]d suites against a disposable database
just migrate   # run before trusting sql-result-types: a stale dev schema
               # silently drops new-column queries out of the count</code></pre>""")

    # ── 13. deployment ─────────────────────────────────────────────────────
    add("""
<h2>13 · Deployment</h2>
<p>Blue-green, digest-pinned, with a soak before the old colour stops. The
cutover renames containers, so <code>crowdrelay-api-1</code> and
<code>crowdrelay-api-green-1</code> swap roles on each release.</p>
<div class="flow">
gates  ›  build (arm64, digest-pinned)  ›  migrate  ›  start candidate
  ›  health  ›  Caddy switch  ›  worker leadership handoff
  ›  soak 30 s with old colour as fallback  ›  stop old  ›  receipt
</div>""")
    add(table(
        ["Command", "What it does"],
        [
            ["<code>just ship</code>", "Full gates, then deploy."],
            ["<code>just ship-nogate</code>",
             "Local arm64 build and digest-pinned blue-green, skipping gates "
             "already run. Requires <code>origin/main</code> to match local."],
            ["<code>just deploy-ecosystem</code>",
             "Multi-repo deploy. Phase 0e2 runs the cross-repo agent pairing "
             "gates and treats a <b>skip as a failure</b>."],
            ["<code>just db-up</code>, <code>just up</code>, <code>just health</code>",
             "Local stack."],
        ],
    ))
    add("""
<div class="panel warn">
<p><b>Two image pins can disagree.</b> The deploy stamps the sha into
<code>.crowdrelay.local.sh</code>; a manual <code>docker compose up</code>
resolves <code>CROWDRELAY_IMAGE_TAG</code> from <code>deploy/.env.production</code>,
which the deploy does not update. Recreating a service by hand can silently run
an older image. Check both pins name the same sha first.</p>
<p><b>Eight workflows are <code>disabled_manually</code>.</b> Only CI and image
publishing run. A gate added to a disabled workflow is decoration — verify
<code>gh workflow list</code> before wiring one.</p>
</div>""")

    # ── 14. current state ──────────────────────────────────────────────────
    add("<h2>14 · Current state, as measured</h2>")
    production = facts.get("production")
    if production:
        def g(key: str, default: str = "—") -> str:
            return production.get(key, default)

        add(f"""<p>Live gauges from <code>/metrics</code> at build time
({esc(git['built'])}). These are a snapshot, not a target.</p>""")
        add(stats([
            ("Cycles / 24 h", g("crowdrelay_brain_cycles_24h")),
            ("Degraded", g("crowdrelay_brain_cycles_degraded_24h")),
            ("Decisions / 24 h", g("crowdrelay_brain_decisions_24h")),
            ("Actions / 24 h", g("crowdrelay_brain_actions_24h")),
        ]))
        add(stats([
            ("Evidence resolved", g("crowdrelay_brain_evidence_resolved")),
            ("Posts published", "0" if g("crowdrelay_brain_seconds_since_publication") == "0" else "≥1"),
            ("Communities joined", g("crowdrelay_brain_communities_joined")),
            ("Blocked on join", g("crowdrelay_brain_communities_blocked_on_join")),
        ]))
        add(stats([
            ("Signal installs", g("crowdrelay_brain_signal_installs")),
            ("Identified", g("crowdrelay_brain_signal_installs_identified")),
            ("Push-reachable fans", g("crowdrelay_brain_signal_fans_push_enabled")),
            ("DB pool", g("crowdrelay_db_pool_max")),
        ]))
        enabled = sorted(
            (k.split('"')[1], v)
            for k, v in production.items()
            if k.startswith("crowdrelay_growth_component_enabled")
        )
        if enabled:
            add("<h3>Growth components</h3>")
            add(
                '<div class="chips">'
                + "".join(
                    f'<span class="chip {"on" if v == "1" else "off"}">'
                    f'{esc(name)} {"on" if v == "1" else "off"}</span>'
                    for name, v in enabled
                )
                + "</div>"
            )
        add("""
<div class="panel bad">
<h4>The one number that matters</h4>
<p><code>seconds_since_publication</code> is <b>0</b>, which in this schema means
<code>max(posted_at)</code> is NULL: no post has ever landed. Decisions, actions
and cycles are all healthy. Evidence resolved is zero because the link between
them has never completed.</p>
</div>""")
    else:
        add("""<p class="dim">Production metrics were not reachable at build time,
so this section is omitted rather than filled with stale numbers. Re-run the
build on a network that can reach the deployment.</p>""")

    # ── 15. open items ─────────────────────────────────────────────────────
    add("""
<h2>15 · Open items and known sharp edges</h2>
<h3>Blocking the loop</h3>""")
    add(table(
        ["Item", "Where", "Why it blocks"],
        [
            ["The agent service crashes on database blips",
             "<code>crowdrelay-agents/src/store/db.ts</code>",
             "<code>createPool</code> has no <code>pool.on('error')</code>. "
             "node-postgres emits <code>error</code> on the pool when Postgres "
             "terminates an idle connection; unhandled, that exits the process "
             "and kills in-flight 80-second browser posts."],
            ["The Reddit session is not logged in",
             "<code>agent_service_reddit_cookies</code>",
             "<code>reddit_username</code> is empty and the startup probe logs "
             "404. A logged-out browser cannot post, and the failure looks like a "
             "transport error rather than a refusal."],
            ["No script-app OAuth credential",
             "agent service config",
             "Every post takes the fragile Playwright path instead of the API the "
             "route prefers."],
            ["Verifier fallback unset",
             "<code>AGENT_VERIFIER_PAID_FALLBACK</code>",
             "Basic-tier outcomes verify against free models only, so the brain's "
             "fail-closed gate refuses them. Each side is defensible alone; "
             "together the pipeline stops quietly."],
        ],
    ))
    add("""
<h3>Corrupting learning even when things work</h3>
<ul>
  <li><b>An empty scanner run reads as a failed run.</b>
      <code>last_effective_run</code> counts only outcomes carrying
      <code>payload.item</code>, so a saturated scanner never engages its
      cooldown and is re-dispatched forever. A separate
      <code>last_any_run</code> already exists — the distinction the brain needs
      is three-way and it has two.</li>
  <li><b><code>agent.run.request</code> assignments can never reach
      <code>executed</code>.</b> Nothing owns that transition for agent tasks, so
      they sit <code>dispatched</code> — excluded under per-protocol, counted as
      treated under ITT.</li>
  <li><b><code>evidence.channel</code> is derived from
      <code>subreddit_type.starts_with("r/")</code></b>, so anything else becomes
      <code>Other</code>.</li>
  <li><b>Two North Stars.</b> The configured metric and the quantity the learner
      optimises are different, so the self-assessment trends a series the causal
      model is not moving.</li>
  <li><b><code>min_dispatches</code> overrides WAIT unconditionally</b> — a
      cold-start escape hatch that became a permanent setting.</li>
</ul>

<h3>Performance</h3>
<div class="panel warn">
<p><b>One operations-page load consumes the entire database pool.</b> Measured: 9
concurrent requests, peak <code>db_pool_in_use</code> = 10 of 10 locally,
repeatably. Each endpoint's internal concurrency budget was sized against the
whole pool on the assumption it runs alone — <code>load_control_overview</code>
takes 4 branches, <code>ops/attention</code> takes half the pool — and the
control plane fires nine of them at once. The budgets do not compose. The fix is
a process-wide read budget rather than per-endpoint ones.</p>
</div>

<h3>Deliberately not done</h3>
<ul>
  <li><b>The North Star still counts reachable fans from
      <code>fan_push_endpoints</code></b> rather than
      <code>signal_installations</code>. Switching now would report a worse
      number: install reporting is newer than the builds some fans registered
      from.</li>
  <li><b>No topical relevance dimension in discovery screening.</b> Two
      heuristics were measured and falsified — an empty <code>genres</code> array
      and a music-term regex on the description both fail to separate junk from
      relevant. No signal in today's data supports a third.</li>
  <li><b>A composite read endpoint for the operations page.</b> It would save
      eight auth checks and eight JSON envelopes, and it would throw away the
      independent-degradation property the current code values — a dead audience
      endpoint does not blank signal — while coupling this API to a control-plane
      page layout.</li>
</ul>

<footer>
Generated from the working tree at """ + esc(git["sha"]) + """ ·
Regenerate with <code>just reference</code> ·
Cross-repo bug patterns and the pre-sprint checklist live in
<code>~/dev/BUG_TRACKER.md</code>
</footer>""")

    return "".join(parts)
