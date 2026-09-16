// 分页窗口与滚动边界（DESIGN 各滚动机制小节）。

import { describe, expect, it } from "vitest";

import {
  beginOlderPage,
  DEFAULT_PAGE_SIZE,
  MAX_PAGE_SIZE,
  mergeNewest,
  newPaging,
  pageSizeForViewport,
  prependOlder,
  refreshLimit,
} from "./paging";

type Item = { id: string; text: string };

const idOf = (item: Item) => item.id;

describe("pageSizeForViewport", () => {
  it("按可视高度与已加载条目均摊高度算出容纳条数", () => {
    // 视口 400、内容 800、已加载 20 条 → 每条 40px → 容纳 10 条
    expect(pageSizeForViewport(400, 800, 20)).toBe(10);
  });

  it("无法计算时回落到默认页大小，且不超过上限", () => {
    expect(pageSizeForViewport(0, 800, 20)).toBe(DEFAULT_PAGE_SIZE);
    expect(pageSizeForViewport(400, 800, 0)).toBe(DEFAULT_PAGE_SIZE);
    expect(pageSizeForViewport(100000, 800, 20)).toBe(MAX_PAGE_SIZE);
  });
});

describe("refreshLimit", () => {
  it("至少覆盖已加载窗口，保证刷新后窗口连续", () => {
    expect(refreshLimit(0, 20)).toBe(20);
    expect(refreshLimit(75, 20)).toBe(75);
  });
});

describe("beginOlderPage", () => {
  it("没有更早条目时不拉取", () => {
    expect(beginOlderPage(newPaging(), 20)).toBeNull();
  });

  it("有更早条目时按已加载条数作为偏移拉一页，并在途标记阻止重复拉取", () => {
    const paging = { ...newPaging(), hasOlder: true, pageSize: 30 };
    const started = beginOlderPage(paging, 45);
    expect(started).toMatchObject({ offset: 45, limit: 30 });
    expect(started!.paging.loadingOlder).toBe(true);
    expect(beginOlderPage(started!.paging, 45)).toBeNull();
  });
});

describe("mergeNewest", () => {
  it("窗口为空时直接采用页，并按页的 hasMore 更新更早标记", () => {
    const result = mergeNewest<Item>([], newPaging(), [{ id: "a", text: "1" }], true, idOf);
    expect(result.items).toEqual([{ id: "a", text: "1" }]);
    expect(result.paging.hasOlder).toBe(true);
  });

  it("同标识条目被页内新内容替换，其余条目保持在前", () => {
    const loaded: Item[] = [
      { id: "old", text: "旧" },
      { id: "a", text: "1" },
    ];
    const result = mergeNewest(loaded, newPaging(), [{ id: "a", text: "1-新" }], false, idOf);
    expect(result.items).toEqual([
      { id: "old", text: "旧" },
      { id: "a", text: "1-新" },
    ]);
    expect(result.paging.hasOlder).toBe(false);
  });
});

describe("prependOlder", () => {
  it("更早一页插到前面并记录位移量（视图据此保持阅读位置）", () => {
    const loaded: Item[] = [{ id: "newer", text: "新" }];
    const page: Item[] = [
      { id: "o1", text: "更早1" },
      { id: "o2", text: "更早2" },
    ];
    const result = prependOlder(loaded, { ...newPaging(), loadingOlder: true }, page);
    expect(result.items.map(idOf)).toEqual(["o1", "o2", "newer"]);
    expect(result.paging.shift).toBe(2);
    expect(result.paging.loadingOlder).toBe(false);
  });
});
