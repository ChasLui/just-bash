import { describe, expect, it } from "vitest";
import type {
  ProcessSubstitutionPart,
  SimpleCommandNode,
  WordPart,
} from "../ast/types.js";
import { serialize } from "../transform/serialize.js";
import { Lexer, TokenType } from "./lexer.js";
import { Parser } from "./parser.js";

/** Parse a script and return the parts of the first command's args. */
function argParts(script: string): WordPart[][] {
  const ast = new Parser().parse(script);
  const command = ast.statements[0].pipelines[0]
    .commands[0] as SimpleCommandNode;
  return command.args.map((arg) => arg.parts);
}

function firstArgPart(script: string): WordPart {
  return argParts(script)[0][0];
}

function asProcSub(part: WordPart): ProcessSubstitutionPart {
  expect(part.type).toBe("ProcessSubstitution");
  return part as ProcessSubstitutionPart;
}

describe("process substitution - lexer", () => {
  it("lexes <(cmd) as a single word token, not a redirection", () => {
    const tokens = new Lexer("cat <(echo hi)").tokenize();
    expect(tokens.map((t) => [t.type, t.value])).toEqual([
      [TokenType.NAME, "cat"],
      [TokenType.WORD, "<(echo hi)"],
      [TokenType.EOF, ""],
    ]);
  });

  it("lexes >(cmd) as a single word token", () => {
    const tokens = new Lexer("tee >(cat)").tokenize();
    expect(tokens.map((t) => [t.type, t.value])).toEqual([
      [TokenType.NAME, "tee"],
      [TokenType.WORD, ">(cat)"],
      [TokenType.EOF, ""],
    ]);
  });

  it("keeps `< (` as a redirection operator followed by a paren", () => {
    const tokens = new Lexer("cat < (echo hi)").tokenize();
    expect(tokens.map((t) => t.type)).toEqual([
      TokenType.NAME,
      TokenType.LESS,
      TokenType.LPAREN,
      TokenType.NAME,
      TokenType.NAME,
      TokenType.RPAREN,
      TokenType.EOF,
    ]);
  });

  it("keeps `<<` here-documents and `<<<` here-strings intact", () => {
    expect(new Lexer("cat <<<(x)").tokenize()[1].type).toBe(TokenType.TLESS);
    expect(new Lexer("cat <<EOF\n(x)\nEOF\n").tokenize()[1].type).toBe(
      TokenType.DLESS,
    );
  });

  it("glues a process substitution onto an adjacent word", () => {
    const tokens = new Lexer("echo a<(echo hi)").tokenize();
    expect(tokens.map((t) => [t.type, t.value])).toEqual([
      [TokenType.NAME, "echo"],
      [TokenType.WORD, "a<(echo hi)"],
      [TokenType.EOF, ""],
    ]);
  });

  it("does not treat a leading digit as a file descriptor", () => {
    const tokens = new Lexer("echo 2>(cat)").tokenize();
    expect(tokens.map((t) => [t.type, t.value])).toEqual([
      [TokenType.NAME, "echo"],
      [TokenType.WORD, "2>(cat)"],
      [TokenType.EOF, ""],
    ]);
  });

  it("leaves `>` alone inside arithmetic contexts", () => {
    const tokens = new Lexer("(( 3 > (1) ))").tokenize();
    expect(tokens.map((t) => t.type)).toEqual([
      TokenType.DPAREN_START,
      TokenType.NUMBER,
      TokenType.GREAT,
      TokenType.LPAREN,
      TokenType.NUMBER,
      TokenType.RPAREN,
      TokenType.DPAREN_END,
      TokenType.EOF,
    ]);
  });

  it("falls back to a redirection when the parens are unbalanced", () => {
    const tokens = new Lexer("cat <(echo hi").tokenize();
    expect(tokens[1].type).toBe(TokenType.LESS);
  });

  it("tracks parens through quotes and nesting", () => {
    expect(new Lexer("cat <(echo 'a)b')").tokenize()[1].value).toBe(
      "<(echo 'a)b')",
    );
    expect(new Lexer('cat <(echo ")")').tokenize()[1].value).toBe(
      'cat <(echo ")")'.slice(4),
    );
    expect(new Lexer("cat <(cat <(echo x))").tokenize()[1].value).toBe(
      "<(cat <(echo x))",
    );
  });
});

describe("process substitution - parser", () => {
  it("builds an input ProcessSubstitution part", () => {
    const part = asProcSub(firstArgPart("cat <(echo hi)"));
    expect(part.direction).toBe("input");
    expect(serialize(part.body)).toBe("echo hi");
  });

  it("builds an output ProcessSubstitution part", () => {
    const part = asProcSub(firstArgPart("tee >(wc -l)"));
    expect(part.direction).toBe("output");
    expect(serialize(part.body)).toBe("wc -l");
  });

  it("parses several substitutions in one command", () => {
    const parts = argParts("diff <(sort a) <(sort b)");
    expect(parts).toHaveLength(2);
    expect(serialize(asProcSub(parts[0][0]).body)).toBe("sort a");
    expect(serialize(asProcSub(parts[1][0]).body)).toBe("sort b");
  });

  it("parses a multi-command body", () => {
    const part = asProcSub(firstArgPart("cat <(echo a; echo b)"));
    expect(serialize(part.body)).toBe("echo a\necho b");
  });

  it("parses a nested process substitution", () => {
    const outer = asProcSub(firstArgPart("cat <(cat <(echo deep))"));
    expect(serialize(outer.body)).toBe("cat <(echo deep)");
  });

  it("concatenates a literal prefix with the substitution", () => {
    const parts = argParts("echo a<(echo hi)")[0];
    expect(parts).toHaveLength(2);
    expect(parts[0]).toEqual({ type: "Literal", value: "a" });
    expect(asProcSub(parts[1]).direction).toBe("input");
  });

  it("keeps quoted text literal", () => {
    expect(argParts('echo "<(echo hi)"')[0]).toEqual([
      {
        type: "DoubleQuoted",
        parts: [{ type: "Literal", value: "<(echo hi)" }],
      },
    ]);
    expect(argParts("echo '<(echo hi)'")[0]).toEqual([
      { type: "SingleQuoted", value: "<(echo hi)" },
    ]);
  });

  it("parses a substitution used as a redirection source", () => {
    const ast = new Parser().parse("cat < <(echo hi)");
    const command = ast.statements[0].pipelines[0]
      .commands[0] as SimpleCommandNode;
    expect(command.redirections).toHaveLength(1);
    expect(command.redirections[0].operator).toBe("<");
    const target = command.redirections[0].target;
    expect(target.type).toBe("Word");
    if (target.type !== "Word") throw new Error("expected a word target");
    expect(asProcSub(target.parts[0]).direction).toBe("input");
  });

  it("round-trips through the serializer", () => {
    const script = "cat <(echo hi) >(cat)";
    expect(serialize(new Parser().parse(script))).toBe(script);
  });

  it("reports a parse error for an unterminated substitution", () => {
    expect(() => new Parser().parse("cat <(echo hi")).toThrow(
      "Expected redirection target",
    );
  });
});
