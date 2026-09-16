// 终端输出：按游标增量拉取，base64 解码为可直接写入 xterm.js 的字节
// （docs/DESIGN.md「终端视图」）。

import type { TerminalOutput } from "./types";

/** 终端读取游标；null 表示从头拉取完整输出。 */
export type TerminalStream = { cursor: number | null };

export function newTerminalStream(): TerminalStream {
  return { cursor: null };
}

/** base64（标准字母表，服务端输出）解码为原始字节。 */
export function decodeBase64(data: string): Uint8Array {
  if (data === "") return new Uint8Array(0);
  const binary = atob(data);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

/** xterm.js 输入需要的 base64 编码。 */
export function encodeBase64(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

/**
 * 并入一次增量输出。
 *
 * 服务端报告 `truncated` 时请求的游标早于缓存起点，本地缓冲需整体重建（`reset` 为 true）。
 */
export function appendTerminalOutput(output: TerminalOutput): {
  stream: TerminalStream;
  bytes: Uint8Array;
  reset: boolean;
} {
  return {
    stream: { cursor: output.nextCursor },
    bytes: decodeBase64(output.data),
    reset: output.truncated === true,
  };
}
