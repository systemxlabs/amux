// 列表分页窗口：按滚动位置保留一段连续窗口，预取相邻页，页大小随面板可视高度自适应
// （docs/DESIGN.md「会话列表滚动机制」「对话滚动机制」「活动列表滚动机制」）。

/** 默认页大小：面板可视高度能容纳的条目数由视图计算并写入，初值取它。 */
export const DEFAULT_PAGE_SIZE = 20;
/** 页大小上限：面板很高时不至于一次拉取过多。 */
export const MAX_PAGE_SIZE = 200;
/** 一次刷新/窗口最多保留的条目数（服务端单次分页上限 500）。 */
export const MAX_WINDOW_ITEMS = 500;

export type Paging = {
  /** 面板可视高度能容纳的条目数，由视图按滚动容器计算并写入 */
  pageSize: number;
  /** 已加载窗口中最老一项的服务端偏移（空窗口为 0） */
  oldestOffset: number;
  /** 已加载窗口中最新一项的服务端偏移；0 表示已到最新端 */
  newestOffset: number;
  /** 更老一端是否还有服务端条目 */
  hasOlder: boolean;
  /** 更新一端是否还有未加载的服务端条目 */
  hasNewer: boolean;
  /** 是否有在途的更老一页拉取（避免同一页重复拉取） */
  loadingOlder: boolean;
  /** 是否有在途的更新一页拉取 */
  loadingNewer: boolean;
  /**
   * 窗口顶部内容是否发生变化（插入更老页 / 移除最老页）。
   * 视图据此用插入前后的容器高度差补偿滚动位置。
   */
  shift: number | null;
};

export function newPaging(): Paging {
  return {
    pageSize: DEFAULT_PAGE_SIZE,
    oldestOffset: 0,
    newestOffset: 0,
    hasOlder: false,
    hasNewer: false,
    loadingOlder: false,
    loadingNewer: false,
    shift: null,
  };
}

/**
 * 页大小：面板可视高度大致能容纳的条目数（内容高度按已加载条目均摊）。
 *
 * 内容未溢满视口时无法据此估算行高：比值会退化成「已加载条数」，页大小随之被压到 1，
 * 刷新窗口（按页大小重新拉取并替换）就会丢掉更早的已加载条目，因此此时保持默认页大小。
 */
export function pageSizeForViewport(
  viewportHeight: number,
  contentHeight: number,
  loaded: number,
): number {
  if (loaded === 0 || viewportHeight <= 0 || contentHeight <= 0 || contentHeight <= viewportHeight) {
    return DEFAULT_PAGE_SIZE;
  }
  const visible = Math.round(viewportHeight / (contentHeight / loaded));
  return Math.min(Math.max(visible, 1), MAX_PAGE_SIZE);
}

/** 刷新窗口上限：最多覆盖 MAX_WINDOW_ITEMS 条，避免拉取量随已加载数量无限增长。 */
export function refreshLimit(loaded: number, pageSize: number): number {
  return Math.min(Math.max(loaded, pageSize, 1), MAX_WINDOW_ITEMS);
}

/**
 * 刷新当前窗口的取数参数。
 * 窗口可能已不在最新端（滚动到很老的历史后回到窗口内），因此按窗口最新一端取数。
 */
export function refreshFetch(
  paging: Paging,
  loaded: number,
): { offset: number; limit: number } {
  if (loaded === 0) return { offset: 0, limit: Math.max(paging.pageSize, 1) };
  return { offset: paging.newestOffset, limit: refreshLimit(loaded, paging.pageSize) };
}

/** 开始拉取更老一页：返回（偏移, 条数）；没有更老条目或已有在途拉取时为 null。 */
export function beginOlderPage(
  paging: Paging,
  loaded: number,
): { paging: Paging; offset: number; limit: number } | null {
  if (!paging.hasOlder || paging.loadingOlder) return null;
  const limit = Math.max(paging.pageSize, 1);
  const offset = loaded === 0 ? 0 : Math.max(paging.oldestOffset + 1, loaded);
  return { paging: { ...paging, loadingOlder: true }, offset, limit };
}

/** 开始拉取更新一页：返回（偏移, 条数）；窗口已在最新端或已有在途拉取时为 null。 */
export function beginNewerPage(
  paging: Paging,
): { paging: Paging; offset: number; limit: number } | null {
  if (paging.newestOffset <= 0 || paging.loadingNewer) return null;
  const limit = Math.max(paging.pageSize, 1);
  const offset = Math.max(0, paging.newestOffset - limit);
  return { paging: { ...paging, loadingNewer: true }, offset, limit };
}

/**
 * 把最新一段并入窗口（升序展示、最新在末尾）。
 *
 * 页里的条目按标识替换窗口中的同一条目，其余条目保持不动、且都排在页之前。
 */
export function mergeNewest<T>(
  items: readonly T[],
  paging: Paging,
  page: readonly T[],
  pageHasMore: boolean,
  idOf: (item: T) => string,
): { items: T[]; paging: Paging } {
  const next: Paging = {
    ...paging,
    hasOlder: pageHasMore,
    hasNewer: false,
    loadingOlder: false,
    loadingNewer: false,
  };
  if (items.length === 0) {
    const items = [...page];
    return {
      items,
      paging: { ...next, oldestOffset: Math.max(0, items.length - 1), newestOffset: 0 },
    };
  }
  const pageIds = new Set(page.map(idOf));
  const kept = items.filter((item) => !pageIds.has(idOf(item)));
  const merged = [...kept, ...page];
  return {
    items: merged,
    paging: {
      ...next,
      oldestOffset: Math.max(0, merged.length - 1),
      newestOffset: 0,
    },
  };
}

/**
 * 更老一页插到窗口前面（升序展示、最新在末尾）。
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
    paging: {
      ...paging,
      loadingOlder: false,
      oldestOffset: paging.oldestOffset + page.length,
      shift: page.length,
    },
  };
}

/**
 * 更新一页插到窗口末尾（升序展示、最新在末尾）。
 *
 * 页内已经是展示顺序（offset 大即更老在前）。
 */
export function prependNewer<T>(
  items: readonly T[],
  paging: Paging,
  page: readonly T[],
  pageOffset: number,
): { items: T[]; paging: Paging } {
  return {
    items: [...items, ...page],
    paging: {
      ...paging,
      loadingNewer: false,
      newestOffset: pageOffset,
      hasNewer: pageOffset > 0,
      shift: 0,
    },
  };
}


/**
 * 用一页刷新替换当前窗口。
 *
 * 页面已经是最老在前的一页；用它对当前窗口做整窗替换。
 * 用于刷新当前窗口：拉取量与窗口大小一致，不超过服务端单次上限。
 */
export function replaceWindow<T>(
  items: readonly T[],
  paging: Paging,
  page: readonly T[],
  pageOffset: number,
  pageHasMore: boolean,
): { items: T[]; paging: Paging } {
  const display = [...page];
  return {
    items: display,
    paging: {
      ...paging,
      loadingOlder: false,
      loadingNewer: false,
      oldestOffset: Math.max(0, pageOffset + display.length - 1),
      newestOffset: pageOffset,
      hasOlder: pageHasMore,
      hasNewer: pageOffset > 0,
      shift: items.length === 0 ? null : 0,
    },
  };
}

/** 窗口超过上限时从最新端裁剪；被裁掉的最新条目在加载更新一页时可重新拉取。 */
export function trimNewest<T>(
  items: readonly T[],
  paging: Paging,
): { items: T[]; paging: Paging } {
  const drop = items.length - MAX_WINDOW_ITEMS;
  if (drop <= 0) return { items: [...items], paging };
  return {
    items: items.slice(0, items.length - drop),
    paging: { ...paging, newestOffset: paging.newestOffset + drop, hasNewer: true, shift: 0 },
  };
}

/** 窗口超过上限时从最老端裁剪；被裁掉的条目在加载更老一页时可重新拉取。 */
export function trimOldest<T>(
  items: readonly T[],
  paging: Paging,
): { items: T[]; paging: Paging } {
  const drop = items.length - MAX_WINDOW_ITEMS;
  // 最老端已是服务端末尾时不移除，避免出现无法重新拉取的缺口
  if (drop <= 0 || !paging.hasOlder) return { items: [...items], paging };
  return {
    items: items.slice(drop),
    paging: {
      ...paging,
      oldestOffset: Math.max(0, paging.oldestOffset - drop),
      shift: 0,
    },
  };
}
