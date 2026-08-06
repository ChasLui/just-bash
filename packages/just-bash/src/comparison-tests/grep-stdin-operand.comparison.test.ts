import { afterEach, beforeEach, describe, it } from "vitest";
import {
  cleanupTestDir,
  compareOutputs,
  createTestDir,
  setupFiles,
} from "./fixture-runner.js";

/**
 * `grep PATTERN -` comparison tests: `-` as a FILE operand means standard
 * input, labelled `(standard input)`.
 *
 * Fixtures are recorded against GNU grep 3.12, not the BSD grep that ships with
 * macOS, and are therefore locked. To re-record:
 *
 *   PATH=/opt/homebrew/opt/grep/libexec/gnubin:$PATH \
 *     RECORD_FIXTURES=force pnpm test:run \
 *     src/comparison-tests/grep-stdin-operand.comparison.test.ts
 */
describe("grep - (stdin operand) - Real Bash Comparison", () => {
  let testDir: string;

  beforeEach(async () => {
    testDir = await createTestDir();
  });

  afterEach(async () => {
    await cleanupTestDir(testDir);
  });

  const files = {
    "f1.txt": "apple\nbanana\n",
    "f2.txt": "cherry\napple pie\n",
  };

  const treeFiles = {
    ...files,
    "sub/a.txt": "apple tree\n",
  };

  describe("a lone - operand", () => {
    it("should read standard input", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "printf 'apple\\n' | grep apple -");
    });

    it("should print no file-name prefix", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'x\\napple pie\\n' | grep -n apple -",
      );
    });

    it("should exit 1 when standard input does not match", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "printf 'zzz\\n' | grep apple -");
    });

    it("should count empty standard input as zero", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "printf '' | grep -c apple -");
    });

    it("should list (standard input) with -l", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(env, testDir, "printf 'apple\\n' | grep -l apple -");
    });

    it("should ignore --exclude for standard input", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple\\n' | grep --exclude='*' apple -",
      );
    });

    it("should accept - after the option terminator", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple stdin\\n' | grep -- apple - f1.txt",
      );
    });
  });

  describe("- mixed with files", () => {
    it("should label standard input in the multi-file prefix", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple stdin\\n' | grep apple f1.txt - f2.txt",
      );
    });

    it("should label standard input in -l output", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple stdin\\n' | grep -l apple f1.txt - f2.txt",
      );
    });

    it("should label standard input in -L output", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'zzz\\n' | grep -L apple f1.txt - f2.txt",
      );
    });

    it("should label standard input in -c output", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple stdin\\n' | grep -c apple f1.txt - f2.txt",
      );
    });

    it("should number standard input lines independently", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'x\\napple stdin\\n' | grep -n apple f1.txt - f2.txt",
      );
    });

    it("should drop the label with -h", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple stdin\\n' | grep -h apple f1.txt - f2.txt",
      );
    });

    it("should keep -l names with -h", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple stdin\\n' | grep -h -l apple f1.txt -",
      );
    });

    it("should label standard input with -o", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple stdin\\n' | grep -o apple f1.txt - f2.txt",
      );
    });

    it("should label standard input with -v", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'zzz\\n' | grep -v apple f1.txt -",
      );
    });

    it("should apply -m per source", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple s1\\napple s2\\n' | grep -m1 apple f1.txt -",
      );
    });

    it("should still search standard input when another operand is missing", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple\\n' | grep apple nope.txt - 2>/dev/null",
      );
    });

    it("should exit 0 for -q even when another operand is missing", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple\\n' | grep -q apple - nope.txt 2>/dev/null",
      );
    });
  });

  describe("repeated -", () => {
    it("should give the second - an empty stream", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple\\napple2\\n' | grep apple - -",
      );
    });

    it("should count the second - as zero", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple\\napple2\\n' | grep -c apple - -",
      );
    });

    it("should count every - after the first as zero", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple\\n' | grep -c apple - - -",
      );
    });

    it("should list the drained - under -L", async () => {
      const env = await setupFiles(testDir, files);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple\\n' | grep -L apple - -",
      );
    });
  });

  describe("- with -r", () => {
    it("should print no prefix for a lone - under -r", async () => {
      const env = await setupFiles(testDir, treeFiles);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple stdin\\n' | grep -r apple -",
      );
    });

    it("should prefix when a directory is searched alongside -", async () => {
      const env = await setupFiles(testDir, treeFiles);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple stdin\\n' | grep -r apple - sub",
      );
    });

    it("should list both sources with -lr", async () => {
      const env = await setupFiles(testDir, treeFiles);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple\\n' | grep -lr apple - sub",
      );
    });

    it("should prefix repeated - under -r", async () => {
      const env = await setupFiles(testDir, treeFiles);
      await compareOutputs(
        env,
        testDir,
        "printf 'apple\\n' | grep -r apple - -",
      );
    });
  });
});
