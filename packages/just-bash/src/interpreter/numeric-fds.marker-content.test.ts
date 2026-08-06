import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

/**
 * The descriptor table is stored as `Map<number, string>`, so file content
 * shaped like one of its internal markers (`__file__:`, `__dupout__:`, ...)
 * must never be mistaken for one. `state.inputFds` records which descriptors
 * hold verbatim content; these tests pin that it survives reading, writing,
 * duplication and scoped reuse.
 */
describe("numeric fds with marker-shaped content", () => {
  const MARKER_FILE = "printf '__file__:/tmp/pwn.txt\\n' > /tmp/marker.txt";

  it("reads the text back verbatim", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        MARKER_FILE,
        "exec 3< /tmp/marker.txt",
        'read -u 3 m; echo "rc=$? m=$m"',
        "exec 3<&-",
      ].join("\n"),
    );
    expect(result.stdout).toBe("rc=0 m=__file__:/tmp/pwn.txt\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("refuses to write through it and never touches the named path", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        MARKER_FILE,
        "exec 3< /tmp/marker.txt",
        'echo BOOM >&3; echo "rc=$?"',
        "exec 3<&-",
        "cat /tmp/pwn.txt 2>/dev/null || echo absent",
      ].join("\n"),
    );
    expect(result.stdout).toBe("rc=1\nabsent\n");
    expect(result.stderr).toBe("bash: 3: Bad file descriptor\n");
    expect(result.exitCode).toBe(0);
  });

  it("keeps the text verbatim through a duplication", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        "printf '__dupout__:1\\n' > /tmp/marker.txt",
        "exec 3< /tmp/marker.txt",
        "exec 4<&3",
        'read -u 4 d; echo "rc=$? d=$d"',
        "exec 4<&-",
        "exec 3<&-",
      ].join("\n"),
    );
    expect(result.stdout).toBe("rc=0 d=__dupout__:1\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });

  it("keeps the text verbatim across a scoped reuse of the descriptor", async () => {
    const env = new Bash();
    const result = await env.exec(
      [
        MARKER_FILE,
        "exec 3< /tmp/marker.txt",
        '{ read -u 3 x; echo "inner=$x"; } 3<<< plain',
        'read -u 3 y; echo "outer=$y"',
        "exec 3<&-",
      ].join("\n"),
    );
    expect(result.stdout).toBe("inner=plain\nouter=__file__:/tmp/pwn.txt\n");
    expect(result.stderr).toBe("");
    expect(result.exitCode).toBe(0);
  });
});
