import type { RedirectionNode } from "../ast/types.js";
import { FIRST_USER_FD } from "./fd-table.js";

/**
 * The descriptor a redirection acts on, or null when it targets both
 * stdout and stderr (`&>`), which never names a user fd.
 */
export function effectiveRedirectFd(redir: RedirectionNode): number | null {
  switch (redir.operator) {
    case "<":
    case "<>":
    case "<<":
    case "<<-":
    case "<<<":
    case "<&":
      return redir.fd ?? 0;
    case ">":
    case ">>":
    case ">|":
    case ">&":
      return redir.fd ?? 1;
    default:
      return null;
  }
}

/** True when this redirection is installed in the descriptor table. */
export function isNumericFdRedirection(redir: RedirectionNode): boolean {
  if (redir.fdVariable) return false;
  const fd = effectiveRedirectFd(redir);
  return fd !== null && fd >= FIRST_USER_FD;
}
