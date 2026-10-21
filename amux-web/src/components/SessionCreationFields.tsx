// 新建会话类表单的可复用字段：工作目录联想、worktree 基分支和已保存计划选择。

import { useEffect, useRef, useState } from "react";
import { ChevronDown, Folder } from "lucide-react";

import { deleteRecentWorkspace } from "../core/actions";
import type { Core } from "../core/core";
import { useCoreState } from "../core/store";
import { branchLabel, preferredRemoteDefault } from "../lib/branch";
import { truncate } from "../lib/format";
import {
  worktreeBaseFromBranch,
  worktreeBaseKey,
  type FsEntry,
  type GitBranch,
  type RecentWorkspace,
  type WorkflowPlanItem,
  type WorktreeBase,
} from "../lib/types";
import { listAllDirs, matchingPrefix, splitDirQuery } from "../lib/workspace";
import { cn } from "../lib/utils";
import { Input } from "./ui/input";
import { Label } from "./ui/label";
import { Textarea } from "./ui/textarea";

type PlanPopupPlacement = { below: boolean; maxHeight: number };
type WorkspacePopup = { kind: "recent" | "suggest"; maxHeight: number };
type DirectoryListing = { machine: string; dir: string; entries: FsEntry[] };

const PLAN_POPUP_HEIGHT = 224;
const POPUP_GAP = 8;
const POPUP_MIN_HEIGHT = 80;

/** 基分支展示名：优先用分支列表里的完整标注，列表未加载时退化为引用名。 */
function worktreeBaseLabel(base: WorktreeBase, branches: readonly GitBranch[]): string {
  const branch = branches.find(
    (item) => worktreeBaseKey(worktreeBaseFromBranch(item)) === worktreeBaseKey(base),
  );
  if (branch !== undefined) return branchLabel(branch);
  return base.kind === "local" ? `本地 ${base.name}` : `远程 ${base.remote}/${base.branch}`;
}

function popupMaxHeight(anchor: DOMRect, below: boolean): number {
  const available = below ? window.innerHeight - anchor.bottom - POPUP_GAP : anchor.top - POPUP_GAP;
  return Math.max(Math.min(PLAN_POPUP_HEIGHT, available), POPUP_MIN_HEIGHT);
}

/**
 * 工作目录字段：点击/聚焦展示最近目录（上拉），手动输入展示目录前缀匹配（下拉）。
 * 新建会话和自动化中的新建会话表单共用同一交互。
 */
export function WorkspaceField({
  core,
  machine,
  value,
  onChange,
  onPickRecent,
  slotPrefix = "workspace",
}: {
  core: Core;
  machine: string;
  value: string;
  onChange: (value: string) => void;
  onPickRecent?: (item: RecentWorkspace) => void;
  slotPrefix?: string;
}) {
  const state = useCoreState();
  const inputRef = useRef<HTMLInputElement | null>(null);
  const listing = useRef<DirectoryListing | null>(null);
  const requestSeq = useRef(0);
  const [popup, setPopup] = useState<WorkspacePopup | null>(null);
  const [suggestions, setSuggestions] = useState<FsEntry[]>([]);
  const recent = state.recentWorkspaces
    .filter((item) => item.machine === machine)
    .sort((a, b) => b.lastUsed - a.lastUsed)
    .slice(0, 8);

  useEffect(() => {
    const query = splitDirQuery(value);
    const client = core.client;
    const seq = requestSeq.current + 1;
    requestSeq.current = seq;
    if (query === null || machine === "" || client === null) {
      listing.current = null;
      setSuggestions([]);
      return;
    }
    if (listing.current?.machine === machine && listing.current.dir === query.dir) {
      setSuggestions(matchingPrefix(listing.current.entries, query.prefix));
      return;
    }
    listing.current = null;
    setSuggestions([]);
    void listAllDirs(client, machine, query.dir).then(
      (entries) => {
        if (requestSeq.current !== seq) return;
        listing.current = { machine, dir: query.dir, entries };
        setSuggestions(matchingPrefix(entries, query.prefix));
      },
      () => {
        if (requestSeq.current === seq) listing.current = null;
      },
    );
  }, [core, machine, value]);

  const pickRecent = (item: RecentWorkspace): void => {
    listing.current = null;
    setSuggestions([]);
    onChange(item.workspace);
    onPickRecent?.(item);
    setPopup(null);
  };

  const pickSuggestion = (path: string): void => {
    const next = path.endsWith("/") ? path : `${path}/`;
    listing.current = null;
    setSuggestions([]);
    onChange(next);
    const input = inputRef.current;
    if (input !== null) {
      setPopup({ kind: "suggest", maxHeight: popupMaxHeight(input.getBoundingClientRect(), true) });
    }
  };

  return (
    <div className="relative">
      <Input
        ref={inputRef}
        data-slot={`${slotPrefix}-input`}
        aria-label="工作目录"
        value={value}
        onFocus={(event) => {
          if (value.trim() === "" && recent.length > 0) {
            setPopup({
              kind: "recent",
              maxHeight: popupMaxHeight(event.currentTarget.getBoundingClientRect(), false),
            });
          }
        }}
        onClick={(event) => {
          if (value.trim() === "" && recent.length > 0) {
            setPopup({
              kind: "recent",
              maxHeight: popupMaxHeight(event.currentTarget.getBoundingClientRect(), false),
            });
          }
        }}
        onBlur={() => setPopup(null)}
        onChange={(event) => {
          const next = event.target.value;
          listing.current = null;
          setSuggestions([]);
          onChange(next);
          if (splitDirQuery(next) === null || machine === "") {
            setPopup(null);
          } else {
            setPopup({
              kind: "suggest",
              maxHeight: popupMaxHeight(event.currentTarget.getBoundingClientRect(), true),
            });
          }
        }}
      />
      {popup?.kind === "recent" && recent.length > 0 ? (
        <div
          data-slot={`${slotPrefix}-recent`}
          style={{ maxHeight: popup.maxHeight }}
          className="absolute bottom-full left-0 z-10 mb-1 flex w-full flex-col overflow-y-auto rounded-md border border-border bg-popover py-1 shadow-lg"
        >
          {recent.map((item) => (
            <div
              key={`${item.machine}:${item.workspace}`}
              data-slot={`${slotPrefix}-recent-item`}
              onMouseDown={(event) => event.preventDefault()}
              className="flex cursor-pointer items-center hover:bg-accent"
            >
              <button
                type="button"
                onClick={() => pickRecent(item)}
                className="min-w-0 flex-1 truncate px-2 py-2 text-left text-sm lg:py-1"
              >
                {item.workspace}
              </button>
              <button
                type="button"
                data-slot={`${slotPrefix}-recent-delete`}
                aria-label={`删除 ${item.workspace}`}
                onClick={() => void deleteRecentWorkspace(core, item.machine, item.workspace)}
                className="shrink-0 px-2 py-2 text-muted-foreground hover:text-foreground lg:py-1"
              >
                ×
              </button>
            </div>
          ))}
        </div>
      ) : null}
      {popup?.kind === "suggest" && suggestions.length > 0 ? (
        <div
          data-slot={`${slotPrefix}-suggestions`}
          style={{ maxHeight: popup.maxHeight }}
          className="absolute top-full left-0 z-10 mt-1 flex w-full flex-col overflow-y-auto rounded-md border border-border bg-popover p-1 shadow-lg"
        >
          {suggestions.map((item) => (
            <button
              key={item.path}
              type="button"
              data-slot={`${slotPrefix}-suggestion`}
              data-path={item.path}
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => pickSuggestion(item.path)}
              className="flex cursor-pointer items-center gap-1.5 rounded-sm px-2 py-2 text-left text-sm hover:bg-accent lg:py-1"
            >
              <Folder className="size-4 shrink-0 text-muted-foreground" />
              <span className="truncate">{item.name}</span>
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );
}

/**
 * worktree 基分支字段：随机器和工作目录拉取可搜索的可选项，
 * 默认选择 origin 的远程仓库默认分支，否则取第一个远程默认分支。
 */
export function WorktreeBranchField({
  core,
  machine,
  workspace,
  value,
  onChange,
}: {
  core: Core;
  machine: string;
  workspace: string;
  value: WorktreeBase | null;
  onChange: (value: WorktreeBase | null) => void;
}) {
  const container = useRef<HTMLDivElement | null>(null);
  const request = useRef(0);
  const onChangeRef = useRef(onChange);
  const valueRef = useRef(value);
  const [branches, setBranches] = useState<GitBranch[]>([]);
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  onChangeRef.current = onChange;
  valueRef.current = value;

  useEffect(() => {
    const repo = workspace.trim();
    if (machine === "" || repo === "" || core.client === null) {
      request.current += 1;
      setBranches([]);
      return;
    }
    const seq = request.current + 1;
    request.current = seq;
    void core.client.listBranches(machine, repo).then(
      (result) => {
        if (request.current !== seq) return;
        setBranches(result.branches);
        const current = valueRef.current;
        const selected =
          current !== null &&
          result.branches.some(
            (branch) =>
              worktreeBaseKey(worktreeBaseFromBranch(branch)) === worktreeBaseKey(current),
          );
        if (selected) return;
        const fallback = preferredRemoteDefault(result.branches);
        onChangeRef.current(fallback === undefined ? null : worktreeBaseFromBranch(fallback));
      },
      () => {
        if (request.current === seq) {
          setBranches([]);
        }
      },
    );
  }, [core, machine, workspace]);

  useEffect(() => {
    if (!open) return;
    const onMouseDown = (event: MouseEvent) => {
      const target = event.target;
      if (!(target instanceof Node)) return;
      if (container.current !== null && !container.current.contains(target)) setOpen(false);
    };
    document.addEventListener("mousedown", onMouseDown);
    return () => document.removeEventListener("mousedown", onMouseDown);
  }, [open]);

  const keyword = query.trim().toLowerCase();
  const matched =
    keyword === ""
      ? branches
      : branches.filter(
          (branch) =>
            branch.name.toLowerCase().includes(keyword) ||
            branch.fullName.toLowerCase().includes(keyword),
        );
  const close = (): void => {
    setOpen(false);
    setQuery("");
  };

  return (
    <div className="flex items-center justify-between gap-3">
      <Label>基分支</Label>
      <div ref={container} className="relative w-64 max-w-[55%]">
        <button
          type="button"
          role="combobox"
          aria-label="选择 worktree 基分支"
          aria-expanded={open}
          data-slot="worktree-base-select"
          onClick={() => (open ? close() : setOpen(true))}
          className={cn(
            "flex h-8 w-full items-center gap-2 rounded-md border border-input bg-background px-2 text-sm text-foreground outline-none transition-colors hover:bg-accent focus-visible:border-ring focus-visible:ring-2 focus-visible:ring-ring/40",
            open && "border-ring ring-2 ring-ring/40",
          )}
        >
          <span
            data-slot="worktree-base-value"
            className={cn("min-w-0 flex-1 truncate text-left", value === null && "text-muted-foreground")}
          >
            {value === null ? "未选择" : worktreeBaseLabel(value, branches)}
          </span>
          <ChevronDown className="size-4 shrink-0 text-muted-foreground" />
        </button>
        {open ? (
          <div
            role="listbox"
            data-slot="worktree-base-list"
            className="absolute top-full right-0 z-20 mt-1 flex max-h-64 w-full flex-col overflow-y-auto rounded-md border border-border bg-popover p-1 shadow-lg"
          >
            <Input
              autoFocus
              data-slot="worktree-base-search"
              aria-label="搜索基分支"
              placeholder="搜索分支"
              className="mb-1 h-8"
              value={query}
              onKeyDown={(event) => {
                if (event.key === "Escape") close();
              }}
              onChange={(event) => setQuery(event.target.value)}
            />
            {matched.length === 0 ? (
              <span className="px-2 py-1.5 text-xs text-muted-foreground">无匹配分支</span>
            ) : (
              matched.map((branch) => {
                const base = worktreeBaseFromBranch(branch);
                return (
                  <button
                    key={branch.fullName}
                    type="button"
                    role="option"
                    aria-selected={
                      value !== null && worktreeBaseKey(value) === worktreeBaseKey(base)
                    }
                    className="flex cursor-pointer items-center rounded-sm px-2 py-1.5 text-left text-sm hover:bg-accent"
                    onClick={() => {
                      onChange(base);
                      close();
                    }}
                  >
                    <span className="min-w-0 flex-1 truncate">{branchLabel(branch)}</span>
                  </button>
                );
              })
            )}
          </div>
        ) : null}
      </div>
    </div>
  );
}

/** 已保存计划字段：聚焦时按可用空间向上或向下展示计划列表。 */
export function WorkflowPlanField({
  plans,
  value,
  selectedPlan,
  onChange,
  onSelectPlan,
  slotPrefix = "workflow-plan",
}: {
  plans: readonly WorkflowPlanItem[];
  value: string;
  selectedPlan: string | null;
  onChange: (value: string) => void;
  onSelectPlan: (plan: WorkflowPlanItem) => void;
  slotPrefix?: string;
}) {
  const field = useRef<HTMLDivElement | null>(null);
  const [popup, setPopup] = useState<PlanPopupPlacement | null>(null);

  useEffect(() => {
    const onMouseDown = (event: MouseEvent) => {
      const target = event.target;
      if (!(target instanceof Node)) return;
      if (field.current !== null && !field.current.contains(target)) setPopup(null);
    };
    document.addEventListener("mousedown", onMouseDown);
    return () => document.removeEventListener("mousedown", onMouseDown);
  }, []);

  const open = (input: HTMLElement): void => {
    if (plans.length === 0) return;
    const rect = input.getBoundingClientRect();
    const above = rect.top - POPUP_GAP;
    const below = window.innerHeight - rect.bottom - POPUP_GAP;
    const useBelow = above < PLAN_POPUP_HEIGHT && below > above;
    setPopup({ below: useBelow, maxHeight: popupMaxHeight(rect, useBelow) });
  };

  return (
    <div ref={field} className="relative">
      {popup !== null && plans.length > 0 ? (
        <div
          data-slot={`${slotPrefix}-popup`}
          data-direction={popup.below ? "below" : "above"}
          style={{ maxHeight: popup.maxHeight }}
          className={cn(
            "absolute left-0 z-10 flex w-full flex-col overflow-y-auto rounded-md border border-border bg-popover py-1 shadow-lg",
            popup.below ? "top-full mt-1" : "bottom-full mb-1",
          )}
        >
          {plans.map((item) => (
            <button
              key={item.name}
              type="button"
              data-slot={`${slotPrefix}-choice`}
              data-selected={selectedPlan === item.name ? "true" : "false"}
              onClick={() => {
                onChange(item.plan);
                onSelectPlan(item);
                setPopup(null);
              }}
              className={cn(
                "flex cursor-pointer flex-col items-start gap-1 px-2 py-2 text-left hover:bg-accent lg:py-1",
                selectedPlan === item.name && "bg-accent",
              )}
            >
              <span className="text-sm">{item.name}</span>
              <span className="text-xs text-muted-foreground">{truncate(item.plan, 80)}</span>
            </button>
          ))}
        </div>
      ) : null}
      <Textarea
        data-slot={`${slotPrefix}-input`}
        aria-label="工作计划"
        className="min-h-32"
        placeholder="输入工作计划"
        value={value}
        onFocus={(event) => open(event.currentTarget)}
        onKeyDown={(event) => {
          if (event.key === "Escape") setPopup(null);
        }}
        onChange={(event) => {
          onChange(event.target.value);
          setPopup(null);
        }}
      />
    </div>
  );
}
