import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

/**
 * Output side of user file descriptors: `N> file`, `N>> file` and `>&N`
 * share the descriptor table with the input side, so a descriptor opened by
 * number is writable through `>&N` for as long as it is open.
 */
describe("numeric output file descriptors", () => {
  it("appends successive writes through a descriptor opened with exec N>", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        "exec 4> /tmp/out.txt",
        "echo hello >&4",
        "echo world >&4",
        "exec 4>&-",
        "cat /tmp/out.txt",
      ].join("\n"),
    );
    expect(result.stdout).toBe("hello\nworld\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("keeps existing content for exec N>> and appends after it", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        "echo one > /tmp/ap.txt",
        "exec 4>> /tmp/ap.txt",
        "echo two >&4",
        "exec 4>&-",
        "cat /tmp/ap.txt",
      ].join("\n"),
    );
    expect(result.stdout).toBe("one\ntwo\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("opens a command-scoped descriptor and writes through it in the same list", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        "echo direct 5> /tmp/five.txt >&5",
        'echo "rc=$?"',
        "cat /tmp/five.txt",
      ].join("\n"),
    );
    expect(result.stdout).toBe("rc=0\ndirect\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("closes a command-scoped output descriptor after the command", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        "true 9> /tmp/fd9.txt",
        'echo world >&9; echo "rc=$?"',
        "cat /tmp/fd9.txt",
      ].join("\n"),
    );
    expect(result.stdout).toBe("rc=1\n");
    expect(result.stderr).toBe("bash: 9: Bad file descriptor\n");
    expect(result.exitCode).toBe(0);
  });

  it("truncates the target of `N> file` even when nothing is written", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        "echo previous > /tmp/t.txt",
        "true 6> /tmp/t.txt",
        "wc -c < /tmp/t.txt",
      ].join("\n"),
    );
    expect(result.stdout).toBe("0\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("reports Bad file descriptor when writing to a read-side descriptor", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        "printf 'a\\n' > /tmp/f.txt",
        "exec 3< /tmp/f.txt",
        'echo boom >&3; echo "rc=$?"',
      ].join("\n"),
    );
    expect(result.stdout).toBe("rc=1\n");
    expect(result.stderr).toBe("bash: 3: Bad file descriptor\n");
    expect(result.exitCode).toBe(0);
  });

  it("reports Bad file descriptor after the descriptor is closed", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        "exec 5> /tmp/five.txt",
        "echo hello >&5",
        "exec 5>&-",
        'echo world >&5; echo "rc=$?"',
        "cat /tmp/five.txt",
      ].join("\n"),
    );
    expect(result.stdout).toBe("rc=1\nhello\n");
    expect(result.stderr).toBe("bash: 5: Bad file descriptor\n");
    expect(result.exitCode).toBe(0);
  });

  it("routes a compound command's output through `done N> file`", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        'for i in 1 2; do echo "line $i" >&7; done 7> /tmp/loop.txt',
        "cat /tmp/loop.txt",
      ].join("\n"),
    );
    expect(result.stdout).toBe("line 1\nline 2\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("moves a descriptor with `exec N>&M-` and leaves M closed", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        "exec 5> /tmp/moved.txt",
        "echo hello5 >&5",
        "exec 6>&5-",
        'echo world5 >&5; echo "rc5=$?"',
        "echo world6 >&6",
        "exec 6>&-",
        "cat /tmp/moved.txt",
      ].join("\n"),
    );
    expect(result.stdout).toBe("rc5=1\nhello5\nworld6\n");
    expect(result.stderr).toBe("bash: 5: Bad file descriptor\n");
    expect(result.exitCode).toBe(0);
  });

  it("supports double-digit descriptors", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        "exec 20> /tmp/twenty.txt",
        "echo hello20 >&20",
        "exec 20>&-",
        "cat /tmp/twenty.txt",
      ].join("\n"),
    );
    expect(result.stdout).toBe("hello20\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("still routes stderr to stdout for 2>&1", async () => {
    const env = new Bash();
    const result = await env.exec("nosuchcmd 2>&1");
    expect(result.stdout).toBe("bash: nosuchcmd: command not found\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(127);
  });

  it("enforces the descriptor limit", async () => {
    const env = new Bash({ executionLimits: { maxFileDescriptors: 3 } });
    const result = await env.exec(
      "exec 3>/tmp/a; exec 4>/tmp/b; exec 5>/tmp/c; exec 6>/tmp/d",
    );
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe(
      "bash: too many open file descriptors (max 3)\n",
    );
    expect(result.exitCode).toBe(126);
  });
});
