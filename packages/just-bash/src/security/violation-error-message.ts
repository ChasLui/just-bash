import type { SecurityViolationType } from "./types.js";

const NON_EXCLUDABLE_VIOLATION_TYPES: ReadonlySet<SecurityViolationType> =
  new Set([
    "function_constructor",
    "async_function_constructor",
    "generator_function_constructor",
    "async_generator_function_constructor",
  ]);

export function assertExcludableViolationTypes(
  violationTypes: readonly SecurityViolationType[] | undefined,
): void {
  const nonExcludable = violationTypes?.find((type) =>
    NON_EXCLUDABLE_VIOLATION_TYPES.has(type),
  );
  if (nonExcludable) {
    throw new RangeError(
      `defenseInDepth.excludeViolationTypes cannot disable the non-excludable "${nonExcludable}" protection`,
    );
  }
}

const DEFENSE_IN_DEPTH_NOTICE =
  "\n\nThis is a defense-in-depth measure and indicates a bug in just-bash. " +
  "Please report this at security@vercel.com";

export function formatViolationErrorMessage(
  message: string,
  violationType: SecurityViolationType,
  canExclude: boolean,
): string {
  const exclusionHint =
    canExclude && !NON_EXCLUDABLE_VIOLATION_TYPES.has(violationType)
      ? `\n\nIf this access is required by trusted host-runtime code, add "${violationType}" to defenseInDepth.excludeViolationTypes.`
      : "";
  return message + DEFENSE_IN_DEPTH_NOTICE + exclusionHint;
}
