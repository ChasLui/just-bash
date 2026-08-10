import { afterEach, beforeEach, describe, it } from "vitest";
import {
  cleanupTestDir,
  compareOutputs,
  createTestDir,
  setupFiles,
} from "./fixture-runner.js";

/**
 * Process substitution comparison tests.
 *
 * Cases deliberately avoid anything bash makes non-deterministic or
 * platform-specific:
 * - `>(cmd)` writers run asynchronously in bash, so their output can interleave
 *   with the shell's in any order; only their *effect* is compared here.
 * - `wc` pads its counts differently across platforms, so counts go through
 *   `tr -d ' '` or `grep -c`.
 * - `diff`'s report format differs between implementations, so only its exit
 *   status is compared.
 */
describe("Process Substitution - Real Bash Comparison", () => {
  let testDir: string;

  beforeEach(async () => {
    testDir = await createTestDir();
  });

  afterEach(async () => {
    await cleanupTestDir(testDir);
  });

  it("feeds a command from <(cmd)", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "cat <(echo hi)");
  });

  it("substitutes a /dev/fd path", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "echo <(true)");
  });

  it("numbers multiple substitutions downwards", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "echo <(true) <(true) <(true)");
  });

  it("passes several substitutions to one command", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(
      env,
      testDir,
      "cat <(echo one) <(echo two) <(echo three)",
    );
  });

  it("supports the comm idiom", async () => {
    const env = await setupFiles(testDir, {
      "x.txt": "b\na\nb\n",
      "y.txt": "c\nb\n",
    });
    await compareOutputs(
      env,
      testDir,
      "comm -12 <(sort -u x.txt) <(sort -u y.txt)",
    );
  });

  it("supports the diff idiom for equal inputs", async () => {
    const env = await setupFiles(testDir, {
      "a.txt": "b\na\n",
      "b.txt": "a\nb\n",
    });
    await compareOutputs(
      env,
      testDir,
      "diff <(sort a.txt) <(sort b.txt) > /dev/null; echo rc=$?",
    );
  });

  it("supports the diff idiom for differing inputs", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(
      env,
      testDir,
      "diff <(echo a) <(echo b) > /dev/null; echo rc=$?",
    );
  });

  it("supports the paste idiom", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(
      env,
      testDir,
      "paste <(printf 'a\\nb\\n') <(printf '1\\n2\\n')",
    );
  });

  it("reads a substitution through an input redirection", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "cat < <(printf 'a\\nb\\n')");
  });

  it("feeds a while-read loop", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(
      env,
      testDir,
      "while read -r l; do echo \"got $l\"; done < <(printf '1\\n2\\n')",
    );
  });

  it("works inside a pipeline", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "cat <(echo hi) | tr a-z A-Z");
  });

  it("works in a non-first pipeline stage", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(
      env,
      testDir,
      "echo ignored | grep -c . <(printf 'a\\nb\\n')",
    );
  });

  it("nests", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "cat <(cat <(echo deep))");
  });

  it("handles a case-pattern closing parenthesis in the body", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(
      env,
      testDir,
      "cat <(case x in x) echo case-ok;; esac)",
    );
  });

  it("handles a closing parenthesis in a heredoc body", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "cat <(cat <<EOF\n)\nEOF\n)");
  });

  it("runs a multi-command body", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "cat <(echo a; echo b)");
  });

  it("preserves a body's missing trailing newline", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "wc -c < <(printf 'abc') | tr -d ' '");
  });

  it("does not let a failing body change the outer status", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "cat <(false); echo rc=$?");
  });

  it("keeps the outer command's own status", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "grep -q zzz <(echo hi); echo rc=$?");
  });

  it("contains exit inside the body", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "cat <(echo a; exit 3); echo rc=$?");
  });

  it("runs the body in a subshell", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(
      env,
      testDir,
      "x=1; cat <(x=2; echo $x); echo after=$x",
    );
  });

  it("sees the caller's variables", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "v=hello; cat <(echo $v)");
  });

  it("concatenates with an adjacent literal", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "echo a<(true)");
  });

  it("concatenates with adjacent literals on both sides", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "echo a<(true)c");
  });

  it("concatenates adjacent input and output substitutions", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "echo <(true)>(cat)");
  });

  it("does not read a leading digit as a file descriptor", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "echo 2>(true)");
  });

  it("stays literal inside quotes", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "echo '<(echo hi)' \"<(echo hi)\"");
  });

  it("leaves > alone inside arithmetic", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "(( 3 > (1) )) && echo yes");
  });

  it("leaves < alone in a C-style for header", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(
      env,
      testDir,
      "for (( i=0; i<(2); i++ )); do echo $i; done",
    );
  });

  it("keeps a spaced < as a redirection", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "cat < (echo hi)", {
      compareStderr: false,
    });
  });

  it("pipes what a command wrote into >(cmd)", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "echo hi > >(tr a-z A-Z)");
  });

  it("supports the tee idiom", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(
      env,
      testDir,
      "printf 'a\\nb\\n' | tee >(grep -c b) > /dev/null",
    );
  });

  it("drives several writers from one command", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(
      env,
      testDir,
      "printf 'x\\n' | tee >(cat) >(cat) > /dev/null",
    );
  });

  it("combines an input and an output substitution", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "cat <(echo in) > >(sed 's/in/OUT/')");
  });

  it("does not let a failing writer change the outer status", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(env, testDir, "echo hi > >(false); echo rc=$?");
  });
});
