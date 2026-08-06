import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

/**
 * Input side of user file descriptors (issue #321).
 *
 * `N< file` opens a real descriptor, `read -u N` and `read <&N` read from it,
 * and the descriptor carries ONE shared read position: successive reads
 * continue where the previous one stopped, exactly like a bash file offset.
 */
describe("numeric input file descriptors", () => {
  describe("issue #321 repros", () => {
    it("runs one iteration per line for `while read -u 3 ... done 3< file`", async () => {
      const env = new Bash();
      const result = await env.exec(
        [
          "printf 'one\\ntwo\\nthree\\n' > /tmp/r.txt",
          'n=0; while read -u 3 line; do n=$((n+1)); echo "got: $line"; done 3< /tmp/r.txt',
          'echo "iterations: $n"',
        ].join("\n"),
      );
      expect(result.stdout).toBe(
        "got: one\ngot: two\ngot: three\niterations: 3\n",
      );
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("reads successive lines from `exec 3< file` via `read <&3`", async () => {
      const env = new Bash();
      const result = await env.exec(
        [
          "printf 'alpha\\nbeta\\n' > /tmp/r.txt",
          "exec 3< /tmp/r.txt",
          'echo "exec rc=$?"',
          'read line <&3; echo "read rc=$? line=$line"',
          'read line2 <&3; echo "read2 rc=$? line2=$line2"',
          'read line3 <&3; echo "read3 rc=$? line3=$line3"',
        ].join("\n"),
      );
      expect(result.stdout).toBe(
        "exec rc=0\nread rc=0 line=alpha\nread2 rc=0 line2=beta\nread3 rc=1 line3=\n",
      );
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("iterates `while IFS= read -r x <&3 ... done 3< file` once per line", async () => {
      const env = new Bash();
      const result = await env.exec(
        [
          "printf 'alpha\\nbeta\\n' > /tmp/r.txt",
          "cnt=0; while IFS= read -r x <&3; do cnt=$((cnt+1)); done 3< /tmp/r.txt",
          'echo "$cnt"',
        ].join("\n"),
      );
      expect(result.stdout).toBe("2\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  });

  describe("read -u N", () => {
    it("shares one position across statements after exec", async () => {
      const env = new Bash();
      const result = await env.exec(
        [
          "printf 'a\\nb\\nc\\n' > /tmp/f.txt",
          "exec 3< /tmp/f.txt",
          'read -u 3 p; echo "p=$p"',
          'read -u 3 q; echo "q=$q"',
          'read -u 3 r; echo "r=$r"',
          'read -u 3 s; echo "eof rc=$? s=$s"',
        ].join("\n"),
      );
      expect(result.stdout).toBe("p=a\nq=b\nr=c\neof rc=1 s=\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("errors on a descriptor that is not open", async () => {
      const env = new Bash();
      const result = await env.exec('read -u 3 x; echo "rc=$?"');
      expect(result.stdout).toBe("rc=1\n");
      expect(result.stderr).toBe(
        "bash: read: 3: invalid file descriptor: Bad file descriptor\n",
      );
      expect(result.exitCode).toBe(0);
    });

    it("errors on a descriptor opened for writing", async () => {
      const env = new Bash();
      const result = await env.exec(
        ["exec 3> /tmp/o.txt", 'read -u 3 z; echo "rc=$?"'].join("\n"),
      );
      expect(result.stdout).toBe("rc=1\n");
      expect(result.stderr).toBe(
        "bash: read: read error: 3: Bad file descriptor\n",
      );
      expect(result.exitCode).toBe(0);
    });

    it("reads stdin for -u 0", async () => {
      const env = new Bash();
      const result = await env.exec('read -u 0 v; echo "v=$v"', {
        stdin: "from-stdin\n",
      });
      expect(result.stdout).toBe("v=from-stdin\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("reports a read error for stdout and stderr", async () => {
      const env = new Bash();
      const result = await env.exec(
        ['read -u 1 a; echo "rc1=$?"', 'read -u 2 b; echo "rc2=$?"'].join("\n"),
      );
      expect(result.stdout).toBe("rc1=1\nrc2=1\n");
      expect(result.stderr).toBe(
        "bash: read: read error: 1: Bad file descriptor\n" +
          "bash: read: read error: 2: Bad file descriptor\n",
      );
      expect(result.exitCode).toBe(0);
    });

    it("still reads a here-doc attached to a numeric fd", async () => {
      const env = new Bash();
      const result = await env.exec(
        ["read -u 3 3<<EOF", "hi", "EOF", "echo reply=$REPLY"].join("\n"),
      );
      expect(result.stdout).toBe("reply=hi\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("reads a here-string attached to a numeric fd", async () => {
      const env = new Bash();
      const result = await env.exec('read -u 3 v 3<<< hello; echo "v=$v"');
      expect(result.stdout).toBe("v=hello\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  });

  describe("<&N", () => {
    it("reports Bad file descriptor for an unopened fd and skips the command", async () => {
      const env = new Bash();
      const result = await env.exec('read line <&3; echo "rc=$? line=$line"');
      expect(result.stdout).toBe("rc=1 line=\n");
      expect(result.stderr).toBe("bash: 3: Bad file descriptor\n");
      expect(result.exitCode).toBe(0);
    });

    it("reports Bad file descriptor when the fd is write-only", async () => {
      const env = new Bash();
      const result = await env.exec(
        ["exec 3> /tmp/o.txt", 'read line <&3; echo "rc=$?"'].join("\n"),
      );
      expect(result.stdout).toBe("rc=1\n");
      expect(result.stderr).toBe("bash: 3: Bad file descriptor\n");
      expect(result.exitCode).toBe(0);
    });

    it("drains the descriptor for a command that consumes all of stdin", async () => {
      const env = new Bash();
      const result = await env.exec(
        [
          "printf 'a\\nb\\nc\\n' > /tmp/f.txt",
          "exec 3< /tmp/f.txt",
          'read -u 3 first; echo "first=$first"',
          "cat <&3",
          'read -u 3 rest; echo "rc=$? rest=$rest"',
        ].join("\n"),
      );
      expect(result.stdout).toBe("first=a\nb\nc\nrc=1 rest=\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("reads stdin through a descriptor saved with `exec 3<&0`", async () => {
      const env = new Bash();
      const result = await env.exec(
        [
          "printf 'p\\nq\\n' | {",
          "  exec 3<&0",
          '  read -u 3 a; echo "a=$a"',
          '  read b <&3; echo "b=$b"',
          "}",
        ].join("\n"),
      );
      expect(result.stdout).toBe("a=p\nb=q\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("advances only past the line a `read` consumed", async () => {
      const env = new Bash();
      const result = await env.exec(
        [
          "printf 'a\\nb\\nc\\n' > /tmp/f.txt",
          "exec 3< /tmp/f.txt",
          'read one <&3; echo "one=$one"',
          "wc -l <&3",
        ].join("\n"),
      );
      expect(result.stdout).toBe("one=a\n2\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  });

  describe("closing", () => {
    it("closes with `exec N<&-` and reports the fd as unopened afterwards", async () => {
      const env = new Bash();
      const result = await env.exec(
        [
          "printf 'a\\nb\\n' > /tmp/f.txt",
          "exec 3< /tmp/f.txt",
          'read -u 3 p; echo "p=$p"',
          "exec 3<&-",
          'read -u 3 q; echo "rc=$? q=$q"',
        ].join("\n"),
      );
      expect(result.stdout).toBe("p=a\nrc=1 q=\n");
      expect(result.stderr).toBe(
        "bash: read: 3: invalid file descriptor: Bad file descriptor\n",
      );
      expect(result.exitCode).toBe(0);
    });

    it("treats closing an unopened descriptor as success", async () => {
      const env = new Bash();
      const result = await env.exec(
        ['exec 3<&-; echo "exec rc=$?"', 'true 3<&-; echo "cmd rc=$?"'].join(
          "\n",
        ),
      );
      expect(result.stdout).toBe("exec rc=0\ncmd rc=0\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("restores a command-scoped close after the command", async () => {
      const env = new Bash();
      const result = await env.exec(
        [
          "printf 'a\\nb\\n' > /tmp/f.txt",
          "exec 3< /tmp/f.txt",
          "true 3<&-",
          'read -u 3 p; echo "p=$p"',
        ].join("\n"),
      );
      expect(result.stdout).toBe("p=a\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  });

  describe("errors", () => {
    it("reports a missing file for `exec N< missing` without exiting", async () => {
      const env = new Bash();
      const result = await env.exec(
        ['exec 3< /tmp/nope.txt; echo "rc=$?"', "echo still-running"].join(
          "\n",
        ),
      );
      expect(result.stdout).toBe("rc=1\nstill-running\n");
      expect(result.stderr).toBe(
        "bash: /tmp/nope.txt: No such file or directory\n",
      );
      expect(result.exitCode).toBe(0);
    });

    it("reports a missing file for a command-scoped `N< missing`", async () => {
      const env = new Bash();
      const result = await env.exec(
        'read -u 3 x 3< /tmp/nope.txt; echo "rc=$?"',
      );
      expect(result.stdout).toBe("rc=1\n");
      expect(result.stderr).toBe(
        "bash: /tmp/nope.txt: No such file or directory\n",
      );
      expect(result.exitCode).toBe(0);
    });

    it("reports Bad file descriptor for `exec N<&M` with M unopened", async () => {
      const env = new Bash();
      const result = await env.exec('exec 3<&7; echo "rc=$?"');
      expect(result.stdout).toBe("rc=1\n");
      expect(result.stderr).toBe("bash: 7: Bad file descriptor\n");
      expect(result.exitCode).toBe(0);
    });
  });
});
