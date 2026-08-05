/**
 * 最小 JSON-RPC 2.0 客户端（kimi acp stdio 传输，NDJSON 帧）。
 * 与 ahal-codex 的 jsonrpc.ts 同构（跨包不共享，保持包独立）。
 */
import { spawn, type ChildProcessByStdio } from "node:child_process";
import { createInterface, type Interface } from "node:readline";
import type { Readable, Writable } from "node:stream";

export interface JsonRpcRequest {
  jsonrpc: "2.0";
  id: number;
  method: string;
  params?: unknown;
}

export interface JsonRpcNotification {
  jsonrpc: "2.0";
  method: string;
  params?: unknown;
}

export interface JsonRpcResponse {
  jsonrpc: "2.0";
  id: number;
  result?: unknown;
  error?: { code: number; message: string; data?: unknown };
}

export type JsonRpcMessage = JsonRpcRequest | JsonRpcNotification | JsonRpcResponse;

export class JsonRpcClient {
  private proc: ChildProcessByStdio<Writable, Readable, Readable>;
  private rl: Interface;
  private nextId = 1;
  private pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: Error) => void }>();
  private listeners = new Set<(msg: JsonRpcNotification) => void>();
  private closed = false;

  constructor(
    cmd: string,
    args: string[],
    opts: { cwd?: string; env?: NodeJS.ProcessEnv; stderr?: (line: string) => void } = {},
  ) {
    this.proc = spawn(cmd, args, { stdio: ["pipe", "pipe", "pipe"], env: opts.env, cwd: opts.cwd });
    const onErr = opts.stderr ?? (() => {});
    this.proc.stderr.on("data", (d: Buffer) => {
      for (const line of d.toString().split("\n")) {
        if (line.trim()) onErr(line);
      }
    });
    this.rl = createInterface({ input: this.proc.stdout });
    this.rl.on("line", (line) => this.handleLine(line));
    this.proc.on("exit", (code, signal) => {
      this.closed = true;
      const err = new Error(`子进程退出 code=${code} signal=${signal}`);
      for (const [, p] of this.pending) p.reject(err);
      this.pending.clear();
    });
    this.proc.on("error", (e) => {
      this.closed = true;
      for (const [, p] of this.pending) p.reject(e);
      this.pending.clear();
    });
  }

  private handleLine(line: string) {
    if (!line.trim()) return;
    let msg: JsonRpcMessage;
    try {
      msg = JSON.parse(line) as JsonRpcMessage;
    } catch {
      return;
    }
    if ("method" in msg) {
      const notif = msg as JsonRpcNotification;
      for (const l of this.listeners) l(notif);
      return;
    }
    const resp = msg as JsonRpcResponse;
    const p = this.pending.get(resp.id);
    if (!p) return;
    this.pending.delete(resp.id);
    if (resp.error) {
      p.reject(new Error(`JSON-RPC error ${resp.error.code}: ${resp.error.message}`));
    } else {
      p.resolve(resp.result);
    }
  }

  request<T = unknown>(method: string, params?: unknown): Promise<T> {
    if (this.closed) return Promise.reject(new Error("连接已关闭"));
    const id = this.nextId++;
    const msg: JsonRpcRequest = { jsonrpc: "2.0", id, method, params };
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: (v) => resolve(v as T), reject });
      this.proc.stdin.write(JSON.stringify(msg) + "\n");
    });
  }

  onNotification(fn: (msg: JsonRpcNotification) => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    try {
      this.proc.stdin.end();
    } catch {
      /* ignore */
    }
    this.proc.kill("SIGTERM");
  }

  get isClosed(): boolean {
    return this.closed;
  }
}
