// 列表分页模型：窗口贴着「最新」一端，随滚动向更早方向按页扩展
// （docs/DESIGN.md「会话列表滚动机制」「对话滚动机制」「活动列表滚动机制」）。

/** 默认页大小：面板可视高度能容纳的条目数由视图计算并写入，初值取它。 */
export const DEFAULT_PAGE_SIZE = 20;
/** 页大小上限：面板很高时不至于一次拉取过多。 */
export const MAX_PAGE_SIZE = 200;

export type Paging = {
  /** 面板可视高度能容纳的条目数，由视图按滚动容器计算并写入 */
  pageSize: number;
  /** 更早一端是否还有服务端条目 */
  hasOlder: boolean;
  /** 是否有在途的更早一页拉取（避免同一页重复拉取） */
  loadingOlder: boolean;
  /** 更早一页插入后在窗口头部产生的位移（条目数）：视图据此把原首条目保持在原位置 */
  shift: number | null;
};

export function newPaging(): Paging {
  return { pageSize: DEFAULT_PAGE_SIZE, hasOlder: false, loadingOlder: false, shift: null };
}

/** 页大小：面板可视高度大致能容纳的条目数（内容高度按已加载条目均摊）。 */
export function pageSizeForViewport(
  viewportHeight: number,
  contentHeight: number,
  loaded: number,
): number {
  if (loaded === 0 || viewportHeight <= 0 || contentHeight <= 0) {
    return DEFAULT_PAGE_SIZE;
  }
  const visible = Math.round(viewportHeight / (contentHeight / loaded));
  return Math.min(Math.max(visible, 1), MAX_PAGE_SIZE);
}

/** 刷新时的拉取条数：至少覆盖已加载窗口，保证刷新后窗口仍是连续的一段。 */
export function refreshLimit(loaded: number, pageSize: number): number {
  return Math.max(loaded, pageSize, 1);
}

/** 开始拉取更早一页：返回（偏移, 条数）；没有更早条目或已有在途拉取时为 null。 */
export function beginOlderPage(
  paging: Paging,
  loaded: number,
): { paging: Paging; offset: number; limit: number } | null {
  if (!paging.hasOlder || paging.loadingOlder) return null;
  const limit = Math.max(paging.pageSize, 1);
  return { paging: { ...paging, loadingOlder: true }, offset: loaded, limit };
}

/** 更早一页拉取失败/结束：复位在途标记与更早标记。 */
export function finishOlderPage(paging: Paging, hasOlder: boolean): Paging {
  return { ...paging, loadingOlder: false, hasOlder };
}

/**
 * 把最新一页并入窗口（升序展示、最新在末尾）。
 *
 * 页里的条目按标识替换窗口中的同一条目（流式输出会改内容，同一条目的位置也可能变到最新端），
 * 其余条目保持不动、且都排在页之前：页是「最新的一段」，窗口只向更新的一端扩展，不会出现缺口。
 */
export function mergeNewest<T>(
  items: readonly T[],
  paging: Paging,
  page: readonly T[],
  pageHasMore: boolean,
  idOf: (item: T) => string,
): { items: T[]; paging: Paging } {
  const next: Paging = { ...paging, hasOlder: pageHasMore };
  if (items.length === 0) return { items: [...page], paging: next };
  const pageIds = new Set(page.map(idOf));
  return {
    items: [...items.filter((item) => !pageIds.has(idOf(item))), ...page],
    paging: next,
  };
}

/**
 * 更早一页插到窗口前面（升序展示、最新在末尾）。
 *
 * 插入会让可视内容整体下移，因此记下插入条数为位移量，由视图在下一节拍把首条目滚回原位。
 */
export function prependOlder<T>(
  items: readonly T[],
  paging: Paging,
  page: readonly T[],
): { items: T[]; paging: Paging } {
  return {
    items: [...page, ...items],
    paging: { ...paging, loadingOlder: false, shift: page.length },
  };
}
