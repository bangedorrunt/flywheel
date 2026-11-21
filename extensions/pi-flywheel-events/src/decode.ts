// Pure decode of toron SEP-3415 events into a D2 hint (bd-4n2bn, ADR-0039 D8).
// No mail body is ever returned — only the event id/name and the fixed hint.

// bd-w32ho: agent_text::render wording (WAKE_PREFIX + plain English + read
// command), not bespoke jargon. The Rust golden pins the same strings.
export const HINT = "[wake] new mail. Read it with: toron next";
export const URGENT_HINT = "[wake] urgent mail. Read it with: toron next";

/** Toron catalogue names that wake an agent. Heartbeats/active/terminated/gap
 * are notifications, not work. */
export const WAKE_NAMES: ReadonlySet<string> = new Set([
  "mail.addressed",
  "reservation.conflict",
  "ack_sla_expired",
  "role.changed",
  "policy.changed",
]);

export interface EventFrame {
  id: string;
  name: string;
  urgent: boolean;
}

/** Decode one SSE `data:` payload routed to our subscription. Returns null for
 * another subscription's frame, a JSON-RPC result, or a non-wake notification.
 * The payload's body is never read or forwarded. */
export function decodeFrame(data: string, subscriptionId: string): EventFrame | null {
  let v: any;
  try {
    v = JSON.parse(data);
  } catch {
    return null;
  }
  if (v?.method !== "notifications/events/event") return null;
  const p = v.params ?? {};
  const routed = p?._meta?.["io.modelcontextprotocol/subscriptionId"];
  if (routed !== subscriptionId) return null;
  // Final: occurrence fields are the params themselves. Tolerate the old
  // nested `event` during the transition (G13 constraint a).
  const event = p.event ?? p;
  const payload = event.data ?? {};
  const name = String(event.name ?? event.kind ?? "");
  if (!WAKE_NAMES.has(name)) return null;
  const id = String(event.eventId ?? event.event_id ?? event.id ?? "");
  if (!id) return null;
  const urgent = payload.priority === "urgent" || payload.importance === "urgent"
    || event.priority === "urgent" || name === "ack_sla_expired";
  return { id, name, urgent };
}

/** One coalesced hint for a burst: the deliverAs mode and meta carry the ids,
 * never the bodies. `count` and `firstId` let the doorbell dedupe a seat reached
 * by both paths. */
export interface HintNotice {
  hint: string;
  deliverAs: "followUp" | "steer";
  count: number;
  firstId: string;
  name: string;
}

export function coalesce(frames: EventFrame[]): HintNotice | null {
  if (frames.length === 0) return null;
  const urgent = frames.some((f) => f.urgent);
  return {
    hint: urgent ? URGENT_HINT : HINT,
    deliverAs: urgent ? "steer" : "followUp",
    count: frames.length,
    firstId: frames[0].id,
    name: frames.length === 1 ? frames[0].name : "mixed",
  };
}
