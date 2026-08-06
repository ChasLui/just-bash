import { afterEach, beforeEach, describe, it } from "vitest";
import {
  cleanupTestDir,
  compareOutputs,
  createTestDir,
  setupFiles,
} from "./fixture-runner.js";

/**
 * Numeric file descriptors (issue #321) against recorded real-bash output.
 *
 * Covers the input side (`N< file`, `read -u N`, `read <&N`, `done N< file`),
 * the output side (`N> file`, `N>> file`, `>&N`) and descriptor lifetime
 * (`exec` persistence vs. command scoping, `N<&-`).
 *
 * Three fixtures are marked `"locked": true` — the ones whose recorded bash
 * run writes a diagnostic to stderr ("Bad file descriptor" for descriptors 9
 * and 5, "No such file or directory" for `exec 3< missing.txt`). Only the
 * argv0 and `line N:` prefix of those messages differs between bash 3.2 and
 * bash 5.x, but re-recording on another platform would rewrite the fixture
 * file and fail the workflow's no-diff check. The compared fields (stdout
 * and exit code) are identical across versions.
 */
describe("numeric file descriptors - Real Bash Comparison", () => {
  let testDir: string;

  beforeEach(async () => {
    testDir = await createTestDir();
  });

  afterEach(async () => {
    await cleanupTestDir(testDir);
  });

  const files = {
    "three.txt": "one\ntwo\nthree\n",
    "two.txt": "alpha\nbeta\n",
    "existing.txt": "one\n",
  };

  it("iterates with read -u 3 over done 3< file", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      'n=0; while read -u 3 line; do n=$((n+1)); echo "got: $line"; done 3< three.txt; echo "iterations: $n"',
    );
  });

  it("reads successive lines through <&3 after exec 3< file", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      'exec 3< two.txt; read line <&3; echo "1:$line"; read line <&3; echo "2:$line"; read line <&3; echo "3:rc=$? [$line]"',
    );
  });

  it("iterates with read <&3 over done 3< file", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      'cnt=0; while IFS= read -r x <&3; do cnt=$((cnt+1)); done 3< two.txt; echo "$cnt"',
    );
  });

  it("closes a descriptor with exec 3<&-", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      'exec 3< two.txt; read -u 3 a; echo "a=$a"; exec 3<&-; read -u 3 b 2>/dev/null; echo "rc=$? b=$b"',
    );
  });

  it("treats closing an unopened descriptor as success", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      'exec 3<&-; echo "exec=$?"; true 3<&-; echo "cmd=$?"',
    );
  });

  it("keeps a command-scoped descriptor out of later statements", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      'read -u 3 x 3< three.txt; echo "x=$x"; read -u 3 y 2>/dev/null; echo "rc=$? y=$y"',
    );
  });

  it("restores an outer descriptor after a nested loop reuses the number", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      'exec 3< three.txt; read -u 3 first; echo "first=$first"; while read -u 3 l; do echo "loop:$l"; done 3< two.txt; read -u 3 second; echo "second=$second"; exec 3<&-',
    );
  });

  it("shares a descriptor across the statements of a group", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      '{ read -u 3 a; read -u 3 b; echo "a=$a b=$b"; } 3< three.txt',
    );
  });

  it("shares a descriptor across the iterations of a for loop", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      'for i in 1 2; do read -u 3 v; echo "i=$i v=$v"; done 3< three.txt',
    );
  });

  it("opens a descriptor for a function call", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      'fn() { read -u 3 a; read -u 3 b; echo "a=$a b=$b"; }; fn 3< three.txt',
    );
  });

  it("keeps loop stdin and a numeric descriptor independent", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      'while read -u 3 fd; do read line; echo "fd=$fd stdin=$line"; done 3< two.txt < three.txt',
    );
  });

  it("drains the descriptor when a command consumes all of it", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      'exec 3< three.txt; read -u 3 first; echo "first=$first"; cat <&3; read -u 3 rest 2>/dev/null; echo "rc=$? rest=$rest"',
    );
  });

  it("writes through a descriptor opened with exec 4>", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      "exec 4> out.txt; echo hello >&4; echo world >&4; exec 4>&-; cat out.txt",
    );
  });

  it("appends through a descriptor opened with exec 4>>", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      "exec 4>> existing.txt; echo two >&4; exec 4>&-; cat existing.txt",
    );
  });

  it("opens and writes a command-scoped output descriptor in one list", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      'echo direct 5> five.txt >&5; echo "rc=$?"; cat five.txt',
    );
  });

  it("closes a command-scoped output descriptor after the command", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      'true 9> nine.txt; echo world >&9 2>/dev/null; echo "rc=$?"; cat nine.txt',
    );
  });

  it("routes a loop's output through done 7> file", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      'for i in 1 2; do echo "line $i" >&7; done 7> loop.txt; cat loop.txt',
    );
  });

  it("reports a bad descriptor for >&N after the descriptor is closed", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      'exec 5> five.txt; echo hello >&5; exec 5>&-; echo world >&5 2>/dev/null; echo "rc=$?"; cat five.txt',
    );
  });

  it("reports a missing file for exec 3< missing without exiting", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      'exec 3< missing.txt 2>/dev/null; echo "rc=$?"; echo still-running',
    );
  });

  it("reads a here-doc attached to a numeric descriptor", async () => {
    const env = await setupFiles(testDir, files);
    await compareOutputs(
      env,
      testDir,
      "read -u 3 3<<EOF\nhi\nEOF\necho reply=$REPLY",
    );
  });
});
