/**
 * Control Flow Execution
 *
 * Handles control flow constructs:
 * - if/elif/else
 * - for loops
 * - C-style for loops
 * - while loops
 * - until loops
 * - case statements
 * - break/continue
 */

import type {
  CaseNode,
  CStyleForNode,
  ForNode,
  HereDocNode,
  IfNode,
  RedirectionNode,
  StatementNode,
  UntilNode,
  WhileNode,
  WordNode,
} from "../ast/types.js";
import { utf8ByteLength } from "../encoding.js";
import type { ExecResult } from "../types.js";
import { evaluateArithmetic } from "./arithmetic.js";
import { matchPattern } from "./conditionals.js";
import {
  BreakError,
  ContinueError,
  ErrexitError,
  ExecutionLimitError,
  ExitError,
  GlobError,
  isScopeExitError,
  SubshellExitError,
} from "./errors.js";
import {
  escapeGlobChars,
  expandWord,
  expandWordWithGlob,
  isWordFullyQuoted,
} from "./expansion.js";
import { appendBoundedElements } from "./helpers/bounded-array.js";
import { executeCondition } from "./helpers/condition.js";
import { getErrorMessage } from "./helpers/errors.js";
import { handleLoopError } from "./helpers/loop.js";
import { failure, throwExecutionLimit } from "./helpers/result.js";
import {
  isNumericFdRedirection,
  withNumericFds,
} from "./numeric-fd-redirects.js";
import {
  applyRedirections,
  type ExpandedRedirectTargets,
  preOpenOutputRedirects,
} from "./redirections.js";
import type { InterpreterContext } from "./types.js";

/**
 * Redirection operators a `while`/`until` loop consumes itself to seed the
 * stdin its body and condition read from. Everything else on the loop is an
 * ordinary compound-command redirection handled by preOpen/applyRedirections.
 */
const LOOP_STDIN_OPERATORS: ReadonlySet<string> = new Set([
  "<",
  "<<",
  "<<-",
  "<<<",
]);

type LoopRedirectPrep =
  | { error: ExecResult }
  | {
      /**
       * stdin supplied by the loop's OWN input redirections, or undefined when
       * the loop has none. Distinguishing "no input redirection" from "an input
       * redirection that produced an empty stream" matters: the first inherits
       * the enclosing read stream, the second replaces it with an empty one.
       */
      ownStdin: string | undefined;
      redirections: RedirectionNode[];
      targets: ExpandedRedirectTargets;
    };

/**
 * Prepare the loop-level redirections of a `while`/`until` loop.
 *
 * Bash installs a loop's redirections once, before the loop starts, keeps them
 * in effect for the condition and every iteration, and processes the list
 * STRICTLY LEFT TO RIGHT — so a redirection that fails leaves every earlier
 * one already applied. `done > out < nosuch` truncates `out` and then fails;
 * `done < nosuch > out` fails first and leaves `out` alone.
 *
 * The walk below reproduces that order:
 *
 * - Input redirections (`< file`, here-docs, here-strings) are consumed as
 *   encountered to produce the stdin the loop body reads from — this is what
 *   makes `while read line; do ...; done < file` work. A failure here aborts
 *   with the diagnostic routed through the redirections opened so far, which
 *   is why `done 2> err < nosuch` lands the message in `err` like bash does.
 * - Every other redirection is collected into a pending run and pre-opened the
 *   moment an input redirection (or the end of the list) is reached, using the
 *   same `preOpenOutputRedirects`/`applyRedirections` pair as `for`/`case`.
 *   Pre-opening truncates `>` targets before the condition can read them, so
 *   `done > f < f` sees an empty `f`.
 *
 * Two invariants worth preserving when touching this:
 *
 * - `ExpandedRedirectTargets` is keyed by POSITION in the array handed to
 *   `preOpenOutputRedirects`. Each pending run is a fresh call, so its keys
 *   are rebased onto the accumulated `opened` array before being merged —
 *   `opened` and `targets` must stay index-coherent for `applyRedirections`.
 * - Input redirections never enter the pre-open/apply list. They are already
 *   fully consumed here, and routing them through `preOpenOutputRedirects`
 *   would expand their targets a second time — with here-string globbing
 *   semantics that bash does not apply to `<<<`.
 *
 * Redirections that name a descriptor by number (`done 3< file`, `done 3> f`)
 * were already applied to the fd table by `withNumericFds` before this walk
 * starts, so they are neither loop stdin nor an ordering barrier here. The
 * `3<` family is skipped outright; the rest ride along in the pending run,
 * where `preOpenOutputRedirects` and `applyRedirections` ignore them by
 * descriptor number. Their targets travel in `fdTargets` — keyed by position
 * in the ORIGINAL list — so a target with side effects (`done 3> "f$((n++))"`)
 * is expanded exactly once. That is a SECOND index space on top of the
 * per-run rebasing above: `fdTargets` is re-keyed onto each run before the
 * call, and the run's result is then re-keyed onto `opened` as usual.
 */
async function prepareLoopRedirections(
  ctx: InterpreterContext,
  redirections: RedirectionNode[],
  fdTargets: ExpandedRedirectTargets,
): Promise<LoopRedirectPrep> {
  let ownStdin: string | undefined;

  // Output redirections already pre-opened, in list order, with their
  // pre-expanded targets keyed by position in `opened`.
  const opened: RedirectionNode[] = [];
  const targets: ExpandedRedirectTargets = new Map();
  // The run of output redirections seen since the last input redirection,
  // alongside each entry's position in the ORIGINAL `redirections` array so
  // the numeric-fd targets can be re-keyed onto the run.
  let pending: RedirectionNode[] = [];
  let pendingSources: number[] = [];

  /** Pre-open the pending run, rebasing its target keys onto `opened`. */
  const openPending = async (): Promise<ExecResult | null> => {
    if (pending.length === 0) {
      return null;
    }
    const run = pending;
    const sources = pendingSources;
    pending = [];
    pendingSources = [];
    // Re-key the targets `withNumericFds` already expanded from original-list
    // positions onto this run's positions.
    const runFdTargets: ExpandedRedirectTargets = new Map();
    for (let i = 0; i < sources.length; i++) {
      const expanded = fdTargets.get(sources[i]);
      if (expanded !== undefined) {
        runFdTargets.set(i, expanded);
      }
    }
    const prepared = await preOpenOutputRedirects(ctx, run, runFdTargets);
    if (prepared.error) {
      return prepared.error;
    }
    const offset = opened.length;
    for (const [index, target] of prepared.targets) {
      targets.set(offset + index, target);
    }
    opened.push(...run);
    return null;
  };

  for (let index = 0; index < redirections.length; index++) {
    const redir = redirections[index];
    if (!LOOP_STDIN_OPERATORS.has(redir.operator)) {
      pending.push(redir);
      pendingSources.push(index);
      continue;
    }

    // `done 3< file` names a descriptor, not this loop's stdin — the fd table
    // already holds it. It is not an ordering barrier either, since the whole
    // numeric pass ran before this walk, so it does not close the pending run.
    if (isNumericFdRedirection(redir)) continue;

    // Everything to the left of this input redirection is opened first.
    const openError = await openPending();
    if (openError) {
      return {
        error: await applyRedirections(ctx, openError, opened, targets),
      };
    }

    if (
      (redir.operator === "<<" || redir.operator === "<<-") &&
      redir.target.type === "HereDoc"
    ) {
      const hereDoc = redir.target as HereDocNode;
      let content = await expandWord(ctx, hereDoc.content);
      if (hereDoc.stripTabs) {
        content = content
          .split("\n")
          .map((line) => line.replace(/^\t+/, ""))
          .join("\n");
      }
      ownStdin = content;
    } else if (redir.operator === "<<<" && redir.target.type === "Word") {
      ownStdin = `${await expandWord(ctx, redir.target as WordNode)}\n`;
    } else if (redir.operator === "<" && redir.target.type === "Word") {
      const target = await expandWord(ctx, redir.target as WordNode);
      try {
        const filePath = ctx.fs.resolvePath(ctx.state.cwd, target);
        ownStdin = await ctx.fs.readFile(filePath);
      } catch {
        // Redirections to the left are already installed and apply to the
        // diagnostic; the ones to the right are never opened.
        return {
          error: await applyRedirections(
            ctx,
            failure(`bash: ${target}: No such file or directory\n`),
            opened,
            targets,
          ),
        };
      }
    }
  }

  const openError = await openPending();
  if (openError) {
    return { error: await applyRedirections(ctx, openError, opened, targets) };
  }

  return { ownStdin, redirections: opened, targets };
}

/**
 * Decide whether a loop takes ownership of the shared read stream
 * (`ctx.state.groupStdin`).
 *
 * A loop only installs — and therefore only restores — a stdin it brought
 * itself: its own input redirection (`< file`, here-doc, here-string), or the
 * stdin handed to it as a pipeline stage. A stream merely INHERITED from an
 * enclosing group or loop must be left in place, because reads inside the body
 * advance it and restoring it afterwards would rewind the shared read
 * position: `printf 'a\nb\n' | { while read x; do break; done; read y; }` has
 * to see `y=b`, not `y=a`.
 *
 * `ownStdin` of `""` still counts as ownership — `done < empty-file` gives the
 * body an empty stream rather than the enclosing one.
 */
function resolveLoopStdin(
  ownStdin: string | undefined,
  pipelineStdin: string,
): { owns: true; stdin: string } | { owns: false } {
  if (ownStdin !== undefined) {
    return { owns: true, stdin: ownStdin };
  }
  if (pipelineStdin !== "") {
    return { owns: true, stdin: pipelineStdin };
  }
  return { owns: false };
}

class CompoundOutput {
  private stdoutChunks: string[] = [];
  private stderrChunks: string[] = [];
  private totalBytes = 0;

  constructor(private readonly ctx: InterpreterContext) {}

  append(stdout: string, stderr: string): void {
    const addedBytes = utf8ByteLength(stdout) + utf8ByteLength(stderr);
    if (addedBytes > this.ctx.limits.maxOutputSize - this.totalBytes) {
      throwExecutionLimit(
        `total output size exceeded (>${this.ctx.limits.maxOutputSize} bytes), increase executionLimits.maxOutputSize`,
        "output_size",
      );
    }
    if (stdout) this.stdoutChunks.push(stdout);
    if (stderr) this.stderrChunks.push(stderr);
    this.totalBytes += addedBytes;
  }

  /** Append output synthesized here rather than relayed from a child. */
  appendUnaccounted(stdout: string, stderr: string): void {
    this.ctx.executionScope.appendOutput("stdout", stdout, "control-flow");
    this.ctx.executionScope.appendOutput("stderr", stderr, "control-flow");
    this.append(stdout, stderr);
  }

  replace(stdout: string, stderr: string): void {
    this.stdoutChunks = [];
    this.stderrChunks = [];
    this.totalBytes = 0;
    this.append(stdout, stderr);
  }

  get stdout(): string {
    return this.stdoutChunks.join("");
  }

  get stderr(): string {
    return this.stderrChunks.join("");
  }

  /** Preserve child accounting while relaying compound-command output. */
  build(exitCode: number): ExecResult {
    const stdout = this.stdout;
    const stderr = this.stderr;
    return {
      stdout,
      stderr,
      exitCode,
      internalOutputAccounting: {
        stdout: utf8ByteLength(stdout),
        stderr: utf8ByteLength(stderr),
      },
    };
  }
}

async function executeBoundedStatements(
  ctx: InterpreterContext,
  statements: StatementNode[],
  output: CompoundOutput,
): Promise<ExecResult> {
  let exitCode = 0;
  try {
    for (const statement of statements) {
      const statementResult = await ctx.executeStatement(statement);
      output.append(statementResult.stdout, statementResult.stderr);
      exitCode = statementResult.exitCode;
    }
  } catch (error) {
    if (
      isScopeExitError(error) ||
      error instanceof ErrexitError ||
      error instanceof ExitError ||
      error instanceof ExecutionLimitError ||
      error instanceof SubshellExitError
    ) {
      error.prependOutput(output.stdout, output.stderr);
      throw error;
    }
    output.appendUnaccounted("", `${getErrorMessage(error)}\n`);
    return output.build(1);
  }
  return output.build(exitCode);
}

export async function executeIf(
  ctx: InterpreterContext,
  node: IfNode,
): Promise<ExecResult> {
  return withNumericFds(ctx, node.redirections, () => executeIfBody(ctx, node));
}

async function executeIfBody(
  ctx: InterpreterContext,
  node: IfNode,
): Promise<ExecResult> {
  const output = new CompoundOutput(ctx);

  for (const clause of node.clauses) {
    // Condition evaluation should not trigger errexit
    const condResult = await executeCondition(ctx, clause.condition);
    output.append(condResult.stdout, condResult.stderr);

    if (condResult.exitCode === 0) {
      return executeBoundedStatements(ctx, clause.body, output);
    }
  }

  if (node.elseBody) {
    return executeBoundedStatements(ctx, node.elseBody, output);
  }

  return output.build(0);
}

export async function executeFor(
  ctx: InterpreterContext,
  node: ForNode,
): Promise<ExecResult> {
  return withNumericFds(ctx, node.redirections, (fdTargets) =>
    executeForBody(ctx, node, fdTargets),
  );
}

async function executeForBody(
  ctx: InterpreterContext,
  node: ForNode,
  fdTargets: ExpandedRedirectTargets,
): Promise<ExecResult> {
  // Pre-open output redirects to truncate files BEFORE expanding words
  // This matches bash behavior where redirect files are opened before
  // any command substitutions in the word list are evaluated
  const preparedRedirects = await preOpenOutputRedirects(
    ctx,
    node.redirections,
    fdTargets,
  );
  if (preparedRedirects.error) {
    return preparedRedirects.error;
  }

  const output = new CompoundOutput(ctx);
  let exitCode = 0;
  let iterations = 0;

  // Validate variable name at runtime (matches bash behavior)
  if (!/^[a-zA-Z_][a-zA-Z0-9_]*$/.test(node.variable)) {
    return failure(`bash: \`${node.variable}': not a valid identifier\n`);
  }

  let words: string[] = [];
  if (node.words === null) {
    words = (ctx.state.env.get("@") || "").split(" ").filter(Boolean);
  } else if (node.words.length === 0) {
    words = [];
  } else {
    try {
      for (const word of node.words) {
        const expanded = await expandWordWithGlob(ctx, word);
        appendBoundedElements(
          words,
          expanded.values,
          ctx.limits.maxArrayElements,
          "for-loop expansion",
        );
      }
    } catch (e) {
      if (e instanceof GlobError) {
        // failglob: return error with exit code 1
        return { stdout: "", stderr: e.stderr, exitCode: 1 };
      }
      throw e;
    }
  }

  ctx.state.loopDepth++;
  try {
    for (const value of words) {
      iterations++;
      if (iterations > ctx.limits.maxLoopIterations) {
        throwExecutionLimit(
          `for loop: too many iterations (${ctx.limits.maxLoopIterations}), increase executionLimits.maxLoopIterations`,
          "iterations",
          output.stdout,
          output.stderr,
        );
      }

      ctx.state.env.set(node.variable, value);

      try {
        for (const stmt of node.body) {
          const stmtResult = await ctx.executeStatement(stmt);
          output.append(stmtResult.stdout, stmtResult.stderr);
          exitCode = stmtResult.exitCode;
        }
      } catch (error) {
        const loopResult = handleLoopError(
          error,
          output.stdout,
          output.stderr,
          ctx.state.loopDepth,
        );
        output.replace(loopResult.stdout, loopResult.stderr);
        if (loopResult.action === "break") break;
        if (loopResult.action === "continue") continue;
        if (loopResult.action === "error") {
          // Apply output redirections before returning
          const bodyResult = output.build(loopResult.exitCode ?? 1);
          return applyRedirections(
            ctx,
            bodyResult,
            node.redirections,
            preparedRedirects.targets,
          );
        }
        throw loopResult.error;
      }
    }
  } finally {
    ctx.state.loopDepth--;
  }

  // Note: In bash, the loop variable persists after the loop with its last value
  // Do NOT ctx.state.env.delete(node.variable) here

  // Apply output redirections
  const bodyResult = output.build(exitCode);
  return applyRedirections(
    ctx,
    bodyResult,
    node.redirections,
    preparedRedirects.targets,
  );
}

export async function executeCStyleFor(
  ctx: InterpreterContext,
  node: CStyleForNode,
): Promise<ExecResult> {
  return withNumericFds(ctx, node.redirections, (fdTargets) =>
    executeCStyleForBody(ctx, node, fdTargets),
  );
}

async function executeCStyleForBody(
  ctx: InterpreterContext,
  node: CStyleForNode,
  fdTargets: ExpandedRedirectTargets,
): Promise<ExecResult> {
  // Pre-open output redirects to truncate files BEFORE evaluating expressions
  // This matches bash behavior where redirect files are opened before
  // any command substitutions in the loop are evaluated
  const preparedRedirects = await preOpenOutputRedirects(
    ctx,
    node.redirections,
    fdTargets,
  );
  if (preparedRedirects.error) {
    return preparedRedirects.error;
  }

  // Update currentLine for $LINENO - set to loop header line
  const loopLine = node.line;
  if (loopLine !== undefined) {
    ctx.state.currentLine = loopLine;
  }

  const output = new CompoundOutput(ctx);
  let exitCode = 0;
  let iterations = 0;

  if (node.init) {
    await evaluateArithmetic(ctx, node.init.expression);
  }

  ctx.state.loopDepth++;
  try {
    while (true) {
      iterations++;
      if (iterations > ctx.limits.maxLoopIterations) {
        throwExecutionLimit(
          `for loop: too many iterations (${ctx.limits.maxLoopIterations}), increase executionLimits.maxLoopIterations`,
          "iterations",
          output.stdout,
          output.stderr,
        );
      }

      if (node.condition) {
        // Set LINENO to loop header line for condition evaluation
        if (loopLine !== undefined) {
          ctx.state.currentLine = loopLine;
        }
        const condResult = await evaluateArithmetic(
          ctx,
          node.condition.expression,
        );
        if (condResult === 0) break;
      }

      try {
        for (const stmt of node.body) {
          const stmtResult = await ctx.executeStatement(stmt);
          output.append(stmtResult.stdout, stmtResult.stderr);
          exitCode = stmtResult.exitCode;
        }
      } catch (error) {
        const loopResult = handleLoopError(
          error,
          output.stdout,
          output.stderr,
          ctx.state.loopDepth,
        );
        output.replace(loopResult.stdout, loopResult.stderr);
        if (loopResult.action === "break") break;
        if (loopResult.action === "continue") {
          // Still need to run the update expression on continue
          if (node.update) {
            await evaluateArithmetic(ctx, node.update.expression);
          }
          continue;
        }
        if (loopResult.action === "error") {
          // Apply output redirections before returning
          const bodyResult = output.build(loopResult.exitCode ?? 1);
          return applyRedirections(
            ctx,
            bodyResult,
            node.redirections,
            preparedRedirects.targets,
          );
        }
        throw loopResult.error;
      }

      if (node.update) {
        await evaluateArithmetic(ctx, node.update.expression);
      }
    }
  } finally {
    ctx.state.loopDepth--;
  }

  // Apply output redirections
  const bodyResult = output.build(exitCode);
  return applyRedirections(
    ctx,
    bodyResult,
    node.redirections,
    preparedRedirects.targets,
  );
}

export async function executeWhile(
  ctx: InterpreterContext,
  node: WhileNode,
  stdin = "",
): Promise<ExecResult> {
  return withNumericFds(ctx, node.redirections, (fdTargets) =>
    executeWhileBody(ctx, node, stdin, fdTargets),
  );
}

async function executeWhileBody(
  ctx: InterpreterContext,
  node: WhileNode,
  stdin: string,
  fdTargets: ExpandedRedirectTargets,
): Promise<ExecResult> {
  const prepared = await prepareLoopRedirections(
    ctx,
    node.redirections,
    fdTargets,
  );
  if ("error" in prepared) {
    return prepared.error;
  }

  const output = new CompoundOutput(ctx);
  let exitCode = 0;
  let iterations = 0;

  // Install groupStdin only for a stream this loop owns (see resolveLoopStdin)
  const loopStdin = resolveLoopStdin(prepared.ownStdin, stdin);
  const savedGroupStdin = ctx.state.groupStdin;
  if (loopStdin.owns) {
    ctx.state.groupStdin = loopStdin.stdin;
  }

  ctx.state.loopDepth++;
  try {
    while (true) {
      iterations++;
      if (iterations > ctx.limits.maxLoopIterations) {
        throwExecutionLimit(
          `while loop: too many iterations (${ctx.limits.maxLoopIterations}), increase executionLimits.maxLoopIterations`,
          "iterations",
          output.stdout,
          output.stderr,
        );
      }

      let conditionExitCode = 0;
      let shouldBreak = false;
      let shouldContinue = false;

      // Condition evaluation should not trigger errexit
      const savedInCondition = ctx.state.inCondition;
      ctx.state.inCondition = true;
      try {
        for (const stmt of node.condition) {
          const result = await ctx.executeStatement(stmt);
          output.append(result.stdout, result.stderr);
          conditionExitCode = result.exitCode;
        }
      } catch (error) {
        // break/continue in condition should affect THIS while loop
        if (error instanceof BreakError) {
          output.append(error.stdout, error.stderr);
          if (error.levels > 1 && ctx.state.loopDepth > 1) {
            error.levels--;
            error.stdout = output.stdout;
            error.stderr = output.stderr;
            ctx.state.inCondition = savedInCondition;
            throw error;
          }
          shouldBreak = true;
        } else if (error instanceof ContinueError) {
          output.append(error.stdout, error.stderr);
          if (error.levels > 1 && ctx.state.loopDepth > 1) {
            error.levels--;
            error.stdout = output.stdout;
            error.stderr = output.stderr;
            ctx.state.inCondition = savedInCondition;
            throw error;
          }
          shouldContinue = true;
        } else {
          ctx.state.inCondition = savedInCondition;
          throw error;
        }
      } finally {
        ctx.state.inCondition = savedInCondition;
      }

      if (shouldBreak) break;
      if (shouldContinue) continue;
      if (conditionExitCode !== 0) break;

      try {
        for (const stmt of node.body) {
          const stmtResult = await ctx.executeStatement(stmt);
          output.append(stmtResult.stdout, stmtResult.stderr);
          exitCode = stmtResult.exitCode;
        }
      } catch (error) {
        const loopResult = handleLoopError(
          error,
          output.stdout,
          output.stderr,
          ctx.state.loopDepth,
        );
        output.replace(loopResult.stdout, loopResult.stderr);
        if (loopResult.action === "break") break;
        if (loopResult.action === "continue") continue;
        if (loopResult.action === "error") {
          // Apply output redirections before returning
          return applyRedirections(
            ctx,
            output.build(loopResult.exitCode ?? 1),
            prepared.redirections,
            prepared.targets,
          );
        }
        throw loopResult.error;
      }
    }
  } finally {
    ctx.state.loopDepth--;
    if (loopStdin.owns) {
      ctx.state.groupStdin = savedGroupStdin;
    }
  }

  // Apply output redirections
  return applyRedirections(
    ctx,
    output.build(exitCode),
    prepared.redirections,
    prepared.targets,
  );
}

export async function executeUntil(
  ctx: InterpreterContext,
  node: UntilNode,
  stdin = "",
): Promise<ExecResult> {
  return withNumericFds(ctx, node.redirections, (fdTargets) =>
    executeUntilBody(ctx, node, stdin, fdTargets),
  );
}

async function executeUntilBody(
  ctx: InterpreterContext,
  node: UntilNode,
  stdin: string,
  fdTargets: ExpandedRedirectTargets,
): Promise<ExecResult> {
  const prepared = await prepareLoopRedirections(
    ctx,
    node.redirections,
    fdTargets,
  );
  if ("error" in prepared) {
    return prepared.error;
  }

  const output = new CompoundOutput(ctx);
  let exitCode = 0;
  let iterations = 0;

  // Install groupStdin only for a stream this loop owns (see resolveLoopStdin)
  const loopStdin = resolveLoopStdin(prepared.ownStdin, stdin);
  const savedGroupStdin = ctx.state.groupStdin;
  if (loopStdin.owns) {
    ctx.state.groupStdin = loopStdin.stdin;
  }

  ctx.state.loopDepth++;
  try {
    while (true) {
      iterations++;
      if (iterations > ctx.limits.maxLoopIterations) {
        throwExecutionLimit(
          `until loop: too many iterations (${ctx.limits.maxLoopIterations}), increase executionLimits.maxLoopIterations`,
          "iterations",
          output.stdout,
          output.stderr,
        );
      }

      // Condition evaluation should not trigger errexit
      const condResult = await executeCondition(ctx, node.condition);
      output.append(condResult.stdout, condResult.stderr);

      if (condResult.exitCode === 0) break;

      try {
        for (const stmt of node.body) {
          const stmtResult = await ctx.executeStatement(stmt);
          output.append(stmtResult.stdout, stmtResult.stderr);
          exitCode = stmtResult.exitCode;
        }
      } catch (error) {
        const loopResult = handleLoopError(
          error,
          output.stdout,
          output.stderr,
          ctx.state.loopDepth,
        );
        output.replace(loopResult.stdout, loopResult.stderr);
        if (loopResult.action === "break") break;
        if (loopResult.action === "continue") continue;
        if (loopResult.action === "error") {
          // Apply output redirections before returning
          return applyRedirections(
            ctx,
            output.build(loopResult.exitCode ?? 1),
            prepared.redirections,
            prepared.targets,
          );
        }
        throw loopResult.error;
      }
    }
  } finally {
    ctx.state.loopDepth--;
    if (loopStdin.owns) {
      ctx.state.groupStdin = savedGroupStdin;
    }
  }

  // Apply output redirections
  return applyRedirections(
    ctx,
    output.build(exitCode),
    prepared.redirections,
    prepared.targets,
  );
}

export async function executeCase(
  ctx: InterpreterContext,
  node: CaseNode,
): Promise<ExecResult> {
  return withNumericFds(ctx, node.redirections, (fdTargets) =>
    executeCaseBody(ctx, node, fdTargets),
  );
}

async function executeCaseBody(
  ctx: InterpreterContext,
  node: CaseNode,
  fdTargets: ExpandedRedirectTargets,
): Promise<ExecResult> {
  // Pre-open output redirects to truncate files BEFORE expanding case word
  // This matches bash behavior where redirect files are opened before
  // any command substitutions in the case word are evaluated
  const preparedRedirects = await preOpenOutputRedirects(
    ctx,
    node.redirections,
    fdTargets,
  );
  if (preparedRedirects.error) {
    return preparedRedirects.error;
  }

  const output = new CompoundOutput(ctx);
  let exitCode = 0;

  const value = await expandWord(ctx, node.word);

  // fallThrough tracks whether we should execute the next case body unconditionally
  // This happens when the previous case ended with ;& (unconditional fall-through)
  let fallThrough = false;

  for (let i = 0; i < node.items.length; i++) {
    const item = node.items[i];
    let matched = fallThrough; // If falling through, automatically match

    if (!fallThrough) {
      // Normal pattern matching
      for (const pattern of item.patterns) {
        let patternStr = await expandWord(ctx, pattern);
        // If the pattern is fully quoted, escape glob characters for literal matching
        if (isWordFullyQuoted(pattern)) {
          patternStr = escapeGlobChars(patternStr);
        }
        const nocasematch = ctx.state.shoptOptions.nocasematch;
        const extglob = ctx.state.shoptOptions.extglob;
        if (
          matchPattern(
            value,
            patternStr,
            nocasematch,
            extglob,
            ctx.limits.maxCallDepth,
          )
        ) {
          matched = true;
          break;
        }
      }
    }

    if (matched) {
      const bodyResult = await executeBoundedStatements(ctx, item.body, output);
      output.replace(bodyResult.stdout, bodyResult.stderr);
      exitCode = bodyResult.exitCode;

      // Handle different terminators:
      // ;; - stop, no fall-through
      // ;& - unconditional fall-through (execute next body without pattern check)
      // ;;& - continue pattern matching (check next case patterns)
      if (item.terminator === ";;") {
        break;
      } else if (item.terminator === ";&") {
        fallThrough = true;
      } else {
        // ;;& - reset fallThrough, continue to next iteration for pattern matching
        fallThrough = false;
      }
    } else {
      fallThrough = false;
    }
  }

  // Apply output redirections
  const bodyResult = output.build(exitCode);
  return applyRedirections(
    ctx,
    bodyResult,
    node.redirections,
    preparedRedirects.targets,
  );
}
