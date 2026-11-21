// bd-gu288: the same plain-English wording agent_text.rs renders (WAKE_PREFIX
// + new/urgent mail + who + the exact read command). Kept in one place in the
// plugin and pinned by a source-scan test against the Rust golden.

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
  return `${WAKE_PREFIX} ${what}${scope}. Read it with: toron next --project ${frame.project} --as ${pin}`;
}
