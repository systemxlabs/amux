// 右键操作菜单（PRD「会话列表视图」：右键点击会话弹出操作菜单）。

import { useEffect, useRef } from "react";

import { cn } from "../lib/utils";

export type MenuItem = { label: string; onSelect: () => void; danger?: boolean };

export type MenuState = { x: number; y: number; items: MenuItem[] };

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
  return (
    <div
      ref={ref}
      data-slot="context-menu"
      className="fixed z-50 min-w-32 overflow-hidden rounded-md border border-border bg-popover py-1 shadow-lg"
      style={{ left: state.x, top: state.y }}
      onMouseDown={(event) => event.stopPropagation()}
    >
      {state.items.map((item) => (
        <button
          key={item.label}
          type="button"
          data-slot="context-menu-item"
          className={cn(
            "block w-full px-3 py-1.5 text-left hover:bg-accent",
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
