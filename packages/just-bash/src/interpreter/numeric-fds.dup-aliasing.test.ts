import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

const FOUR_LINES = "printf 'l1\\nl2\\nl3\\nl4\\n' > /tmp/dup.txt";
const OTHER_FILE = "printf 'g1\\ng2\\ng3\\n' > /tmp/dup2.txt";

/**
 * `N<&M` duplicates the descriptor, not the file: both names refer to one
 * open file description and therefore share a single read offset. Reading
 * through either advances both, and closing one leaves the other where the
 * last read left it.
 */
describe("duplicated descriptors share a read offset", () => {
  it("advances the source when the duplicate is read", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        FOUR_LINES,
        "exec 3< /tmp/dup.txt",
        "exec 4<&3",
        'read -u 3 a; echo "a=$a"',
        'read -u 4 b; echo "b=$b"',
        'read -u 3 c; echo "c=$c"',
        "exec 4<&-",
        "exec 3<&-",
      ].join("\n"),
    );
    expect(result.stdout).toBe("a=l1\nb=l2\nc=l3\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("keeps the position when the other descriptor is closed", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        FOUR_LINES,
        "exec 3< /tmp/dup.txt",
        "exec 4<&3",
        'read -u 3 a; echo "a=$a"',
        "exec 3<&-",
        'read -u 4 b; echo "b=$b"',
        "exec 4<&-",
      ].join("\n"),
    );
    expect(result.stdout).toBe("a=l1\nb=l2\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("chains through a duplicate of a duplicate", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        FOUR_LINES,
        "exec 3< /tmp/dup.txt",
        "exec 4<&3",
        "exec 5<&4",
        'read -u 5 c; echo "c=$c"',
        'read -u 3 d; echo "d=$d"',
        'read -u 4 e; echo "e=$e"',
        "exec 5<&-; exec 4<&-; exec 3<&-",
      ].join("\n"),
    );
    expect(result.stdout).toBe("c=l1\nd=l2\ne=l3\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("stops sharing once a descriptor is re-opened", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        FOUR_LINES,
        OTHER_FILE,
        "exec 3< /tmp/dup.txt",
        "exec 4<&3",
        "exec 4< /tmp/dup2.txt",
        'read -u 4 a; echo "a=$a"',
        'read -u 3 b; echo "b=$b"',
        "exec 4<&-; exec 3<&-",
      ].join("\n"),
    );
    expect(result.stdout).toBe("a=g1\nb=l1\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("keeps the description across a move", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        FOUR_LINES,
        "exec 3< /tmp/dup.txt",
        'read -u 3 g0; echo "g0=$g0"',
        "exec 4<&3-",
        'read -u 4 g1; echo "g1=$g1"',
        "exec 4<&-",
      ].join("\n"),
    );
    expect(result.stdout).toBe("g0=l1\ng1=l2\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  describe("scoped duplicates", () => {
    it("leaves the source advanced after a command-scoped dup", async () => {
      const env = new Bash();
      const result = await env.exec(
        [
          FOUR_LINES,
          "exec 3< /tmp/dup.txt",
          'read -u 4 t1 4<&3; echo "t1=$t1"',
          'read -u 3 t2; echo "t2=$t2"',
          "exec 3<&-",
        ].join("\n"),
      );
      expect(result.stdout).toBe("t1=l1\nt2=l2\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("leaves the source advanced after a group-scoped dup", async () => {
      const env = new Bash();
      const result = await env.exec(
        [
          FOUR_LINES,
          "exec 3< /tmp/dup.txt",
          'read -u 3 s0; echo "s0=$s0"',
          '{ read -u 4 s1; echo "s1=$s1"; } 4<&3',
          'read -u 3 s2; echo "s2=$s2"',
          "exec 3<&-",
        ].join("\n"),
      );
      expect(result.stdout).toBe("s0=l1\ns1=l2\ns2=l3\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("leaves the source advanced after a loop-scoped dup", async () => {
      const env = new Bash();
      const result = await env.exec(
        [
          FOUR_LINES,
          "exec 3< /tmp/dup.txt",
          'while read -u 4 l; do echo "loop:$l"; break; done 4<&3',
          'read -u 3 h; echo "h=$h"',
          "exec 3<&-",
        ].join("\n"),
      );
      expect(result.stdout).toBe("loop:l1\nh=l2\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("restores a descriptor that was already open onto its own file", async () => {
      const env = new Bash();
      const result = await env.exec(
        [
          FOUR_LINES,
          OTHER_FILE,
          "exec 3< /tmp/dup.txt",
          "exec 4< /tmp/dup2.txt",
          'read -u 4 f0; echo "f0=$f0"',
          '{ read -u 4 f1; echo "f1=$f1"; } 4<&3',
          'read -u 4 f2; echo "f2=$f2"',
          'read -u 3 f3; echo "f3=$f3"',
          "exec 4<&-; exec 3<&-",
        ].join("\n"),
      );
      // fd 4 goes back to its own file at its own offset; fd 3 keeps the
      // position the scoped read moved it to.
      expect(result.stdout).toBe("f0=g1\nf1=l1\nf2=g2\nf3=l2\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  });
});
