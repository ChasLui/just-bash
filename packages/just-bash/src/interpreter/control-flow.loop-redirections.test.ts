import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

/**
 * Loop-level redirections (`while ... done > file`, `until ... done 2> file`).
 *
 * Bash installs these before the loop runs and keeps them in effect for the
 * condition and every iteration. Every expectation here was verified against
 * real bash before being written down.
 */
describe("while/until loop-level redirections", () => {
  describe("stdout redirection", () => {
    it("should discard loop output with >/dev/null", async () => {
      const env = new Bash();
      const result = await env.exec(
        `while true; do echo x; break; done >/dev/null; echo end`,
      );
      expect(result.stdout).toBe("end\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should write while-loop output to a file", async () => {
      const env = new Bash();
      const result = await env.exec(`
        i=0
        while [ $i -lt 3 ]; do echo "line$i"; i=$((i + 1)); done > out.txt
        echo "---"
        cat out.txt
      `);
      expect(result.stdout).toBe("---\nline0\nline1\nline2\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should append while-loop output with >>", async () => {
      const env = new Bash();
      const result = await env.exec(`
        echo pre > out.txt
        i=0
        while [ $i -lt 2 ]; do echo "l$i"; i=$((i + 1)); done >> out.txt
        cat out.txt
      `);
      expect(result.stdout).toBe("pre\nl0\nl1\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should write until-loop output to a file", async () => {
      const env = new Bash();
      const result = await env.exec(`
        i=0
        until [ $i -ge 2 ]; do echo "u$i"; i=$((i + 1)); done > out.txt
        echo "---"
        cat out.txt
      `);
      expect(result.stdout).toBe("---\nu0\nu1\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should redirect output produced by the loop condition", async () => {
      const env = new Bash();
      const result = await env.exec(`
        i=0
        while echo "cond$i"; [ $i -lt 1 ]; do echo "body$i"; i=$((i + 1)); done > out.txt
        echo "---"
        cat out.txt
      `);
      expect(result.stdout).toBe("---\ncond0\nbody0\ncond1\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  });

  describe("stderr redirection", () => {
    it("should send loop stderr to a file with 2>", async () => {
      const env = new Bash();
      const result = await env.exec(`
        i=0
        while [ $i -lt 2 ]; do echo "o$i"; echo "e$i" >&2; i=$((i + 1)); done 2> err.txt
        echo "---"
        cat err.txt
      `);
      expect(result.stdout).toBe("o0\no1\n---\ne0\ne1\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should merge stderr into the file with > out 2>&1", async () => {
      const env = new Bash();
      const result = await env.exec(`
        while true; do echo o; echo e >&2; break; done > out.txt 2>&1
        cat out.txt
      `);
      expect(result.stdout).toBe("o\ne\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should keep stderr on the caller's stdout with 2>&1 > out", async () => {
      const env = new Bash();
      const result = await env.exec(`
        while true; do echo o; echo e >&2; break; done 2>&1 > out.txt
        echo "---"
        cat out.txt
      `);
      expect(result.stdout).toBe("e\n---\no\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should send both streams to a file with &>", async () => {
      const env = new Bash();
      const result = await env.exec(`
        until false; do echo o; echo e >&2; break; done &> both.txt
        cat both.txt
      `);
      expect(result.stdout).toBe("o\ne\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  });

  describe("combined with input redirection", () => {
    it("should read from a file and write to another (while)", async () => {
      const env = new Bash({ files: { "/in.txt": "a\nb\n" } });
      const result = await env.exec(`
        while read l; do echo "[$l]"; done < /in.txt > out.txt
        cat out.txt
      `);
      expect(result.stdout).toBe("[a]\n[b]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should read from a file and write to another (until)", async () => {
      const env = new Bash({ files: { "/in.txt": "c\nd\n" } });
      const result = await env.exec(`
        until ! read l; do echo "<$l>"; done < /in.txt > out.txt
        cat out.txt
      `);
      expect(result.stdout).toBe("<c>\n<d>\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should keep plain `done < file` working", async () => {
      const env = new Bash({ files: { "/in.txt": "one\ntwo\n" } });
      const result = await env.exec(
        `while read l; do echo "got:$l"; done < /in.txt`,
      );
      expect(result.stdout).toBe("got:one\ngot:two\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should redirect a here-doc-fed loop", async () => {
      const env = new Bash();
      const result = await env.exec(`
        while read l; do echo "h:$l"; done > out.txt <<EOF
alpha
beta
EOF
        cat out.txt
      `);
      expect(result.stdout).toBe("h:alpha\nh:beta\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should redirect a here-string-fed loop", async () => {
      const env = new Bash();
      const result = await env.exec(`
        while read l; do echo "s:$l"; done <<< "hello" > out.txt
        cat out.txt
      `);
      expect(result.stdout).toBe("s:hello\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should not glob-expand a here-string target", async () => {
      const env = new Bash({ files: { "/aglob1": "", "/aglob2": "" } });
      const result = await env.exec(
        `cd / && while read l; do echo "s:[$l]"; done <<< *`,
      );
      expect(result.stdout).toBe("s:[*]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should fail without truncating a later output target", async () => {
      const env = new Bash({ files: { "/out.txt": "keep\n" } });
      const result = await env.exec(`
        while read l; do echo "$l"; done < /nosuch.txt > /out.txt
        echo "rc=$?"
        cat /out.txt
      `);
      expect(result.stdout).toBe("rc=1\nkeep\n");
      expect(result.stderr).toBe(
        "bash: /nosuch.txt: No such file or directory\n",
      );
      expect(result.exitCode).toBe(0);
    });

    it("should report a missing input file for until loops", async () => {
      const env = new Bash();
      const result = await env.exec(`
        until ! read l; do echo "$l"; done < /nosuch.txt
        echo "rc=$?"
      `);
      expect(result.stdout).toBe("rc=1\n");
      expect(result.stderr).toBe(
        "bash: /nosuch.txt: No such file or directory\n",
      );
      expect(result.exitCode).toBe(0);
    });
  });
});
