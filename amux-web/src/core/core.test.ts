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
