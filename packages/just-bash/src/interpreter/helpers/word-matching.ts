/**
 * Interpreter Utility Functions
 *
 * Standalone helper functions used by the interpreter.
 */

import type { WordNode } from "../../ast/types.js";

/**
 * Check if a WordNode is a literal match for any of the given strings.
 * Returns true only if the word is a single literal (no expansions, no quoting)
 * that matches one of the target strings.
 *
 * This is used to detect assignment builtins at "parse time" - bash determines
 * whether a command is export/declare/etc based on the literal token, not the
 * runtime value after expansion.
 */
export function isWordLiteralMatch(word: WordNode, targets: string[]): boolean {
  // Must be a single part
  if (word.parts.length !== 1) {
    return false;
  }
  const part = word.parts[0];
  // Must be a simple literal (not quoted, not an expansion)
  if (part.type !== "Literal") {
    return false;
  }
  return targets.includes(part.value);
}
