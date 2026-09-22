# Debate-loop prompt templates 

Templates for the N-round A/B debate + judge recipe. Each prompt is delivered
via `send_message` on thread `debate-<ts>` (thread continuity) with
`ack_required=true` (turn finality). The orchestrator harvests the latest
message per speaker via `search_messages(thread_id, sender, ranking=recency,
limit=1)`.

## OPENING (agent A, round 1)

```
You are debating proposition: {proposition}

Argue FOR the proposition. Be concise and concrete (<= 120 words). Provide
your single strongest argument with a brief reason.

After replying in this thread, call acknowledge_message on the orchestrator's
opening message (thread {thread}) to signal you have finished your turn.
```

## COUNTER (agent B / A, rounds 2..N)

```
You are debating proposition: {proposition}

Counter the opponent's latest argument with a NEW point (do not repeat your
earlier arguments). Keep it concise (<= 120 words). Acknowledge the
orchestrator's prompt in thread {thread} when your turn is done.

Opponent's latest argument: {reply}
```

## JUDGE (after N rounds)

```
You are the judge of this debate on: {proposition}

Read the full thread {thread} via summarize_thread (it contains N rounds of
FOR/COUNTER). Pick a winner in <= 100 words: name the winner's argument and
why it is stronger. Reply in thread {thread} and acknowledge when done.
```

## Invariants

- Every prompt carries `ack_required=true`; turn advancement is ack-gated.
- The proposition appears ONLY in the opening + judge prompts (counters
  reference the prior reply, since long prompts degrade quality).
- The judge uses `summarize_thread`, not raw message scrolling.
