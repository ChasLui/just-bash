import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

const THREE_LINES = "printf 'a\\nb\\nc\\n' > /tmp/f.txt";
const TWO_OTHER_LINES = "printf 'X\\nY\\n' > /tmp/g.txt";

/**
 * Lifetime of a numeric descriptor.
 *
 * `exec N< file` keeps it open until it is closed; every other construct —
 * simple command, loop, group, subshell, function — gets it only for the
 * duration of that command, and the enclosing shell's descriptor (and its
 * read position) come back untouched afterwards.
 */
describe("numeric fd scoping", () => {
  it("closes a loop descriptor when the loop ends", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        THREE_LINES,
        'while read -u 3 l; do echo "L:$l"; done 3< /tmp/f.txt',
        'read -u 3 z; echo "after rc=$? z=$z"',
      ].join("\n"),
    );
    expect(result.stdout).toBe("L:a\nL:b\nL:c\nafter rc=1 z=\n");
    expect(result.stderr).toBe(
      "bash: read: 3: invalid file descriptor: Bad file descriptor\n",
    );
    expect(result.exitCode).toBe(0);
  });

  it("restores an outer descriptor and its position after a nested loop reuses the number", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        THREE_LINES,
        TWO_OTHER_LINES,
        "exec 3< /tmp/f.txt",
        'read -u 3 first; echo "first=$first"',
        'while read -u 3 l; do echo "loop:$l"; done 3< /tmp/g.txt',
        'read -u 3 second; echo "second=$second"',
        "exec 3<&-",
      ].join("\n"),
    );
    expect(result.stdout).toBe("first=a\nloop:X\nloop:Y\nsecond=b\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("shares the descriptor across the statements of a group", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        THREE_LINES,
        '{ read -u 3 g1; read -u 3 g2; echo "g1=$g1 g2=$g2"; } 3< /tmp/f.txt',
      ].join("\n"),
    );
    expect(result.stdout).toBe("g1=a g2=b\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("shares the descriptor across iterations of a for loop", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        THREE_LINES,
        'for i in 1 2; do read -u 3 v; echo "i=$i v=$v"; done 3< /tmp/f.txt',
      ].join("\n"),
    );
    expect(result.stdout).toBe("i=1 v=a\ni=2 v=b\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("shares the descriptor across an until loop body", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        THREE_LINES,
        "n=0",
        'until [ "$n" -eq 2 ]; do read -u 3 v; echo "v=$v"; n=$((n+1)); done 3< /tmp/f.txt',
      ].join("\n"),
    );
    expect(result.stdout).toBe("v=a\nv=b\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("shares the descriptor across an if body", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        THREE_LINES,
        'if true; then read -u 3 v; read -u 3 w; echo "v=$v w=$w"; fi 3< /tmp/f.txt',
      ].join("\n"),
    );
    expect(result.stdout).toBe("v=a w=b\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("shares the descriptor across a case body", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        THREE_LINES,
        'case x in x) read -u 3 v; read -u 3 w; echo "v=$v w=$w";; esac 3< /tmp/f.txt',
      ].join("\n"),
    );
    expect(result.stdout).toBe("v=a w=b\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("opens the descriptor for a function call", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        THREE_LINES,
        'fn() { read -u 3 a; read -u 3 b; echo "fn a=$a b=$b"; }',
        "fn 3< /tmp/f.txt",
        'read -u 3 after; echo "after rc=$?"',
      ].join("\n"),
    );
    expect(result.stdout).toBe("fn a=a b=b\nafter rc=1\n");
    expect(result.stderr).toBe(
      "bash: read: 3: invalid file descriptor: Bad file descriptor\n",
    );
    expect(result.exitCode).toBe(0);
  });

  it("opens a descriptor attached to a function definition on every call", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        THREE_LINES,
        'fn() { read -u 3 a; echo "a=$a"; } 3< /tmp/f.txt',
        "fn",
        "fn",
      ].join("\n"),
    );
    // Each call re-opens the file, so both calls see the first line.
    expect(result.stdout).toBe("a=a\na=a\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("keeps a subshell's descriptor changes out of the parent", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        THREE_LINES,
        '( exec 3< /tmp/f.txt; read -u 3 s; echo "sub=$s" )',
        'read -u 3 p; echo "parent rc=$?"',
      ].join("\n"),
    );
    expect(result.stdout).toBe("sub=a\nparent rc=1\n");
    expect(result.stderr).toBe(
      "bash: read: 3: invalid file descriptor: Bad file descriptor\n",
    );
    expect(result.exitCode).toBe(0);
  });

  it("gives a subshell the descriptor its own redirection opened", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        THREE_LINES,
        '( read -u 3 s1; read -u 3 s2; echo "s1=$s1 s2=$s2" ) 3< /tmp/f.txt',
      ].join("\n"),
    );
    expect(result.stdout).toBe("s1=a s2=b\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("keeps the loop's own stdin separate from the numeric descriptor", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        THREE_LINES,
        TWO_OTHER_LINES,
        'while read -u 3 fd; do read stdin_line; echo "fd=$fd stdin=$stdin_line"; done 3< /tmp/g.txt < /tmp/f.txt',
      ].join("\n"),
    );
    expect(result.stdout).toBe("fd=X stdin=a\nfd=Y stdin=b\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  // A command word that expands to nothing takes an early exit out of the
  // simple-command path; its descriptors still have to come back down.
  describe("empty command name", () => {
    it("closes the descriptor when the first argument becomes the command", async () => {
      const env = new Bash();
      const result = await env.exec(
        [
          THREE_LINES,
          "x=''; $x true 3< /tmp/f.txt",
          'read -u 3 v; echo "rc=$? v=$v"',
        ].join("\n"),
      );
      expect(result.stdout).toBe("rc=1 v=\n");
      expect(result.stderr).toBe(
        "bash: read: 3: invalid file descriptor: Bad file descriptor\n",
      );
      expect(result.exitCode).toBe(0);
    });

    it("closes the descriptor when the command expands to nothing at all", async () => {
      const env = new Bash();
      const result = await env.exec(
        [
          THREE_LINES,
          "y=''; $y 3< /tmp/f.txt",
          'read -u 3 w; echo "rc=$? w=$w"',
        ].join("\n"),
      );
      expect(result.stdout).toBe("rc=1 w=\n");
      expect(result.stderr).toBe(
        "bash: read: 3: invalid file descriptor: Bad file descriptor\n",
      );
      expect(result.exitCode).toBe(0);
    });

    it("closes the descriptor for a literal empty command name", async () => {
      const env = new Bash();
      const result = await env.exec(
        [
          THREE_LINES,
          "'' 3< /tmp/f.txt",
          'echo "cmd rc=$?"',
          'read -u 3 z; echo "rc=$? z=$z"',
        ].join("\n"),
      );
      expect(result.stdout).toBe("cmd rc=127\nrc=1 z=\n");
      expect(result.stderr).toBe(
        "bash: : command not found\n" +
          "bash: read: 3: invalid file descriptor: Bad file descriptor\n",
      );
      expect(result.exitCode).toBe(0);
    });

    it("closes an output descriptor opened on the empty-name path", async () => {
      const env = new Bash();
      const result = await env.exec(
        ["q=''; $q true 4> /tmp/o5.txt", 'echo hi >&4; echo "rc=$?"'].join(
          "\n",
        ),
      );
      expect(result.stdout).toBe("rc=1\n");
      expect(result.stderr).toBe("bash: 4: Bad file descriptor\n");
      expect(result.exitCode).toBe(0);
    });
  });

  it("survives a break out of the loop body", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        THREE_LINES,
        'exec 4< /tmp/f.txt; read -u 4 keep; echo "keep=$keep"',
        'while read -u 3 l; do echo "L:$l"; break; done 3< /tmp/f.txt',
        'read -u 4 next; echo "next=$next"',
        "exec 4<&-",
      ].join("\n"),
    );
    expect(result.stdout).toBe("keep=a\nL:a\nnext=b\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});
