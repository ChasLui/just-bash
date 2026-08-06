import { afterEach, beforeEach, describe, it } from "vitest";
import {
  cleanupTestDir,
  compareOutputs,
  createTestDir,
  setupFiles,
} from "./fixture-runner.js";

/**
 * `grep -L` / `--files-without-match` exit-status comparison tests.
 *
 * GNU grep's status reports whether a line was *selected*, not whether a
 * filename was *printed*, so `-L` exits 0 when every file matched (printing
 * nothing) and 1 when none did (printing every name).
 *
 * Fixtures are recorded against GNU grep 3.12, not the BSD grep that ships with
 * macOS, and are therefore locked. To re-record:
 *
 *   PATH=/opt/homebrew/opt/grep/libexec/gnubin:$PATH \
 *     RECORD_FIXTURES=force pnpm test:run \
 *     src/comparison-tests/grep-files-without-match.comparison.test.ts
 */
describe("grep -L - Real Bash Comparison", () => {
  let testDir: string;

  beforeEach(async () => {
    testDir = await createTestDir();
  });

  afterEach(async () => {
    await cleanupTestDir(testDir);
  });

  const files = {
    "m1.txt": "hello\nworld\n",
    "m2.txt": "hello there\n",
    "n1.txt": "nothing here\n",
    "n2.txt": "nope\n",
    "empty.txt": "",
  };

  describe("exit status", () => {
    it("should exit 0 and print nothing when every file matches", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L hello m1.txt m2.txt");
    });

    it("should exit 1 and list every file when no file matches", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L hello n1.txt n2.txt");
    });

    it("should exit 0 and list only the non-matching file on mixed input", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L hello m1.txt n1.txt");
    });

    it("should exit 0 for a single matching file", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L hello m1.txt");
    });

    it("should exit 1 for a single non-matching file", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L hello n1.txt");
    });

    it("should exit 1 for an empty file", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L hello empty.txt");
    });

    it("should report the status through $? after -L", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "grep -L hello m1.txt m2.txt; echo status=$?",
      );
    });

    it("should report the status through $? when nothing matched", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "grep -L hello n1.txt n2.txt; echo status=$?",
      );
    });

    it("should be usable as an if condition when every file matches", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "if grep -L hello m1.txt m2.txt >/dev/null; then echo yes; else echo no; fi",
      );
    });

    it("should be usable as an if condition when no file matches", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "if grep -L hello n1.txt n2.txt >/dev/null; then echo yes; else echo no; fi",
      );
    });
  });

  describe("mirror of -l", () => {
    it("should exit 0 and list matching files under -l", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -l hello m1.txt n1.txt");
    });

    it("should exit 1 and print nothing under -l when nothing matches", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -l hello n1.txt n2.txt");
    });

    it("should exit 0 under -l when every file matches", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -l hello m1.txt m2.txt");
    });
  });

  describe("unreadable operands", () => {
    it("should exit 2 when an operand is missing even though another matched", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L hello m1.txt missing.txt");
    });

    it("should exit 2 and still list the resolved non-matching file", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L hello n1.txt missing.txt");
    });

    it("should exit 2 when the only operand is missing", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L hello missing.txt");
    });
  });

  describe("combined with -q", () => {
    it("should exit 0 quietly when every file matches", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L -q hello m1.txt m2.txt");
    });

    it("should exit 1 quietly when no file matches", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L -q hello n1.txt n2.txt");
    });

    it("should exit 0 quietly on mixed input", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L -q hello m1.txt n1.txt");
    });

    it("should let a -q match outrank a missing operand", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L -q hello m1.txt missing.txt");
    });
  });

  describe("combined with -c", () => {
    it("should print names rather than counts when every file matches", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L -c hello m1.txt m2.txt");
    });

    it("should print names rather than counts when no file matches", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L -c hello n1.txt n2.txt");
    });

    it("should print names rather than counts on mixed input", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L -c hello m1.txt n1.txt");
    });
  });

  describe("combined with other output options", () => {
    it("should ignore -n under -L", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L -n hello m1.txt n1.txt");
    });

    it("should ignore -o under -L", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L -o hello m1.txt n1.txt");
    });

    it("should ignore -m under -L", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L -m 1 hello m1.txt n1.txt");
    });

    it("should invert which files are listed under -v", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "grep -L -v hello m1.txt n2.txt");
    });

    it("should use the same rule under --files-without-match", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "grep --files-without-match hello n1.txt n2.txt",
      );
    });

    it("should use the same rule under --files-without-match when all match", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "grep --files-without-match hello m1.txt m2.txt",
      );
    });
  });
});
