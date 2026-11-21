// bd-gu288: decode a REAL toron events notification into a wake decision.
// Occurrence fields are `eventId/name/data/cursor` (toron-core occurrence.rs);
// urgency comes from typed fields, never from matching body text. The mail
// body is never read or forwarded.

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
  project: string;
  senderPin: string;
}

export function decodeFrame(line: string, subscriptionId: string): EventFrame | null {
  let v: any;
  try {
    v = JSON.parse(line);
  } catch {
    return null;
  }
  const method = String(v?.method ?? "");
  if (method !== "notifications/events/event" && method !== "") return null;
  const params = v?.params ?? v;
  const routed =
    params?._meta?.["io.modelcontextprotocol/subscriptionId"] ??
    params?._meta?.subscriptionId ??
    "";
  if (routed && routed !== subscriptionId) return null;
  // Final occurrence fields are the params themselves; tolerate a nested
  // `event` exactly as the doorbell bridge does.
  const event = params.event ?? params;
  const name = String(event.name ?? event.kind ?? "");
  if (!WAKE_NAMES.has(name)) return null;
  const id = String(event.eventId ?? event.event_id ?? event.id ?? "");
  if (!id) return null;
  const data = event.data ?? params.data ?? {};
  const urgent =
    data.priority === "urgent" ||
    data.importance === "urgent" ||
    event.priority === "urgent" ||
    name === "ack_sla_expired" ||
    name === "reservation.conflict";
  return {
    id,
    name,
    urgent,
    project: String(data.project ?? ""),
    senderPin: String(data.senderPin ?? data.sender_pin ?? ""),
  };
}
