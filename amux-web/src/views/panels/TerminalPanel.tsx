// 终端面板：终端列表 + xterm.js 视图（docs/PRD.md「主页面」、docs/DESIGN.md「终端视图」）。

import { useEffect, useRef } from "react";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";

import { Button } from "../../components/ui/button";
import {
  closeTerminal,
  openTerminal,
  resizeTerminal,
  selectTerminal,
  sendTerminalInput,
} from "../../core/actions";
import { useCore, useCoreState } from "../../core/store";
import { truncate } from "../../lib/format";
import { cn } from "../../lib/utils";

/** 未知终端尺寸时的回退（xterm.js 默认值）。 */
const DEFAULT_COLS = 80;
const DEFAULT_ROWS = 24;
/** 字号对齐桌面应用（FONT_SIZE = 13px）。 */
const FONT_SIZE = 13;
/**
 * 行高：xterm.js 的 lineHeight 是「字体自然行高的倍数」，取 1 即自然行高，
 * 该字体在 13px 下的自然行高（约 17px）与桌面应用 1.3em 的格子高一致，因此不再额外放大。
 */
const LINE_HEIGHT = 1;

/** 取全局等宽字体栈（index.css 的 --font-mono）；读不到时退回系统等宽。 */
function monoFontFamily(): string {
  const value = getComputedStyle(document.documentElement).getPropertyValue("--font-mono").trim();
  return value === "" ? "monospace" : value;
}

/** 浅色终端 ANSI 调色板（取 VS Code Light 主题，保证白底上十六色都可读）。 */
const LIGHT_ANSI_COLORS = {
  black: "#000000",
  red: "#cd3131",
  green: "#107c10",
  yellow: "#7a6400",
  blue: "#0451a5",
  magenta: "#bc05bc",
  cyan: "#0598bc",
  white: "#555555",
  brightBlack: "#666666",
  brightRed: "#cd3131",
  brightGreen: "#107c10",
  brightYellow: "#7a6400",
  brightBlue: "#0451a5",
  brightMagenta: "#bc05bc",
  brightCyan: "#0598bc",
  brightWhite: "#555555",
  selectionBackground: "#bfdbfe",
};

export function TerminalPanel() {
  const core = useCore();
  const state = useCoreState();
  const containerRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const activeTerminal = state.detail.activeTerminal;
  const chunk = state.detail.chunk;

  // 活动终端变化时重建实例：VT 网格无法跨实例迁移，重建后的完整输出由后续 chunk 写入
  useEffect(() => {
    const container = containerRef.current;
    if (container === null || activeTerminal === null) return;
    const tokens = getComputedStyle(document.documentElement);
    const term = new Terminal({
      fontSize: FONT_SIZE,
      lineHeight: LINE_HEIGHT,
      fontFamily: monoFontFamily(),
      cursorBlink: true,
      theme: {
        background: tokens.getPropertyValue("--color-background").trim(),
        foreground: tokens.getPropertyValue("--color-foreground").trim(),
        cursor: tokens.getPropertyValue("--color-foreground").trim(),
        // 浅色底需要显式的 ANSI 调色板：xterm 默认的高亮色（黄/白）在白色背景上不可读
        ...LIGHT_ANSI_COLORS,
      },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(container);
    const onData = term.onData((data) => {
      void sendTerminalInput(core, new TextEncoder().encode(data));
    });
    termRef.current = term;

    fit.fit();
    void resizeTerminal(core, term.cols, term.rows);
    const observer = new ResizeObserver(() => {
      fit.fit();
      void resizeTerminal(core, term.cols, term.rows);
    });
    observer.observe(container);

    return () => {
      observer.disconnect();
      onData.dispose();
      term.dispose();
      termRef.current = null;
    };
  }, [core, activeTerminal]);

  // 输出：chunk 序号变化时写入；缓冲被服务端截断时先重置网格
  useEffect(() => {
    const term = termRef.current;
    if (term === null) return;
    if (chunk.reset) term.reset();
    if (chunk.bytes.length > 0) term.write(chunk.bytes);
  }, [chunk]);

  const createTerminal = () => {
    const term = termRef.current;
    void openTerminal(core, term?.cols ?? DEFAULT_COLS, term?.rows ?? DEFAULT_ROWS);
  };

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div data-slot="terminal-list" className="flex shrink-0 flex-wrap items-center gap-1 px-3 py-2">
        {state.detail.terminals.map((terminal) => (
          <div
            key={terminal.id}
            data-slot="terminal-item"
            className={cn(
              "flex cursor-pointer items-center gap-1 rounded-md px-2 py-1 text-xs",
              terminal.id === activeTerminal ? "bg-accent text-accent-foreground" : "bg-muted/40",
            )}
            onClick={() => selectTerminal(core, terminal.id)}
          >
            <span className="max-w-24 truncate">{truncate(terminal.id, 12)}</span>
            {terminal.state === "exited" && <span className="text-muted-foreground">退出</span>}
            <Button
              type="button"
              variant="ghost"
              size="sm"
              data-slot="terminal-close"
              onClick={(event) => {
                event.stopPropagation();
                void closeTerminal(core, terminal.id);
              }}
            >
              关闭
            </Button>
          </div>
        ))}
        <Button
          type="button"
          variant="ghost"
          size="sm"
          data-slot="terminal-new"
          onClick={createTerminal}
        >
          新建终端
        </Button>
      </div>
      <div
        ref={containerRef}
        data-slot="terminal-container"
        className="min-h-0 flex-1 overflow-hidden bg-background p-1"
      />
    </div>
  );
}
