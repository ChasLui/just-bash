import { describe, expect, it } from "vitest";
import type { CommandNode } from "../ast/types.js";
import { Bash } from "../Bash.js";

describe("unknown command nodes", () => {
  it("fails instead of treating an unknown node as successful", async () => {
    const bash = new Bash();
    bash.registerTransformPlugin({
      name: "unknown-command-node",
      transform: ({ ast }) => {
        ast.statements[0].pipelines[0].commands = [
          { type: "UnknownCommand" } as unknown as CommandNode,
        ];
        return { ast };
      },
    });

    await expect(bash.exec("true")).rejects.toThrow(
      'Unsupported command node: {"type":"UnknownCommand"}',
    );
  });
});
