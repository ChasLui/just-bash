import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

/**
 * A pipeline inherits the enclosing shell's stdin for its first stage and
 * must not consume it on the pipeline's behalf: running `true | true` reads
 * nothing, so a following `read` still sees the next line. Regression tests
 * for https://github.com/vercel-labs/just-bash/issues/323, where every
 * pipeline drained the shared stdin to EOF.
 */

const FIVE_LINES = "L1\nL2\nL3\nL4\nL5\n";
const THREE_LINES = "L1\nL2\nL3\n";

function makeBash(content = FIVE_LINES): Bash {
  return new Bash({ files: { "/loop.txt": content }, cwd: "/" });
}

describe("pipeline stdin — issue #323 repros", () => {
  it("a pipeline between two reads leaves the shared position alone", async () => {
    const result = await makeBash(THREE_LINES).exec(
      '{ read a; echo "a=[$a]"; true | true; read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[L1]\nb=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a `while read` loop with a pipeline in its body runs once per line", async () => {
    const result = await makeBash().exec(
      "n=0; while read a; do n=$((n+1)); echo hi | head -1 >/dev/null; done < /loop.txt\n" +
        'echo "iterations: $n"',
    );
    expect(result.stdout).toBe("iterations: 5\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("pipeline stdin — loop body scope table (issue #323)", () => {
  // Mirrors the table in issue #323: every one of these bodies must leave the
  // loop's stdin untouched, so the loop sees all five lines.
  const bodies: Array<[label: string, body: string]> = [
    ["empty body", ":"],
    ["command group", "{ echo x; } >/dev/null"],
    ["command substitution in body does not consume loop input", "y=$(echo z)"],
    ["true | true", "true | true"],
    ["echo x | echo y", "echo x | echo y >/dev/null"],
    ["echo abc | tr a-z A-Z", "echo abc | tr a-z A-Z >/dev/null"],
    ["echo hi | head -1", "echo hi | head -1 >/dev/null"],
    ["printf | sed 1q", "printf 'x\\ny\\n' | sed 1q >/dev/null"],
    ["three-stage pipeline", "echo a | cat | cat >/dev/null"],
    ["negated pipeline", "! echo a | grep -q zzz"],
    ["|& pipeline", "echo a |& cat >/dev/null"],
    ["pipeline inside a command group", "{ echo x | cat; } >/dev/null"],
    ["pipeline inside a subshell", "(echo x | cat) >/dev/null"],
    [
      "pipeline inside a for loop",
      "for i in 1 2; do echo x | cat; done >/dev/null",
    ],
    [
      "pipeline inside a case arm",
      "case x in x) echo q | cat;; esac >/dev/null",
    ],
    ["pipeline inside eval", "eval 'echo x | cat' >/dev/null"],
  ];

  for (const [label, body] of bodies) {
    it(`${label} → 5 iterations`, async () => {
      const result = await makeBash().exec(
        `n=0; while read a; do n=$((n+1)); ${body}; done < /loop.txt\necho "iterations: $n"`,
      );
      expect(result.stdout).toBe("iterations: 5\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  }

  it("a pipeline in a function called from the loop body → 5 iterations", async () => {
    const result = await makeBash().exec(
      "f() { echo x | cat >/dev/null; }\n" +
        'n=0; while read a; do n=$((n+1)); f; done < /loop.txt\necho "iterations: $n"',
    );
    expect(result.stdout).toBe("iterations: 5\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("the loop still sees every line, not just the count", async () => {
    const result = await makeBash(THREE_LINES).exec(
      'while read a; do echo "got:$a"; echo z | cat >/dev/null; done < /loop.txt',
    );
    expect(result.stdout).toBe("got:L1\ngot:L2\ngot:L3\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("pipeline stdin — stdin sources other than `< file`", () => {
  it("here-string backing the loop survives a pipeline", async () => {
    const result = await new Bash().exec(
      "n=0; while read a; do n=$((n+1)); true | true; done <<< $'L1\\nL2\\nL3'\n" +
        'echo "iterations: $n"',
    );
    expect(result.stdout).toBe("iterations: 3\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("here-doc backing the loop survives a pipeline", async () => {
    const result = await new Bash().exec(
      "n=0; while read a; do n=$((n+1)); echo x | cat >/dev/null; done <<EOT\nL1\nL2\nL3\nEOT\n" +
        'echo "iterations: $n"',
    );
    expect(result.stdout).toBe("iterations: 3\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a group's here-doc stdin survives a pipeline", async () => {
    const result = await new Bash().exec(
      '{ read a; true | true; read b; echo "a=[$a] b=[$b]"; } <<EOT\nL1\nL2\nEOT',
    );
    expect(result.stdout).toBe("a=[L1] b=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("pipeline stdin — stage wiring", () => {
  it("the first stage reads the shell's stdin", async () => {
    const result = await makeBash(THREE_LINES).exec(
      "{ cat | tr a-z A-Z; } < /loop.txt",
    );
    expect(result.stdout).toBe("L1\nL2\nL3\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a read in the first stage advances the shared position", async () => {
    const result = await makeBash(THREE_LINES).exec(
      '{ read x | cat; read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("b=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a later stage reads the previous stage's stdout, not the shell's stdin", async () => {
    const result = await makeBash(THREE_LINES).exec(
      "{ echo A | cat; } < /loop.txt",
    );
    expect(result.stdout).toBe("A\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a later stage fed empty output does not fall back to the shell's stdin", async () => {
    const result = await makeBash(THREE_LINES).exec(
      '{ true | cat; echo "---"; read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("---\nb=[L1]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a non-matching grep does not let the next stage see the shell's stdin", async () => {
    const result = await makeBash(THREE_LINES).exec(
      '{ echo abc | grep zzz | head -1; echo "---"; } < /loop.txt',
    );
    expect(result.stdout).toBe("---\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a read in a non-first stage takes the pipe, leaving the shell's stdin alone", async () => {
    const result = await makeBash("L1\nL2\n").exec(
      '{ echo piped | read v; echo "v=[$v]"; read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("v=[]\nb=[L1]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a pipeline that exits mid-stage still hands the position back", async () => {
    const result = await makeBash(THREE_LINES).exec(
      '{ read a; (exit 3) | true; read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[L1] b=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a failing command in a pipeline still hands the position back", async () => {
    const result = await makeBash(THREE_LINES).exec(
      '{ read a; no_such_command_xyz | true; read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[L1] b=[L2]\n");
    expect(result.stderr).toBe(
      "bash: no_such_command_xyz: command not found\n",
    );
    expect(result.exitCode).toBe(0);
  });

  it("PIPESTATUS is unaffected by the stdin wiring", async () => {
    const result = await makeBash(THREE_LINES).exec(
      '{ read a; false | true | false; echo "${PIPESTATUS[*]}"; read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("1 0 1\nb=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("pipeline stdin — mapfile and nested loops", () => {
  it("mapfile after a pipeline still sees the remaining lines", async () => {
    const result = await makeBash(THREE_LINES).exec(
      '{ read a; true | true; mapfile -t arr; echo "a=[$a] n=${#arr[@]} first=[${arr[0]}]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[L1] n=2 first=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a read loop after a pipeline still sees every remaining line", async () => {
    const result = await makeBash("L1\nL2\nL3\nL4\n").exec(
      '{ echo start | cat >/dev/null; while read l; do echo "L:$l"; done; } < /loop.txt',
    );
    expect(result.stdout).toBe("L:L1\nL:L2\nL:L3\nL:L4\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a pipeline in the loop condition does not drain the loop's stdin", async () => {
    const result = await makeBash(THREE_LINES).exec(
      "n=0; while echo y | grep -q y && read a; do n=$((n+1)); done < /loop.txt\n" +
        'echo "iterations: $n"',
    );
    expect(result.stdout).toBe("iterations: 3\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("nested read loops over separate files keep their own positions", async () => {
    const bash = new Bash({
      files: { "/outer.txt": "A\nB\n", "/inner.txt": "1\n2\n" },
      cwd: "/",
    });
    const result = await bash.exec(
      "while read o; do\n" +
        '  while read i; do echo "$o$i"; echo x | cat >/dev/null; done < /inner.txt\n' +
        "done < /outer.txt",
    );
    expect(result.stdout).toBe("A1\nA2\nB1\nB2\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("the exec() stdin option survives a pipeline, like `bash -c … < file`", async () => {
    const result = await new Bash().exec(
      'read a; echo "a=[$a]"; true | true; read b; echo "b=[$b]"; ' +
        'n=0; while read l; do n=$((n+1)); echo x | cat >/dev/null; done; echo "rest: $n"',
      { stdin: "L1\nL2\nL3\nL4\n" },
    );
    expect(result.stdout).toBe("a=[L1]\nb=[L2]\nrest: 2\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});
