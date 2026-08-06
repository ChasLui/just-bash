/**
 * Numeric File Descriptor Redirections
 *
 * Applies the redirections that target a user descriptor (fd >= 3) to the
 * shell's descriptor table: `N< file`, `N> file`, `N>> file`, `N<> file`,
 * `N<<EOF`, `N<<<word`, `N<&M`, `N>&M`, `N<&M-`, `N>&M-`, `N<&-`, `N>&-`.
 *
 * The same routine serves every construct that can carry redirections —
 * simple commands, `exec`, compound commands (`done N< file`) and function
 * calls — so there is a single place that decides what a numeric fd means.
 *
 * Lifetime follows bash:
 * - `exec N< file` (persistent) keeps the descriptor open until it is closed.
 * - Every other construct gets the descriptor for the duration of that
 *   command only; {@link openNumericFds} returns a `restore` closure that
 *   puts the table back exactly as it was, including a descriptor's read
 *   position.
 *
 * Descriptors 0/1/2 are deliberately untouched here: stdin is resolved by
 * the caller and stdout/stderr are delivered by `applyRedirections`.
 */

import type { HereDocNode, RedirectionNode, WordNode } from "../ast/types.js";
import type { ExecResult } from "../types.js";
import {
  expandRedirectTarget,
  expandWord,
  hasQuotedMultiValueAt,
} from "./expansion.js";
import {
  closeFd,
  dupFd,
  type FdEntry,
  type FdSnapshot,
  FIRST_USER_FD,
  rememberFd,
  restoreFds,
  setFdEntry,
} from "./fd-table.js";
import { result as makeResult } from "./helpers/result.js";
import type { ExpandedRedirectTargets } from "./redirections.js";
import type { InterpreterContext } from "./types.js";

/** Result of applying the numeric-fd redirections of one command. */
export interface NumericFdScope {
  /** Non-null when a redirection failed; the command must not run. */
  error: ExecResult | null;
  /** Targets expanded here, so callers never expand them a second time. */
  targets: ExpandedRedirectTargets;
  /** Undo the scoped descriptor changes. Idempotent. */
  restore: () => void;
}

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

/** True when this redirection is handled by the fd table rather than stdio. */
export function isNumericFdRedirection(redir: RedirectionNode): boolean {
  if (redir.fdVariable) return false;
  const fd = effectiveRedirectFd(redir);
  return fd !== null && fd >= FIRST_USER_FD;
}

function badFd(fd: number): ExecResult {
  return makeResult("", `bash: ${fd}: Bad file descriptor\n`, 1);
}

async function hereDocContent(
  ctx: InterpreterContext,
  hereDoc: HereDocNode,
): Promise<string> {
  const content = await expandWord(ctx, hereDoc.content);
  if (!hereDoc.stripTabs) return content;
  return content
    .split("\n")
    .map((line) => line.replace(/^\t+/, ""))
    .join("\n");
}

/**
 * Duplicate `sourceFd` onto `fd`. Copying the source's table entry is what
 * makes `N<&M` share the file — and, for input descriptors, the read
 * position — with M, the way `dup2()` does.
 */
function duplicateFd(
  ctx: InterpreterContext,
  fd: number,
  sourceFd: number,
  isInput: boolean,
): ExecResult | null {
  if (dupFd(ctx, fd, sourceFd)) return null;
  if (sourceFd >= FIRST_USER_FD) return badFd(sourceFd);
  // 0/1/2 never live in the table; record which standard stream this is.
  const entry: FdEntry = isInput
    ? { kind: "dup-in", sourceFd }
    : { kind: "dup-out", sourceFd };
  setFdEntry(ctx, fd, entry);
  return null;
}

/**
 * Apply every numeric-fd redirection in `redirections`, in order.
 *
 * Returns as soon as one fails, with `error` set: bash abandons the rest of
 * the redirection list and does not run the command.
 *
 * Whether the descriptors survive the command is the CALLER's decision: it
 * simply calls `restore` (ordinary commands, compound commands, functions)
 * or does not (`exec`, whose redirections apply to the whole shell).
 */
export async function openNumericFds(
  ctx: InterpreterContext,
  redirections: RedirectionNode[],
): Promise<NumericFdScope> {
  const targets: ExpandedRedirectTargets = new Map();
  const snapshot: FdSnapshot = new Map();
  let restored = false;
  const restore = (): void => {
    if (restored) return;
    restored = true;
    restoreFds(ctx, snapshot);
  };
  const empty: NumericFdScope = { error: null, targets, restore };

  if (!redirections.some(isNumericFdRedirection)) return empty;

  const remember = (fd: number): void => rememberFd(ctx, snapshot, fd);
  const fail = (error: ExecResult): NumericFdScope => ({
    error,
    targets,
    restore,
  });

  for (let index = 0; index < redirections.length; index++) {
    const redir = redirections[index];
    if (!isNumericFdRedirection(redir)) continue;
    const fd = effectiveRedirectFd(redir) as number;

    if (redir.target.type === "HereDoc") {
      remember(fd);
      setFdEntry(ctx, fd, {
        kind: "input",
        content: await hereDocContent(ctx, redir.target),
      });
      continue;
    }

    const isDup = redir.operator === "<&" || redir.operator === ">&";
    let target: string;
    if (isDup) {
      if (hasQuotedMultiValueAt(ctx, redir.target as WordNode)) {
        return fail(makeResult("", "bash: $@: ambiguous redirect\n", 1));
      }
      target = await expandWord(ctx, redir.target as WordNode);
    } else {
      const expanded = await expandRedirectTarget(
        ctx,
        redir.target as WordNode,
      );
      if ("error" in expanded) {
        return fail(makeResult("", expanded.error, 1));
      }
      target = expanded.target;
    }
    targets.set(index, target);

    if (target.includes("\0")) {
      return fail(
        makeResult(
          "",
          `bash: ${target.replace(/\0/g, "")}: No such file or directory\n`,
          1,
        ),
      );
    }

    if (isDup) {
      // `N>&-` / `N<&-`: closing a descriptor that is not open is not an
      // error in bash, it is simply a no-op.
      if (target === "-") {
        remember(fd);
        closeFd(ctx, fd);
        continue;
      }
      // `N>&M-` / `N<&M-`: duplicate M onto N, then close M.
      const isMove = target.endsWith("-");
      const sourceText = isMove ? target.slice(0, -1) : target;
      const sourceFd = Number.parseInt(sourceText, 10);
      if (Number.isNaN(sourceFd) || !/^\d+$/.test(sourceText)) {
        if (redir.operator === "<&") {
          return fail(
            makeResult("", `bash: ${target}: ambiguous redirect\n`, 1),
          );
        }
        // `N>&word` with a non-numeric word is a plain file redirect.
        remember(fd);
        const filePath = ctx.fs.resolvePath(ctx.state.cwd, target);
        const openError = await openOutputFile(
          ctx,
          fd,
          filePath,
          target,
          false,
        );
        if (openError) return fail(openError);
        continue;
      }
      remember(fd);
      const dupError = duplicateFd(ctx, fd, sourceFd, redir.operator === "<&");
      if (dupError) return fail(dupError);
      // The source of a move is deliberately NOT snapshotted: bash does not
      // reopen it when the command's redirections are undone, so after
      // `: 6>&7-` descriptor 7 stays closed. The spec suite pins this
      // ("1>&2- (Bash bug: fail to restore closed fd)").
      if (isMove && sourceFd !== fd) closeFd(ctx, sourceFd);
      continue;
    }

    const filePath = ctx.fs.resolvePath(ctx.state.cwd, target);
    remember(fd);

    switch (redir.operator) {
      case "<": {
        // Text read, matching `exec N< file` and `while ... < file`: the
        // content lands in shell variables via `read`, so it must stay
        // decoded rather than becoming a latin1 byte buffer.
        let content: string;
        try {
          content = await ctx.fs.readFile(filePath);
        } catch {
          return fail(
            makeResult("", `bash: ${target}: No such file or directory\n`, 1),
          );
        }
        setFdEntry(ctx, fd, { kind: "input", content });
        break;
      }
      case "<<<": {
        setFdEntry(ctx, fd, { kind: "input", content: `${target}\n` });
        break;
      }
      case ">":
      case ">|":
      case ">>": {
        const append = redir.operator === ">>";
        const openError = await openOutputFile(
          ctx,
          fd,
          filePath,
          target,
          append,
          redir.operator === ">|",
        );
        if (openError) return fail(openError);
        break;
      }
      case "<>": {
        let content: string;
        try {
          content = await ctx.fs.readFile(filePath);
        } catch {
          content = "";
          try {
            await ctx.fs.writeFile(filePath, "", "utf8");
          } catch {
            return fail(
              makeResult("", `bash: ${target}: No such file or directory\n`, 1),
            );
          }
        }
        setFdEntry(ctx, fd, {
          kind: "readwrite",
          path: filePath,
          position: 0,
          content,
        });
        break;
      }
    }
  }

  return empty;
}

/**
 * Run a compound command with its numeric-fd redirections installed.
 *
 * `done 3< file` keeps descriptor 3 open for every statement in the body and
 * takes it back down afterwards, restoring whatever the enclosing shell had
 * on 3 — including that descriptor's read position.
 *
 * The expanded targets are handed to `run` so the body's own redirection
 * pass reuses them instead of expanding (and re-running) them a second time.
 */
export async function withNumericFds(
  ctx: InterpreterContext,
  redirections: RedirectionNode[],
  run: (targets: ExpandedRedirectTargets) => Promise<ExecResult>,
): Promise<ExecResult> {
  const scope = await openNumericFds(ctx, redirections);
  if (scope.error) {
    scope.restore();
    return scope.error;
  }
  try {
    return await run(scope.targets);
  } finally {
    scope.restore();
  }
}

/**
 * Open `filePath` on `fd` for writing. `>` truncates at open time even when
 * nothing is ever written to the descriptor, so the file is created here.
 */
async function openOutputFile(
  ctx: InterpreterContext,
  fd: number,
  filePath: string,
  target: string,
  append: boolean,
  isClobber = false,
): Promise<ExecResult | null> {
  try {
    if (await ctx.fs.exists(filePath)) {
      const stat = await ctx.fs.stat(filePath);
      if (stat.isDirectory) {
        return makeResult("", `bash: ${target}: Is a directory\n`, 1);
      }
      if (
        !append &&
        ctx.state.options.noclobber &&
        !isClobber &&
        target !== "/dev/null"
      ) {
        return makeResult(
          "",
          `bash: ${target}: cannot overwrite existing file\n`,
          1,
        );
      }
    }
  } catch {
    return makeResult("", `bash: ${target}: cannot open redirect target\n`, 1);
  }

  try {
    if (append) await ctx.fs.appendFile(filePath, "", "binary");
    else await ctx.fs.writeFile(filePath, "", "binary");
  } catch {
    return makeResult("", `bash: ${target}: cannot open redirect target\n`, 1);
  }
  setFdEntry(ctx, fd, { kind: "output", path: filePath, append });
  return null;
}
