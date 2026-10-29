// 会话活动视图：活动条目列表（docs/PRD.md「主页面」、docs/DESIGN.md「活动视图」）。

import { useEffect, useLayoutEffect, useRef, useState } from "react";

import { loadNewerActivities, loadOlderActivities } from "../../core/poll";
import { useCore, useCoreState } from "../../core/store";
import {
  activityDetail,
  activityKindLabel,
  activitySummary,
  formatTime,
} from "../../lib/format";
import type { Activity } from "../../lib/types";

/** 距离列表边界不足该数量的视口时预取相邻页。 */
const PREFETCH_VIEWPORTS = 2;

type ScrollAnchor = { id: string; offset: number };

function captureScrollAnchor(element: HTMLDivElement): ScrollAnchor | null {
  const containerTop = element.getBoundingClientRect().top;
  const first = [...element.querySelectorAll<HTMLElement>("[data-scroll-item]")].find(
    (item) => item.getBoundingClientRect().bottom > containerTop,
  );
  if (first === undefined) return null;
  return {
    id: first.dataset.scrollItem ?? "",
    offset: first.getBoundingClientRect().top - containerTop,
  };
}

function restoreScrollAnchor(element: HTMLDivElement, anchor: ScrollAnchor): boolean {
  const current = [...element.querySelectorAll<HTMLElement>("[data-scroll-item]")].find(
    (item) => item.dataset.scrollItem === anchor.id,
  );
  if (current === undefined) return false;
  const containerTop = element.getBoundingClientRect().top;
  element.scrollTop += current.getBoundingClientRect().top - containerTop - anchor.offset;
  return true;
}

type RowProps = { activity: Activity; expanded: boolean; onToggle: () => void };

function ActivityRow({ activity, expanded, onToggle }: RowProps) {
  return (
    <div
      data-slot="activity-item"
      data-scroll-item={activity.id}
      role="button"
      tabIndex={0}
      className="cursor-pointer rounded-md bg-muted/40 p-2 hover:bg-muted"
      onClick={onToggle}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") onToggle();
      }}
    >
      <div data-slot="activity-summary" className="flex items-start gap-2">
        <span className="shrink-0 text-xs text-muted-foreground">{activityKindLabel(activity)}</span>
        <span className="min-w-0 flex-1 truncate text-sm">
          {activitySummary(activity)}
        </span>
        <span className="shrink-0 text-xs text-muted-foreground">
          {formatTime(activity.timestamp)}
        </span>
      </div>
      {expanded && (
        <div
          data-slot="activity-detail"
          className="mt-1 whitespace-pre-wrap text-xs text-muted-foreground"
        >
          {activityDetail(activity)}
        </div>
      )}
    </div>
  );
}

export function ActivitiesPanel() {
  const core = useCore();
  const state = useCoreState();
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const previousHeight = useRef(0);
  const scrollOnEntry = useRef(true);
  const followingBottom = useRef(true);
  const initialPositioned = useRef(false);
  const scrollAnchor = useRef<ScrollAnchor | null>(null);
  const [expanded, setExpanded] = useState<string[]>([]);

  const activities = state.detail.activities;
  const paging = state.detail.activitiesPaging;
  const shift = paging.shift;
  const target = state.open;

  const ensureBuffers = (): void => {
    const element = scrollRef.current;
    if (element === null || !initialPositioned.current) return;
    const current = core.state.detail.activitiesPaging;
    const viewport = element.clientHeight;
    if (current.hasOlder && element.scrollTop <= viewport * PREFETCH_VIEWPORTS) {
      if (!current.loadingOlder) {
        if (!followingBottom.current) {
          scrollAnchor.current = captureScrollAnchor(element);
        }
        void loadOlderActivities(core);
      }
      return;
    }
    const bottomGap = element.scrollHeight - element.scrollTop - element.clientHeight;
    if (
      current.hasNewer &&
      bottomGap <= viewport * PREFETCH_VIEWPORTS &&
      !current.loadingNewer
    ) {
      void loadNewerActivities(core);
    }
  };

  // 每条进入时默认滚动到底部；切换会话（面板保持打开）时重新贴底
  useLayoutEffect(() => {
    scrollOnEntry.current = true;
    followingBottom.current = true;
    initialPositioned.current = false;
    scrollAnchor.current = null;
  }, [core, target?.kind, target?.id]);

  useLayoutEffect(() => {
    const element = scrollRef.current;
    if (element === null || activities.length === 0) return;
    if (!scrollOnEntry.current && !followingBottom.current) return;
    element.scrollTop = element.scrollHeight;
    scrollOnEntry.current = false;
    initialPositioned.current = true;
  }, [activities]);

  // 更早一页插入后以首个可见条目为锚点恢复位置；无法找到时回退到高度差补偿。
  useLayoutEffect(() => {
    const element = scrollRef.current;
    if (element === null) return;
    const height = element.scrollHeight;
    if (shift !== null) {
      if (!followingBottom.current) {
        const anchor = scrollAnchor.current;
        if (anchor === null || !restoreScrollAnchor(element, anchor)) {
          if (previousHeight.current > 0) {
            element.scrollTop += height - previousHeight.current;
          }
        }
      }
      scrollAnchor.current = null;
      core.update((next) => {
        next.detail.activitiesPaging.shift = null;
      });
    }
    previousHeight.current = height;
  }, [core, activities, shift, expanded]);

  // 首屏定位和每次窗口变化后补足上下缓冲；加载结果会再次进入本副作用。
  useEffect(() => {
    const element = scrollRef.current;
    ensureBuffers();
    if (element === null) return;
    const observer = new ResizeObserver(ensureBuffers);
    observer.observe(element);
    return () => observer.disconnect();
  }, [
    core,
    activities,
    paging.hasNewer,
    paging.hasOlder,
    paging.loadingNewer,
    paging.loadingOlder,
  ]);

  const handleScroll = () => {
    const element = scrollRef.current;
    if (element === null) return;
    followingBottom.current =
      element.scrollHeight - element.scrollTop - element.clientHeight <= 1;
    ensureBuffers();
  };

  const toggle = (id: string) => {
    setExpanded((prev) => (prev.includes(id) ? prev.filter((item) => item !== id) : [...prev, id]));
  };

  return (
    <div data-slot="activities-panel" className="flex h-full min-h-0 flex-col">
      <div
        ref={scrollRef}
        onScroll={handleScroll}
        className="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto p-3"
      >
        {activities.length === 0 ? (
          <div className="text-xs text-muted-foreground">暂无活动</div>
        ) : (
          activities.map((activity) => (
            <ActivityRow
              key={activity.id}
              activity={activity}
              expanded={expanded.includes(activity.id)}
              onToggle={() => toggle(activity.id)}
            />
          ))
        )}
      </div>
    </div>
  );
}
