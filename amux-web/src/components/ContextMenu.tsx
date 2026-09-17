// 右键操作菜单（PRD「会话列表视图」：右键点击会话弹出操作菜单）。

import { useEffect, useRef } from "react";

import { cn } from "../lib/utils";

export type MenuItem = { label: string; onSelect: () => void; danger?: boolean };

export type MenuState = { x: number; y: number; items: MenuItem[] };

/** 收敛菜单位置用的估算尺寸：菜单宽度（min-w-32）与每项高度（见下方按钮的 h-10/h-8）。 */
const MENU_WIDTH = 128;
const ITEM_HEIGHT = 40;
const VIEWPORT_GAP = 8;

export function ContextMenu({
  state,
  onClose,
}: {
  state: MenuState | null;
  onClose: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!state) return;
    const close = () => onClose();
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("mousedown", close);
    window.addEventListener("resize", close);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("resize", close);
      window.removeEventListener("keydown", onKey);
    };
  }, [state, onClose]);

  if (!state) return null;
  // 锚点可能有按键位置（窄视口的「会话操作」按钮）或指针位置（右键）：都收敛到视口内，
  // 否则菜单会有一部分落在屏幕外点不到
  const left = Math.min(state.x, window.innerWidth - MENU_WIDTH - VIEWPORT_GAP);
  const top = Math.min(
    state.y,
    window.innerHeight - state.items.length * ITEM_HEIGHT - VIEWPORT_GAP * 2,
  );
  return (
    <div
      ref={ref}
      data-slot="context-menu"
      className="fixed z-50 min-w-32 overflow-hidden rounded-md border border-border bg-popover py-1 shadow-lg"
      style={{ left: Math.max(VIEWPORT_GAP, left), top: Math.max(VIEWPORT_GAP, top) }}
      onMouseDown={(event) => event.stopPropagation()}
    >
      {state.items.map((item) => (
        <button
          key={item.label}
          type="button"
          data-slot="context-menu-item"
          className={cn(
            "flex h-10 w-full items-center px-3 text-left hover:bg-accent lg:h-8",
            item.danger ? "text-destructive" : "text-popover-foreground",
          )}
          onClick={() => {
            onClose();
            item.onSelect();
          }}
        >
          {item.label}
        </button>
      ))}
    </div>
  );
}
