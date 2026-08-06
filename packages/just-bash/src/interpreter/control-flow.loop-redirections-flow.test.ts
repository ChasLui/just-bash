import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

/**
 * Loop-level redirections under multi-level break/continue and inside
 * pipelines. Companion to control-flow.loop-redirections.test.ts; every
 * expectation was verified against real bash before being written.
 */
describe("while/until loop redirections under flow control", () => {
  describe("multi-level break and continue", () => {
    // Only the redirect on the loop that `break 2`/`continue 2` TARGETS is
    // covered here. Breaking out of an inner loop that carries its own
    // redirect still leaks that loop's output to the caller, exactly as it
    // does for `for` — the unwinding error carries the output past
    // applyRedirections.
    it("should redirect the loop targeted by break 2", async () => {
      const env = new Bash();
      const result = await env.exec(`
        while true; do
          while true; do echo inner; break 2; done
          echo "not reached"
        done > out.txt
        cat out.txt
      `);
      expect(result.stdout).toBe("inner\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should redirect the loop targeted by continue 2", async () => {
      const env = new Bash();
      const result = await env.exec(`
        i=0
        while [ $i -lt 2 ]; do
          i=$((i + 1))
          while true; do echo "d$i"; continue 2; done
          echo "not reached"
        done > out.txt
        cat out.txt
      `);
      expect(result.stdout).toBe("d1\nd2\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should redirect the until loop targeted by break 2", async () => {
      const env = new Bash();
      const result = await env.exec(`
        until false; do
          until false; do echo uinner; break 2; done
          echo "not reached"
        done > out.txt
        cat out.txt
      `);
      expect(result.stdout).toBe("uinner\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  });

  describe("piped loops", () => {
    it("should redirect before the pipe", async () => {
      const env = new Bash();
      const result = await env.exec(`
        while true; do echo x; break; done > out.txt | cat
        echo "---"
        cat out.txt
      `);
      expect(result.stdout).toBe("---\nx\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should merge stderr into the pipe with 2>&1", async () => {
      const env = new Bash();
      const result = await env.exec(
        `while true; do echo o; echo e >&2; break; done 2>&1 | cat`,
      );
      expect(result.stdout).toBe("o\ne\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should feed a piped until loop from stdin", async () => {
      const env = new Bash();
      const result = await env.exec(`
        printf 'p1\\np2\\n' | until ! read l; do echo "u:$l"; done > out.txt
        cat out.txt
      `);
      expect(result.stdout).toBe("u:p1\nu:p2\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  });
});
