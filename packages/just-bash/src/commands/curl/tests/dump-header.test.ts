/**
 * Tests for curl -D/--dump-header
 */

import {
  afterAll,
  beforeAll,
  beforeEach,
  describe,
  expect,
  it,
  vi,
} from "vitest";
import { Bash } from "../../../Bash.js";

const originalFetch = global.fetch;

const mockFetch = vi.fn(async (url: string, _options?: RequestInit) => {
  const href = String(url);
  if (href.includes("/redirect")) {
    return new Response("", {
      status: 302,
      statusText: "Found",
      headers: {
        location: "https://api.example.com/final",
        "x-hop": "1",
      },
    });
  }
  if (href.includes("/final")) {
    return new Response('{"ok":true}', {
      status: 200,
      statusText: "OK",
      headers: {
        "content-type": "application/json",
        "x-custom": "yes",
      },
    });
  }
  if (href.includes("/fail")) {
    return new Response("nope", {
      status: 404,
      statusText: "Not Found",
      headers: {
        "content-type": "text/plain",
        "x-error": "missing",
      },
    });
  }
  return new Response('{"ok":true}', {
    status: 200,
    statusText: "OK",
    headers: {
      "content-type": "application/json",
      "x-custom": "yes",
    },
  });
});

beforeAll(() => {
  global.fetch = mockFetch as typeof fetch;
});

afterAll(() => {
  global.fetch = originalFetch;
});

const createEnv = () =>
  new Bash({
    network: { allowedUrlPrefixes: ["https://api.example.com"] },
  });

describe("curl -D/--dump-header", () => {
  beforeEach(() => {
    mockFetch.mockClear();
  });

  it("writes headers to a file with -D", async () => {
    const env = createEnv();
    const result = await env.exec(
      "curl -s -D /tmp/headers.txt -o /tmp/body.json https://api.example.com/test",
    );
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");

    const headers = await env.exec("cat /tmp/headers.txt");
    expect(headers.stdout).toBe(
      "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-custom: yes\r\n\r\n",
    );

    const body = await env.exec("cat /tmp/body.json");
    expect(body.stdout).toBe('{"ok":true}');
  });

  it("accepts --dump-header FILE", async () => {
    const env = createEnv();
    const result = await env.exec(
      "curl -s --dump-header /tmp/h2.txt https://api.example.com/test",
    );
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toBe('{"ok":true}');

    const headers = await env.exec("cat /tmp/h2.txt");
    expect(headers.stdout).toBe(
      "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-custom: yes\r\n\r\n",
    );
  });

  it("accepts --dump-header=FILE", async () => {
    const env = createEnv();
    const result = await env.exec(
      "curl -s --dump-header=/tmp/h-eq.txt -o /dev/null https://api.example.com/test",
    );
    expect(result.exitCode).toBe(0);
    const headers = await env.exec("cat /tmp/h-eq.txt");
    expect(headers.stdout).toContain("x-custom: yes");
  });

  it("accepts attached -Dfile form", async () => {
    const env = createEnv();
    const result = await env.exec(
      "curl -s -D/tmp/h3.txt -o /dev/null https://api.example.com/test",
    );
    expect(result.exitCode).toBe(0);

    const headers = await env.exec("cat /tmp/h3.txt");
    expect(headers.stdout).toContain("x-custom: yes");
  });

  it("writes headers to stdout with -D -", async () => {
    const env = createEnv();
    const result = await env.exec(
      "curl -s -D - -o /tmp/body-only.json https://api.example.com/test",
    );
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toBe(
      "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-custom: yes\r\n\r\n",
    );
    expect(result.stderr).toBe("");

    const body = await env.exec("cat /tmp/body-only.json");
    expect(body.stdout).toBe('{"ok":true}');
  });

  it("emits -D - header block even with -v -o", async () => {
    const env = createEnv();
    const result = await env.exec(
      "curl -s -v -D - -o /tmp/body-v.json https://api.example.com/test",
    );
    expect(result.exitCode).toBe(0);
    expect(result.stdout.startsWith("HTTP/1.1 200 OK\r\n")).toBe(true);
    expect(result.stdout).toContain("x-custom: yes");
    expect(result.stdout).toContain("> GET");
    const body = await env.exec("cat /tmp/body-v.json");
    expect(body.stdout).toBe('{"ok":true}');
  });

  it("dumps 4xx headers with -f -D before failing", async () => {
    const env = createEnv();
    const result = await env.exec(
      "curl -s -f -D /tmp/fail-headers.txt -o /dev/null https://api.example.com/fail",
    );
    expect(result.exitCode).toBe(22);
    expect(result.stdout).toBe("");
    const headers = await env.exec("cat /tmp/fail-headers.txt");
    expect(headers.stdout).toBe(
      "HTTP/1.1 404 Not Found\r\ncontent-type: text/plain\r\nx-error: missing\r\n\r\n",
    );
  });

  it("clears a stale dump file on the same VFS when the request fails", async () => {
    const env = createEnv();
    await env.exec("printf 'STALE' > /tmp/stale2.txt");
    // Override fetch to throw after the dump file was truncated.
    const boom = vi.fn(async () => {
      throw new Error("connect failed");
    });
    const prev = global.fetch;
    global.fetch = boom as typeof fetch;
    try {
      const result = await env.exec(
        "curl -s -D /tmp/stale2.txt -o /dev/null https://api.example.com/test",
      );
      expect(result.exitCode).not.toBe(0);
      const dumped = await env.exec("cat /tmp/stale2.txt");
      expect(dumped.stdout).toBe("");
    } finally {
      global.fetch = prev;
    }
  });

  it("dumps intermediate redirect headers with -D -L", async () => {
    const env = createEnv();
    const result = await env.exec(
      "curl -s -L -D /tmp/redir.txt -o /dev/null https://api.example.com/redirect",
    );
    expect(result.exitCode).toBe(0);
    const headers = await env.exec("cat /tmp/redir.txt");
    expect(headers.stdout).toContain("HTTP/1.1 302 Found\r\n");
    expect(headers.stdout).toContain(
      "location: https://api.example.com/final\r\n",
    );
    expect(headers.stdout).toContain("x-hop: 1\r\n");
    expect(headers.stdout).toContain(
      "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-custom: yes\r\n\r\n",
    );
    // Intermediate hop then final response, in order
    expect(headers.stdout.indexOf("302")).toBeLessThan(
      headers.stdout.indexOf("200 OK"),
    );
  });

  it("prepends headers before the body with -D - and no -o", async () => {
    const env = createEnv();
    const result = await env.exec("curl -s -D - https://api.example.com/test");
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toBe(
      'HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-custom: yes\r\n\r\n{"ok":true}',
    );
  });

  it("rejects missing -D argument", async () => {
    const env = createEnv();
    const result = await env.exec("curl -s -D");
    expect(result.exitCode).toBe(1);
    expect(result.stderr).toBe("curl: option -D: requires parameter\n");
    expect(result.stdout).toBe("");
  });

  it("rejects blank -D argument", async () => {
    const env = createEnv();
    const result = await env.exec("curl -s -D '' https://api.example.com/test");
    expect(result.exitCode).toBe(1);
    expect(result.stderr).toBe(
      "curl: option -D: blank argument where content is expected\n",
    );
  });
});
