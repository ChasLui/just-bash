import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

describe("process substitution - output >(cmd)", () => {
  it("feeds what the outer command wrote into the body", async () => {
    const env = new Bash();
    const result = await env.exec("echo hi > >(tr a-z A-Z)");
    expect(result.stdout).toBe("HI\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("substitutes a writable /dev/fd path", async () => {
    const env = new Bash();
    const result = await env.exec("echo hi >(cat)");
    expect(result.stdout).toBe("hi /dev/fd/63\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("supports the tee idiom", async () => {
    const env = new Bash();
    const result = await env.exec(
      "printf 'a\\nb\\n' | tee >(wc -l) > /dev/null",
    );
    expect(result.stdout).toBe("2\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("drives several writers from one command", async () => {
    const env = new Bash();
    const result = await env.exec(
      "printf 'x\\n' | tee >(cat) >(cat) > /dev/null",
    );
    expect(result.stdout).toBe("x\nx\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("combines with an input substitution", async () => {
    const env = new Bash();
    const result = await env.exec("cat <(echo in) > >(sed 's/in/OUT/')");
    expect(result.stdout).toBe("OUT\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("runs the body with empty stdin when nothing was written", async () => {
    const env = new Bash();
    const result = await env.exec("true > >(wc -c)");
    expect(result.stdout).toBe("0\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("does not let a failing body change the outer status", async () => {
    const env = new Bash();
    const result = await env.exec("echo hi > >(false); echo rc=$?");
    expect(result.stdout).toBe("rc=0\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("sends the body's stderr to the shell's stderr", async () => {
    const env = new Bash();
    const result = await env.exec("echo hi > >(cat >&2)");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("hi\n");
    expect(result.exitCode).toBe(0);
  });

  it("discards variable assignments made in the body", async () => {
    const env = new Bash();
    const result = await env.exec("x=1; echo hi > >(read x); echo after=$x");
    expect(result.stdout).toBe("after=1\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("removes the backing file once the command finishes", async () => {
    const env = new Bash();
    const result = await env.exec("echo hi > >(cat); ls /dev/fd | wc -l");
    expect(result.stdout).toBe("hi\n0\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("bounds a non-terminating body with the command limit", async () => {
    const env = new Bash({ executionLimits: { maxCommandCount: 50 } });
    const result = await env.exec("echo hi > >(while true; do echo x; done)");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe(
      "bash: too many commands executed (>50), increase executionLimits.maxCommandCount\n",
    );
    expect(result.exitCode).toBe(126);
  });
});
