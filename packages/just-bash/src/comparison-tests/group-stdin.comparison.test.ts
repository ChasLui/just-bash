import { afterEach, beforeEach, describe, it } from "vitest";
import {
  cleanupTestDir,
  compareOutputs,
  createTestDir,
  setupFiles,
} from "./fixture-runner.js";

/**
 * Groups, functions and `eval` restore only the stdin they actually replaced.
 * Recorded against GNU bash 3.2.57. Every command redirects stdin at its
 * outermost level so recording never blocks on the recorder's own stdin.
 */

const FIVE_LINES = { "loop.txt": "L1\nL2\nL3\nL4\nL5\n" };
const TWO_FILES = { ...FIVE_LINES, "other.txt": "O1\nO2\nO3\n" };

describe("group/function/eval stdin - Real Bash Comparison", () => {
  let testDir: string;

  beforeEach(async () => {
    testDir = await createTestDir();
  });

  afterEach(async () => {
    await cleanupTestDir(testDir);
  });

  it("an inner group's read advances the shared position", async () => {
    const env = await setupFiles(testDir, FIVE_LINES);
    await compareOutputs(
      env,
      testDir,
      '{ { read a; }; read b; echo "a=[$a] b=[$b]"; } < loop.txt',
    );
  });

  it("three nested groups still share one position", async () => {
    const env = await setupFiles(testDir, FIVE_LINES);
    await compareOutputs(
      env,
      testDir,
      '{ { { read a; }; }; read b; echo "a=[$a] b=[$b]"; } < loop.txt',
    );
  });

  it("reads inside and outside an inner group interleave", async () => {
    const env = await setupFiles(testDir, FIVE_LINES);
    await compareOutputs(
      env,
      testDir,
      '{ { { read a; }; read c; }; read b; echo "a=[$a] c=[$c] b=[$b]"; } < loop.txt',
    );
  });

  it("a group that read to EOF leaves the next read at EOF", async () => {
    const env = await setupFiles(testDir, FIVE_LINES);
    await compareOutputs(
      env,
      testDir,
      "{ { read a; read c; read d; read e; read f; }; " +
        'read b; rc=$?; echo "b=[$b] rc=$rc"; } < loop.txt',
    );
  });

  it("an output redirection does not give the group its own stdin", async () => {
    const env = await setupFiles(testDir, FIVE_LINES);
    await compareOutputs(
      env,
      testDir,
      '{ { read a; } >/dev/null; read b; echo "a=[$a] b=[$b]"; } < loop.txt',
    );
  });

  it("cat after an inner group starts where the group stopped", async () => {
    const env = await setupFiles(testDir, FIVE_LINES);
    await compareOutputs(env, testDir, "{ { read a; }; cat; } < loop.txt");
  });

  it("`< file` on the inner group leaves the outer position untouched", async () => {
    const env = await setupFiles(testDir, TWO_FILES);
    await compareOutputs(
      env,
      testDir,
      '{ read a; { read x; echo "x=[$x]"; } < other.txt; ' +
        'read b; echo "a=[$a] b=[$b]"; } < loop.txt',
    );
  });

  it("an empty file on the inner group means EOF inside", async () => {
    const env = await setupFiles(testDir, { ...FIVE_LINES, "empty.txt": "" });
    await compareOutputs(
      env,
      testDir,
      '{ read a; { read x; echo "x=[$x]"; } < empty.txt; ' +
        'read b; echo "a=[$a] b=[$b]"; } < loop.txt',
    );
  });

  it("a heredoc on the inner group leaves the outer position untouched", async () => {
    const env = await setupFiles(testDir, FIVE_LINES);
    await compareOutputs(
      env,
      testDir,
      '{ { read a; echo "a=[$a]"; } <<EOT\nH1\nEOT\n' +
        'read b; echo "b=[$b]"; } < loop.txt',
    );
  });

  it("a here-string on the inner group leaves the outer position untouched", async () => {
    const env = await setupFiles(testDir, FIVE_LINES);
    await compareOutputs(
      env,
      testDir,
      '{ { read a; echo "a=[$a]"; } <<< "S1"; read b; echo "b=[$b]"; } < loop.txt',
    );
  });

  it("a heredoc on the outer group is the shared position", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(
      env,
      testDir,
      '{ { read a; }; read b; echo "a=[$a] b=[$b]"; } <<EOT\nH1\nH2\nEOT',
    );
  });

  it("a function that reads leaves the caller at the next line", async () => {
    const env = await setupFiles(testDir, FIVE_LINES);
    await compareOutputs(
      env,
      testDir,
      'f() { read a; echo "in=[$a]"; }; ' +
        '{ f; f; read b; echo "b=[$b]"; } < loop.txt',
    );
  });

  it("a redirection on the function definition gives it its own stdin", async () => {
    const env = await setupFiles(testDir, TWO_FILES);
    await compareOutputs(
      env,
      testDir,
      'f() { read a; echo "in=[$a]"; } < other.txt; ' +
        '{ f; read b; echo "b=[$b]"; } < loop.txt',
    );
  });

  it("a redirection on the call gives the function its own stdin", async () => {
    const env = await setupFiles(testDir, TWO_FILES);
    await compareOutputs(
      env,
      testDir,
      'f() { read a; echo "in=[$a]"; }; ' +
        '{ f < other.txt; read b; echo "b=[$b]"; } < loop.txt',
    );
  });

  it("a loop body calling a reading function consumes two lines per turn", async () => {
    const env = await setupFiles(testDir, FIVE_LINES);
    await compareOutputs(
      env,
      testDir,
      'f() { read b; echo "f got [$b]"; }; ' +
        'while read a; do echo "a=[$a]"; f; done < loop.txt',
    );
  });

  it("a read inside eval advances the shared position", async () => {
    const env = await setupFiles(testDir, FIVE_LINES);
    await compareOutputs(
      env,
      testDir,
      "{ eval 'read a; read c'; read b; " +
        'echo "a=[$a] c=[$c] b=[$b]"; } < loop.txt',
    );
  });

  it("a read loop inside eval sees every line", async () => {
    const env = await setupFiles(testDir, FIVE_LINES);
    await compareOutputs(
      env,
      testDir,
      "{ eval 'while read l; do echo \"E:$l\"; done'; } < loop.txt",
    );
  });

  it("a redirection on eval gives it its own stdin", async () => {
    const env = await setupFiles(testDir, TWO_FILES);
    await compareOutputs(
      env,
      testDir,
      "{ eval 'read a; echo \"a=[$a]\"' < other.txt; " +
        'read b; echo "b=[$b]"; } < loop.txt',
    );
  });

  it("an empty file on the call means EOF inside the function", async () => {
    const env = await setupFiles(testDir, { ...FIVE_LINES, "empty.txt": "" });
    await compareOutputs(
      env,
      testDir,
      'f() { read x; echo "x=[$x]"; }; { read a; f < empty.txt; ' +
        'read b; echo "a=[$a] b=[$b]"; } < loop.txt',
    );
  });

  it("an empty file on the definition means EOF inside the function", async () => {
    const env = await setupFiles(testDir, { ...FIVE_LINES, "empty.txt": "" });
    await compareOutputs(
      env,
      testDir,
      'f() { read x; echo "x=[$x]"; } < empty.txt; { read a; f; ' +
        'read b; echo "a=[$a] b=[$b]"; } < loop.txt',
    );
  });

  it("an empty file on eval means EOF inside eval", async () => {
    const env = await setupFiles(testDir, { ...FIVE_LINES, "empty.txt": "" });
    await compareOutputs(
      env,
      testDir,
      "{ read a; eval 'read x; echo \"x=[$x]\"' < empty.txt; " +
        'read b; echo "a=[$a] b=[$b]"; } < loop.txt',
    );
  });

  it("a loop body shielded with /dev/null still runs once per line", async () => {
    const env = await setupFiles(testDir, FIVE_LINES);
    await compareOutputs(
      env,
      testDir,
      "f() { read q; }; n=0; while read x; do n=$((n+1)); f < /dev/null; " +
        'done < loop.txt; echo "iterations: $n"',
    );
  });

  it("a group in the loop body pairs the lines two at a time", async () => {
    const env = await setupFiles(testDir, FIVE_LINES);
    await compareOutputs(
      env,
      testDir,
      'while read a; do { read b; }; echo "pair=[$a][$b]"; done < loop.txt',
    );
  });

  it("a pipeline inside a group body keeps the loop running per line", async () => {
    const env = await setupFiles(testDir, FIVE_LINES);
    await compareOutputs(
      env,
      testDir,
      "n=0; while read a; do n=$((n+1)); { echo x | cat; } >/dev/null; " +
        'done < loop.txt; echo "iterations: $n"',
    );
  });

  it("a loop with its own stdin leaves the enclosing position alone", async () => {
    const env = await setupFiles(testDir, TWO_FILES);
    await compareOutputs(
      env,
      testDir,
      '{ read a; while read x; do echo "X:$x"; done < other.txt; ' +
        'read b; echo "a=[$a] b=[$b]"; } < loop.txt',
    );
  });
});
