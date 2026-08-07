/**
 * server 配置与 token：命令行参数 / 环境变量 / 数据目录 config.json 三级覆盖。
 * 数据目录默认 ~/.amux/server。
 *
 * 认证 token 不落盘：每次启动由用户指定，统一名称 token——
 * `--token <值>` 或环境变量 `AMUX_TOKEN`；未指定则拒绝启动。
 */

import { existsSync, mkdirSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export interface ServerConfig {
  host: string;
  port: number;
  dataDir: string;
  harnesses: Record<string, { defaultModel?: string }>;
  /** 本次启动指定的 token（--token / AMUX_TOKEN）；未指定则无法启动 */
  token?: string;
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
    harnesses: fileConfig.harnesses ?? {},
    token: args.token ?? process.env.AMUX_TOKEN ?? undefined,
  };
}

/**
 * 解析认证 token：仅接受用户显式指定（--token / AMUX_TOKEN），
 * 不落盘、不生成——每次启动都必须指定，未指定则抛错（由启动入口拒绝启动）。
 */
export function requireToken(provided: string | undefined): string {
  if (provided !== undefined && provided.length > 0) return provided;
  throw new Error("未指定认证 token：请用 --token <值> 或环境变量 AMUX_TOKEN 指定后启动（token 不落盘，每次启动需重新指定）");
}
