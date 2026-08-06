import { afterEach, beforeEach, describe, it } from "vitest";
import {
  cleanupTestDir,
  compareOutputs,
  createTestDir,
  setupFiles,
} from "./fixture-runner.js";

/**
 * Output redirections attached to a whole loop (`while ... done > file`).
 *
 * Every case ends by printing the redirect target so the file contents show up
 * in the compared stdout.
 *
 * Two fixtures here are locked, because their RECORDED stderr is
 * bash-version-sensitive even though the runner only compares stdout and exit
 * code: bash prefixes redirection diagnostics with the script line number
 * ("cannot overwrite existing file", "Is a directory"), and the line bash
 * attributes them to differs between 3.2 and 5.x. Re-recording on another bash
 * would rewrite those strings and make the comparison-tests workflow fail on
 * the resulting git diff. Their stdout and exit code are version-stable, so
 * locking loses no coverage.
 *
 * Loop-level input redirection lives in
 * loop-redirections-stdin.comparison.test.ts.
 */
describe("loop output redirections - Real Bash Comparison", () => {
  let testDir: string;

  beforeEach(async () => {
    testDir = await createTestDir();
  });

  afterEach(async () => {
    await cleanupTestDir(testDir);
  });

  describe("while loops", () => {
    it("should discard output redirected to /dev/null", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        "while true; do echo x; break; done >/dev/null; echo end",
      );
    });

    it("should write loop output to a file", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          "i=0",
          'while [ $i -lt 3 ]; do echo "line$i"; i=$((i + 1)); done > out.txt',
          'echo "---"',
          "cat out.txt",
        ].join("\n"),
      );
    });

    it("should append loop output with >>", async () => {
      const env = await setupFiles(testDir, { "out.txt": "pre\n" });
      await compareOutputs(
        env,
        testDir,
        [
          "i=0",
          'while [ $i -lt 2 ]; do echo "l$i"; i=$((i + 1)); done >> out.txt',
          "cat out.txt",
        ].join("\n"),
      );
    });

    it("should redirect stderr with 2>", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          "i=0",
          'while [ $i -lt 2 ]; do echo "o$i"; echo "e$i" >&2; i=$((i + 1)); done 2> err.txt',
          'echo "---"',
          "cat err.txt",
        ].join("\n"),
      );
    });

    it("should merge stderr into the file with > out 2>&1", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          "while true; do echo o; echo e >&2; break; done > out.txt 2>&1",
          "cat out.txt",
        ].join("\n"),
      );
    });

    it("should keep stderr on stdout with 2>&1 > out", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          "while true; do echo o; echo e >&2; break; done 2>&1 > out.txt",
          'echo "---"',
          "cat out.txt",
        ].join("\n"),
      );
    });

    it("should send both streams to a file with &>", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          "while true; do echo o; echo e >&2; break; done &> both.txt",
          "cat both.txt",
        ].join("\n"),
      );
    });

    it("should redirect output produced by the loop condition", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          "i=0",
          'while echo "cond$i"; [ $i -lt 1 ]; do echo "body$i"; i=$((i + 1)); done > out.txt',
          'echo "---"',
          "cat out.txt",
        ].join("\n"),
      );
    });

    it("should truncate the target before the loop body reads it", async () => {
      const env = await setupFiles(testDir, { "data.txt": "hi\n" });
      await compareOutputs(
        env,
        testDir,
        [
          "while true; do cat data.txt; break; done > data.txt",
          'echo "[$(cat data.txt)]"',
        ].join("\n"),
      );
    });

    it("should expand the redirect target before running the body", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          "n=0",
          'while true; do echo body; break; done > "f$((n++)).txt"',
          'echo "n=$n"',
          "cat f0.txt",
        ].join("\n"),
      );
    });

    // Fixture eb83615d48785472 is locked: recorded stderr carries a
    // bash-version-sensitive "line N:" prefix (see file header).
    it("should fail when the target is a directory", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          "mkdir adir",
          "while true; do echo x; break; done > adir",
          'echo "rc=$?"',
        ].join("\n"),
      );
    });

    // Fixture 240730055fc830ec is locked: recorded stderr carries a
    // bash-version-sensitive "line N:" prefix (see file header).
    it("should honor noclobber", async () => {
      const env = await setupFiles(testDir, { "out.txt": "keep\n" });
      await compareOutputs(
        env,
        testDir,
        [
          "set -o noclobber",
          "while true; do echo new; break; done > out.txt",
          'echo "rc=$?"',
          "cat out.txt",
        ].join("\n"),
      );
    });

    it("should override noclobber with >|", async () => {
      const env = await setupFiles(testDir, { "out.txt": "keep\n" });
      await compareOutputs(
        env,
        testDir,
        [
          "set -o noclobber",
          "while true; do echo new; break; done >| out.txt",
          "cat out.txt",
        ].join("\n"),
      );
    });

    it("should keep the last of two stdout targets", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          "while true; do echo m; break; done > a.txt > b.txt",
          'echo "a=[$(cat a.txt)] b=[$(cat b.txt)]"',
        ].join("\n"),
      );
    });
  });

  describe("until loops", () => {
    it("should write loop output to a file", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          "i=0",
          'until [ $i -ge 2 ]; do echo "u$i"; i=$((i + 1)); done > out.txt',
          'echo "---"',
          "cat out.txt",
        ].join("\n"),
      );
    });

    it("should append loop output with >>", async () => {
      const env = await setupFiles(testDir, { "out.txt": "start\n" });
      await compareOutputs(
        env,
        testDir,
        [
          "i=0",
          'until [ $i -ge 2 ]; do echo "a$i"; i=$((i + 1)); done >> out.txt',
          "cat out.txt",
        ].join("\n"),
      );
    });

    it("should redirect stderr with 2>", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          "i=0",
          'until [ $i -ge 2 ]; do echo "e$i" >&2; i=$((i + 1)); done 2> err.txt',
          "cat err.txt",
        ].join("\n"),
      );
    });

    it("should redirect around break and continue", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        [
          "i=0",
          "until [ $i -ge 4 ]; do",
          "  i=$((i + 1))",
          "  if [ $i -eq 2 ]; then continue; fi",
          "  if [ $i -eq 4 ]; then break; fi",
          '  echo "u$i"',
          "done > out.txt",
          "cat out.txt",
        ].join("\n"),
      );
    });
  });
});
