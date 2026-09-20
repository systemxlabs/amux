/** 增量解析 SSE `data:` 字段；注释、event/id 等字段对当前终端流无意义。 */
export function createSseDecoder(): { push(chunk: string): string[] } {
  let buffer = "";
  let data = "";

  return {
    push(chunk: string): string[] {
      buffer += chunk;
      const events: string[] = [];
      for (;;) {
        const lineEnd = buffer.indexOf("\n");
        if (lineEnd < 0) break;
        const line = buffer.slice(0, lineEnd).replace(/\r$/, "");
        buffer = buffer.slice(lineEnd + 1);
        if (line === "") {
          if (data !== "") {
            events.push(data);
            data = "";
          }
          continue;
        }
        if (!line.startsWith("data:")) continue;
        const value = line.slice(5).replace(/^ /, "");
        data = data === "" ? value : `${data}\n${value}`;
      }
      return events;
    },
  };
}
