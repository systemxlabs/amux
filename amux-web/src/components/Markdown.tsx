// 消息气泡内容的 markdown 渲染（docs/PRD.md「会话交互视图」）。
//
// 与桌面应用保持一致：GFM 语法（表格、删除线、任务列表、自动链接）+ 单个换行按换行展示。
// 不解析原始 HTML（react-markdown 默认行为），链接一律新窗口打开。

import { memo } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkBreaks from "remark-breaks";
import remarkGfm from "remark-gfm";

const REMARK_PLUGINS = [remarkGfm, remarkBreaks];

/**
 * 元素样式：Tailwind 预检清掉了标题、列表、引用的默认样式，这里按气泡底色重新给定。
 * 正文用当前文字色，代码底色用半透明黑以同时适配普通气泡与用户气泡。
 */
const COMPONENTS: Components = {
  p: ({ children }) => <p className="whitespace-pre-wrap break-words">{children}</p>,
  a: ({ children, href }) => (
    <a
      data-slot="markdown-link"
      href={href}
      target="_blank"
      rel="noreferrer"
      className="underline underline-offset-2 hover:opacity-80"
    >
      {children}
    </a>
  ),
  code: ({ children }) => (
    <code className="rounded-sm bg-foreground/10 px-1 py-px font-mono text-[0.85em] break-words">
      {children}
    </code>
  ),
  // 代码块：内联代码的底色在 pre 内复位，避免两层底色叠加
  pre: ({ children }) => (
    <pre className="overflow-x-auto rounded-sm bg-foreground/10 p-2 font-mono text-[0.85em] [&>code]:bg-transparent [&>code]:p-0">
      {children}
    </pre>
  ),
  ul: ({ children }) => <ul className="list-disc pl-5">{children}</ul>,
  ol: ({ children }) => <ol className="list-decimal pl-5">{children}</ol>,
  li: ({ children }) => <li className="break-words">{children}</li>,
  h1: ({ children }) => <h1 className="text-base font-semibold">{children}</h1>,
  h2: ({ children }) => <h2 className="text-sm font-semibold">{children}</h2>,
  h3: ({ children }) => <h3 className="text-sm font-semibold">{children}</h3>,
  h4: ({ children }) => <h4 className="text-sm font-semibold">{children}</h4>,
  h5: ({ children }) => <h5 className="text-sm font-semibold">{children}</h5>,
  h6: ({ children }) => <h6 className="text-sm font-semibold">{children}</h6>,
  blockquote: ({ children }) => (
    <blockquote className="border-l-2 border-current/40 pl-2 opacity-90">{children}</blockquote>
  ),
  hr: () => <hr className="border-current/30" />,
  // 宽表格横向滚动，不撑破气泡
  table: ({ children }) => (
    <div className="overflow-x-auto">
      <table className="border-collapse text-xs">{children}</table>
    </div>
  ),
  th: ({ children }) => (
    <th className="border border-current/30 px-2 py-1 text-left font-semibold">{children}</th>
  ),
  td: ({ children }) => <td className="border border-current/30 px-2 py-1">{children}</td>,
};

/**
 * 消息内容：`memo` 按文本命中，轮询整窗重取导致条目对象重建时不会重复解析 markdown。
 */
export const Markdown = memo(function Markdown({ text }: { text: string }) {
  return (
    <div data-slot="markdown" className="flex min-w-0 flex-col gap-1.5 break-words">
      <ReactMarkdown remarkPlugins={REMARK_PLUGINS} components={COMPONENTS}>
        {text}
      </ReactMarkdown>
    </div>
  );
});
