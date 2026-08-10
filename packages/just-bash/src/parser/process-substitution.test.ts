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
  it("lexes <(cmd) as adjacent operator and parenthesis tokens", () => {
    const tokens = new Lexer("cat <(echo hi)").tokenize();
    expect(tokens.map((t) => [t.type, t.value])).toEqual([
      [TokenType.NAME, "cat"],
      [TokenType.LESS, "<"],
      [TokenType.LPAREN, "("],
      [TokenType.NAME, "echo"],
      [TokenType.NAME, "hi"],
      [TokenType.RPAREN, ")"],
      [TokenType.EOF, ""],
    ]);
    expect(tokens[1].end).toBe(tokens[2].start);
  });

  it("lexes >(cmd) as adjacent operator and parenthesis tokens", () => {
    const tokens = new Lexer("tee >(cat)").tokenize();
    expect(tokens.map((t) => [t.type, t.value])).toEqual([
      [TokenType.NAME, "tee"],
      [TokenType.GREAT, ">"],
      [TokenType.LPAREN, "("],
      [TokenType.NAME, "cat"],
      [TokenType.RPAREN, ")"],
      [TokenType.EOF, ""],
    ]);
    expect(tokens[1].end).toBe(tokens[2].start);
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
      [TokenType.NAME, "a"],
      [TokenType.LESS, "<"],
      [TokenType.LPAREN, "("],
      [TokenType.NAME, "echo"],
      [TokenType.NAME, "hi"],
      [TokenType.RPAREN, ")"],
      [TokenType.EOF, ""],
    ]);
  });

  it("does not treat a leading digit as a file descriptor", () => {
    const tokens = new Lexer("echo 2>(b)").tokenize();
    expect(tokens.map((t) => [t.type, t.value])).toEqual([
      [TokenType.NAME, "echo"],
      [TokenType.NUMBER, "2"],
      [TokenType.GREAT, ">"],
      [TokenType.LPAREN, "("],
      [TokenType.NAME, "b"],
      [TokenType.RPAREN, ")"],
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

  it("preserves an unterminated process substitution for parser diagnostics", () => {
    const tokens = new Lexer("cat <(echo hi").tokenize();
    expect(tokens.map((token) => token.type)).toEqual([
      TokenType.NAME,
      TokenType.LESS,
      TokenType.LPAREN,
      TokenType.NAME,
      TokenType.NAME,
      TokenType.EOF,
    ]);
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

  it("concatenates literal prefixes and suffixes", () => {
    const parts = argParts("echo a<(b)c")[0];
    expect(parts).toHaveLength(3);
    expect(parts[0]).toEqual({ type: "Literal", value: "a" });
    expect(asProcSub(parts[1]).direction).toBe("input");
    expect(parts[2]).toEqual({ type: "Literal", value: "c" });
  });

  it("concatenates adjacent process substitutions", () => {
    const parts = argParts("echo <(a)>(b)")[0];
    expect(parts).toHaveLength(2);
    expect(asProcSub(parts[0]).direction).toBe("input");
    expect(asProcSub(parts[1]).direction).toBe("output");
  });

  it("concatenates an assignment-shaped suffix", () => {
    const parts = argParts("echo <(true)x=y")[0];
    expect(parts).toHaveLength(2);
    expect(asProcSub(parts[0]).direction).toBe("input");
    expect(parts[1]).toEqual({ type: "Literal", value: "x=y" });
  });

  it("keeps an assignment-shaped suffix in the assignment value", () => {
    const command = new Parser().parse("x=<(true)y=z").statements[0]
      .pipelines[0].commands[0] as SimpleCommandNode;
    expect(command.assignments).toHaveLength(1);
    expect(command.args).toHaveLength(0);
    const value = command.assignments[0].value;
    if (!value) throw new Error("expected an assignment value");
    expect(value.parts).toHaveLength(2);
    expect(asProcSub(value.parts[0]).direction).toBe("input");
    expect(value.parts[1]).toEqual({ type: "Literal", value: "y=z" });
  });

  it("treats grammar-shaped prefixes as command words", () => {
    for (const [script, prefix, direction] of [
      ["if<(true)", "if", "input"],
      ["!<(true)", "!", "input"],
      ["time<(true)", "time", "input"],
      ["do<(true)", "do", "input"],
      ["fi<(true)", "fi", "input"],
      ["else<(true)", "else", "input"],
      ["}<(true)", "}", "input"],
      ["]]<(true)", "]]", "input"],
      ["{fd}>(true)", "{fd}", "output"],
    ] as const) {
      const command = new Parser().parse(script).statements[0].pipelines[0]
        .commands[0] as SimpleCommandNode;
      if (!command.name) throw new Error("expected a command name");
      expect(command.name.parts[0]).toEqual({ type: "Literal", value: prefix });
      expect(asProcSub(command.name.parts[1]).direction).toBe(direction);
    }
  });

  it("keeps control operators and parentheses in the command grammar", () => {
    for (const script of [
      "&<(true); echo survived",
      "&&<(true); echo survived",
      "||<(true); echo survived",
    ]) {
      expect(() => new Parser().parse(script)).toThrow("syntax error");
    }

    const subshell = new Parser().parse("(<(true))").statements[0].pipelines[0]
      .commands[0];
    expect(subshell.type).toBe("Subshell");

    const arithmetic = new Parser().parse("((<(true)))").statements[0]
      .pipelines[0].commands[0];
    expect(arithmetic.type).toBe("ArithmeticCommand");
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
      "unexpected EOF while looking for matching `)'",
    );
  });

  it("rejects a missing heredoc delimiter", () => {
    expect(() => new Parser().parse("cat <<; echo survived")).toThrow(
      "Expected here-document delimiter",
    );
  });

  it("rejects an unterminated quoted heredoc delimiter", () => {
    expect(() => new Parser().parse('cat <<"EOF\nbody\nEOF')).toThrow(
      "unexpected EOF while looking for matching",
    );
  });

  it("recognizes a comment after a heredoc operator", () => {
    for (const script of [
      "cat << #comment\necho survived\n",
      "cat <<#comment\necho survived\n",
    ]) {
      expect(() => new Parser().parse(script)).toThrow(
        "Expected here-document delimiter",
      );
    }
  });

  it("rejects an unterminated process-like heredoc delimiter", () => {
    expect(() => new Parser().parse("cat <<EOF<(x\nbody\nEOF")).toThrow(
      "unexpected EOF while looking for matching `)'",
    );
  });
});
