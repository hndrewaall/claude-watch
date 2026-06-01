# The event hierarchy: events vs. obligations vs. interruptions

This is the conceptual entry point for the three distinct signaling
mechanisms claude-watch and its sibling tools expose to a Claude Code main
loop, plus the watchers that feed them. If you are a fresh agent trying to
understand "what kinds of things can reach into the loop, and how do they
differ," start here, then follow the cross-links into the per-subsystem docs.

All three demand handling — none of them is a passive FYI. They differ in
*when* the loop must handle them, *how* (judgment vs. mechanical gate vs.
preemption), and *what they cost*:

| | **event** | **obligation** | **interruption** |
|---|---|---|---|
| **What it is** | An actionable signal the loop must triage / act on | A hard gate on tool calls | A forced preemption of the current turn |
| **When it reaches the loop** | At the next loop pass (next prompt) | At a tool-call boundary, before the call runs | Mid-generation, immediately |
| **How it's handled** | By the loop's *judgment* on the next pass — decide and act | By a *mechanical* predicate at the tool-call boundary — no judgment, the call is blocked until the predicate passes | By seizing the turn — the loop must deal with it now |
| **What if it's not handled?** | A failure: it's a dropped actionable signal (and the source of context noise). The `event-must-act` layer flags it and can escalate it into an obligation — events are not meant to be droppable | The matching tool call cannot proceed until its predicate is satisfied | The turn is already seized; in-flight generation is cancelled |
| **Granularity** | Per loop pass | Per tool call | Per keystroke / generation |
| **Who emits it** | A watcher / producer via the event bus | A `PreToolUse` / `PostToolUse` hook evaluating a predicate | The monitoring daemon, via tmux `send-keys` (out-of-band) |
| **Cost of using it** | Cheap to deliver, but every event spends loop attention — mint one only for things that truly need acting on | Medium; blocks work, must be satisfied or overridden | High; discards partial work |

The one-line mnemonic — a single ladder of increasing force, all three of
which the loop must handle:

```
event             →  obligation        →  interruption (tmux send-keys)
(act next pass)      (forcing function)   (escalation beyond the gate)
"triage + act         "you may not         "stop what you're
 next pass"             do X until Y"        doing right now"
```

All three require handling; the rungs differ in *how* that handling is forced.
An **event** must be triaged and acted on by the loop's judgment on its next
pass. An **obligation** turns "should" into "must" *mechanically* by blocking a
tool call. A **tmux send-keys interruption** is the rung beyond the gate — it
preempts the turn outright when even a blocking gate isn't enough (or fires too
late). A signal that genuinely needs no action does not belong on this ladder
at all — see the anti-noise rule below.

---

## event — an actionable signal to triage on the next loop pass

An **event** is a signal the main loop **must act on** — it surfaces state
into context so that the loop can triage it and respond on its next pass. It
is not a passive FYI. It does not block a tool call and it does not preempt
the turn, but it does demand a decision: act now, schedule the work, or
explicitly dispatch it. Letting an event slide by unhandled is a failure mode,
not a normal outcome.

- **Mechanism**: a producer emits a small JSON record onto the event bus; a
  watcher debounces a burst of these and surfaces a one-line-per-event batch
  into the loop's context (typically at the next `UserPromptSubmit`).
- **Use it when** the loop genuinely needs to *do something* the next time it
  comes around: queue state transitions that need follow-up, a completed
  background job whose result must be processed, a scheduled task that must
  run, an alert that warrants a response.
- **Cron is a first-class event *producer*.** A scheduled job is the simplest
  way to get periodic or time-triggered work into the loop: the cron job runs
  on its schedule, emits a claude-event, and the event-watcher surfaces that
  event on the next loop pass — no dedicated long-lived process required. This
  is the preferred shape for anything that doesn't need sub-minute reactivity
  (health checks, promotion scans, scheduled tasks). See "Where watchers
  fit" below for why cron-as-producer is what keeps the watcher count low.
- **Every event must be handled.** The bus only moves the signal into context;
  the *requirement* that an actionable event actually gets triaged is enforced
  by a companion layer that watches for dropped/ignored actionable events and
  escalates one into a blocking gate (an obligation). That escalation path is
  what keeps "events must act" honest — events are not droppable by design.

> **Anti-noise rule: if a signal is genuinely safe to ignore, it should not be
> an event.** Minting ignorable things as events is exactly how the loop's
> context fills with noise. Route a no-action-needed signal to a different
> channel instead: a push-notification channel (for a human to glance at), a
> plain log line, or a metric on a dashboard. Reserve events for things the
> loop must actually act on.

> "event → must act" is a deliberately separate concern: the bus delivers,
> the enforcement layer (an obligation instance) ensures actionable ones
> actually get triaged. See [`../event-must-act.md`](../event-must-act.md).

See [`../events.md`](../events.md) for the bus, schema, and CLIs; see
[`../watchers.md`](../watchers.md) for the watchers that produce them.

---

## obligation — a hard gate that blocks tool calls

An **obligation** is a "must do X before Y" guardrail. Unlike an event, it
does not merely inform — it **denies** a matching tool call until a named
predicate is satisfied. It fires at a tool-call boundary (via a `PreToolUse`
hook), so it is checked *before* the tool runs, never mid-generation.

- **Mechanism**: a `PreToolUse` / `PostToolUse` hook evaluates registered
  predicates against the pending tool call. A failing predicate returns a
  DENY decision and the tool call never executes. The loop must satisfy the
  underlying state (then the predicate passes) or invoke a documented,
  audited, time-boxed override before the call goes through.
- **Use it when** an invariant must hold before a *class* of tool calls runs:
  must-acknowledge-inbound before sending a message, must-read captured
  watcher output before restarting watchers, must-include a queue id before
  spawning a sub-agent, no-leakage gates on public-repo work.
- **It enforces mechanically.** Where an event relies on the loop's judgment
  to get handled, an obligation needs no judgment at all — a failing predicate
  denies the call outright. It is also *narrowly scoped*: it only affects tool
  calls that match its pattern, and its failure mode is conservative
  (default-open on internal error) so a broken gate does not wedge the loop.

See the "Obligations gate" section of [`../hooks.md`](../hooks.md) for the
predicate vocabulary, enforcement modes (`gate` vs. `inform`), scope guards,
auto-satisfaction, and the override / exempt mechanics.

---

## interruption — the forcing function beyond an obligation (tmux send-keys)

An **interruption** is the next rung up from an obligation. Where an event
waits for the next pass and an obligation waits for the next tool call, an
interruption does not wait at all — it preempts the current turn and forces
the loop to deal with something *now*. It is what you reach for when even a
blocking gate isn't enough, or fires too late to prevent the harm.

- **Mechanism: tmux `send-keys`.** claude-watch's interruption/preemption is
  delivered by the monitoring daemon injecting keystrokes directly into the
  main loop's tmux session — this is the daemon's one out-of-band action. The
  injection seizes the current turn rather than gating a future tool call.
  (A genuine human preemption arrives the same way, through the real input
  channel.)
- **send-keys is the general out-of-band injection channel, not only the
  escalation rung.** Escalation/preemption (this rung, above) is one use, but
  the *same* mechanism also carries routine operational injections — e.g.
  triggering a controlled self-clear (orderly context compaction) or a
  restart to pick up a freshly deployed binary. Just as an obligation has uses
  beyond forcing an ignored event, send-keys has uses beyond escalation; the
  forced-preemption framing below describes its highest-stakes use, not its
  only one.
- **Use it when** waiting for a turn boundary is too late: context-window
  exhaustion approaching (compaction with uncommitted state is worse than a
  cancelled message), a stalled / zombie session, a dead watcher pipeline, or
  prolonged unproductive generation.
- **It is the rung above the obligation, not a separate axis.** Event →
  obligation → tmux send-keys is one ladder of increasing force: notify, then
  block a tool call, then preempt the turn. An obligation that consistently
  "fires too late" (the bad state is already in motion by the time a tool call
  is attempted) is the canonical candidate to be *promoted* to a tmux
  send-keys interruption. The nuance that distinguishes it from the lower
  rungs: it **seizes the current turn** (cancels in-flight generation) instead
  of gating a future tool call.

### CRITICAL: a harness-injected rejection is NOT an interruption

This is the single most important distinction to internalize, because the two
look superficially alike and conflating them causes the loop to wrongly
abandon a correct plan.

A tool call can come back as *rejected* with text like "the user doesn't want
to proceed with this tool use," accompanied by `<system-reminder>` /
additional-context blocks (recent messages, pending events, gate text). **That
is the harness injecting state at a tool-call boundary — it is an obligation
gate denying the call and/or the prompt-submit hook attaching fresh context.
It is NOT a human interrupting you.**

How to tell them apart:

- **Harness rejection (an obligation / context injection, not an
  interruption):** the rejection carries a `<system-reminder>` /
  additional-context payload — recent messages, a pending-events digest,
  gate-denial text. The correct response is usually to satisfy the gate (read
  the captured output, restart the down watcher, include the missing id) and
  **re-attempt the same action** — do not change your plan. A "rejected" call
  may even have partially run; verify state before retrying so you don't
  double-execute.
- **Real interruption:** a genuine preemption arrives as an actual
  cancellation of the current generation — the daemon's tmux `send-keys`
  injection into the loop's session — or, in a messaging-driven loop, as a
  fresh inbound *message*, not as tool-rejection text. If a human wants to
  redirect you, that redirect comes through the real input channel, not
  disguised as a denied tool call.

When in doubt: inspect the rejection body. If it is hook/gate/context text,
treat it as an obligation to satisfy (or context to absorb) and continue;
do not read it as a veto of your approach.

---

## Where watchers fit: they are the *immediate notifiers*

**Watchers are the live processes that surface state into the loop.** They are
not a fourth tier — they sit underneath the "event" mechanism as its delivery
layer: the immediate notifier that takes a producer's signal and pushes it
into context on the next loop pass. A watcher is a long-lived, supervised
process (a filesystem-event poller, an inbox tailer, a queue observer) owned
by the main loop: the loop spawns it, the loop restarts it on resume, and the
loop is the only thing that may start it. When a watcher observes an external
state change — or picks up an event a producer emitted — it surfaces it as an
event on the next loop pass.

The ownership rule matters: because watchers belong to the main loop, after
any resume / clear / compaction the loop must restart them (it keeps no handle
across the boundary), and a watcher must never be started by anything other
than the loop — otherwise its output goes nowhere.

### Keep the watcher count near one

**Prefer a single general-purpose event-watcher that multiplexes many event
types over a watcher per concern.** Each watcher is a tax, not a feature: it
consumes a background-task handle slot, generates restart noise on every
resume / `/clear` / compaction, triggers DOWN-state alerts when it crashes,
and adds mental load to track across sessions. The general case should stay
near *one* live watcher: a single event-watcher tailing the event bus and
surfacing every event type that lands on it.

The way you keep that count low is **cron-as-producer**: route a new periodic
or scheduled signal through a cron job that *emits a claude-event*, which the
one event-watcher then surfaces — instead of standing up another long-lived
watcher process for it. Reach for a dedicated watcher only when sub-minute
reactivity is genuinely required *and* no kernel event mechanism (inotify,
systemd path units) fits. See the "Watchers are a tax, not a feature" and
"Watcher vs. cron" sections of [`../watchers.md`](../watchers.md) for the full
rationale and decision criteria, and [`../adding-watchers.md`](../adding-watchers.md)
for authoring one.

```
cron / external state change  ──emits──▶  event bus
                                              │
                          event-watcher (the immediate notifier) surfaces it as…
                                              ▼
                                    event (next loop pass)
                                              │
                          (if an actionable event is repeatedly ignored,
                           an enforcement layer escalates it into…)
                                              ▼
                                    obligation (blocks a tool call)
                                              │
                          (if even a blocking gate fires too late,
                           promote it one rung further to…)
                                              ▼
                          interruption — tmux send-keys (preempts the turn)
```

---

## Escalation relationship: one ladder

Event, obligation, and interruption form a single ladder of increasing force.
All three must be handled; the rungs differ in *how* handling is forced — by
judgment, by a mechanical gate, or by seizing the turn:

- An **event** is the lowest rung — it surfaces actionable state that the loop
  must triage and act on by its own judgment on the next pass. (A signal that
  needs no action is not an event at all — it goes to a push-notification
  channel, a log, or a metric.)
- An **obligation** is the next rung up — it *enforces* an invariant
  mechanically, blocking a class of tool calls until satisfied. An actionable
  event that is repeatedly missed is the canonical thing to *promote* into an
  obligation.
- An **interruption (tmux send-keys)** is the top rung — when even a blocking
  gate isn't enough or fires too late, it preempts the turn outright rather
  than gating a future tool call. Promote toward an interruption only when a
  gate demonstrably fires too late to prevent the harm.

Design rule: put each signal at the **lowest rung that actually works**.
First decide whether the loop must act at all — if not, it is not an event;
route it to a push notification, a log, or a metric. If it does need acting
on, reach for an event; promote to an obligation only when relying on the
loop's judgment to handle it is not enough and the "must" needs a mechanical
gate; promote to a tmux send-keys interruption only when "wait for the next
tool call" is too late. The README's
[Alerting hierarchy](../../README.md#alerting-hierarchy) section has the
visual diagram and the mechanism table; this doc is the conceptual companion
that focuses on *how the three differ* rather than on the wiring.

---

## See also

- [`../events.md`](../events.md) — the claude-event bus: emit/read CLIs, schema, debounce.
- [`../event-must-act.md`](../event-must-act.md) — the enforcement layer that escalates an ignored actionable event into a blocking gate.
- [`../watchers.md`](../watchers.md) — watcher lifecycle, ownership, hygiene, and the watcher-vs-cron decision (why the watcher count stays near one).
- [`../adding-watchers.md`](../adding-watchers.md) — authoring a new watcher.
- [`../hooks.md`](../hooks.md) — the hooks layer and the "Obligations gate" (predicate vocabulary, modes, overrides).
- [`../two-channel-design.md`](../two-channel-design.md) — the session/observation channel split where mid-generation interruption mechanics live.
- [README → Alerting hierarchy](../../README.md#alerting-hierarchy) — the escalation diagram + mechanism table.
