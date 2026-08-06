import { describe, expect, it } from "vitest";
import { Bash } from "../Bash.js";

/**
 * Which loops own the shared read stream (`ctx.state.groupStdin`).
 *
 * A `while`/`until` loop installs — and therefore restores — stdin only when
 * it brought its own: an input redirection on the loop, or the stdin handed to
 * it as a pipeline stage. A stream inherited from an enclosing group must be
 * left alone so reads inside the loop advance it, otherwise restoring it
 * rewinds the shared read position and the next `read` re-reads a consumed
 * line.
 *
 * Every expectation here was verified against real bash before being written.
 */
describe("while/until loop stdin ownership", () => {
  describe("inherited streams keep advancing", () => {
    it("should not rewind the group stream after an until loop", async () => {
      const env = new Bash();
      const result = await env.exec(
        `printf 'a\\nb\\nc\\n' | { until ! read l; do echo "loop $l"; break; done; read r; echo "after=[$r]"; }`,
      );
      expect(result.stdout).toBe("loop a\nafter=[b]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should not rewind the group stream after a while loop", async () => {
      const env = new Bash();
      const result = await env.exec(
        `printf 'a\\nb\\nc\\n' | { while read l; do echo "loop $l"; break; done; read r; echo "after=[$r]"; }`,
      );
      expect(result.stdout).toBe("loop a\nafter=[b]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should advance across two sequential until loops", async () => {
      const env = new Bash();
      const result = await env.exec(
        `printf 'a\\nb\\nc\\n' | { until ! read x; do break; done; until ! read y; do break; done; echo "x=$x y=$y"; }`,
      );
      expect(result.stdout).toBe("x=a y=b\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should advance across two sequential while loops", async () => {
      const env = new Bash();
      const result = await env.exec(
        `printf 'a\\nb\\nc\\n' | { while read x; do break; done; while read y; do break; done; echo "x=$x y=$y"; }`,
      );
      expect(result.stdout).toBe("x=a y=b\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should leave the stream at EOF after an until loop drains it", async () => {
      const env = new Bash();
      const result = await env.exec(
        `printf 'a\\nb\\n' | { until ! read l; do echo "l=$l"; done; read r; echo "after=[$r] code=$?"; }`,
      );
      expect(result.stdout).toBe("l=a\nl=b\nafter=[] code=1\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should leave the stream at EOF after a while loop drains it", async () => {
      const env = new Bash();
      const result = await env.exec(
        `printf 'a\\nb\\n' | { while read l; do echo "l=$l"; done; read r; echo "after=[$r] code=$?"; }`,
      );
      expect(result.stdout).toBe("l=a\nl=b\nafter=[] code=1\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should share one stream between nested while loops", async () => {
      const env = new Bash();
      const result = await env.exec(
        `printf 'a\\nb\\nc\\nd\\n' | while read x; do while read y; do echo "$x-$y"; break; done; done`,
      );
      expect(result.stdout).toBe("a-b\nc-d\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should share one stream between nested until loops", async () => {
      const env = new Bash();
      const result = await env.exec(
        `printf 'a\\nb\\nc\\nd\\n' | until ! read x; do until ! read y; do echo "$x=$y"; break; done; done`,
      );
      expect(result.stdout).toBe("a=b\nc=d\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  });

  describe("own streams are installed and restored", () => {
    it("should restore the outer stream after a redirected while loop", async () => {
      const env = new Bash({
        files: { "/four.txt": "a\nb\nc\nd\n", "/inner.txt": "x\ny\n" },
      });
      const result = await env.exec(
        `{ read first; while read l; do echo "in:$l"; break; done < /inner.txt; read r; echo "first=$first r=[$r]"; } < /four.txt`,
      );
      expect(result.stdout).toBe("in:x\nfirst=a r=[b]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should restore the outer stream after a redirected until loop", async () => {
      const env = new Bash({
        files: { "/four.txt": "a\nb\nc\nd\n", "/inner.txt": "x\ny\n" },
      });
      const result = await env.exec(
        `{ until ! read l; do echo "in:$l"; break; done < /inner.txt; read r; echo "r=[$r]"; } < /four.txt`,
      );
      expect(result.stdout).toBe("in:x\nr=[a]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should give a while loop an empty stream from an empty file", async () => {
      const env = new Bash({
        files: { "/four.txt": "a\nb\nc\nd\n", "/empty.txt": "" },
      });
      const result = await env.exec(
        `{ while read l; do echo "L$l"; done < /empty.txt; read r; echo "r=[$r]"; } < /four.txt`,
      );
      expect(result.stdout).toBe("r=[a]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should give an until loop an empty stream from an empty file", async () => {
      const env = new Bash({
        files: { "/four.txt": "a\nb\nc\nd\n", "/empty.txt": "" },
      });
      const result = await env.exec(
        `{ until ! read l; do echo "U$l"; done < /empty.txt; read r; echo "r=[$r]"; } < /four.txt`,
      );
      expect(result.stdout).toBe("r=[a]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });

    it("should restore the outer stream after a here-doc-fed loop", async () => {
      const env = new Bash({ files: { "/four.txt": "a\nb\nc\nd\n" } });
      const result = await env.exec(`{ while read l; do echo "h:$l"; done <<EOF
z1
EOF
read r; echo "r=[$r]"; } < /four.txt`);
      expect(result.stdout).toBe("h:z1\nr=[a]\n");
      expect(result.stderr).toBe("");
      expect(result.exitCode).toBe(0);
    });
  });
});
