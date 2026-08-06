import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

describe("process substitution - input <(cmd)", () => {
  it("substitutes a readable path for the body's stdout", async () => {
    const env = new Bash();
    const result = await env.exec("cat <(echo hi)");
    expect(result.stdout).toBe("hi\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("substitutes a /dev/fd path counting down from 63", async () => {
    const env = new Bash();
    const result = await env.exec("echo <(true) <(true) <(true)");
    expect(result.stdout).toBe("/dev/fd/63 /dev/fd/62 /dev/fd/61\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("passes several substitutions to one command in order", async () => {
    const env = new Bash();
    const result = await env.exec("cat <(echo one) <(echo two) <(echo three)");
    expect(result.stdout).toBe("one\ntwo\nthree\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("supports the comm idiom", async () => {
    const env = new Bash({
      files: { "/x.txt": "b\na\nb\n", "/y.txt": "c\nb\n" },
    });
    const result = await env.exec(
      "comm -12 <(sort -u /x.txt) <(sort -u /y.txt)",
    );
    expect(result.stdout).toBe("b\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("supports the diff idiom", async () => {
    const env = new Bash({
      files: { "/a.txt": "b\na\n", "/b.txt": "a\nb\n" },
    });
    const result = await env.exec(
      "diff <(sort /a.txt) <(sort /b.txt) > /dev/null; echo rc=$?",
    );
    expect(result.stdout).toBe("rc=0\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("reports a difference through diff's exit status", async () => {
    const env = new Bash();
    const result = await env.exec(
      "diff <(echo a) <(echo b) > /dev/null; echo rc=$?",
    );
    expect(result.stdout).toBe("rc=1\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("works as the source of an input redirection", async () => {
    const env = new Bash();
    const result = await env.exec("cat < <(printf 'a\\nb\\n')");
    expect(result.stdout).toBe("a\nb\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("feeds a while-read loop", async () => {
    const env = new Bash();
    const result = await env.exec(
      "while read -r l; do echo \"got $l\"; done < <(printf '1\\n2\\n')",
    );
    expect(result.stdout).toBe("got 1\ngot 2\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("works inside a pipeline", async () => {
    const env = new Bash();
    const result = await env.exec("cat <(echo hi) | tr a-z A-Z");
    expect(result.stdout).toBe("HI\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("works in a non-first pipeline stage", async () => {
    const env = new Bash();
    const result = await env.exec(
      "echo ignored | grep -c . <(printf 'a\\nb\\n')",
    );
    expect(result.stdout).toBe("2\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("nests", async () => {
    const env = new Bash();
    const result = await env.exec("cat <(cat <(echo deep))");
    expect(result.stdout).toBe("deep\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("preserves the body's exact bytes, including a missing newline", async () => {
    const env = new Bash();
    const result = await env.exec("wc -c < <(printf 'abc')");
    expect(result.stdout).toBe("3\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("concatenates with an adjacent literal", async () => {
    const env = new Bash();
    const result = await env.exec("echo a<(true)");
    expect(result.stdout).toBe("a/dev/fd/63\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("stays literal inside quotes", async () => {
    const env = new Bash();
    const result = await env.exec("echo '<(echo hi)' \"<(echo hi)\"");
    expect(result.stdout).toBe("<(echo hi) <(echo hi)\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("process substitution - exit status", () => {
  it("does not let a failing body change the outer status", async () => {
    const env = new Bash();
    const result = await env.exec("cat <(false); echo rc=$?");
    expect(result.stdout).toBe("rc=0\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("keeps the outer command's own status", async () => {
    const env = new Bash();
    const result = await env.exec("grep -q zzz <(echo hi); echo rc=$?");
    expect(result.stdout).toBe("rc=1\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("contains `exit` inside the body", async () => {
    const env = new Bash();
    const result = await env.exec("cat <(echo a; exit 3); echo rc=$?");
    expect(result.stdout).toBe("a\nrc=0\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("does not abort the script under set -e", async () => {
    const env = new Bash();
    const result = await env.exec("set -e; cat <(false); echo after");
    expect(result.stdout).toBe("after\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("sends the body's stderr to the shell's stderr", async () => {
    const env = new Bash();
    const result = await env.exec("cat <(echo oops >&2; echo ok)");
    expect(result.stdout).toBe("ok\n");
    expect(result.stderr).toBe("oops\n");
    expect(result.exitCode).toBe(0);
  });
});

describe("process substitution - subshell semantics", () => {
  it("discards variable assignments made in the body", async () => {
    const env = new Bash();
    const result = await env.exec("x=1; cat <(x=2; echo $x); echo after=$x");
    expect(result.stdout).toBe("2\nafter=1\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("discards directory changes made in the body", async () => {
    const env = new Bash();
    const result = await env.exec("cd /tmp; cat <(cd /; pwd); pwd");
    expect(result.stdout).toBe("/\n/tmp\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("sees the caller's variables", async () => {
    const env = new Bash();
    const result = await env.exec("v=hello; cat <(echo $v)");
    expect(result.stdout).toBe("hello\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("process substitution - cleanup", () => {
  it("removes the backing file once the command finishes", async () => {
    const env = new Bash();
    const result = await env.exec("cat <(echo hi); cat /dev/fd/63; echo rc=$?");
    expect(result.stdout).toBe("hi\nrc=1\n");
    expect(result.stderr).toBe("cat: /dev/fd/63: No such file or directory\n");
    expect(result.exitCode).toBe(0);
  });

  it("does not accumulate backing files across commands", async () => {
    const env = new Bash();
    const result = await env.exec(
      "for i in 1 2 3; do cat <(echo $i) > /dev/null; done; ls /dev/fd | wc -l",
    );
    expect(result.stdout).toBe("0\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("reuses descriptor numbers across commands", async () => {
    const env = new Bash();
    const result = await env.exec("echo <(true); echo <(true)");
    expect(result.stdout).toBe("/dev/fd/63\n/dev/fd/63\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("cleans up when the outer command fails", async () => {
    const env = new Bash();
    const result = await env.exec("nosuchcmd <(echo hi); ls /dev/fd | wc -l");
    expect(result.stdout).toBe("0\n");
    expect(result.stderr).toBe("bash: nosuchcmd: command not found\n");
    expect(result.exitCode).toBe(0);
  });
});

describe("process substitution - limits", () => {
  it("bounds a non-terminating body with the command limit", async () => {
    const env = new Bash({ executionLimits: { maxCommandCount: 50 } });
    const result = await env.exec("cat <(while true; do echo x; done)");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe(
      "bash: too many commands executed (>50), increase executionLimits.maxCommandCount\n",
    );
    expect(result.exitCode).toBe(126);
  });

  it("bounds body output with the string length limit", async () => {
    const env = new Bash({ executionLimits: { maxStringLength: 8 } });
    const result = await env.exec("cat <(echo aaa; echo bbb; echo ccc)");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe(
      "bash: process substitution: string length limit exceeded (8 bytes)\n",
    );
    expect(result.exitCode).toBe(126);
  });

  it("bounds nesting depth", async () => {
    const env = new Bash({ executionLimits: { maxSubstitutionDepth: 2 } });
    const result = await env.exec("cat <(cat <(cat <(echo deep)))");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe(
      "bash: Process substitution nesting limit exceeded (2)\n",
    );
    expect(result.exitCode).toBe(126);
  });
});
