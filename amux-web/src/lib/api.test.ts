// 驱动真实 `ApiClient`：起一个真实 HTTP server，校验请求头、请求体与响应解码（不打桩被测单元）。

import { createServer, type IncomingMessage, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";

import { ApiClient, errorMessage } from "./api";

type Seen = { url: string; method: string; authorization: string | undefined; body: string };

let server: Server;
let base = "";
const seen: Seen[] = [];
/** 队列化的响应体，供 POST/PUT/DELETE 逐条取用。 */
let replies: { status: number; body: string }[] = [];

function readBody(request: IncomingMessage): Promise<string> {
  return new Promise((resolve) => {
    let data = "";
    request.on("data", (chunk) => (data += chunk));
    request.on("end", () => resolve(data));
  });
}

beforeAll(async () => {
  server = createServer(async (request, response) => {
    const body = await readBody(request);
    seen.push({
      url: request.url ?? "",
      method: request.method ?? "",
      authorization: request.headers.authorization,
      body,
    });
    if (request.headers.authorization !== "Bearer tk") {
      response.writeHead(401, { "content-type": "text/plain" });
      response.end("unauthorized\n");
      return;
    }
    const url = request.url ?? "";
    if (url === "/machines") {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify([
          {
            name: "localpc",
            os: "linux",
            arch: "x86_64",
            hostname: "pc",
            tempDir: "/tmp/amux",
            version: "0.1.0",
          },
        ]),
      );
      return;
    }
    if (/^\/machines\/[^/]+\/list_dir\?/.test(url)) {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          path: "/home/",
          entries: [{ name: "tom", path: "/home/tom", isDir: true, size: 0 }],
          hasMore: false,
          nextOffset: 0,
        }),
      );
      return;
    }
    if (/^\/sessions\/[^/]+\/history\?/.test(url)) {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          items: [
            { role: "user", id: "m1", content: [{ type: "text", text: "hi" }], timestamp: 7 },
          ],
          hasMore: false,
        }),
      );
      return;
    }
    if (/^\/sessions\/[^/]+\/terminals\/[^/]+$/.test(url) && request.method === "GET") {
      response.writeHead(200, { "content-type": "text/event-stream" });
      response.write('event: output\ndata: {"data":"aGk=","nextCursor":2}\n\n');
      response.write('event: output\ndata: {"data":"IQ==","nextCursor":3}\n\n');
      response.end();
      return;
    }
    const reply = replies.shift();
    if (!reply) {
      response.writeHead(404, { "content-type": "text/plain" });
      response.end("会话不存在\n");
      return;
    }
    response.writeHead(reply.status, { "content-type": "application/json" });
    response.end(reply.body);
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", () => resolve()));
  base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
});

afterAll(async () => {
  await new Promise<void>((resolve) => server.close(() => resolve()));
});

describe("ApiClient", () => {
  it("携带 Bearer token 并解析 camelCase 响应", async () => {
    const client = new ApiClient(base, "tk");
    const machines = await client.machines();
    expect(machines).toHaveLength(1);
    expect(machines[0].tempDir).toBe("/tmp/amux");
    expect(seen.at(-1)?.authorization).toBe("Bearer tk");
  });

  it("回答非 2xx 时给出带状态码与响应体的错误消息", async () => {
    const client = new ApiClient(base, "bad");
    await expect(client.machines()).rejects.toThrow("HTTP 401: unauthorized");
  });

  it("并发 401 只触发一次认证回调并保留状态码", async () => {
    const onUnauthorized = vi.fn();
    const client = new ApiClient(
      base,
      "bad",
      globalThis.fetch.bind(globalThis),
      onUnauthorized,
    );
    const results = await Promise.allSettled([client.machines(), client.sessions(1, 0)]);

    expect(results.every((result) => result.status === "rejected")).toBe(true);
    expect(onUnauthorized).toHaveBeenCalledTimes(1);
    expect(onUnauthorized).toHaveBeenCalledWith(client);
    const error = results[0].status === "rejected" ? results[0].reason : null;
    expect(error).toMatchObject({ status: 401, message: "HTTP 401: unauthorized" });
  });

  it("list_dir 按查询参数传递路径与仅目录开关，响应解析为条目", async () => {
    const client = new ApiClient(base, "tk");
    const result = await client.listDir("localpc", "/home/me/My Docs", 500, 0, true);
    expect(result.entries[0]).toEqual({
      name: "tom",
      path: "/home/tom",
      isDir: true,
      size: 0,
    });
    const url = new URL(seen.at(-1)!.url, base);
    expect(url.pathname).toBe("/machines/localpc/list_dir");
    expect(url.searchParams.get("path")).toBe("/home/me/My Docs");
    expect(url.searchParams.get("dirs_only")).toBe("true");
  });

  it("仅目录开关为假时省略参数（服务端默认为假）", async () => {
    const client = new ApiClient(base, "tk");
    await client.listDir("localpc", "/home");
    const url = new URL(seen.at(-1)!.url, base);
    expect(url.searchParams.has("dirs_only")).toBe(false);
  });

  it("发送指令把内容块作为请求体，配置会话按需带上标题与选项", async () => {
    replies = [
      { status: 200, body: JSON.stringify({ ok: true }) },
      { status: 200, body: JSON.stringify({ ok: true }) },
      { status: 200, body: JSON.stringify({ ok: true }) },
      { status: 200, body: JSON.stringify({ ok: true }) },
    ];
    const client = new ApiClient(base, "tk");
    await client.promptSession("s1", [{ type: "text", text: "你好" }]);
    expect(seen.at(-1)?.method).toBe("POST");
    expect(seen.at(-1)?.url).toBe("/sessions/s1");
    expect(JSON.parse(seen.at(-1)!.body)).toEqual({
      input: [{ type: "text", text: "你好" }],
    });

    await client.configureSession("s1", null, {
      configId: "model",
      type: "value_id",
      value: "gpt-5",
    });
    expect(seen.at(-1)?.url).toBe("/sessions/s1/configure");
    expect(JSON.parse(seen.at(-1)!.body)).toEqual({
      config: { configId: "model", type: "value_id", value: "gpt-5" },
    });

    await client.configureSession("s1", null, null, null);
    expect(JSON.parse(seen.at(-1)!.body)).toEqual({ project: null });

    await client.configureWorkflow("w1", null, null);
    expect(JSON.parse(seen.at(-1)!.body)).toEqual({ project: null });
  });

  it("对话历史保留服务端返回的条目与时序号", async () => {
    const client = new ApiClient(base, "tk");
    const page = await client.history("s1", 20, 0);
    expect(page.items[0]).toEqual({
      role: "user",
      id: "m1",
      content: [{ type: "text", text: "hi" }],
      timestamp: 7,
    });
    expect(page.hasMore).toBe(false);
  });

  it("diff 编码基准分支，branches 解析分支标记", async () => {
    replies = [
      { status: 200, body: JSON.stringify({ files: [] }) },
      {
        status: 200,
        body: JSON.stringify({
          branches: [
            { name: "main", isWorktreeSource: true, isDefault: true },
            { name: "feature/login", isWorktreeSource: false, isDefault: false },
          ],
        }),
      },
    ];
    const client = new ApiClient(base, "tk");
    await client.diff("s1", "feature/login");
    const diffUrl = new URL(seen.at(-1)!.url, base);
    expect(diffUrl.pathname).toBe("/sessions/s1/diff");
    expect(diffUrl.searchParams.get("base")).toBe("feature/login");

    const result = await client.branches("s1");
    expect(seen.at(-1)?.url).toBe("/sessions/s1/branches");
    expect(result.branches[0]).toEqual({
      name: "main",
      isWorktreeSource: true,
      isDefault: true,
    });
  });

  it("终端 SSE 携带认证并逐事件解析输出", async () => {
    const client = new ApiClient(base, "tk");
    const outputs: { data: string; nextCursor: number }[] = [];
    await client.terminalOutputStream(
      "s1",
      "t1",
      new AbortController().signal,
      (output) => outputs.push(output),
    );

    expect(seen.at(-1)?.authorization).toBe("Bearer tk");
    expect(outputs).toEqual([
      { data: "aGk=", nextCursor: 2 },
      { data: "IQ==", nextCursor: 3 },
    ]);
  });

  it("错误消息拼接：空响应体只给状态码", () => {
    expect(errorMessage(404, "会话不存在\n")).toBe("HTTP 404: 会话不存在");
    expect(errorMessage(500, "")).toBe("HTTP 500");
  });
});
