import { describe, expect, it } from "vitest";
import type { CommandNode, SimpleCommandNode } from "../ast/types.js";
import { parse } from "./parser.js";

const expectSimpleCommand = (command: CommandNode): SimpleCommandNode => {
  expect(command.type).toBe("SimpleCommand");
  if (command.type !== "SimpleCommand") {
    throw new Error(`Expected SimpleCommand, received ${command.type}`);
  }
  return command;
};

describe("fd-variable redirection parsing", () => {
  it("preserves the descriptor variable on a bare top-level redirection", () => {
    const script = parse("{output}>output.log");
    const command = expectSimpleCommand(
      script.statements[0].pipelines[0].commands[0],
    );

    expect(command.name).toBeNull();
    expect(command.redirections).toHaveLength(1);
    expect(command.redirections[0].fdVariable).toBe("output");
  });

  it("preserves the descriptor variable in a compound list", () => {
    const script = parse("if true; then echo before; {output}>output.log; fi");
    const ifCommand = script.statements[0].pipelines[0].commands[0];
    expect(ifCommand.type).toBe("If");
    if (ifCommand.type !== "If") {
      throw new Error(`Expected If, received ${ifCommand.type}`);
    }
    const command = expectSimpleCommand(
      ifCommand.clauses[0].body[1].pipelines[0].commands[0],
    );

    expect(command.name).toBeNull();
    expect(command.redirections).toHaveLength(1);
    expect(command.redirections[0].fdVariable).toBe("output");
  });

  it("preserves the descriptor variable on a heredoc", () => {
    const script = parse("{input}<<EOF\nvalue\nEOF");
    const command = expectSimpleCommand(
      script.statements[0].pipelines[0].commands[0],
    );

    expect(command.redirections).toHaveLength(1);
    expect(command.redirections[0].fdVariable).toBe("input");
    expect(command.redirections[0].target.type).toBe("HereDoc");
  });
});
