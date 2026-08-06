import { describe, expect, it } from "vitest";
import { Bash } from "../../Bash.js";

/**
 * `-` as a FILE operand means standard input. GNU labels it
 * `(standard input)` wherever a file name would appear, and treats it as a
 * stream: only the first `-` of an invocation sees any content.
 *
 * Expectations verified against GNU grep 3.12.
 */
const files = {
  "/f1.txt": "apple\nbanana\n",
  "/f2.txt": "cherry\napple pie\n",
};

describe("grep - (stdin operand)", () => {
  it("reads standard input for a lone - operand", async () => {
    const env = new Bash();
    const result = await env.exec("printf 'apple\\n' | grep apple -");
    expect(result.stdout).toBe("apple\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("prints no file-name prefix for a lone - operand", async () => {
    const env = new Bash();
    const result = await env.exec(
      "printf 'x\\napple pie\\n' | grep -n apple -",
    );
    expect(result.stdout).toBe("2:apple pie\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("exits 1 when standard input has no match", async () => {
    const env = new Bash();
    const result = await env.exec("printf 'zzz\\n' | grep apple -");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(1);
  });

  it("exits 1 for empty standard input", async () => {
    const env = new Bash();
    const result = await env.exec("printf '' | grep apple -");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(1);
  });

  it("treats absent standard input as empty", async () => {
    const env = new Bash();
    const result = await env.exec("grep apple -");
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(1);
  });

  it("still accepts - as the PATTERN operand", async () => {
    const env = new Bash({ files: { "/dash.txt": "a-b\nxy\n" } });
    const result = await env.exec("grep - /dash.txt");
    expect(result.stdout).toBe("a-b\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("accepts - after the -- option terminator", async () => {
    const env = new Bash();
    const result = await env.exec("printf 'apple\\n' | grep -- apple -");
    expect(result.stdout).toBe("apple\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("accepts - alongside -e", async () => {
    const env = new Bash();
    const result = await env.exec("printf 'apple\\n' | grep -e apple -");
    expect(result.stdout).toBe("apple\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("grep - (stdin operand) mixed with files", () => {
  it("labels standard input in the multi-file prefix", async () => {
    const env = new Bash({ files });
    const result = await env.exec(
      "printf 'apple stdin\\n' | grep apple /f1.txt - /f2.txt",
    );
    expect(result.stdout).toBe(
      "/f1.txt:apple\n(standard input):apple stdin\n/f2.txt:apple pie\n",
    );
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("keeps operand order when - comes first", async () => {
    const env = new Bash({ files });
    const result = await env.exec(
      "printf 'apple stdin\\n' | grep apple - /f1.txt",
    );
    expect(result.stdout).toBe("(standard input):apple stdin\n/f1.txt:apple\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("labels standard input in -l output", async () => {
    const env = new Bash({ files });
    const result = await env.exec(
      "printf 'apple stdin\\n' | grep -l apple /f1.txt - /f2.txt",
    );
    expect(result.stdout).toBe("/f1.txt\n(standard input)\n/f2.txt\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("labels standard input in -L output", async () => {
    const env = new Bash({ files });
    const result = await env.exec(
      "printf 'zzz\\n' | grep -L apple /f1.txt - /f2.txt",
    );
    expect(result.stdout).toBe("(standard input)\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("labels standard input in -c output", async () => {
    const env = new Bash({ files });
    const result = await env.exec(
      "printf 'apple stdin\\n' | grep -c apple /f1.txt - /f2.txt",
    );
    expect(result.stdout).toBe("/f1.txt:1\n(standard input):1\n/f2.txt:1\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("numbers standard input lines independently with -n", async () => {
    const env = new Bash({ files });
    const result = await env.exec(
      "printf 'x\\napple stdin\\n' | grep -n apple /f1.txt - /f2.txt",
    );
    expect(result.stdout).toBe(
      "/f1.txt:1:apple\n(standard input):2:apple stdin\n/f2.txt:2:apple pie\n",
    );
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("suppresses the (standard input) label with -h", async () => {
    const env = new Bash({ files });
    const result = await env.exec(
      "printf 'apple stdin\\n' | grep -h apple /f1.txt - /f2.txt",
    );
    expect(result.stdout).toBe("apple\napple stdin\napple pie\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("keeps the -l listing unprefixed by -h", async () => {
    const env = new Bash({ files });
    const result = await env.exec(
      "printf 'apple stdin\\n' | grep -h -l apple /f1.txt -",
    );
    expect(result.stdout).toBe("/f1.txt\n(standard input)\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("reports a missing file but still searches standard input", async () => {
    const env = new Bash({ files });
    const result = await env.exec("printf 'apple\\n' | grep apple /nope.txt -");
    expect(result.stdout).toBe("(standard input):apple\n");
    expect(result.stderr).toBe("grep: /nope.txt: No such file or directory\n");
    expect(result.exitCode).toBe(2);
  });

  it("exits 0 for -q even when another operand is missing", async () => {
    const env = new Bash({ files });
    const result = await env.exec(
      "printf 'apple\\n' | grep -q apple - /nope.txt",
    );
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("ignores --include and --exclude for standard input", async () => {
    const env = new Bash({ files });
    const result = await env.exec(
      "printf 'apple stdin\\n' | grep --include='*.txt' apple - /f1.txt",
    );
    expect(result.stdout).toBe("(standard input):apple stdin\n/f1.txt:apple\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("grep - (stdin operand) repeated", () => {
  it("gives the second - an empty stream", async () => {
    const env = new Bash();
    const result = await env.exec(
      "printf 'apple\\napple2\\n' | grep apple - -",
    );
    expect(result.stdout).toBe(
      "(standard input):apple\n(standard input):apple2\n",
    );
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("counts the second - as zero", async () => {
    const env = new Bash();
    const result = await env.exec(
      "printf 'apple\\napple2\\n' | grep -c apple - -",
    );
    expect(result.stdout).toBe("(standard input):2\n(standard input):0\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("counts every - after the first as zero", async () => {
    const env = new Bash();
    const result = await env.exec("printf 'apple\\n' | grep -c apple - - -");
    expect(result.stdout).toBe(
      "(standard input):1\n(standard input):0\n(standard input):0\n",
    );
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("lists the drained - under -L", async () => {
    const env = new Bash();
    const result = await env.exec("printf 'apple\\n' | grep -L apple - -");
    expect(result.stdout).toBe("(standard input)\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("stops the first - at -m and leaves the second empty", async () => {
    const env = new Bash();
    const result = await env.exec(
      "printf 'apple s1\\napple s2\\n' | grep -m1 apple - -",
    );
    expect(result.stdout).toBe("(standard input):apple s1\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});

describe("grep - (stdin operand) with -r", () => {
  it("prints no prefix for a lone - under -r", async () => {
    const env = new Bash();
    const result = await env.exec("printf 'apple stdin\\n' | grep -r apple -");
    expect(result.stdout).toBe("apple stdin\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("prefixes when -r is given a directory alongside -", async () => {
    const env = new Bash({ files: { "/sub/a.txt": "apple tree\n" } });
    const result = await env.exec(
      "printf 'apple stdin\\n' | grep -r apple - /sub",
    );
    expect(result.stdout).toBe(
      "(standard input):apple stdin\n/sub/a.txt:apple tree\n",
    );
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("lists both sources with -lr", async () => {
    const env = new Bash({ files: { "/sub/a.txt": "apple tree\n" } });
    const result = await env.exec("printf 'apple\\n' | grep -lr apple - /sub");
    expect(result.stdout).toBe("(standard input)\n/sub/a.txt\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("prefixes repeated - under -r", async () => {
    const env = new Bash();
    const result = await env.exec("printf 'apple\\n' | grep -r apple - -");
    expect(result.stdout).toBe("(standard input):apple\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});
