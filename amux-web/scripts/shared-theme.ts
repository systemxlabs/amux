// 共享主题读取（docs/DESIGN.md「共享主题」）。
//
// 项目根 theme.json 是唯一主题来源：Web 端由 Vite 插件在启动/构建时把它转成
// `src/theme.generated.css`（index.css @import 引用）；桌面端经 include_str!
// 在编译期内嵌同一文件解析，两端不会漂移。

import fs from "node:fs";
import { fileURLToPath, URL } from "node:url";
import type { Plugin } from "vite";

/** theme.json 结构：颜色（#rrggbb）、圆角（px）、正文字号（px）。 */
export type SharedTheme = {
  color: Record<string, string>;
  radius: { sm: number; md: number; lg: number };
  font: { body: number };
};

/** 项目根 theme.json 的绝对路径（本脚本位于 amux-web/scripts/）。 */
export const themePath = fileURLToPath(new URL("../../theme.json", import.meta.url));

/** 读取共享主题；缺字段/坏格式直接抛错，构建期失败优于回退旧配色。 */
export function loadTheme(): SharedTheme {
  return JSON.parse(fs.readFileSync(themePath, "utf8")) as SharedTheme;
}

/** camelCase 键 → kebab-case 的 CSS 变量名（--color-<kebab>）。 */
function colorVar(key: string): string {
  return `--color-${key.replace(/([A-Z])/g, "-$1").toLowerCase()}`;
}

/** 校验并透传 #rrggbb 颜色。 */
function hex(hex: string): string {
  if (!/^#[0-9a-f]{6}$/i.test(hex)) {
    throw new Error(`theme.json 颜色需为 #rrggbb：${hex}`);
  }
  return hex;
}

/** 生成 @theme inline 块：颜色、圆角（rem，基准 16px）、字号。 */
export function themeCss(): string {
  const theme = loadTheme();
  const colors = Object.entries(theme.color)
    .map(([key, value]) => `  ${colorVar(key)}: ${hex(value)};`)
    .join("\n");
  const radius = Object.entries(theme.radius)
    .map(([key, value]) => `  --radius-${key}: ${(value / 16).toFixed(3)}rem;`)
    .join("\n");
  return [
    "/* 由 theme.json 生成（docs/DESIGN.md「共享主题」），勿手改 */",
    "@theme inline {",
    colors,
    radius,
    `  --font-body: ${theme.font.body}px;`,
    "}",
    "",
  ].join("\n");
}

/** Vite 插件：启动与 theme.json 变更时重写 src/theme.generated.css。 */
export function sharedThemePlugin(): Plugin {
  const outputFile = fileURLToPath(new URL("../src/theme.generated.css", import.meta.url));
  return {
    name: "amux-shared-theme",
    buildStart() {
      fs.writeFileSync(outputFile, themeCss());
    },
    handleHotUpdate({ file }) {
      if (file !== themePath) return;
      fs.writeFileSync(outputFile, themeCss());
    },
  };
}
