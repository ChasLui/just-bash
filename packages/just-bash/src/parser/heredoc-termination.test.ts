import { describe, expect, it } from "vitest";
import type { HereDocNode } from "../ast/types.js";
import { parse } from "./parser.js";

const parseHeredoc = (source: string): HereDocNode => {
  const command = parse(source).statements[0]?.pipelines[0]?.commands[0];
  if (command?.type !== "SimpleCommand") {
    throw new Error("Expected a simple command");
  }
  const target = command.redirections[0]?.target;
  if (target?.type !== "HereDoc") {
    throw new Error("Expected a here-document");
  }
  return target;
};

describe("heredoc termination", () => {
  it("records a consumed delimiter", () => {
    expect(parseHeredoc("cat <<EOF\nbody\nEOF").terminated).toBe(true);
  });

  it("records end-of-input after a newline-terminated body", () => {
    expect(parseHeredoc("cat <<EOF\nbody\n").terminated).toBe(false);
  });

  it("records end-of-input immediately after the delimiter word", () => {
    expect(parseHeredoc("cat <<EOF").terminated).toBe(false);
  });

  it("finalizes every pending heredoc at end-of-input", () => {
    const command = parse("cat <<FIRST <<SECOND").statements[0].pipelines[0]
      .commands[0];
    if (command.type !== "SimpleCommand") throw new Error("expected command");
    expect(
      command.redirections.map((redirection) =>
        redirection.target.type === "HereDoc"
          ? redirection.target.terminated
          : undefined,
      ),
    ).toEqual([false, false]);
  });

  it("completes a partial final body line", () => {
    expect(parseHeredoc("cat <<EOF\nbody")).toMatchObject({
      terminated: false,
      content: {
        parts: [{ type: "Literal", value: "body\n" }],
      },
    });
  });

  it("removes an unquoted trailing backslash continuation", () => {
    expect(parseHeredoc("cat <<EOF\nbody\\")).toMatchObject({
      terminated: false,
      content: {
        parts: [{ type: "Literal", value: "body" }],
      },
    });
  });

  it("recognizes a delimiter split by an unquoted continuation", () => {
    expect(parseHeredoc("cat <<EOF\nEO\\\nF")).toMatchObject({
      terminated: true,
      content: { parts: [] },
    });
  });

  it("does not recognize a delimiter after an unquoted continuation", () => {
    expect(parseHeredoc("cat <<EOF\nbody\\\nEOF")).toMatchObject({
      terminated: false,
      content: {
        parts: [{ type: "Literal", value: "bodyEOF\n" }],
      },
    });
  });

  it("does not fold continuations in quoted heredocs", () => {
    expect(parseHeredoc("cat <<'EOF'\nEO\\\nF")).toMatchObject({
      terminated: false,
      content: {
        parts: [{ type: "Literal", value: "EO\\\nF\n" }],
      },
    });
  });

  it("does not fold an even number of unquoted trailing backslashes", () => {
    expect(parseHeredoc("cat <<EOF\nbody\\\\\nEOF").terminated).toBe(true);
  });

  it("strips tabs after joining an unquoted continuation", () => {
    expect(parseHeredoc("cat <<-EOF\n\tEO\\\n\tF").terminated).toBe(false);
  });

  it("records missing quoted and tab-stripping delimiters", () => {
    expect(parseHeredoc("cat <<'EOF'\nbody").terminated).toBe(false);
    expect(parseHeredoc("cat <<-EOF\n\tbody").terminated).toBe(false);
  });

  it("accepts a tab-indented delimiter for <<-", () => {
    expect(parseHeredoc("cat <<-EOF\n\tbody\n\tEOF").terminated).toBe(true);
  });
});
