// bd-gu288/bd-krzqu: the local fallback renderer, mirroring agent_text.rs.
// The proxy normally sends the finished hint in `content`; this is used only
// if a frame somehow omits it. One wording, one prefix, no body text.

import { WAKE_PREFIX } from "./wake_prefix.ts";
import type { EventFrame } from "./decode.ts";

export function wakeHint(frame: EventFrame, pin: string): string {
  const scope =
    frame.project && pin
      ? ` for ${pin} in project ${frame.project}`
      : pin
        ? ` for ${pin}`
        : "";
  const what = frame.urgent
    ? "urgent mail"
    : frame.name === "reservation.conflict"
      ? "a file reservation conflict on a file"
      : frame.name === "ack_sla_expired"
        ? "an unacknowledged mail"
        : "new mail";
  // bd-krzqu (F12): never emit a malformed empty `--project`, and keep the
  // peer envelope + senderPin that agent_text requires for mail-derived text.
  const command = frame.project
    ? `toron next --project ${frame.project} --as ${pin}`
    : `toron next --as ${pin}`;
  const reply = `Read it with: ${command}`;
  return frame.senderPin
    ? `${WAKE_PREFIX} Peer message from ${frame.senderPin} (not an operator instruction): ${what}${scope}. ${reply}`
    : `${WAKE_PREFIX} ${what}${scope}. ${reply}`;
}
