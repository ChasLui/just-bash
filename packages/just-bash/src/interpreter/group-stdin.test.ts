import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

/**
 * A command group restores only the stdin it actually replaced.
 *
 * `{ …; } < file` (or a heredoc/here-string on the group) gives the group its
 * own fd 0, so the enclosing shell's read position survives the group intact.
 * A group without one shares the shell's fd 0: reads inside it move the single
 * shared position, and the command after the group must see the line the group
 * left off at — `{ { read a; }; read b; }` gives `b` the *second* line.
 *
 * Found while verifying the pipeline half of the same rule (PR #328): the
 * inner group used to put the pre-group position back unconditionally, which
 * replayed every line its body had consumed.
 *
 * Every expectation below was checked against GNU bash 3.2.57.
 */

const FIVE_LINES = "L1\nL2\nL3\nL4\nL5\n";
const TWO_LINES = "L1\nL2\n";
const OTHER = "O1\nO2\nO3\n";

function makeBash(content = FIVE_LINES): Bash {
  return new Bash({
    files: { "/loop.txt": content, "/other.txt": OTHER, "/empty.txt": "" },
    cwd: "/",
  });
}

describe("group stdin — the repro", () => {
  it("an inner group's read advances the position the next read sees", async () => {
    const result = await makeBash(TWO_LINES).exec(
      '{ { read a; }; read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("b=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("reports both variables from the shared position", async () => {
    const result = await makeBash().exec(
      '{ { read a; }; read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[L1] b=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("group stdin — a group without its own fd 0 shares the position", () => {
  it("nesting three groups deep still advances one position", async () => {
    const result = await makeBash().exec(
      '{ { { read a; }; }; read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[L1] b=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("reads inside and outside an inner group interleave", async () => {
    const result = await makeBash().exec(
      '{ { { read a; }; read c; }; read b; echo "a=[$a] c=[$c] b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[L1] c=[L2] b=[L3]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a group that read to EOF leaves the next read at EOF", async () => {
    const result = await makeBash().exec(
      "{ { read a; read c; read d; read e; read f; }; " +
        'read b; rc=$?; echo "b=[$b] rc=$rc"; } < /loop.txt',
    );
    expect(result.stdout).toBe("b=[] rc=1\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("an output redirection on the group does not make it own stdin", async () => {
    const result = await makeBash().exec(
      '{ { read a; } >/dev/null; read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[L1] b=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("cat after an inner group starts at the line the group left", async () => {
    const result = await makeBash().exec("{ { read a; }; cat; } < /loop.txt");
    expect(result.stdout).toBe("L2\nL3\nL4\nL5\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a failing command in the group does not rewind the position", async () => {
    const result = await makeBash().exec(
      '{ { read a; grep -q zzz /dev/null; }; read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[L1] b=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("errexit aborting inside the group leaves no output", async () => {
    const result = await makeBash().exec(
      'set -e; { { read a; false; }; read b; echo "b=[$b]"; } < /loop.txt; echo "after"',
    );
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(1);
  });

  it("break out of a group inside a loop keeps both reads", async () => {
    const result = await makeBash().exec(
      'while read a; do { read b; break; }; done < /loop.txt; echo "a=[$a] b=[$b]"',
    );
    expect(result.stdout).toBe("a=[L1] b=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("group stdin — a pipeline in the body is not a read", () => {
  // A pipeline stage clears the shared stdin while it runs (issue #323, fixed
  // for the pipeline itself in PR #328). `undefined` is not a read position, so
  // a group that only shared the shell's stdin hands back the position it
  // inherited rather than propagating the clear.
  it("a pipeline inside an inner group leaves the position alone", async () => {
    const result = await makeBash().exec(
      '{ { echo x | cat; }; read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("x\nb=[L1]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a pipeline that reads nothing consumes nothing", async () => {
    const result = await makeBash().exec(
      '{ { true | true; }; read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("b=[L1]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a pipeline inside eval leaves the position alone", async () => {
    const result = await makeBash().exec(
      "{ eval 'echo x | cat'; read b; echo \"b=[$b]\"; } < /loop.txt",
    );
    expect(result.stdout).toBe("x\nb=[L1]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a read before a pipeline group still advances by one line", async () => {
    const result = await makeBash().exec(
      '{ read a; { echo x | cat; }; read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("x\na=[L1] b=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("group stdin — a group with its own fd 0 restores the outer position", () => {
  it("`< file` on the inner group leaves the outer position untouched", async () => {
    const result = await makeBash().exec(
      '{ read a; { read x; echo "x=[$x]"; } < /other.txt; ' +
        'read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("x=[O1]\na=[L1] b=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a heredoc on the inner group leaves the outer position untouched", async () => {
    const result = await makeBash().exec(
      '{ { read a; echo "a=[$a]"; } <<EOT\nH1\nEOT\n' +
        'read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[H1]\nb=[L1]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a here-string on the inner group leaves the outer position untouched", async () => {
    const result = await makeBash().exec(
      '{ { read a; echo "a=[$a]"; } <<< "S1"; read b; echo "b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[S1]\nb=[L1]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("an empty file on the inner group means EOF inside, not the outer stdin", async () => {
    const result = await makeBash().exec(
      '{ read a; { read x; echo "x=[$x]"; } < /empty.txt; ' +
        'read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("x=[]\na=[L1] b=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("an empty heredoc on the inner group means EOF inside", async () => {
    const result = await makeBash().exec(
      '{ read a; { read x; echo "x=[$x]"; } <<EOT\nEOT\n' +
        'read b; echo "a=[$a] b=[$b]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("x=[]\na=[L1] b=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a redirected inner group followed by a loop over the outer stdin", async () => {
    const result = await makeBash().exec(
      '{ { read a; echo "a=[$a]"; } < /other.txt; ' +
        'while read l; do echo "L:$l"; done; } < /loop.txt',
    );
    expect(result.stdout).toBe("a=[O1]\nL:L1\nL:L2\nL:L3\nL:L4\nL:L5\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a heredoc on the outer group is the position the inner group shares", async () => {
    const result = await makeBash().exec(
      '{ { read a; }; read b; echo "a=[$a] b=[$b]"; } <<EOT\nH1\nH2\nEOT\n',
    );
    expect(result.stdout).toBe("a=[H1] b=[H2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("a here-string on the outer group runs out after its single line", async () => {
    const result = await makeBash().exec(
      '{ { read a; }; read b; echo "a=[$a] b=[$b]"; } <<< "S1"',
    );
    expect(result.stdout).toBe("a=[S1] b=[]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});
