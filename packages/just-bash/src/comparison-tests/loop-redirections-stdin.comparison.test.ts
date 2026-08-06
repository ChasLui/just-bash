import { afterEach, beforeEach, describe, it } from "vitest";
import {
  cleanupTestDir,
  compareOutputs,
  createTestDir,
  setupFiles,
} from "./fixture-runner.js";

/**
 * Loop-level input redirection (`done < file`, here-docs, here-strings, pipes)
 * combined with output redirection, plus nested and piped loops.
 *
 * As in loop-redirections.comparison.test.ts, each case prints the redirect
 * target so file contents land in the compared stdout.
 *
 * The two missing-input-file fixtures are locked. The runner compares stdout
 * and exit code only, but it still RECORDS stderr, and bash's
 * "No such file or directory" diagnostic carries a script line-number prefix
 * that differs between 3.2 and 5.x — re-recording on another bash would
 * rewrite those strings and fail the comparison-tests workflow on the git
 * diff. Their stdout and exit code are version-stable, so locking loses no
 * coverage.
 */
describe("loop stdin redirections - Real Bash Comparison", () => {
  let testDir: string;

  beforeEach(async () => {
    testDir = await createTestDir();
  });

  afterEach(async () => {
    await cleanupTestDir(testDir);
  });

  describe("input redirection combined with output redirection", () => {
    it("should read a file and write another (while)", async () => {
      const env = await setupFiles(testDir, { "in.txt": "a\nb\n" });
      await compareOutputs(
        env,
        testDir,
        [
          'while read l; do echo "[$l]"; done < in.txt > out.txt',
          "cat out.txt",
        ].join("\n"),
      );
    });

    it("should read a file and write another (until)", async () => {
      const env = await setupFiles(testDir, { "in.txt": "c\nd\n" });
      await compareOutputs(
        env,
        testDir,
        [
          'until ! read l; do echo "<$l>"; done < in.txt > out.txt',
          "cat out.txt",
        ].join("\n"),
      );
    });

    it("should send output to the last target with > a < in > b", async () => {
      const env = await setupFiles(testDir, { "in.txt": "e\nf\n" });
      await compareOutputs(
        env,
        testDir,
        [
          'while read l; do echo "{$l}"; done > a.txt < in.txt > b.txt',
          'echo "a=[$(cat a.txt)] b=[$(cat b.txt)]"',
        ].join("\n"),
      );
    });

    it("should truncate before reading with > f < f", async () => {
      const env = await setupFiles(testDir, { "f.txt": "l1\nl2\n" });
      await compareOutputs(
        env,
        testDir,
        [
          'while read l; do echo "got:$l"; done > f.txt < f.txt',
          'echo "rc=$? f=[$(cat f.txt)]"',
        ].join("\n"),
      );
    });

    it("should keep plain `done < file` working", async () => {
      const env = await setupFiles(testDir, { "in.txt": "one\ntwo\n" });
      await compareOutputs(
        env,
        testDir,
        'while read l; do echo "got:$l"; done < in.txt',
      );
    });

    it("should redirect a here-doc-fed loop", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          'while read l; do echo "h:$l"; done > out.txt <<EOF',
          "alpha",
          "beta",
          "EOF",
          "cat out.txt",
        ].join("\n"),
      );
    });

    it("should redirect a here-string-fed loop", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          'while read l; do echo "s:$l"; done <<< "hello" > out.txt',
          "cat out.txt",
        ].join("\n"),
      );
    });

    it("should not glob-expand a here-string target", async () => {
      const env = await setupFiles(testDir, { aglob1: "", aglob2: "" });
      await compareOutputs(
        env,
        testDir,
        'while read l; do echo "s:[$l]"; done <<< *',
      );
    });

    // Fixture 98da27b468bc4899 is locked: recorded stderr is
    // bash-version-sensitive (see file header).
    it("should not truncate a later target when the input file is missing", async () => {
      const env = await setupFiles(testDir, { "out.txt": "keep\n" });
      await compareOutputs(
        env,
        testDir,
        [
          'while read l; do echo "$l"; done < nosuch.txt > out.txt',
          'echo "rc=$?"',
          "cat out.txt",
        ].join("\n"),
      );
    });

    // Fixture ad1c8cf5e7b450d8 is locked: recorded stderr is
    // bash-version-sensitive (see file header).
    it("should report a missing input file for until loops", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          'until ! read l; do echo "$l"; done < nosuch.txt',
          'echo "rc=$?"',
        ].join("\n"),
      );
    });
  });

  describe("nesting and pipelines", () => {
    it("should redirect an outer for loop containing a while loop", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          "for i in 1 2; do",
          '  while true; do echo "n$i"; break; done',
          "done > out.txt",
          "cat out.txt",
        ].join("\n"),
      );
    });

    it("should redirect nested while loops independently", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          "while true; do",
          "  while true; do echo inner; break; done > inner.txt",
          "  echo outer",
          "  break",
          "done > outer.txt",
          'echo "inner=[$(cat inner.txt)] outer=[$(cat outer.txt)]"',
        ].join("\n"),
      );
    });

    it("should apply the redirect before the pipe", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          "while true; do echo x; break; done > out.txt | cat",
          'echo "---"',
          "cat out.txt",
        ].join("\n"),
      );
    });

    it("should merge stderr into the pipe with 2>&1", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        "while true; do echo o; echo e >&2; break; done 2>&1 | cat",
      );
    });

    it("should feed a piped until loop from stdin", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          "printf 'p1\\np2\\n' | until ! read l; do echo \"u:$l\"; done > out.txt",
          "cat out.txt",
        ].join("\n"),
      );
    });
  });
});
