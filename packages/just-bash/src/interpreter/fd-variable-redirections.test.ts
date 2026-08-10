import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

describe("fd-variable redirections", () => {
  it("creates a bare target but unsets and closes the descriptor", async () => {
    const env = new Bash();
    const result = await env.exec(
      '{output}>/tmp/out; printf "[%s]" "$output"; printf closed >&10; printf ":%s" "$?"',
    );

    expect(result.stdout).toBe("[]:1");
    expect(result.stderr).toBe("bash: 10: Bad file descriptor\n");
    expect(result.exitCode).toBe(0);
    expect(await env.readFile("/tmp/out")).toBe("");
  });

  it("makes a bare descriptor reusable by the next allocation", async () => {
    const env = new Bash();
    const result = await env.exec(
      '{bare}>/tmp/bare; : {named}>/tmp/named; printf "%s" "$named"',
    );

    expect(result.stdout).toBe("10");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("scopes a bare heredoc descriptor", async () => {
    const env = new Bash();
    const result = await env.exec(
      '{input}<<EOF 3<&$input\nvalue\nEOF\nprintf "input=[%s] status=%s\\n" "$input" "$?"',
    );

    expect(result.stdout).toBe("input=[] status=0\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("keeps a named-command heredoc descriptor available", async () => {
    const env = new Bash();
    const result = await env.exec(
      ': {input}<<EOF\nvalue\nEOF\nread -u "$input" line; printf "input=%s fd=%s\\n" "$line" "$input"',
    );

    expect(result.stdout).toBe("input=value fd=10\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});
