import { describe, expect, it } from "vitest";
import { Bash } from "../../Bash.js";

/**
 * Exit-status truth table for `grep -L` / `--files-without-match`.
 *
 * GNU grep's status reports whether a *line was selected*, never whether a
 * *filename was printed*. `-L` therefore has the same status rule as plain
 * grep and `-l`: 0 if any input matched, 1 if none did, 2 on error. The
 * counter-intuitive consequence is that `-L` exits 1 in exactly the case where
 * it prints the most output (nothing matched, so every name is listed) and 0
 * when it prints nothing at all (everything matched).
 *
 * Every expectation below was measured against GNU grep 3.12 (Homebrew
 * `gnubin/grep`); the mixed-input cases were cross-checked against BSD grep,
 * which agrees. BusyBox grep is the odd one out and deliberately inverts this
 * — see the note in src/spec-tests/grep/skips.ts.
 */

const FILES = {
  "/m1.txt": "hello\nworld\n",
  "/m2.txt": "hello there\n",
  "/n1.txt": "nothing here\n",
  "/n2.txt": "nope\n",
  "/empty.txt": "",
};

const env = (): Bash => new Bash({ files: FILES });

describe("grep -L exit status (GNU semantics)", () => {
  it("exits 0 and prints nothing when every file matches", async () => {
    const result = await env().exec("grep -L hello /m1.txt /m2.txt");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("exits 1 and lists every file when no file matches", async () => {
    const result = await env().exec("grep -L hello /n1.txt /n2.txt");
    expect(result.stdout).toBe("/n1.txt\n/n2.txt\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(1);
  });

  it("exits 0 and lists only the non-matching file when input is mixed", async () => {
    const result = await env().exec("grep -L hello /m1.txt /n1.txt");
    expect(result.stdout).toBe("/n1.txt\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("exits 0 and prints nothing for a single matching file", async () => {
    const result = await env().exec("grep -L hello /m1.txt");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("exits 1 and prints the name for a single non-matching file", async () => {
    const result = await env().exec("grep -L hello /n1.txt");
    expect(result.stdout).toBe("/n1.txt\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(1);
  });

  it("exits 1 for an empty file, which can never match", async () => {
    const result = await env().exec("grep -L hello /empty.txt");
    expect(result.stdout).toBe("/empty.txt\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(1);
  });

  it("keeps -l as the mirror image of -L", async () => {
    const listed = await env().exec("grep -l hello /m1.txt /n1.txt");
    expect(listed.stdout).toBe("/m1.txt\n");
    expect(listed.stderr).toBe("");
    expect(listed.exitCode).toBe(0);

    const none = await env().exec("grep -l hello /n1.txt /n2.txt");
    expect(none.stdout).toBe("");
    expect(none.stderr).toBe("");
    expect(none.exitCode).toBe(1);
  });
});

describe("grep -L exit status with unreadable operands", () => {
  it("exits 2 when an operand is missing even though another matched", async () => {
    const result = await env().exec("grep -L hello /m1.txt /missing.txt");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe(
      "grep: /missing.txt: No such file or directory\n",
    );
    expect(result.exitCode).toBe(2);
  });

  it("exits 2 when an operand is missing and prints the names it did resolve", async () => {
    const result = await env().exec("grep -L hello /n1.txt /missing.txt");
    expect(result.stdout).toBe("/n1.txt\n");
    expect(result.stderr).toBe(
      "grep: /missing.txt: No such file or directory\n",
    );
    expect(result.exitCode).toBe(2);
  });

  it("exits 2 when the only operand is missing", async () => {
    const result = await env().exec("grep -L hello /missing.txt");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe(
      "grep: /missing.txt: No such file or directory\n",
    );
    expect(result.exitCode).toBe(2);
  });

  it("lets an error outrank a listed name but not a -q match", async () => {
    // GNU 3.12 exits before it ever opens /missing.txt here, so there is no
    // diagnostic to print and the selected line wins over the error.
    const result = await env().exec("grep -L -q hello /m1.txt /missing.txt");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("grep -L combined with -q", () => {
  it("exits 0 and stays silent when every file matches", async () => {
    const result = await env().exec("grep -L -q hello /m1.txt /m2.txt");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("exits 1 and stays silent when no file matches", async () => {
    const result = await env().exec("grep -L -q hello /n1.txt /n2.txt");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(1);
  });

  it("exits 0 and stays silent on mixed input", async () => {
    const result = await env().exec("grep -L -q hello /m1.txt /n1.txt");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("grep -L combined with -c", () => {
  // GNU lets -L win over -c: names are printed, counts are not, and the status
  // rule is unchanged.
  it("exits 0 and prints nothing when every file matches", async () => {
    const result = await env().exec("grep -L -c hello /m1.txt /m2.txt");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("exits 1 and prints names, not counts, when no file matches", async () => {
    const result = await env().exec("grep -L -c hello /n1.txt /n2.txt");
    expect(result.stdout).toBe("/n1.txt\n/n2.txt\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(1);
  });

  it("exits 0 and prints only the non-matching name on mixed input", async () => {
    const result = await env().exec("grep -L -c hello /m1.txt /n1.txt");
    expect(result.stdout).toBe("/n1.txt\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("grep -L combined with other output options", () => {
  it("ignores -n, which selects no lines to number", async () => {
    const result = await env().exec("grep -L -n hello /m1.txt /n1.txt");
    expect(result.stdout).toBe("/n1.txt\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("ignores -o, which selects no matches to print", async () => {
    const result = await env().exec("grep -L -o hello /m1.txt /n1.txt");
    expect(result.stdout).toBe("/n1.txt\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("ignores -m, which only caps matches per file", async () => {
    const result = await env().exec("grep -L -m 1 hello /m1.txt /n1.txt");
    expect(result.stdout).toBe("/n1.txt\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("inverts which files are listed under -v", async () => {
    // /m1.txt holds `hello` and `world`, so -v selects the `world` line and the
    // file is not listed; /n2.txt is a single non-`hello` line, which -v selects
    // as well. Nothing is left to list, and a line was selected, so exit 0.
    const result = await env().exec("grep -L -v hello /m1.txt /n2.txt");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("uses the same rule under --files-without-match", async () => {
    const allMatch = await env().exec(
      "grep --files-without-match hello /m1.txt /m2.txt",
    );
    expect(allMatch.stdout).toBe("");
    expect(allMatch.stderr).toBe("");
    expect(allMatch.exitCode).toBe(0);

    const noneMatch = await env().exec(
      "grep --files-without-match hello /n1.txt /n2.txt",
    );
    expect(noneMatch.stdout).toBe("/n1.txt\n/n2.txt\n");
    expect(noneMatch.stderr).toBe("");
    expect(noneMatch.exitCode).toBe(1);
  });
});

describe("grep -L with recursive search", () => {
  const tree = (): Bash =>
    new Bash({
      files: {
        "/dir/has.txt": "hello\n",
        "/dir/lacks.txt": "goodbye\n",
        "/dir/sub/has2.txt": "hello\n",
        "/dir/sub/lacks2.txt": "goodbye\n",
      },
    });

  it("exits 0 while listing the non-matching files under the tree", async () => {
    const result = await tree().exec("grep -rL hello /dir");
    expect(result.stdout).toBe("/dir/lacks.txt\n/dir/sub/lacks2.txt\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("exits 1 when nothing under the tree matches", async () => {
    const result = await tree().exec("grep -rL nowhere /dir");
    expect(result.stdout).toBe(
      "/dir/has.txt\n/dir/lacks.txt\n/dir/sub/has2.txt\n/dir/sub/lacks2.txt\n",
    );
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(1);
  });
});
