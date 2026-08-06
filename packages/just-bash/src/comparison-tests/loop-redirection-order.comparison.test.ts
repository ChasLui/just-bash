import { afterEach, beforeEach, describe, it } from "vitest";
import {
  cleanupTestDir,
  compareOutputs,
  createTestDir,
  setupFiles,
} from "./fixture-runner.js";

/**
 * Left-to-right processing of a loop's redirection list: a failing redirection
 * leaves every earlier one already applied.
 *
 * Each case prints the state of its targets so the file effects land in the
 * compared stdout. Recorded outputs are identical on bash 3.2.57 and 5.3.15,
 * which were both measured before these expectations were written.
 *
 * Nine of the ten fixtures here are locked. Those cases let a redirection
 * FAIL, so bash writes a diagnostic, and the recorded stderr carries bash's
 * shell path and "line N:" script-position prefix. Both move between bash
 * versions — re-recording this file against bash 5.3.15 rewrote every one of
 * them (`/bin/bash: nosuch:` → `/opt/homebrew/bin/bash: line 1: nosuch:`,
 * `line 1:` → `line 2:`) while leaving stdout and exit code byte-identical.
 * The runner compares only stdout and exit code, but it still records stderr,
 * and the comparison-tests workflow fails on any git diff after CI re-records
 * on ubuntu bash 5.x — so locking is what keeps that green, and it costs no
 * coverage.
 *
 * The tenth fixture (`625853946873d829`, the `2> e.txt` case) is deliberately
 * NOT locked: its diagnostic goes into the redirect target rather than to
 * stderr, so its recorded stderr is empty and stable.
 */
describe("loop redirection order - Real Bash Comparison", () => {
  let testDir: string;

  beforeEach(async () => {
    testDir = await createTestDir();
  });

  afterEach(async () => {
    await cleanupTestDir(testDir);
  });

  it("should truncate a > target that precedes a failing input redirect", async () => {
    const env = await setupFiles(testDir, { "o.txt": "keep\n" });
    await compareOutputs(
      env,
      testDir,
      [
        "while true; do echo x; break; done > o.txt < nosuch",
        'echo "rc=$?"',
        'echo "o=[$(cat o.txt)]"',
      ].join("\n"),
    );
  });

  it("should truncate a > target preceding a failing input on until loops", async () => {
    const env = await setupFiles(testDir, { "o.txt": "keep\n" });
    await compareOutputs(
      env,
      testDir,
      [
        "until false; do echo u; break; done > o.txt < nosuch",
        'echo "rc=$?"',
        'echo "o=[$(cat o.txt)]"',
      ].join("\n"),
    );
  });

  it("should keep the contents of an earlier >> target", async () => {
    const env = await setupFiles(testDir, { "o.txt": "keep\n" });
    await compareOutputs(
      env,
      testDir,
      [
        "while true; do echo x; break; done >> o.txt < nosuch",
        'echo "rc=$?"',
        'echo "o=[$(cat o.txt)]"',
      ].join("\n"),
    );
  });

  it("should create a missing >> target before failing", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(
      env,
      testDir,
      [
        "while true; do echo x; break; done >> o.txt < nosuch",
        'echo "rc=$?"',
        'echo "exists=$([ -e o.txt ] && echo yes || echo no) o=[$(cat o.txt)]"',
      ].join("\n"),
    );
  });

  it("should apply only the outputs left of a failing input", async () => {
    const env = await setupFiles(testDir, {
      "a.txt": "keepa\n",
      "b.txt": "keepb\n",
    });
    await compareOutputs(
      env,
      testDir,
      [
        "while true; do echo x; break; done > a.txt < nosuch > b.txt",
        'echo "rc=$?"',
        'echo "a=[$(cat a.txt)] b=[$(cat b.txt)]"',
      ].join("\n"),
    );
  });

  it("should create only the target left of a failing input", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(
      env,
      testDir,
      [
        "while true; do echo x; break; done > a.txt < nosuch > b.txt",
        'echo "rc=$?"',
        'echo "a=$([ -e a.txt ] && echo yes || echo no) b=$([ -e b.txt ] && echo yes || echo no)"',
      ].join("\n"),
    );
  });

  // Compares the diagnostic's PRESENCE in the file, not its text: the message
  // body ("No such file or directory") is stable across bash versions but its
  // "line N:" prefix is not.
  it("should route the diagnostic through an earlier 2>", async () => {
    const env = await setupFiles(testDir, { "e.txt": "keep\n" });
    await compareOutputs(
      env,
      testDir,
      [
        "while true; do echo x; break; done 2> e.txt < nosuch",
        'echo "rc=$?"',
        "echo \"hits=$(grep -c 'No such file or directory' e.txt)\"",
      ].join("\n"),
    );
  });

  it("should expand an earlier target exactly once", async () => {
    const env = await setupFiles(testDir, {});
    await compareOutputs(
      env,
      testDir,
      [
        "n=0",
        'while true; do echo x; break; done > "s$((n++)).txt" < nosuch',
        'echo "rc=$? n=$n"',
        'echo "s0=$([ -e s0.txt ] && echo yes || echo no) s1=$([ -e s1.txt ] && echo yes || echo no)"',
      ].join("\n"),
    );
  });

  it("should leave a > target that follows a failing input untouched", async () => {
    const env = await setupFiles(testDir, { "o.txt": "keep\n" });
    await compareOutputs(
      env,
      testDir,
      [
        "while true; do echo x; break; done < nosuch > o.txt",
        'echo "rc=$?"',
        'echo "o=[$(cat o.txt)]"',
      ].join("\n"),
    );
  });

  it("should apply an earlier output redirect around a here-doc", async () => {
    const env = await setupFiles(testDir, { "o.txt": "keep\n" });
    await compareOutputs(
      env,
      testDir,
      [
        'while read l; do echo "h:$l"; done > o.txt <<EOF < nosuch',
        "alpha",
        "EOF",
        'echo "rc=$?"',
        'echo "o=[$(cat o.txt)]"',
      ].join("\n"),
    );
  });
});
