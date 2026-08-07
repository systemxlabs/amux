/**
 * amux server 常驻进程入口。
 * 由机器自行启动（手动 / systemd / 安装脚本）；GUI 不负责拉起。
 * 启动流程：配置 → token → 注册表/历史 → 恢复会话 → 监听 WebSocket。
 */

import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { Broadcaster } from "./broadcast.js";
import { loadConfig, requireToken } from "./config.js";
import { GitRunner } from "./git.js";
import { defaultHarnessSpecs, HarnessRegistry } from "./harness.js";
import { HistoryStore } from "./history.js";
import { encodeMessage, errorResponse, parseJsonRpc } from "./jsonrpc.js";
import { SessionRegistry } from "./registry.js";
import { RpcServer } from "./rpc.js";
import { buildMethodHandlers } from "./rpchandler.js";
import { SessionManager } from "./sessions.js";
import { Transport } from "./transport.js";

export const SERVER_VERSION = "0.1.0";

async function main(): Promise<void> {
  const config = loadConfig(process.argv.slice(2));
  let token: string;
  try {
    token = requireToken(config.token);
  } catch (e) {
    console.error((e as Error).message);
    process.exit(1);
  }
  mkdirSync(config.dataDir, { recursive: true });

  const registry = new SessionRegistry(join(config.dataDir, "sessions.json"));
  registry.load();
  const history = new HistoryStore(join(config.dataDir, "history"));
  const broadcaster = new Broadcaster();
  const harnesses = new HarnessRegistry(defaultHarnessSpecs(config));
  const manager = new SessionManager({
    registry,
    history,
    broadcast: broadcaster,
    harnesses,
    logger: (line) => console.log(line),
  });
  const git = new GitRunner();

  const rpc = new RpcServer();
  for (const [method, handler] of Object.entries(
    buildMethodHandlers({ manager, git, harnesses, serverVersion: SERVER_VERSION }),
  )) {
    rpc.register(method, handler);
  }

  const transport = new Transport({
    host: config.host,
    port: config.port,
    token,
    onConnection: (s) => broadcaster.add(s),
    onClose: (s) => broadcaster.remove(s),
    onMessage: (s, text) => {
      const frame = parseJsonRpc(text);
      if (frame.kind === "error") {
        s.send(encodeMessage(errorResponse(frame.error)));
        return;
      }
      if (frame.kind === "request" || frame.kind === "notification") {
        // 连接上下文：按连接补齐状态 + 向本连接发送通知（get_history 缺口补发用）
        const catchup = broadcaster.catchupFor(s);
        const ctx =
          catchup === undefined
            ? undefined
            : {
                catchup,
                send: (method: string, params: unknown) => broadcaster.sendTo(s, method, params),
              };
        void rpc
          .handle(frame.kind === "request" ? frame.request : frame.notification, ctx)
          .then(({ response, afterSend }) => {
            if (response && frame.kind === "request") s.send(encodeMessage(response));
            afterSend?.();
          })
          .catch((e) => {
            console.error(`处理消息失败: ${(e as Error).message}`);
          });
      }
      // response 帧：server 侧忽略
    },
    logger: (line) => console.log(line),
  });

  await manager.restore();
  await transport.start();

  const addr = transport.address();
  console.log(`amux server v${SERVER_VERSION} listening on ws://${addr.host}:${addr.port} (数据目录: ${config.dataDir})`);

  let shuttingDown = false;
  const shutdown = async (sig: string): Promise<void> => {
    if (shuttingDown) return;
    shuttingDown = true;
    console.log(`收到 ${sig}，正在关闭…`);
    await manager.shutdown();
    await transport.close();
    process.exit(0);
  };
  process.on("SIGINT", () => void shutdown("SIGINT"));
  process.on("SIGTERM", () => void shutdown("SIGTERM"));
}

main().catch((e) => {
  console.error(`启动失败: ${(e as Error).message}`);
  process.exit(1);
});
