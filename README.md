# CrowdRelay

**The core platform that runs fan-growth operations for a music label or artist roster.**

CrowdRelay owns the durable business state behind audience growth and live operations: fans and consent, events, tickets, admission, merch, referrals, venue relationships, community outreach, and every action taken around them. It decides what to do within explicit authority limits, executes through external systems, measures what happened, and feeds results into the next decision cycle.

## What it does

Runs a whole roster from one seat. Each artist is a workspace inside a label organization. The portfolio layer lets a roster's audiences amplify each other through explicit, revocable, capped consent edges — the one capability that only exists when one platform holds every artist's fan graph.

The Autopilot evaluates twenty-plus bounded contexts on every cycle — ticket yield, fan lifecycle, merchandising, booking opportunities, outreach to curators and press, promotion budget, show operations, community engagement, and more. Each action passes through a shared funnel: confidence gate, authority level, class ceiling, envelope budget, deliverability halt. The stricter limit wins.

External services — email, payment, calendar, ad platforms, workflow automation — are adapters, not sources of truth. A successful API call proves request handling, not delivery. The transactional outbox owns durability, retries, and dead-state inspection.

## What it solves

Music teams juggle a dozen disconnected tools: a spreadsheet for fans, an email platform for campaigns, a separate tool for ads, a Slack thread for outreach tracking, and no unified view of who a fan is or what they've done. CrowdRelay replaces that with one system that holds the fan graph, makes decisions, executes work, measures outcomes, and learns from them.

## The brain

CrowdRelay separates decision-making from model-generated content.

The brain maintains the operational state, finds opportunities, decides what is worth doing, applies authority and resource constraints, executes actions, measures outcomes, and uses what it learns to change future decisions.

Its decision layer combines:

- candidate generation and information-seeking signals
- portfolio optimization
- causal experiments and attribution
- outcome and strategy learning
- provenance and evidence tracking
- explicit authority, budget, and safety constraints

Models can produce language or creative work. The brain decides whether that work should happen, under what constraints, and what should happen next. LLM-assisted creative work (press pitches, social posts, campaign analysis) is dispatched to the separately deployed [`crowdrelay-agents`](https://github.com/CrowdRelay/crowdrelay-agents) service; the LLMs produce drafts seeded with real tenant data, and the brain decides what to do with them.

The system is deliberately built so that:

what happened ≠ what was attributed ≠ what was causally supported ≠ what was predicted ≠ what was worth doing

The core execution and decision infrastructure is already in place. The learning and causal layer is actively evolving as it is exercised against real outcomes and adversarial tests.

The goal is not better suggestions. The goal is a system that gets better at making decisions that produce incremental durable fans.

## Authority model

| Posture | What the engine does | What it never does |
|---|---|---|
| **Grounded** | Observes and rehearses everything (dry run) | Touch anyone |
| **Working** | First-party work runs; outward contact drafts for approval | Send to fans or curators unattended |
| **Full send** | Owned audience sends within limits; free pitching runs unattended | Spend money, sign contracts |

Money and contracts stay behind approval in every posture.

## Measurement and learning

Executed actions settle against benchmarks after their horizon. Effects are labelled `improved`, `neutral`, or `worsened` — never a raw score without context. Strategies whose record repeatedly worsens retire themselves with a stated reason. Attribution is honest: smart-link clicks are attribution; follower movement after a campaign is correlational, always labelled as such. Causal experiments (randomized holdout with power analysis) separate treatment effect from observational correlation — where the experiment population is large enough to support it.

The daily brief breaks silence only for things that lie when quiet: halted ceilings, stale approvals, dead executors, pending withdrawals. Everything else waits for somebody to look at the panel.

## Safety

- Sending volume earns its ceiling from zero; bounce or complaint rates close it before damage
- Every action carries an idempotency key; retries are bounded and auditable
- Circuit breakers open on executor failure storms
- Kill switch stops all outbound work without touching data
- Blue-green deploys with zero-downtime cutover and automatic rollback
- HMAC-signed webhooks with replay protection
- No unsafe code in the runtime; panics forbidden at compile time

## Deploy

Blue-green with zero-downtime Caddy cutover. The deploy waits for CI, pulls immutable image digests, starts the new release alongside the current one, health-checks it, switches traffic, then stops the old release. Rollback is automatic on any failure. Bootstrap and recovery use a force-recreate fallback when no blue or green container is running.

## Ecosystem

| Repository | Role |
|-----------|------|
| [crowdrelay-control-plane](https://github.com/CrowdRelay/crowdrelay-control-plane) | Operator plane — tenant provisioning, runtime health, audit |
| [crowdrelay-agents](https://github.com/CrowdRelay/crowdrelay-agents) | LLM worker service — press pitches, social posts, campaign analysis |
| [virya](https://github.com/CrowdRelay/virya) | Public website — tickets, merch, fan experiences |
| [virya-signal](https://github.com/CrowdRelay/virya-signal) | Mobile client — fan wallet, ticket scanning, staff operations |
| [synesthesia](https://github.com/CrowdRelay/synesthesia) | Interactive album — playable companion to *Echoes Of The Modern Mind* |

---

<p align="center">
  Built with Rust, Postgres, and a stubborn refusal to let AI make business decisions it can't learn from.
</p>
