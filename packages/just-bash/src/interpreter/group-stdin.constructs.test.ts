import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

/**
 * The "restore only what you replaced" rule for stdin, as it shows up in the
 * constructs that run their body through a command group: function bodies and
 * `eval`. Both share the enclosing shell's fd 0 unless a redirection or a
 * pipeline gives them their own, so reads inside them move the one shared
 * position instead of being replayed afterwards.
 *
 * The read-loop idioms at the bottom are guards: a loop *does* own the stdin
 * its `done < file` opened, so those must keep running once per line.
 *
 * Every expectation below was checked against GNU bash 3.2.57.
 */

const FIVE_LINES = "L1\nL2\nL3\nL4\nL5\n";
const OTHER = "O1\nO2\nO3\n";

function makeBash(content = FIVE_LINES): Bash {
  return new Bash({
    files: { "/loop.txt": content, "/other.txt": OTHER, "/empty.txt": "" },
    cwd: "/",
  });
}

describe("function bodies share the shell's stdin position", () => {
  it("a function that reads leaves the caller at the next line", async () => {
    const result = await makeBash().exec(
      'f() { read a; echo "in=[$a]"; }; ' +
        '{ f; read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("in=[L1]\nb=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("two calls read two different lines", async () => {
    const result = await makeBash().exec(
      'f() { read a; echo "in=[$a]"; }; ' +
        '{ f; f; read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("in=[L1]\nin=[L2]\nb=[L3]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a group nested in the function body shares the same position", async () => {
    const result = await makeBash().exec(
      'f() { { read a; }; read c; echo "a=[$a] c=[$c]"; }; ' +
        '{ f; read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[L1] c=[L2]\nb=[L3]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("`return` from inside a group does not rewind the position", async () => {
    const result = await makeBash().exec(
      "f() { { read a; return 0; }; }; " +
        '{ f; read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("b=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a redirection on the definition gives the function its own stdin", async () => {
    const result = await makeBash().exec(
      'f() { read a; echo "in=[$a]"; } < /other.txt; ' +
        '{ f; read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("in=[O1]\nb=[L1]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a redirection on the call gives the function its own stdin", async () => {
    const result = await makeBash().exec(
      'f() { read a; echo "in=[$a]"; }; ' +
        '{ f < /other.txt; read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("in=[O1]\nb=[L1]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a loop body calling a reading function consumes two lines per turn", async () => {
    const result = await makeBash().exec(
      'f() { read b; echo "f got [$b]"; }; ' +
        'while read a; do echo "a=[$a]"; f; done < /loop.txt',
    );
    expect(result.stdout).toBe(
      "a=[L1]\nf got [L2]\na=[L3]\nf got [L4]\na=[L5]\nf got []\n",
    );
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("eval shares the shell's stdin position", () => {
  it("a read inside eval advances the position the next read sees", async () => {
    const result = await makeBash().exec(
      '{ eval \'read a\'; echo "a=[$a]"; read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[L1]\nb=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("two reads inside eval consume two lines", async () => {
    const result = await makeBash().exec(
      "{ eval 'read a; read c'; read b; " +
        'echo "a=[$a] c=[$c] b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[L1] c=[L2] b=[L3]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a read loop inside eval sees every line of the shell's stdin", async () => {
    const result = await makeBash().exec(
      "{ eval 'while read l; do echo \"E:$l\"; done'; } < /loop.txt",
    );
    expect(result.stdout).toBe("E:L1\nE:L2\nE:L3\nE:L4\nE:L5\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a redirection on eval gives it its own stdin", async () => {
    const result = await makeBash().exec(
      "{ eval 'read a; echo \"a=[$a]\"' < /other.txt; " +
        'read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[O1]\nb=[L1]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a heredoc on eval gives it its own stdin", async () => {
    const result = await makeBash().exec(
      "{ eval 'read a; echo \"a=[$a]\"' <<EOT\nH1\nEOT\n" +
        'read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[H1]\nb=[L1]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("an empty redirection is still an owned fd 0", () => {
  // `f < empty-file` hands the body an empty stdin, which is EOF — not "no
  // redirection, inherit the shell's". The two are the same empty string at
  // the builtin boundary, so ownership travels beside it as its own flag.
  const shields: Array<[label: string, script: string]> = [
    [
      "a redirection on the call",
      'f() { read x; echo "x=[$x]"; }; { read a; f < /empty.txt; ' +
        'read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    ],
    [
      "a redirection on the definition",
      'f() { read x; echo "x=[$x]"; } < /empty.txt; { read a; f; ' +
        'read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    ],
    [
      "/dev/null on the call",
      'f() { read x; echo "x=[$x]"; }; { read a; f < /dev/null; ' +
        'read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    ],
    [
      "a redirection on eval",
      "{ read a; eval 'read x; echo \"x=[$x]\"' < /empty.txt; " +
        'read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    ],
    [
      "/dev/null on eval",
      "{ read a; eval 'read x; echo \"x=[$x]\"' < /dev/null; " +
        'read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    ],
    [
      "an empty file on a group",
      '{ read a; { read x; echo "x=[$x]"; } < /empty.txt; ' +
        'read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    ],
  ];

  for (const [label, script] of shields) {
    it(`reads EOF, not the shell's stdin, with ${label}`, async () => {
      const result = await makeBash().exec(script);
      expect(result.stdout).toBe("x=[]\na=[L1] b=[L2]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  }

  // The idiom this protects: shielding a loop body from the loop's own stdin.
  const loopShields: Array<[label: string, body: string]> = [
    ["a function call", "f < /dev/null"],
    ["a function call from an empty file", "f < /empty.txt"],
    ["eval", "eval 'read q' < /dev/null"],
    ["a group", "{ read q; } < /dev/null"],
    ["a subshell", "(read q) < /dev/null"],
  ];

  for (const [label, body] of loopShields) {
    it(`a loop whose body is ${label} still runs once per line`, async () => {
      const result = await makeBash().exec(
        "f() { read q; }; n=0\n" +
          `while read x; do n=$((n+1)); ${body}; done < /loop.txt\n` +
          'echo "iterations: $n"',
      );
      expect(result.stdout).toBe("iterations: 5\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  }
});

describe("read-loop idioms keep their own stdin", () => {
  const bodies: Array<[label: string, body: string]> = [
    ["empty body", ":"],
    ["command group", "{ echo x; } >/dev/null"],
    ["subshell", "(echo x) >/dev/null"],
    ["function call", "f >/dev/null"],
    ["eval", "eval 'echo x' >/dev/null"],
    ["pipeline inside a command group", "{ echo x | cat; } >/dev/null"],
    ["pipeline inside a function", "g >/dev/null"],
    ["pipeline inside eval", "eval 'echo x | cat' >/dev/null"],
    ["pipeline inside a subshell", "(echo x | cat) >/dev/null"],
  ];

  for (const [label, body] of bodies) {
    it(`runs once per line with ${label} in the body`, async () => {
      const result = await makeBash().exec(
        "f() { echo x; }; g() { echo x | cat; }; n=0\n" +
          `while read a; do n=$((n+1)); ${body}; done < /loop.txt\n` +
          'echo "iterations: $n"',
      );
      expect(result.stdout).toBe("iterations: 5\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  }

  it("a group in the loop body pairs the lines two at a time", async () => {
    const result = await makeBash().exec(
      'while read a; do { read b; }; echo "pair=[$a][$b]"; done < /loop.txt',
    );
    expect(result.stdout).toBe("pair=[L1][L2]\npair=[L3][L4]\npair=[L5][]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a loop with its own stdin leaves the enclosing position alone", async () => {
    const result = await makeBash().exec(
      '{ read a; while read x; do echo "X:$x"; done < /other.txt; ' +
        'read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("X:O1\nX:O2\nX:O3\na=[L1] b=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});
