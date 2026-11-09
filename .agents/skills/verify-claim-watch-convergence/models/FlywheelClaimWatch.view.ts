export function state(vars: Record<string, any>): string {
  const ls = String(vars.ledger_state ?? "Ready");
  const watch = Boolean(vars.watch_active);
  const polls = Number(vars.unclaimed_polls ?? 0);
  const grace = Number(vars.grace_ticks ?? 0);
  const ver = Boolean(vars.verifier_dispatched);
  const locks = Boolean(vars.locks_released);
  const reopened = Boolean(vars.reopened_by_loop);

  if (!watch) {
    if (ls === "Ready" && polls >= 2) {
      return "Watch cleared: ReassignOrRecover (unclaimed)";
    }
    if (ls === "NotReadyOpen") {
      return "Watch cleared: NotReadyOpen (locks kept)";
    }
    if (ls === "ClosedLegal") {
      return "Converged: ClosedLegal (verifier+locks released)";
    }
    if ((ls === "Blocked" || ls === "CaptainHeld") && !reopened && !locks && !ver) {
      return `Watch cleared: ${ls} (locks kept)`;
    }
    if ((ls === "ClosedNoCommit" || ls === "ClosedMissingVerdict") && reopened && locks && !ver) {
      return `Reopened unverified ${ls} (locks released, no verifier)`;
    }
    if (reopened || locks || ver) {
      return `VIOLATION on ${ls}: reopened=${reopened}, verifier=${ver}, locks_rel=${locks}`;
    }
    return `Watch inactive (${ls})`;
  }

  if (ls === "Ready") {
    return "Watching Ready";
  }
  if (ls === "InProgress") {
    return "Watching InProgress";
  }
  if (ls === "ClosedMissingVerdict" && grace > 0) {
    return `Grace tick ${grace}/2 on ClosedMissingVerdict (verifier=${ver})`;
  }
  return `Watching ${ls} (absent from ready+inprog)`;
}

export function step(
  action: string,
  _from: Record<string, any>,
  _to: Record<string, any>,
): string | null {
  if (action === "Done") return null;
  return action;
}
