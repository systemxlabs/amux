// 分页窗口与滚动边界（DESIGN 各滚动机制小节）。

import { describe, expect, it } from "vitest";

import {
  beginNewerPage,
  beginOlderPage,
  DEFAULT_PAGE_SIZE,
  MAX_PAGE_SIZE,
  MAX_WINDOW_ITEMS,
  mergeNewest,
  newPaging,
  pageSizeForViewport,
  prependNewer,
  prependOlder,
  refreshFetch,
  refreshLimit,
  replaceWindow,
  trimNewest,
  trimOldest,
} from "./paging";

type Item = { id: string; text: string };

const idOf = (item: Item) => item.id;

describe("pageSizeForViewport", () => {
  it("按可视高度与已加载条目均摊高度算出容纳条数", () => {
    // 视口 400、内容 800、已加载 20 条 → 每条 40px → 容纳 10 条
    expect(pageSizeForViewport(400, 800, 20)).toBe(10);
  });

  it("无法计算时回落到默认页大小", () => {
    expect(pageSizeForViewport(0, 800, 20)).toBe(DEFAULT_PAGE_SIZE);
    expect(pageSizeForViewport(400, 800, 0)).toBe(DEFAULT_PAGE_SIZE);
  });

  it("行数远多于可视高度时夹到上限", () => {
    // 视口 1000、内容 2000、已加载 1000 条 → 每条 2px → 可容纳 500 条 → 取上限
    expect(pageSizeForViewport(1000, 2000, 1000)).toBe(MAX_PAGE_SIZE);
  });

  it("内容未溢满视口时保持默认页大小（比值退化为已加载条数会让刷新丢掉更早条目）", () => {
    // 列表只有 1 条且未溢出：比值 = 1，若据此写入页大小，刷新时 limit 塌到 1，
    // 服务端返回的最新一条会替换掉整个窗口，导致更早的用户消息从界面上消失。
    expect(pageSizeForViewport(830, 830, 1)).toBe(DEFAULT_PAGE_SIZE);
    expect(pageSizeForViewport(830, 400, 1)).toBe(DEFAULT_PAGE_SIZE);
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

describe("refreshFetch", () => {
  it("空窗口从最新端取一页", () => {
    expect(refreshFetch(newPaging(), 0)).toEqual({ offset: 0, limit: DEFAULT_PAGE_SIZE });
  });

  it("取当前窗口：以窗口最新一端为偏移，拉取量不超过已加载量", () => {
    const paging = { ...newPaging(), pageSize: 30, oldestOffset: 74, newestOffset: 5 };
    expect(refreshFetch(paging, 70)).toEqual({ offset: 5, limit: 70 });
  });
});

describe("beginNewerPage", () => {
  it("窗口已在最新端时不拉取", () => {
    expect(beginNewerPage(newPaging())).toBeNull();
  });

  it("有更新内容时按页大小取偏移，并在途标记阻止重复拉取", () => {
    const paging = { ...newPaging(), pageSize: 30, newestOffset: 75, hasNewer: true };
    const started = beginNewerPage(paging);
    expect(started).toMatchObject({ offset: 45, limit: 30 });
    expect(started!.paging.loadingNewer).toBe(true);
    expect(beginNewerPage(started!.paging)).toBeNull();
  });
});

describe("prependNewer", () => {
  const ids = ["a", "b"];

  it("把更新一页接到窗口末尾并更新窗口偏移", () => {
    const result = prependNewer(
      [{ id: "old", text: "旧" }],
      { ...newPaging(), newestOffset: 10 },
      ids.map((id) => ({ id, text: id } as Item)),
      0,
    );
    expect(result.items.map((item) => item.id)).toEqual(["old", "a", "b"]);
    expect(result.paging.newestOffset).toBe(0);
    expect(result.paging.hasNewer).toBe(false);
    expect(result.paging.loadingNewer).toBe(false);
  });
});

describe("replaceWindow", () => {
  it("整窗替换并保持偏移连续（刷新不必从 0 重取）", () => {
    const page: Item[] = [
      { id: "oldest", text: "最老" },
      { id: "a", text: "a" },
      { id: "newest", text: "最新" },
    ];
    const result = replaceWindow<Item>(
      [{ id: "x", text: "旧" }],
      { ...newPaging(), oldestOffset: 40, newestOffset: 5 },
      page,
      5,
      true,
    );
    expect(result.items).toEqual(page);
    expect(result.paging.oldestOffset).toBe(7);
    expect(result.paging.newestOffset).toBe(5);
    expect(result.paging.hasOlder).toBe(true);
    expect(result.paging.hasNewer).toBe(true);
  });
});

describe("窗口裁剪", () => {
  const many: Item[] = Array.from({ length: MAX_WINDOW_ITEMS + 10 }, (_, i) => ({
    id: String(i),
    text: String(i),
  }));

  it("加更老页后从最新端裁剪，更新端可再拉取", () => {
    const paging = { ...newPaging(), oldestOffset: MAX_WINDOW_ITEMS + 9, newestOffset: 0 };
    const result = trimNewest(many, paging);
    expect(result.items).toHaveLength(MAX_WINDOW_ITEMS);
    expect(result.paging.newestOffset).toBe(10);
    expect(result.paging.hasNewer).toBe(true);
  });

  it("加更新页后从最老端裁剪；最老端已到末尾时不裁", () => {
    const withTail = trimOldest(many, { ...newPaging(), hasOlder: true, oldestOffset: MAX_WINDOW_ITEMS + 9 });
    expect(withTail.items).toHaveLength(MAX_WINDOW_ITEMS);
    expect(withTail.paging.oldestOffset).toBe(MAX_WINDOW_ITEMS - 1);

    const end = trimOldest(many, { ...newPaging(), hasOlder: false, oldestOffset: MAX_WINDOW_ITEMS + 9 });
    expect(end.items).toHaveLength(many.length);
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
