import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

describe("redirection transactions", () => {
  it("prepares redirects introduced by alias expansion", async () => {
    const env = new Bash();
    const result = await env.exec(
      "shopt -s expand_aliases; alias routed=': >/tmp/alias'; routed; printf after",
    );

    expect(result.stdout).toBe("after");
    expect(result.stderr).toBe("");
    expect(await env.readFile("/tmp/alias")).toBe("");
  });

  it("selects persistent exec policy from the expanded command name", async () => {
    const env = new Bash();
    const result = await env.exec(
      "runner=exec; $runner >/tmp/out; printf after",
    );

    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(await env.readFile("/tmp/out")).toBe("after");
  });

  it("persists both stdout and stderr for exec >&file", async () => {
    const env = new Bash();
    const result = await env.exec(
      "exec >&/tmp/all; printf out; printf err >&2",
    );

    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(await env.readFile("/tmp/all")).toBe("outerr");
  });

  it("uses persistent fd 0 as later command input", async () => {
    const env = new Bash();
    const result = await env.exec(
      "printf 'a\\nb\\n' >/tmp/in; exec </tmp/in; read x; read y; printf '%s:%s' \"$x\" \"$y\"",
    );

    expect(result.stdout).toBe("a:b");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("does not drain persistent fd 0 for non-reading commands", async () => {
    const env = new Bash();
    const result = await env.exec(
      "printf 'a\\nb\\n' >/tmp/in; exec </tmp/in; echo ignored; true; :; read x; read y; printf '%s:%s' \"$x\" \"$y\"",
    );

    expect(result.stdout).toBe("ignored\na:b");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("does not drain persistent fd 0 for external non-readers", async () => {
    const env = new Bash();
    const result = await env.exec(
      "printf 'a\nb\n' >/tmp/in; exec </tmp/in; pwd >/dev/null; read x; read y; printf '%s:%s' \"$x\" \"$y\"",
    );

    expect(result.stdout).toBe("a:b");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("does not drain persistent fd 0 for external non-readers", async () => {
    const env = new Bash();
    const result = await env.exec(
      "printf 'a\nb\n' >/tmp/in; exec </tmp/in; pwd >/dev/null; read x; read y; printf '%s:%s' \"$x\" \"$y\"",
    );

    expect(result.stdout).toBe("a:b");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("shares a persistent standard input alias with its source descriptor", async () => {
    const env = new Bash();
    const result = await env.exec(
      "printf 'ab' >/tmp/in; exec 3</tmp/in; exec 0<&3; read -n1 a; read -n1 -u3 b; printf '%s:%s' \"$a\" \"$b\"",
    );

    expect(result.stdout).toBe("a:b");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("reads temporary fd 0 duplications from their prepared route", async () => {
    const env = new Bash();
    const result = await env.exec("printf value >/tmp/in; cat </tmp/in <&0");

    expect(result.stdout).toBe("value");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("advances a compound command's descriptor input", async () => {
    const env = new Bash();
    const result = await env.exec(
      "printf 'a\\nb\\n' >/tmp/in; exec 3</tmp/in; if read x; then printf '%s' \"$x\"; fi <&3; read -u3 y; printf ':%s' \"$y\"",
    );

    expect(result.stdout).toBe("a:b");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("advances a group command's descriptor input", async () => {
    const env = new Bash();
    const result = await env.exec(
      "printf 'a\\nb\\n' >/tmp/in; exec 3</tmp/in; { read x; } <&3; read -u3 y; printf '%s:%s' \"$x\" \"$y\"",
    );

    expect(result.stdout).toBe("a:b");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("advances a subshell's descriptor input", async () => {
    const env = new Bash();
    const result = await env.exec(
      "printf 'a\\nb\\n' >/tmp/in; exec 3</tmp/in; (read x) <&3; read -u3 y; printf '%s:%s' \"$x\" \"$y\"",
    );

    expect(result.stdout).toBe(":b");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("keeps persistent closed stdout closed", async () => {
    const env = new Bash();
    const result = await env.exec(
      "exec 3>&1; exec 1>&-; echo hi; echo rc=$? >&3",
    );

    expect(result.stdout).toBe("rc=1\n");
    expect(result.stderr).toBe(
      "bash: echo: write error: Bad file descriptor\n",
    );
    expect(result.exitCode).toBe(0);
  });

  it("rejects duplication from a persistent closed stdout", async () => {
    const env = new Bash();
    const result = await env.exec("exec 1>&-; : 3>&1; echo rc=$? >&2");

    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("bash: 1: Bad file descriptor\nrc=1\n");
    expect(result.exitCode).toBe(0);
  });

  it("closes a standard descriptor moved by persistent exec", async () => {
    const env = new Bash();
    const result = await env.exec(
      "exec 3>&1; exec 1>&2-; printf err >&2; printf rc=$? >&3",
    );

    expect(result.stdout).toBe("rc=1");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("aborts when a redirection exceeds the descriptor limit", async () => {
    const env = new Bash({ executionLimits: { maxFileDescriptors: 1 } });
    const result = await env.exec(
      "exec 3>/tmp/held; : 4>/tmp/excess; printf after",
    );

    expect(result.stdout).toBe("");
    expect(result.stderr).toBe(
      "bash: too many open file descriptors (max 1)\n",
    );
    expect(result.exitCode).toBe(126);
    await expect(env.readFile("/tmp/excess")).rejects.toThrow();
  });

  it("preserves a named fd-variable allocation through ReturnError", async () => {
    const env = new Bash();
    const result = await env.exec(
      "fn() { return {output}>/tmp/out; }; fn; printf kept >&$output",
    );

    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(await env.readFile("/tmp/out")).toBe("kept");
  });

  it("restores a temporary numeric descriptor through ReturnError", async () => {
    const env = new Bash();
    const result = await env.exec(
      "fn() { return 3>/tmp/out; }; fn; printf leak >&3; printf ':%s' \"$?\"",
    );

    expect(result.stdout).toBe(":1");
    expect(result.stderr).toBe("bash: 3: Bad file descriptor\n");
    expect(await env.readFile("/tmp/out")).toBe("");
  });

  it("routes function execution-limit output through definition redirects", async () => {
    const env = new Bash({ executionLimits: { maxLoopIterations: 1 } });
    const result = await env.exec(
      "fn() { printf before; while true; do :; done; } >/tmp/out 2>/tmp/err; fn",
    );

    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(126);
    expect(await env.readFile("/tmp/out")).toBe("before");
    expect(await env.readFile("/tmp/err")).toContain("too many iterations");
  });

  it("routes function ExitError output and accounting through definition redirects", async () => {
    const env = new Bash();
    const result = await env.exec(
      "fn() { printf before; printf problem >&2; exit 7; } >/tmp/out 2>/tmp/err; fn",
    );

    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(7);
    expect(result.internalOutputAccounting).toStrictEqual({
      stdout: 0,
      stderr: 0,
    });
    expect(await env.readFile("/tmp/out")).toBe("before");
    expect(await env.readFile("/tmp/err")).toBe("problem");
  });

  it("routes function Errexit output through definition redirects", async () => {
    const env = new Bash();
    const result = await env.exec(
      "set -e; fn() { printf before; printf problem >&2; false; } >/tmp/out 2>/tmp/err; fn",
    );

    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(1);
    expect(await env.readFile("/tmp/out")).toBe("before");
    expect(await env.readFile("/tmp/err")).toBe("problem");
  });

  it("binds an fd1 heredoc without replacing stdin", async () => {
    const env = new Bash();
    const result = await env.exec(
      "read -u 1 line 1<<EOF\nvalue\nEOF\nprintf '%s' \"$line\"",
    );

    expect(result.stdout).toBe("value");
    expect(result.stderr).toBe("");
  });

  it("binds an fd2 heredoc without replacing stdin", async () => {
    const env = new Bash();
    const result = await env.exec(
      "read -u 2 line 2<<EOF\nvalue\nEOF\nprintf '%s' \"$line\"",
    );

    expect(result.stdout).toBe("value");
    expect(result.stderr).toBe("");
  });

  it("routes an fd1 heredoc write failure without live stdout", async () => {
    const env = new Bash();
    const result = await env.exec("echo hi 1<<EOF\nvalue\nEOF");

    expect(result.stdout).toBe("");
    expect(result.stderr).toBe(
      "bash: echo: write error: Bad file descriptor\n",
    );
    expect(result.exitCode).toBe(1);
    expect(result.internalOutputAccounting).toStrictEqual({
      stdout: 0,
      stderr: 45,
    });
  });

  it("attributes an fd1 heredoc write failure to its producer", async () => {
    for (const command of [
      "printf hi",
      "builtin printf hi",
      "command printf hi",
    ]) {
      const env = new Bash();
      const result = await env.exec(`${command} 1<<EOF\nvalue\nEOF`);

      expect(result.stdout).toBe("");
      expect(result.stderr).toBe(
        "bash: printf: write error: Bad file descriptor\n",
      );
      expect(result.exitCode).toBe(1);
      expect(result.internalOutputAccounting).toStrictEqual({
        stdout: 0,
        stderr: 47,
      });
    }

    const execResult = await new Bash().exec(
      "exec printf hi 1<<EOF\nvalue\nEOF",
    );
    expect(execResult.stdout).toBe("");
    expect(execResult.stderr).toBe(
      "printf: write error: Bad file descriptor\n",
    );
    expect(execResult.exitCode).toBe(1);
    expect(execResult.internalOutputAccounting).toStrictEqual({
      stdout: 0,
      stderr: 41,
    });
  });

  it("suppresses command diagnostics written through an fd2 heredoc", async () => {
    const env = new Bash();
    const result = await env.exec("nosuchcommand 2<<EOF\nvalue\nEOF");

    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(127);
    expect(result.internalOutputAccounting).toStrictEqual({
      stdout: 0,
      stderr: 0,
    });
  });
});
