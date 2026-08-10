import { describe, expect, it } from "vitest";
import type {
  ConditionalCommandNode,
  ProcessSubstitutionPart,
  SimpleCommandNode,
  WordPart,
} from "../ast/types.js";
import { Parser } from "./parser.js";

const asSimpleCommand = (script: string): SimpleCommandNode =>
  new Parser().parse(script).statements[0].pipelines[0]
    .commands[0] as SimpleCommandNode;

const asProcessSubstitution = (part: WordPart): ProcessSubstitutionPart => {
  expect(part.type).toBe("ProcessSubstitution");
  return part as ProcessSubstitutionPart;
};

describe("process substitution parser contexts", () => {
  it("preserves the outer parenthesis before an arithmetic command", () => {
    const parts = asSimpleCommand("echo 2<(((1)))").args[0].parts;
    expect(parts[0]).toEqual({ type: "Literal", value: "2" });
    const processSubstitution = asProcessSubstitution(parts[1]);
    expect(
      processSubstitution.body.statements[0].pipelines[0].commands[0].type,
    ).toBe("ArithmeticCommand");
  });

  it("preserves no-brace-expansion context on adjacent atoms", () => {
    const command = new Parser().parse(
      "[[ <(true){a,b} == '/dev/fd/63{a,b}' ]]",
    ).statements[0].pipelines[0].commands[0] as ConditionalCommandNode;
    expect(command.expression.type).toBe("CondBinary");
    if (command.expression.type !== "CondBinary") {
      throw new Error("expected a binary conditional");
    }
    expect(command.expression.left.parts).toHaveLength(2);
    asProcessSubstitution(command.expression.left.parts[0]);
    expect(command.expression.left.parts[1]).toEqual({
      type: "Literal",
      value: "{a,b}",
    });
  });

  it("preserves regex context on adjacent atoms", () => {
    const command = new Parser().parse("[[ /dev/fd/63+ =~ <(true)\\+ ]]")
      .statements[0].pipelines[0].commands[0] as ConditionalCommandNode;
    expect(command.expression.type).toBe("CondBinary");
    if (command.expression.type !== "CondBinary") {
      throw new Error("expected a binary conditional");
    }
    expect(command.expression.right.parts).toHaveLength(2);
    asProcessSubstitution(command.expression.right.parts[0]);
    expect(command.expression.right.parts[1]).toEqual({
      type: "Escaped",
      value: "+",
    });
  });

  it("keeps process-like heredoc delimiters literal", () => {
    const script = "cat <<EOF<(echo hi)\nbody\nEOF<(echo hi)\n";
    const command = asSimpleCommand(script);
    expect(command.args).toHaveLength(0);
    expect(command.redirections[0].target).toMatchObject({
      type: "HereDoc",
      delimiter: "EOF<(echo hi)",
    });
  });

  it("preserves non-special backslashes in double-quoted heredoc delimiters", () => {
    const command = asSimpleCommand('cat <<"E\\OF"\nbody\nE\\OF\n');
    expect(command.redirections[0].target).toMatchObject({
      type: "HereDoc",
      delimiter: "E\\OF",
    });
  });

  it("keeps command substitution syntax literal in heredoc delimiters", () => {
    const command = asSimpleCommand("cat <<EOF$(echo x)\nbody\nEOF$(echo x)\n");
    expect(command.redirections[0].target).toMatchObject({
      type: "HereDoc",
      delimiter: "EOF$(echo x)",
    });
  });

  it("keeps joined terminator words inside process substitution bodies", () => {
    const outerCommand = asSimpleCommand("printf '%s\\n' <(do<(true))");
    const outerProcess = asProcessSubstitution(outerCommand.args[1].parts[0]);
    const innerCommand = outerProcess.body.statements[0].pipelines[0]
      .commands[0] as SimpleCommandNode;
    expect(innerCommand.name?.parts).toHaveLength(2);
    expect(innerCommand.name?.parts[0]).toEqual({
      type: "Literal",
      value: "do",
    });
    asProcessSubstitution(innerCommand.name?.parts[1] as WordPart);
  });

  for (const delimiter of ["EOF$(echo 'x')", "EOF<(echo 'x')"]) {
    it(`does not quote heredoc bodies from nested quotes in ${delimiter.slice(3, 5)}`, () => {
      const command = asSimpleCommand(
        `cat <<${delimiter}\n$HOME\n${delimiter}\n`,
      );
      expect(command.redirections[0].target).toMatchObject({
        type: "HereDoc",
        delimiter,
        quoted: false,
      });
    });
  }

  it("preserves literal tildes after preceding process substitution parts", () => {
    const command = asSimpleCommand("printf '<%s>\\n' <(true)~");
    expect(command.args[1].parts).toHaveLength(2);
    asProcessSubstitution(command.args[1].parts[0]);
    expect(command.args[1].parts[1]).toEqual({
      type: "Literal",
      value: "~",
    });
  });

  it("preserves adjacent hash suffixes after process substitutions", () => {
    const command = asSimpleCommand("printf '<%s>\\n' <(true)#suffix");
    expect(command.args[1].parts).toHaveLength(2);
    asProcessSubstitution(command.args[1].parts[0]);
    expect(command.args[1].parts[1]).toEqual({
      type: "Literal",
      value: "#suffix",
    });
  });

  it("preserves assignment expansion for joined assignment arguments", () => {
    const command = asSimpleCommand("printf '<%s>\\n' x=<(true):~");
    expect(command.args[1].parts).toHaveLength(4);
    expect(command.args[1].parts[0]).toEqual({
      type: "Literal",
      value: "x=",
    });
    asProcessSubstitution(command.args[1].parts[1]);
    expect(command.args[1].parts[2]).toEqual({
      type: "Literal",
      value: ":",
    });
    expect(command.args[1].parts[3]).toEqual({
      type: "TildeExpansion",
      user: null,
    });
  });

  it("preserves assignment expansion on adjacent suffixes", () => {
    const command = asSimpleCommand("x=<(true):~");
    const value = command.assignments[0].value;
    if (!value) throw new Error("expected an assignment value");
    expect(value.parts).toHaveLength(3);
    asProcessSubstitution(value.parts[0]);
    expect(value.parts[1]).toEqual({ type: "Literal", value: ":" });
    expect(value.parts[2]).toEqual({ type: "TildeExpansion", user: null });
  });

  it("parses process substitutions inside parameter-expansion operands", () => {
    const value = asSimpleCommand("p=${x:-<(printf ok)}").assignments[0].value;
    if (!value) throw new Error("expected an assignment value");
    const parameter = value.parts[0];
    if (parameter.type !== "ParameterExpansion") {
      throw new Error("expected a parameter expansion");
    }
    if (parameter.operation?.type !== "DefaultValue") {
      throw new Error("expected a default-value operation");
    }
    asProcessSubstitution(parameter.operation.word.parts[0]);
  });

  it("keeps process syntax literal in double-quoted parameter operands", () => {
    const value = asSimpleCommand('p="${x:-<(printf ok)}"').assignments[0]
      .value;
    if (!value || value.parts[0].type !== "DoubleQuoted") {
      throw new Error("expected a double-quoted value");
    }
    const parameter = value.parts[0].parts[0];
    if (parameter.type !== "ParameterExpansion") {
      throw new Error("expected a parameter expansion");
    }
    if (parameter.operation?.type !== "DefaultValue") {
      throw new Error("expected a default-value operation");
    }
    expect(parameter.operation.word.parts).toEqual([
      { type: "Literal", value: "<(printf ok)" },
    ]);
  });

  it("owns LINENO relative to each process body", () => {
    const ast = new Parser().parse(`true
cat <(

echo outer=$LINENO

echo second=$LINENO
cat <(
echo inner=$LINENO
)
)`);
    const outerCommand = ast.statements[1].pipelines[0]
      .commands[0] as SimpleCommandNode;
    const outerProcess = asProcessSubstitution(outerCommand.args[0].parts[0]);
    const outerEcho = outerProcess.body.statements[0].pipelines[0]
      .commands[0] as SimpleCommandNode;
    const secondEcho = outerProcess.body.statements[1].pipelines[0]
      .commands[0] as SimpleCommandNode;
    const nestedCat = outerProcess.body.statements[2].pipelines[0]
      .commands[0] as SimpleCommandNode;
    const innerProcess = asProcessSubstitution(nestedCat.args[0].parts[0]);
    const innerEcho = innerProcess.body.statements[0].pipelines[0]
      .commands[0] as SimpleCommandNode;
    expect(outerEcho.line).toBe(2);
    expect(secondEcho.line).toBe(3);
    expect(innerEcho.line).toBe(4);
  });
});
