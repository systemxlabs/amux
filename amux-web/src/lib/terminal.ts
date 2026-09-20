// 终端输出：base64 与 xterm.js 需要的原始字节互转。

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
