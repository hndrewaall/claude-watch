# The event hierarchy: events vs. obligations vs. interruptions

This is the conceptual entry point for the three distinct signaling
mechanisms claude-watch and its sibling tools expose to a Claude Code main
loop, plus the watchers that feed them. If you are a fresh agent trying to
understand "what kinds of things can reach into the loop, and how do they
differ," start here, then follow the cross-links into the per-subsystem docs.

The three mechanisms are **not** variations on one idea. They differ in
*when* they reach the loop, *whether they can be ignored*, and *what they
cost*:

| | **event** | **obligation** | **interruption** |
|---|---|---|---|
| **What it is** | An informational signal surfaced into context | A hard gate on tool calls | A forced preemption of the current turn |
| **When it reaches the loop** | At the next loop pass (next prompt) | At a tool-call boundary, before the call runs | Mid-generation, immediately |
| **Can it be ignored?** | Yes — it's just a line in context | No — it BLOCKS the matching tool call until its predicate is satisfied | No — it cancels what the loop is currently doing |
| **Granularity** | Per loop pass | Per tool call | Per keystroke / generation |
| **Who emits it** | A watcher / producer via the event bus | A `PreToolUse` / `PostToolUse` hook evaluating a predicate | The monitoring daemon (out-of-band) |
| **Cost of using it** | Cheap; adds context noise | Medium; blocks work, must be satisfied or overridden | High; discards partial work |
| **Failure posture** | Best-effort; can be missed | Default-open on internal error, but otherwise blocks | Reserved for can't-wait cases |

The one-line mnemonic:

```
event        <  obligation     <  interruption
(informational) (blocking)        (forced)
"check this      "you may not    "stop what you're
 next pass"        do X until Y"   doing right now"
```

---

## event — an informational signal for the next loop pass

An **event** is a piece of information surfaced into the main loop's context
so that the loop can *decide* whether to act on it. It does not block
anything and it does not preempt anything. The loop sees it on its next pass
and is free to act, defer, or absorb it as context.

- **Mechanism**: a producer emits a small JSON record onto the event bus; a
  watcher debounces a burst of these and surfaces a one-line-per-event batch
  into the loop's context (typically at the next `UserPromptSubmit`).
- **Use it when** the right response is "next time you come around, look at
  this." Periodic ticks, queue state transitions, completed background jobs,
  scheduled reminders, non-blocking alerts.
- **It can be ignored.** There is no enforcement *on the bus itself* — the
  bus only moves the signal into context. If a class of event MUST be handled
  before some other action proceeds, that "must" is not the event's job; it
  is an **obligation** (see below). A companion enforcement layer can be
  layered on top of events to count missed opportunities and escalate an
  ignored-but-actionable event into a blocking gate — that layer is itself an
  obligation, not a property of the event bus.

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
- **It cannot be ignored** the way an event can — it is the enforcement
  layer. But it is also *narrowly scoped*: it only affects tool calls that
  match its pattern, and its failure mode is conservative (default-open on
  internal error) so a broken gate does not wedge the loop.

See the "Obligations gate" section of [`../hooks.md`](../hooks.md) for the
predicate vocabulary, enforcement modes (`gate` vs. `inform`), scope guards,
auto-satisfaction, and the override / exempt mechanics.

---

## interruption — a forced, mid-generation preemption

An **interruption** is a real preemption signal that breaks into the current
turn. Where an event waits for the next pass and an obligation waits for the
next tool call, an interruption does not wait at all — it cancels in-flight
generation and forces the loop to deal with something *now*.

- **Mechanism**: an out-of-band monitor (the daemon, or a genuine human
  preemption) injects directly into the loop's running session, canceling the
  current generation. This is the most disruptive mechanism and is reserved
  for situations where letting the current turn finish would make recovery
  harder or impossible.
- **Use it when** waiting for a turn boundary is too late: context-window
  exhaustion approaching (compaction with uncommitted state is worse than a
  cancelled message), a stalled / zombie session, a dead watcher pipeline, or
  prolonged unproductive generation.
- **It is orthogonal to the event/obligation pair.** Events and obligations
  are about *signal* and *enforcement on signal*; an interruption is about
  *seizing the turn*. An obligation that consistently "fires too late" (the
  bad state is already in motion by the time a tool call is attempted) is a
  candidate to be backed by an interruption instead.

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
- **Real interruption:** a genuine human preemption arrives as an actual
  cancellation of the current generation (e.g. an out-of-band stop), or — in
  a messaging-driven loop — as a fresh inbound *message*, not as tool-rejection
  text. If a human wants to redirect you, that redirect comes through the real
  input channel, not disguised as a denied tool call.

When in doubt: inspect the rejection body. If it is hook/gate/context text,
treat it as an obligation to satisfy (or context to absorb) and continue;
do not read it as a veto of your approach.

---

## Where watchers fit: they are the event *sources*

**Watchers are the background tasks that produce events.** They are not a
fourth tier — they sit underneath the "event" mechanism as its source layer.
A watcher is a long-lived, supervised process (a filesystem-event poller, an
inbox tailer, a queue observer) owned by the main loop: the loop spawns it,
the loop restarts it on resume, and the loop is the only thing that may start
it. When a watcher observes an external state change it emits onto the event
bus, which surfaces it as an event on the next loop pass.

The ownership rule matters: because watchers belong to the main loop, after
any resume / clear / compaction the loop must restart them (it keeps no handle
across the boundary), and a watcher must never be started by anything other
than the loop — otherwise its output goes nowhere. See
[`../watchers.md`](../watchers.md) for the full lifecycle and hygiene rules,
and [`../adding-watchers.md`](../adding-watchers.md) for authoring one.

```
watcher  ──emits──▶  event bus  ──surfaces──▶  event (next loop pass)
                                                  │
                          (if an actionable event is repeatedly ignored,
                           an enforcement layer escalates it into…)
                                                  ▼
                                              obligation (blocks a tool call)

   …and entirely separately, an out-of-band monitor may raise an
   interruption that preempts the current turn regardless of the above.
```

---

## Escalation relationship (and what is orthogonal)

Events and obligations form an escalation ladder; interruptions are a
separate axis:

- An **event** is the lowest tier — it surfaces state and can be ignored.
- An **obligation** is the next tier up — it *enforces* an invariant on
  state, blocking a class of tool calls until satisfied. An actionable event
  that is repeatedly missed is the canonical thing to *promote* into an
  obligation.
- An **interruption** is orthogonal: rather than informing or gating, it
  seizes the turn. Promote toward an interruption only when a gate
  demonstrably fires too late to prevent the harm.

Design rule: put each signal at the **lowest tier that actually works**.
Reach for an event first; promote to an obligation only when "can be ignored"
is unacceptable; reach for an interruption only when "wait for the next tool
call" is too late. The README's
[Alerting hierarchy](../../README.md#alerting-hierarchy) section has the
visual diagram and the mechanism table; this doc is the conceptual companion
that focuses on *how the three differ* rather than on the wiring.

---

## See also

- [`../events.md`](../events.md) — the claude-event bus: emit/read CLIs, schema, debounce.
- [`../event-must-act.md`](../event-must-act.md) — the enforcement layer that escalates an ignored actionable event into a blocking gate.
- [`../watchers.md`](../watchers.md) — watcher lifecycle, ownership, and operator hygiene (the event sources).
- [`../adding-watchers.md`](../adding-watchers.md) — authoring a new watcher.
- [`../hooks.md`](../hooks.md) — the hooks layer and the "Obligations gate" (predicate vocabulary, modes, overrides).
- [`../two-channel-design.md`](../two-channel-design.md) — the session/observation channel split where mid-generation interruption mechanics live.
- [README → Alerting hierarchy](../../README.md#alerting-hierarchy) — the escalation diagram + mechanism table.
