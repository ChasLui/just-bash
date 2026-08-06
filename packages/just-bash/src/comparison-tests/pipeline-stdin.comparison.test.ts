import { afterEach, beforeEach, describe, it } from "vitest";
import {
  cleanupTestDir,
  compareOutputs,
  createTestDir,
  setupFiles,
} from "./fixture-runner.js";

/**
 * A pipeline inherits the enclosing shell's stdin for its first stage only,
 * and never consumes it on the pipeline's behalf. Recorded against real bash
 * for https://github.com/vercel-labs/just-bash/issues/323.
 */

const FIVE_LINES = "L1\nL2\nL3\nL4\nL5\n";
const THREE_LINES = "L1\nL2\nL3\n";

describe("Pipeline stdin - Real Bash Comparison", () => {
  let testDir: string;

  beforeEach(async () => {
    testDir = await createTestDir();
  });

  afterEach(async () => {
    await cleanupTestDir(testDir);
  });

  describe("issue #323 repros", () => {
    it("keeps the shared position across a pipeline between two reads", async () => {
      const env = await setupFiles(testDir, { "loop.txt": THREE_LINES });
      await compareOutputs(
        env,
        testDir,
        '{ read a; echo "a=[$a]"; true | true; read b; echo "b=[$b]"; } < loop.txt',
      );
    });

    it("runs the loop body once per line when it contains a pipeline", async () => {
      const env = await setupFiles(testDir, { "loop.txt": FIVE_LINES });
      await compareOutputs(
        env,
        testDir,
        'n=0; while read a; do n=$((n+1)); echo hi | head -1 >/dev/null; done < loop.txt; echo "iterations: $n"',
      );
    });
  });

  describe("loop body scope table", () => {
    const bodies: Array<[label: string, body: string]> = [
      ["empty body", ":"],
      ["command group", "{ echo x; } >/dev/null"],
      ["true | true", "true | true"],
      ["echo x | echo y", "echo x | echo y >/dev/null"],
      ["echo abc | tr a-z A-Z", "echo abc | tr a-z A-Z >/dev/null"],
      ["echo hi | head -1", "echo hi | head -1 >/dev/null"],
      ["printf | sed 1q", "printf 'x\\ny\\n' | sed 1q >/dev/null"],
      ["three-stage pipeline", "echo a | cat | cat >/dev/null"],
      ["negated pipeline", "! echo a | grep -q zzz"],
      ["pipeline in a command group", "{ echo x | cat; } >/dev/null"],
      ["pipeline in a subshell", "(echo x | cat) >/dev/null"],
      [
        "pipeline in a for loop",
        "for i in 1 2; do echo x | cat; done >/dev/null",
      ],
      ["pipeline in a case arm", "case x in x) echo q | cat;; esac >/dev/null"],
      ["pipeline in eval", "eval 'echo x | cat' >/dev/null"],
    ];

    for (const [label, body] of bodies) {
      it(`${label} iterates once per line`, async () => {
        const env = await setupFiles(testDir, { "loop.txt": FIVE_LINES });
        await compareOutputs(
          env,
          testDir,
          `n=0; while read a; do n=$((n+1)); ${body}; done < loop.txt; echo "iterations: $n"`,
        );
      });
    }

    it("pipeline in a function called from the loop body", async () => {
      const env = await setupFiles(testDir, { "loop.txt": FIVE_LINES });
      await compareOutputs(
        env,
        testDir,
        'f() { echo x | cat >/dev/null; }; n=0; while read a; do n=$((n+1)); f; done < loop.txt; echo "iterations: $n"',
      );
    });

    it("reads every line, not just the first", async () => {
      const env = await setupFiles(testDir, { "loop.txt": THREE_LINES });
      await compareOutputs(
        env,
        testDir,
        'while read a; do echo "got:$a"; echo z | cat >/dev/null; done < loop.txt',
      );
    });
  });

  describe("other stdin sources", () => {
    it("here-string backing the loop survives a pipeline", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        "n=0; while read a; do n=$((n+1)); true | true; done <<< $'L1\\nL2\\nL3'; echo \"iterations: $n\"",
      );
    });

    it("group here-doc stdin survives a pipeline", async () => {
      const env = await setupFiles(testDir, {});
      await compareOutputs(
        env,
        testDir,
        '{ read a; true | true; read b; echo "a=[$a] b=[$b]"; } <<EOT\nL1\nL2\nEOT\n',
      );
    });
  });

  describe("stage wiring", () => {
    it("first stage reads the shell's stdin", async () => {
      const env = await setupFiles(testDir, { "loop.txt": THREE_LINES });
      await compareOutputs(env, testDir, "{ cat | tr a-z A-Z; } < loop.txt");
    });

    it("a read in the first stage advances the shared position", async () => {
      const env = await setupFiles(testDir, { "loop.txt": THREE_LINES });
      await compareOutputs(
        env,
        testDir,
        '{ read x | cat; read b; echo "b=[$b]"; } < loop.txt',
      );
    });

    it("a later stage takes the pipe, not the shell's stdin", async () => {
      const env = await setupFiles(testDir, { "loop.txt": THREE_LINES });
      await compareOutputs(env, testDir, "{ echo A | cat; } < loop.txt");
    });

    it("a later stage fed empty output does not fall back to the shell's stdin", async () => {
      const env = await setupFiles(testDir, { "loop.txt": THREE_LINES });
      await compareOutputs(
        env,
        testDir,
        '{ true | cat; echo "---"; read b; echo "b=[$b]"; } < loop.txt',
      );
    });

    it("a non-matching grep does not leak the shell's stdin to the next stage", async () => {
      const env = await setupFiles(testDir, { "loop.txt": THREE_LINES });
      await compareOutputs(
        env,
        testDir,
        '{ echo abc | grep zzz | head -1; echo "---"; } < loop.txt',
      );
    });

    it("a read in a non-first stage leaves the shell's stdin alone", async () => {
      const env = await setupFiles(testDir, { "loop.txt": "L1\nL2\n" });
      await compareOutputs(
        env,
        testDir,
        '{ echo piped | read v; echo "v=[$v]"; read b; echo "b=[$b]"; } < loop.txt',
      );
    });

    it("a stage that exits still hands the position back", async () => {
      const env = await setupFiles(testDir, { "loop.txt": THREE_LINES });
      await compareOutputs(
        env,
        testDir,
        '{ read a; (exit 3) | true; read b; echo "a=[$a] b=[$b]"; } < loop.txt',
      );
    });

    it("PIPESTATUS is unaffected by the stdin wiring", async () => {
      const env = await setupFiles(testDir, { "loop.txt": THREE_LINES });
      await compareOutputs(
        env,
        testDir,
        '{ read a; false | true | false; echo "${PIPESTATUS[*]}"; read b; echo "b=[$b]"; } < loop.txt',
      );
    });
  });

  describe("loops after a pipeline", () => {
    it("a read loop after a pipeline sees every remaining line", async () => {
      const env = await setupFiles(testDir, { "loop.txt": "L1\nL2\nL3\nL4\n" });
      await compareOutputs(
        env,
        testDir,
        '{ echo start | cat >/dev/null; while read l; do echo "L:$l"; done; } < loop.txt',
      );
    });

    it("a pipeline in the loop condition does not drain the loop's stdin", async () => {
      const env = await setupFiles(testDir, { "loop.txt": THREE_LINES });
      await compareOutputs(
        env,
        testDir,
        'n=0; while echo y | grep -q y && read a; do n=$((n+1)); done < loop.txt; echo "iterations: $n"',
      );
    });

    it("nested read loops over separate files keep their own positions", async () => {
      const env = await setupFiles(testDir, {
        "outer.txt": "A\nB\n",
        "inner.txt": "1\n2\n",
      });
      await compareOutputs(
        env,
        testDir,
        'while read o; do while read i; do echo "$o$i"; echo x | cat >/dev/null; done < inner.txt; done < outer.txt',
      );
    });
  });
});
