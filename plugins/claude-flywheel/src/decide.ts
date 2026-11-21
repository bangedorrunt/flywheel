// bd-9os20: the plugin-side mirror of src/hook_decide.rs. Kept in sync by the
// same unit cases; the binary is authoritative, this copy lets the module
// decide without a process call on every event.

export type Action = "submit" | "append" | "drop";

export interface Wake {
  id: string;
  urgent: boolean;
  idle: boolean;
  subagent: boolean;
  claimed: boolean;
  failures: number;
}

export interface Decision {
  action: Action;
  status: string;
}

export const MAX_FAILURES = 3;

export function statusHealthy(): string {
  return "flywheel wake delivery active";
}

export function statusStopped(failures: number): string {
  return `flywheel wake delivery stopped: ${failures} failed checks (check toron MCP)`;
}

export function decide(w: Wake): Decision {
  const healthy = w.failures < MAX_FAILURES && !w.subagent && w.claimed;
  let action: Action = "drop";
  if (healthy) {
    if (w.urgent) action = "append";
    else if (w.idle) action = "submit";
  }
  const status = w.subagent
    ? "flywheel wake delivery: subagents do not receive notices"
    : w.failures >= MAX_FAILURES
      ? statusStopped(w.failures)
      : statusHealthy();
  return { action, status };
}
