// bd-krzqu: decode the frame `flywheel mcp-events` actually emits. The proxy is
// a stdio JSON-RPC server: on `events/poll` it writes a
// `notifications/claude/channel` notification whose `content` is the fully
// rendered agent_text hint and whose `meta` carries the bus ids. Never the body.

export const WAKE_NAMES: ReadonlySet<string> = new Set([
  "mail.addressed",
  "reservation.conflict",
  "ack_sla_expired",
  "role.changed",
  "policy.changed",
]);

/** The one wire method the proxy emits. We do not accept the old occurrence shape. */
export const PROXY_METHOD = "notifications/claude/channel";

export interface EventFrame {
  id: string;
  name: string;
  urgent: boolean;
  project: string;
  senderPin: string;
  /** The hint the proxy rendered (agent_text wording); used verbatim. */
  hint: string;
}

export function decodeFrame(line: string, subscriptionId: string): EventFrame | null {
  let v: any;
  try {
    v = JSON.parse(line);
  } catch {
    return null;
  }
  if (String(v?.method ?? "") !== PROXY_METHOD) return null;

  const params = v?.params ?? {};
  const meta = params.meta ?? {};
  const name = String(meta.event_name ?? "");
  if (!WAKE_NAMES.has(name)) return null;
  const id = String(meta.event_id ?? "");
  if (!id) return null;

  // mcp-events is launched with --subscription <pin>; native_subscribe_result
  // echoes it. If a routed frame names a different subscription, ignore it.
  const routed = String(meta.subscription_id ?? meta.subscriptionId ?? "");
  if (routed && routed !== subscriptionId) return null;

  // `content` is the plain-English hint. Never parse or forward the mail body.
  const hint = String(params.content ?? "");
  if (!hint) return null;

  const urgent =
    meta.importance === "urgent" ||
    meta.priority === "urgent" ||
    name === "ack_sla_expired" ||
    name === "reservation.conflict";
  return {
    id,
    name,
    urgent,
    project: String(meta.project ?? ""),
    senderPin: String(meta.sender_pin ?? meta.senderPin ?? ""),
    hint,
  };
}
