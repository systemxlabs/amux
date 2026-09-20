import { describe, expect, it } from "vitest";

import type { ApiClient } from "../lib/api";
import { Core } from "./core";

describe("Core.invalidateAuthentication", () => {
  it("当前连接认证失效时清空客户端并进入登录页", () => {
    const core = new Core();
    const client = {} as ApiClient;
    core.client = client;
    core.state.status = "online";
    core.state.notice = { kind: "error", text: "旧错误" };

    core.invalidateAuthentication(client);

    expect(core.client).toBeNull();
    expect(core.state.status).toBe("failed");
    expect(core.state.error).toBe("登录已失效，请重新输入 token");
    expect(core.state.notice).toBeNull();
  });

  it("旧连接的迟到 401 不影响重新登录后的新连接", () => {
    const core = new Core();
    const stale = {} as ApiClient;
    const current = {} as ApiClient;
    core.client = current;
    core.state.status = "online";

    core.invalidateAuthentication(stale);

    expect(core.client).toBe(current);
    expect(core.state.status).toBe("online");
  });
});

describe("Core terminal chunks", () => {
  it("批量事件按序保留，并只确认已写入的序号", () => {
    const core = new Core();
    core.pushTerminalChunk(new Uint8Array([1]), true);
    core.pushTerminalChunk(new Uint8Array([2]), false);
    core.acknowledgeTerminalChunks(1);
    expect(core.state.detail.terminalChunks.map((chunk) => chunk.seq)).toEqual([2]);

    core.pushTerminalChunk(new Uint8Array([3]), false);
    core.acknowledgeTerminalChunks(2);
    expect(core.state.detail.terminalChunks.map((chunk) => chunk.seq)).toEqual([3]);

    core.resetDetail();
    core.pushTerminalChunk(new Uint8Array([4]), true);
    expect(core.state.detail.terminalChunks[0].seq).toBe(4);
  });
});
