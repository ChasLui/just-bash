import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

/**
 * `command` and `builtin` re-enter dispatch with the *same* stdin, so fd-0
 * ownership has to travel with it. Without the forward, `command eval '…' <
 * empty-file` looked unredirected to the wrapped `eval`, which then read the
 * enclosing shell's stdin instead of seeing EOF — and a loop body shielded
 * that way lost a line per turn.
 *
 * Every expectation below was checked against GNU bash 3.2.57.
 */

const FIVE_LINES = "L1\nL2\nL3\nL4\nL5\n";
const OTHER = "O1\nO2\nO3\n";
const READ_X = "'read x; echo \"x=[$x]\"'";

function makeBash(): Bash {
  return new Bash({
    files: { "/loop.txt": FIVE_LINES, "/other.txt": OTHER, "/empty.txt": "" },
    cwd: "/",
  });
}

describe("stdin ownership survives the command/builtin wrappers", () => {
  const wrappers = ["command", "builtin"] as const;

  for (const wrapper of wrappers) {
    it(`\`${wrapper} eval\` with an empty file reads EOF, not the shell's stdin`, async () => {
      const result = await makeBash().exec(
        `{ ${wrapper} eval ${READ_X} < /empty.txt; ` +
          'read y; echo "y=[$y]"; } < /loop.txt',
      );
      expect(result.stdout).toBe("x=[]\ny=[L1]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it(`\`${wrapper} eval\` with /dev/null reads EOF, not the shell's stdin`, async () => {
      const result = await makeBash().exec(
        `{ ${wrapper} eval ${READ_X} < /dev/null; ` +
          'read y; echo "y=[$y]"; } < /loop.txt',
      );
      expect(result.stdout).toBe("x=[]\ny=[L1]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it(`\`${wrapper} eval\` with a file of its own leaves the shell's position`, async () => {
      const result = await makeBash().exec(
        `{ ${wrapper} eval ${READ_X} < /other.txt; ` +
          'read y; echo "y=[$y]"; } < /loop.txt',
      );
      expect(result.stdout).toBe("x=[O1]\ny=[L1]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it(`\`${wrapper} eval\` without a redirection shares the shell's position`, async () => {
      const result = await makeBash().exec(
        `{ ${wrapper} eval ${READ_X}; read y; echo "y=[$y]"; } < /loop.txt`,
      );
      expect(result.stdout).toBe("x=[L1]\ny=[L2]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it(`a loop body shielded with \`${wrapper} eval … < /dev/null\` runs once per line`, async () => {
      const result = await makeBash().exec(
        `n=0; while read q; do n=$((n+1)); ${wrapper} eval 'read x' < /dev/null; ` +
          'done < /loop.txt; echo "iterations: $n"',
      );
      expect(result.stdout).toBe("iterations: 5\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  }

  it("`command read` without a redirection still reads the shell's stdin", async () => {
    const result = await makeBash().exec(
      '{ command read x; echo "x=[$x]"; read y; echo "y=[$y]"; } < /loop.txt',
    );
    expect(result.stdout).toBe("x=[L1]\ny=[L2]\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});
