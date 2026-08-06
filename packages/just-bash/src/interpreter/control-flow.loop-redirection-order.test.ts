import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

/**
 * Left-to-right processing of a loop's redirection list.
 *
 * Bash installs loop redirections in order, so a failing one leaves every
 * earlier redirection already applied: `done > out < nosuch` truncates `out`
 * and then fails, while `done < nosuch > out` fails first and never touches
 * `out`. Reported by the Vercel review bot on control-flow.ts:134.
 *
 * Every expectation here was measured against real bash before being written,
 * and is identical on /bin/bash 3.2.57 and /opt/homebrew/bin/bash 5.3.15.
 */
describe("while/until loop redirection order", () => {
  describe("output redirection before a failing input redirection", () => {
    it("should truncate an earlier > target", async () => {
      const env = new Bash({ files: { "/o.txt": "keep\n" } });
      const result = await env.exec(`
        cd /
        while true; do echo x; break; done > o.txt < nosuch
        echo "rc=$?"
        echo "o=[$(cat o.txt)]"
      `);
      expect(result.stdout).toBe("rc=1\no=[]\n");
      expect(result.stderr).toBe("bash: nosuch: No such file or directory\n");
      expect(result.exitCode).toBe(0);
    });

    it("should truncate an earlier > target on until loops", async () => {
      const env = new Bash({ files: { "/o.txt": "keep\n" } });
      const result = await env.exec(`
        cd /
        until false; do echo u; break; done > o.txt < nosuch
        echo "rc=$?"
        echo "o=[$(cat o.txt)]"
      `);
      expect(result.stdout).toBe("rc=1\no=[]\n");
      expect(result.stderr).toBe("bash: nosuch: No such file or directory\n");
      expect(result.exitCode).toBe(0);
    });

    it("should leave an earlier >> target's contents alone", async () => {
      const env = new Bash({ files: { "/o.txt": "keep\n" } });
      const result = await env.exec(`
        cd /
        while true; do echo x; break; done >> o.txt < nosuch
        echo "rc=$?"
        echo "o=[$(cat o.txt)]"
      `);
      expect(result.stdout).toBe("rc=1\no=[keep]\n");
      expect(result.stderr).toBe("bash: nosuch: No such file or directory\n");
      expect(result.exitCode).toBe(0);
    });

    it("should create a missing >> target before failing", async () => {
      const env = new Bash();
      const result = await env.exec(`
        cd /
        while true; do echo x; break; done >> o.txt < nosuch
        echo "rc=$?"
        echo "exists=$([ -e o.txt ] && echo yes || echo no) o=[$(cat o.txt)]"
      `);
      expect(result.stdout).toBe("rc=1\nexists=yes o=[]\n");
      expect(result.stderr).toBe("bash: nosuch: No such file or directory\n");
      expect(result.exitCode).toBe(0);
    });

    it("should expand an earlier target exactly once", async () => {
      const env = new Bash();
      const result = await env.exec(`
        cd /
        n=0
        while true; do echo x; break; done > "s$((n++)).txt" < nosuch
        echo "rc=$? n=$n"
        echo "s0=$([ -e s0.txt ] && echo yes || echo no) s1=$([ -e s1.txt ] && echo yes || echo no)"
      `);
      expect(result.stdout).toBe("rc=1 n=1\ns0=yes s1=no\n");
      expect(result.stderr).toBe("bash: nosuch: No such file or directory\n");
      expect(result.exitCode).toBe(0);
    });

    it("should route the diagnostic through an earlier 2>", async () => {
      const env = new Bash({ files: { "/e.txt": "keep\n" } });
      const result = await env.exec(`
        cd /
        while true; do echo x; break; done 2> e.txt < nosuch
        echo "rc=$?"
        echo "e=[$(cat e.txt)]"
      `);
      expect(result.stdout).toBe(
        "rc=1\ne=[bash: nosuch: No such file or directory]\n",
      );
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  });

  describe("failing input redirection before an output redirection", () => {
    it("should leave a later > target untouched", async () => {
      const env = new Bash({ files: { "/o.txt": "keep\n" } });
      const result = await env.exec(`
        cd /
        while true; do echo x; break; done < nosuch > o.txt
        echo "rc=$?"
        echo "o=[$(cat o.txt)]"
      `);
      expect(result.stdout).toBe("rc=1\no=[keep]\n");
      expect(result.stderr).toBe("bash: nosuch: No such file or directory\n");
      expect(result.exitCode).toBe(0);
    });

    it("should leave a later > target uncreated", async () => {
      const env = new Bash();
      const result = await env.exec(`
        cd /
        until false; do echo u; break; done < nosuch > o.txt
        echo "rc=$?"
        echo "exists=$([ -e o.txt ] && echo yes || echo no)"
      `);
      expect(result.stdout).toBe("rc=1\nexists=no\n");
      expect(result.stderr).toBe("bash: nosuch: No such file or directory\n");
      expect(result.exitCode).toBe(0);
    });
  });

  describe("outputs straddling a failing input redirection", () => {
    it("should apply only the redirections to the left", async () => {
      const env = new Bash({
        files: { "/a.txt": "keepa\n", "/b.txt": "keepb\n" },
      });
      const result = await env.exec(`
        cd /
        while true; do echo x; break; done > a.txt < nosuch > b.txt
        echo "rc=$?"
        echo "a=[$(cat a.txt)] b=[$(cat b.txt)]"
      `);
      expect(result.stdout).toBe("rc=1\na=[] b=[keepb]\n");
      expect(result.stderr).toBe("bash: nosuch: No such file or directory\n");
      expect(result.exitCode).toBe(0);
    });

    it("should create only the target to the left", async () => {
      const env = new Bash();
      const result = await env.exec(`
        cd /
        while true; do echo x; break; done > a.txt < nosuch > b.txt
        echo "rc=$?"
        echo "a=$([ -e a.txt ] && echo yes || echo no) b=$([ -e b.txt ] && echo yes || echo no)"
      `);
      expect(result.stdout).toBe("rc=1\na=yes b=no\n");
      expect(result.stderr).toBe("bash: nosuch: No such file or directory\n");
      expect(result.exitCode).toBe(0);
    });

    it("should apply an earlier output redirect before a failing here-doc-adjacent input", async () => {
      const env = new Bash({ files: { "/o.txt": "keep\n" } });
      const result = await env.exec(`cd /
while read l; do echo "h:$l"; done > o.txt <<EOF < nosuch
alpha
EOF
echo "rc=$?"
echo "o=[$(cat o.txt)]"`);
      expect(result.stdout).toBe("rc=1\no=[]\n");
      expect(result.stderr).toBe("bash: nosuch: No such file or directory\n");
      expect(result.exitCode).toBe(0);
    });
  });

  describe("self-referential redirection", () => {
    it("should truncate before reading with > f < f", async () => {
      const env = new Bash({ files: { "/f.txt": "l1\nl2\n" } });
      const result = await env.exec(`
        cd /
        while read l; do echo "got:$l"; done > f.txt < f.txt
        echo "rc=$?"
        echo "f=[$(cat f.txt)]"
      `);
      expect(result.stdout).toBe("rc=0\nf=[]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  });

  describe("success paths keep working", () => {
    it("should handle > out < in", async () => {
      const env = new Bash({ files: { "/in.txt": "a\nb\n" } });
      const result = await env.exec(`
        cd /
        while read l; do echo "[$l]"; done > out.txt < in.txt
        echo "rc=$?"
        cat out.txt
      `);
      expect(result.stdout).toBe("rc=0\n[a]\n[b]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should handle < in > out", async () => {
      const env = new Bash({ files: { "/in.txt": "c\nd\n" } });
      const result = await env.exec(`
        cd /
        until ! read l; do echo "<$l>"; done < in.txt > out.txt
        echo "rc=$?"
        cat out.txt
      `);
      expect(result.stdout).toBe("rc=0\n<c>\n<d>\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should handle an output redirect on each side of the input", async () => {
      const env = new Bash({ files: { "/in.txt": "e\nf\n" } });
      const result = await env.exec(`
        cd /
        while read l; do echo "{$l}"; done > a.txt < in.txt > b.txt
        echo "rc=$?"
        echo "a=[$(cat a.txt)] b=[$(cat b.txt)]"
      `);
      expect(result.stdout).toBe("rc=0\na=[] b=[{e}\n{f}]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  });
});
