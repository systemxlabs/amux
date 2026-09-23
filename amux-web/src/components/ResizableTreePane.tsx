import { useRef, useState, type CSSProperties, type PointerEvent, type ReactNode } from "react";

import { cn } from "../lib/utils";

const MIN_PANE_WIDTH = 120;
const DEFAULT_TREE_WIDTH = "33%";

type DragState = {
  pointerId: number;
  startX: number;
  startWidth: number;
  containerWidth: number;
};

type ResizableTreePaneProps = {
  label: string;
  tree: ReactNode | null;
  content: ReactNode | null;
  treeSlot: string;
  contentSlot: string;
  treeClassName: string;
  contentClassName: string;
  contentCollapsed?: boolean;
};

export function ResizableTreePane({
  label,
  tree,
  content,
  treeSlot,
  contentSlot,
  treeClassName,
  contentClassName,
  contentCollapsed,
}: ResizableTreePaneProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const treeRef = useRef<HTMLDivElement>(null);
  const dragRef = useRef<DragState | null>(null);
  const [treeWidth, setTreeWidth] = useState<number | null>(null);
  const resizable = tree !== null && content !== null;

  const beginResize = (event: PointerEvent<HTMLDivElement>) => {
    const container = containerRef.current;
    const treeNode = treeRef.current;
    if (container === null || treeNode === null) return;

    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    dragRef.current = {
      pointerId: event.pointerId,
      startX: event.clientX,
      startWidth: treeNode.getBoundingClientRect().width,
      containerWidth: container.getBoundingClientRect().width,
    };
  };

  const resize = (event: PointerEvent<HTMLDivElement>) => {
    const drag = dragRef.current;
    if (drag === null || drag.pointerId !== event.pointerId) return;

    const min = Math.min(MIN_PANE_WIDTH, drag.containerWidth / 2);
    const max = Math.max(min, drag.containerWidth - min);
    const width = drag.startWidth + event.clientX - drag.startX;
    setTreeWidth(Math.min(Math.max(width, min), max));
  };

  const endResize = (event: PointerEvent<HTMLDivElement>) => {
    if (dragRef.current?.pointerId !== event.pointerId) return;
    dragRef.current = null;
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
  };

  const style = resizable
    ? ({
        "--resizable-tree-width": treeWidth === null ? DEFAULT_TREE_WIDTH : `${treeWidth}px`,
      } as CSSProperties)
    : undefined;

  return (
    <div
      ref={containerRef}
      style={style}
      className="flex min-h-0 flex-1 flex-col items-stretch gap-2 px-3 pb-3 lg:flex-row"
    >
      {tree !== null ? (
        <div
          ref={treeRef}
          data-slot={treeSlot}
          className={cn(treeClassName, resizable && "lg:w-[var(--resizable-tree-width)] lg:shrink-0")}
        >
          {tree}
        </div>
      ) : null}
      {resizable ? (
        <div
          role="separator"
          aria-label={label}
          aria-orientation="vertical"
          className="hidden w-1 shrink-0 cursor-col-resize touch-none rounded-full bg-border/60 hover:bg-primary/60 lg:block"
          onPointerDown={beginResize}
          onPointerMove={resize}
          onPointerUp={endResize}
          onPointerCancel={endResize}
        />
      ) : null}
      {content !== null ? (
        <div
          data-slot={contentSlot}
          data-collapsed={contentCollapsed === undefined ? undefined : String(contentCollapsed)}
          className={contentClassName}
        >
          {content}
        </div>
      ) : null}
    </div>
  );
}
