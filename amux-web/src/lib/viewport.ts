// 视口断点：窄视口下主页面改用抽屉 + 整屏浮层（见 views/Main.tsx）。
//
// 阈值取 Tailwind 的 `lg` 断点（1024px）：手机横屏约 844～932px 宽，三栏布局在这个宽度下
// 每栏只剩一两百像素，因此 1024px 以下统一走抽屉 + 浮层。样式用 `lg:` 切换布局，
// 需要 JS 决策的行为（如移动端回车换行而不是发送）用这个 hook 判断，两者必须同一阈值。

import { useSyncExternalStore } from "react";

const MOBILE_QUERY = "(max-width: 1023px)";

function subscribe(onChange: () => void): () => void {
  const list = window.matchMedia(MOBILE_QUERY);
  list.addEventListener("change", onChange);
  return () => list.removeEventListener("change", onChange);
}

function matches(): boolean {
  return window.matchMedia(MOBILE_QUERY).matches;
}

/** 当前是否为手机等窄视口。 */
export function useIsMobile(): boolean {
  return useSyncExternalStore(subscribe, matches, matches);
}
