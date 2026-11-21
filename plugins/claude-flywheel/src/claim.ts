// bd-tdpix wake-rung, node spelling: O_CREAT|O_EXCL, exactly one claimer per
// bus event id across Rust and TypeScript. The plugin claims BEFORE delivery.

import { closeSync, mkdirSync, openSync } from "node:fs";
import { join } from "node:path";

export function markerPath(dir: string, eventId: string): string {
  const safe = [...eventId]
    .map((c) => (/[A-Za-z0-9_-]/.test(c) ? c : "_"))
    .slice(0, 120)
    .join("");
  return join(dir, `${safe || "empty"}.rung`);
}

export function claimEvent(dir: string, eventId: string): boolean {
  mkdirSync(dir, { recursive: true });
  try {
    const fd = openSync(markerPath(dir, eventId), "wx");
    closeSync(fd);
    return true;
  } catch (e: any) {
    if (e?.code === "EEXIST") return false;
    throw e;
  }
}
