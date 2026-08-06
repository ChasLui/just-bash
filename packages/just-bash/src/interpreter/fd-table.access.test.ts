import { describe, expect, it } from "vitest";
import {
  advanceFd,
  closeFd,
  dupFd,
  FIRST_USER_FD,
  getFdEntry,
  getRawFd,
  isFdOpen,
  readFd,
  restoreFds,
  setFdEntry,
  setRawFd,
  snapshotFds,
} from "./fd-table.js";
import type { InterpreterContext } from "./types.js";

/**
 * The descriptor table is stored as `Map<number, string>` because it is part
 * of the public CommandContext surface. These tests pin the encoding and the
 * read/advance semantics every redirection site relies on.
 */
function makeCtx(maxFileDescriptors = 64): InterpreterContext {
  return {
    state: { fileDescriptors: new Map<number, string>() },
    limits: { maxFileDescriptors },
  } as unknown as InterpreterContext;
}

describe("fd-table accessors", () => {
  it("reports the first user descriptor as 3", () => {
    expect(FIRST_USER_FD).toBe(3);
  });

  it("stores and reads back a typed entry", () => {
    const ctx = makeCtx();
    setFdEntry(ctx, 3, { kind: "input", content: "one\ntwo\n" });
    expect(isFdOpen(ctx, 3)).toBe(true);
    expect(getRawFd(ctx, 3)).toBe("one\ntwo\n");
    expect(getFdEntry(ctx, 3)).toEqual({
      kind: "input",
      content: "one\ntwo\n",
    });
  });

  it("reports an unopened descriptor as closed", () => {
    const ctx = makeCtx();
    expect(isFdOpen(ctx, 3)).toBe(false);
    expect(getFdEntry(ctx, 3)).toBeUndefined();
    expect(readFd(ctx, 3)).toEqual({ error: "not-open" });
  });

  it("closes a descriptor", () => {
    const ctx = makeCtx();
    setFdEntry(ctx, 3, { kind: "input", content: "x" });
    closeFd(ctx, 3);
    expect(isFdOpen(ctx, 3)).toBe(false);
  });

  it("closing an unopened descriptor is a no-op", () => {
    const ctx = makeCtx();
    closeFd(ctx, 9);
    expect(ctx.state.fileDescriptors?.size).toBe(0);
  });

  it("charges the descriptor limit only for newly opened fds", () => {
    const ctx = makeCtx(2);
    setRawFd(ctx, 3, "a");
    setRawFd(ctx, 4, "b");
    // Rewriting an open descriptor (what a read does) must not be charged.
    expect(() => setRawFd(ctx, 3, "a2")).not.toThrow();
    expect(() => setRawFd(ctx, 5, "c")).toThrowError(
      /too many open file descriptors \(max 2\)/,
    );
  });
});

describe("fd-table reading", () => {
  it("returns the unread remainder of an input descriptor", () => {
    const ctx = makeCtx();
    setFdEntry(ctx, 3, { kind: "input", content: "one\ntwo\n" });
    expect(readFd(ctx, 3)).toEqual({ content: "one\ntwo\n" });
  });

  it("returns the content after the position of a read-write descriptor", () => {
    const ctx = makeCtx();
    setFdEntry(ctx, 3, {
      kind: "readwrite",
      path: "/tmp/rw",
      position: 4,
      content: "one\ntwo\n",
    });
    expect(readFd(ctx, 3)).toEqual({ content: "two\n" });
  });

  it("refuses to read a write-only descriptor", () => {
    const ctx = makeCtx();
    setFdEntry(ctx, 3, { kind: "output", path: "/tmp/o", append: false });
    expect(readFd(ctx, 3)).toEqual({ error: "write-only" });
    setFdEntry(ctx, 4, { kind: "dup-out", sourceFd: 1 });
    expect(readFd(ctx, 4)).toEqual({ error: "write-only" });
  });

  it("advances the shared position of an input descriptor", () => {
    const ctx = makeCtx();
    setFdEntry(ctx, 3, { kind: "input", content: "one\ntwo\n" });
    advanceFd(ctx, 3, 4);
    expect(readFd(ctx, 3)).toEqual({ content: "two\n" });
    advanceFd(ctx, 3, 4);
    expect(readFd(ctx, 3)).toEqual({ content: "" });
  });

  it("advances the position of a read-write descriptor without losing content", () => {
    const ctx = makeCtx();
    setFdEntry(ctx, 3, {
      kind: "readwrite",
      path: "/tmp/rw",
      position: 0,
      content: "one\ntwo\n",
    });
    advanceFd(ctx, 3, 4);
    expect(getFdEntry(ctx, 3)).toEqual({
      kind: "readwrite",
      path: "/tmp/rw",
      position: 4,
      content: "one\ntwo\n",
    });
  });

  it("advancing an unopened descriptor is a no-op", () => {
    const ctx = makeCtx();
    advanceFd(ctx, 3, 10);
    expect(ctx.state.fileDescriptors?.size).toBe(0);
  });
});

describe("fd-table snapshot/restore", () => {
  it("restores a descriptor's previous content, including its position", () => {
    const ctx = makeCtx();
    setFdEntry(ctx, 3, { kind: "input", content: "b\nc\n" });
    const snapshot = snapshotFds(ctx, [3]);
    setFdEntry(ctx, 3, { kind: "input", content: "X\nY\n" });
    advanceFd(ctx, 3, 2);
    restoreFds(ctx, snapshot);
    expect(readFd(ctx, 3)).toEqual({ content: "b\nc\n" });
  });

  it("closes descriptors that were not open when the snapshot was taken", () => {
    const ctx = makeCtx();
    const snapshot = snapshotFds(ctx, [3, 4]);
    setFdEntry(ctx, 3, { kind: "input", content: "x" });
    setFdEntry(ctx, 4, { kind: "output", path: "/tmp/o", append: false });
    restoreFds(ctx, snapshot);
    expect(isFdOpen(ctx, 3)).toBe(false);
    expect(isFdOpen(ctx, 4)).toBe(false);
  });

  it("records the first value seen for a repeated descriptor", () => {
    const ctx = makeCtx();
    setFdEntry(ctx, 3, { kind: "input", content: "first" });
    const snapshot = snapshotFds(ctx, [3, 3]);
    expect(snapshot.size).toBe(1);
    setFdEntry(ctx, 3, { kind: "input", content: "second" });
    restoreFds(ctx, snapshot);
    expect(readFd(ctx, 3)).toEqual({ content: "first" });
  });

  it("restores the content classification, not just the raw value", () => {
    const ctx = makeCtx();
    setFdEntry(ctx, 3, { kind: "input", content: "__file__:/tmp/pwn" });
    const snapshot = snapshotFds(ctx, [3]);
    setFdEntry(ctx, 3, { kind: "output", path: "/tmp/o", append: false });
    restoreFds(ctx, snapshot);
    expect(getFdEntry(ctx, 3)).toEqual({
      kind: "input",
      content: "__file__:/tmp/pwn",
    });
  });

  it("leaves no classification behind for a descriptor it closes", () => {
    const ctx = makeCtx();
    const snapshot = snapshotFds(ctx, [3]);
    setFdEntry(ctx, 3, { kind: "input", content: "__file__:/tmp/pwn" });
    restoreFds(ctx, snapshot);
    ctx.state.fileDescriptors?.set(3, "__file__:/tmp/real");
    expect(getFdEntry(ctx, 3)).toEqual({
      kind: "output",
      path: "/tmp/real",
      append: false,
    });
  });
});

describe("fd-table shared open file descriptions", () => {
  const openThree = (ctx: ReturnType<typeof makeCtx>) =>
    setFdEntry(ctx, 3, { kind: "input", content: "l1\nl2\nl3\n" });

  it("advances every alias when one of them is read", () => {
    const ctx = makeCtx();
    openThree(ctx);
    dupFd(ctx, 4, 3);
    advanceFd(ctx, 4, 3);
    expect(readFd(ctx, 3)).toEqual({ content: "l2\nl3\n" });
    expect(readFd(ctx, 4)).toEqual({ content: "l2\nl3\n" });
  });

  it("keeps the position on the survivor when an alias is closed", () => {
    const ctx = makeCtx();
    openThree(ctx);
    dupFd(ctx, 4, 3);
    advanceFd(ctx, 3, 3);
    closeFd(ctx, 3);
    expect(readFd(ctx, 4)).toEqual({ content: "l2\nl3\n" });
  });

  it("shares one description across a chain of duplicates", () => {
    const ctx = makeCtx();
    openThree(ctx);
    dupFd(ctx, 4, 3);
    dupFd(ctx, 5, 4);
    advanceFd(ctx, 5, 3);
    expect(readFd(ctx, 3)).toEqual({ content: "l2\nl3\n" });
    expect(readFd(ctx, 4)).toEqual({ content: "l2\nl3\n" });
  });

  it("stops sharing when a descriptor is re-opened", () => {
    const ctx = makeCtx();
    openThree(ctx);
    dupFd(ctx, 4, 3);
    setFdEntry(ctx, 4, { kind: "input", content: "g1\ng2\n" });
    advanceFd(ctx, 4, 3);
    expect(readFd(ctx, 3)).toEqual({ content: "l1\nl2\nl3\n" });
    expect(readFd(ctx, 4)).toEqual({ content: "g2\n" });
  });

  it("does not restore the offset a scoped alias moved", () => {
    const ctx = makeCtx();
    openThree(ctx);
    const snapshot = snapshotFds(ctx, [4]);
    dupFd(ctx, 4, 3);
    advanceFd(ctx, 4, 3);
    restoreFds(ctx, snapshot);
    expect(isFdOpen(ctx, 4)).toBe(false);
    expect(readFd(ctx, 3)).toEqual({ content: "l2\nl3\n" });
  });

  it("re-attaches a restored descriptor to a surviving co-member", () => {
    const ctx = makeCtx();
    openThree(ctx);
    dupFd(ctx, 4, 3);
    const snapshot = snapshotFds(ctx, [4]);
    setFdEntry(ctx, 4, { kind: "input", content: "g1\n" });
    restoreFds(ctx, snapshot);
    advanceFd(ctx, 3, 3);
    expect(readFd(ctx, 4)).toEqual({ content: "l2\nl3\n" });
  });
});
