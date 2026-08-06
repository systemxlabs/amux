/**
 * server 配置与 token：命令行参数 / 环境变量 / 数据目录 config.json 三级覆盖。
 * 数据目录默认 ~/.amux/server；token 首次启动生成、仅展示一次（存 token 文件）。
 */

import { randomBytes } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export interface ServerConfig {
  host: string;
  port: number;
  dataDir: string;
  /** 每会话有界事件缓冲条数 */
  maxBufferEvents: number;
  harnesses: Record<string, { defaultModel?: string }>;
}

export function defaultDataDir(): string {
  return process.env.AMUX_DATA_DIR ?? join(homedir(), ".amux", "server");
}

function parseArgs(argv: string[]): Record<string, string> {
  const out: Record<string, string> = {};
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (!arg.startsWith("--")) continue;
    const eq = arg.indexOf("=");
    if (eq !== -1) {
      out[arg.slice(2, eq)] = arg.slice(eq + 1);
    } else {
      const key = arg.slice(2);
      const next = argv[i + 1];
      if (next !== undefined && !next.startsWith("--")) {
        out[key] = next; // --flag value（空格分隔）
        i++;
      } else {
        out[key] = "true";
      }
    }
  }
  return out;
}

function toInt(v: string | undefined): number | undefined {
  if (v === undefined) return undefined;
  const n = Number(v);
  return Number.isInteger(n) && n > 0 ? n : undefined;
}

export function loadConfig(argv: string[]): ServerConfig {
  const args = parseArgs(argv);
  const dataDir = args["data-dir"] ?? defaultDataDir();
  let fileConfig: Partial<ServerConfig> = {};
  const configFile = join(dataDir, "config.json");
  if (existsSync(configFile)) {
    try {
      fileConfig = JSON.parse(readFileSync(configFile, "utf8")) as Partial<ServerConfig>;
    } catch {
      // 损坏则忽略，用默认值
    }
  }
  return {
    host: args.host ?? process.env.AMUX_HOST ?? fileConfig.host ?? "127.0.0.1",
    port: toInt(args.port) ?? toInt(process.env.AMUX_PORT) ?? (typeof fileConfig.port === "number" ? fileConfig.port : undefined) ?? 34567,
    dataDir,
    maxBufferEvents: toInt(args["max-buffer"]) ?? (typeof fileConfig.maxBufferEvents === "number" ? fileConfig.maxBufferEvents : undefined) ?? 2000,
    harnesses: fileConfig.harnesses ?? {},
  };
}

export function loadOrCreateToken(dataDir: string): { token: string; newlyCreated: boolean } {
  const file = join(dataDir, "token");
  if (existsSync(file)) {
    const t = readFileSync(file, "utf8").trim();
    if (t) return { token: t, newlyCreated: false };
  }
  const token = randomBytes(24).toString("base64url");
  mkdirSync(dataDir, { recursive: true });
  writeFileSync(file, token + "\n", { mode: 0o600 });
  return { token, newlyCreated: true };
}
