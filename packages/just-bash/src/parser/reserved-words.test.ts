import { describe, expect, it } from "vitest";
import { serialize } from "../transform/serialize.js";
import { Lexer, TokenType } from "./lexer.js";
import { parse } from "./parser.js";

const RESERVED_WORDS = [
  ["if", TokenType.IF],
  ["then", TokenType.THEN],
  ["elif", TokenType.ELIF],
  ["else", TokenType.ELSE],
  ["fi", TokenType.FI],
  ["time", TokenType.TIME],
  ["for", TokenType.FOR],
  ["in", TokenType.IN],
  ["until", TokenType.UNTIL],
  ["while", TokenType.WHILE],
  ["do", TokenType.DO],
  ["done", TokenType.DONE],
  ["case", TokenType.CASE],
  ["esac", TokenType.ESAC],
  ["coproc", TokenType.COPROC],
  ["select", TokenType.SELECT],
  ["function", TokenType.FUNCTION],
  ["{", TokenType.LBRACE],
  ["}", TokenType.RBRACE],
  ["[[", TokenType.DBRACK_START],
  ["]]", TokenType.DBRACK_END],
  ["!", TokenType.BANG],
] as const;

describe("reserved words", () => {
  it.each(
    RESERVED_WORDS,
  )("treats escaped %s as an ordinary word", (word, reservedType) => {
    const escaped = `\\${word}`;
    const [token] = new Lexer(escaped).tokenize();

    expect(token).toMatchObject({
      type: TokenType.WORD,
      value: escaped,
    });
    expect(token.type).not.toBe(reservedType);

    const command = parse(escaped).statements[0].pipelines[0].commands[0];
    expect(command.type).toBe("SimpleCommand");
  });

  describe.each(["'", '"'])("when quoted with %s", (quote) => {
    it.each(
      RESERVED_WORDS,
    )("treats %s as an ordinary word", (word, reservedType) => {
      const source = `${quote}${word}${quote}`;
      const [token] = new Lexer(source).tokenize();

      expect(token.value).toBe(word);
      expect(token.type).not.toBe(reservedType);

      const command = parse(source).statements[0].pipelines[0].commands[0];
      expect(command.type).toBe("SimpleCommand");
    });
  });

  it.each(
    RESERVED_WORDS.filter(([word]) => word.length > 1),
  )("does not recognize partially quoted %s as reserved", (word, reservedType) => {
    for (let quotedIndex = 0; quotedIndex < word.length; quotedIndex++) {
      const escaped = `${word.slice(0, quotedIndex)}\\${word.slice(quotedIndex)}`;
      const quoted = `${word.slice(0, quotedIndex)}"${word[quotedIndex]}"${word.slice(quotedIndex + 1)}`;

      for (const source of [escaped, quoted]) {
        const [token] = new Lexer(source).tokenize();
        expect(token.type).not.toBe(reservedType);
        expect(parse(source).statements[0].pipelines[0].commands[0].type).toBe(
          "SimpleCommand",
        );
      }
    }
  });

  it("preserves an escaped reserved word as an untimed command", () => {
    const source = "\\time echo";
    const script = parse(source);
    const pipeline = script.statements[0].pipelines[0];
    const command = pipeline.commands[0];

    expect(pipeline.timed).toBe(false);
    expect(command).toMatchObject({
      type: "SimpleCommand",
      name: {
        type: "Word",
        parts: [
          { type: "Escaped", value: "t" },
          { type: "Literal", value: "ime" },
        ],
      },
      args: [
        {
          type: "Word",
          parts: [{ type: "Literal", value: "echo" }],
        },
      ],
    });
    expect(serialize(script)).toBe(source);
  });

  it("recognizes a reserved word across a backslash-newline", () => {
    const source = "ti\\\nme echo";
    const [token] = new Lexer(source).tokenize();
    const pipeline = parse(source).statements[0].pipelines[0];

    expect(token).toMatchObject({ type: TokenType.TIME, value: "time" });
    expect(pipeline.timed).toBe(true);
  });
});
