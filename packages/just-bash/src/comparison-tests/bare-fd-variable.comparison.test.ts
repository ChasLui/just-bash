import { afterEach, beforeEach, describe, it } from "vitest";
import {
  cleanupTestDir,
  compareOutputs,
  createTestDir,
  setupFiles,
} from "./fixture-runner.js";

/** Locked because macOS Bash 3.2 predates `{var}` descriptor allocation. */
describe("bare fd-variable redirections - GNU Bash Comparison", () => {
  let testDirectory: string;

  beforeEach(async () => {
    testDirectory = await createTestDir();
  });

  afterEach(async () => {
    await cleanupTestDir(testDirectory);
  });

  it("creates the target with a command-scoped descriptor", async () => {
    const env = await setupFiles(testDirectory, {});
    await compareOutputs(
      env,
      testDirectory,
      '{output}>out.txt; printf "value=[%s] fd10=" "$output"; if : 2>/dev/null >&10; then echo open; else echo closed; fi; test -f out.txt && echo exists; test ! -s out.txt && echo empty',
    );
  });

  it("reuses fd 10 for a following named allocation", async () => {
    const env = await setupFiles(testDirectory, {});
    await compareOutputs(
      env,
      testDirectory,
      '{bare}>bare.txt; : {named}>named.txt; printf "bare=[%s] named=%s\n" "$bare" "$named"',
    );
  });

  it("scopes a bare heredoc descriptor variable", async () => {
    const env = await setupFiles(testDirectory, {});
    await compareOutputs(
      env,
      testDirectory,
      '{input}<<EOF 3<&$input\nvalue\nEOF\nprintf "input=[%s] status=%s\\n" "$input" "$?"',
    );
  });

  it("keeps a named-command heredoc descriptor available", async () => {
    const env = await setupFiles(testDirectory, {});
    await compareOutputs(
      env,
      testDirectory,
      ': {input}<<EOF\nvalue\nEOF\nread -u "$input" line; printf "input=%s fd=%s\\n" "$line" "$input"',
    );
  });
});
