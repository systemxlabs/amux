// 多选组合框：触发器内展示已选标签，展开后从候选列表勾选。

import { useEffect, useId, useRef, useState } from "react";
import { Check, ChevronDown, X } from "lucide-react";

import { cn } from "../lib/utils";

export function MultiSelectCombobox<T extends string>({
  options,
  value,
  onChange,
  placeholder,
  ariaLabel,
}: {
  options: readonly T[];
  value: readonly T[];
  onChange: (value: T[]) => void;
  placeholder?: string;
  ariaLabel: string;
}) {
  const container = useRef<HTMLDivElement | null>(null);
  const listboxId = useId();
  const [open, setOpen] = useState(false);

  useEffect(() => {
    if (!open) return;
    const onMouseDown = (event: MouseEvent) => {
      const target = event.target;
      if (!(target instanceof Node)) return;
      if (container.current !== null && !container.current.contains(target)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", onMouseDown);
    return () => document.removeEventListener("mousedown", onMouseDown);
  }, [open]);

  const toggle = (option: T): void => {
    onChange(
      value.includes(option)
        ? value.filter((item) => item !== option)
        : [...value, option],
    );
  };

  return (
    <div ref={container} className="relative">
      <div
        role="combobox"
        aria-label={ariaLabel}
        aria-expanded={open}
        aria-controls={listboxId}
        tabIndex={0}
        className={cn(
          "flex min-h-10 w-full cursor-pointer flex-wrap items-center gap-1.5 rounded-md border border-input bg-background px-2 py-1.5 text-sm text-foreground outline-none transition-colors focus-visible:border-ring focus-visible:ring-2 focus-visible:ring-ring/40 lg:min-h-8",
          open && "border-ring ring-2 ring-ring/40",
        )}
        onClick={() => setOpen((current) => !current)}
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            setOpen(false);
          } else if (event.key === "ArrowDown" || event.key === "Enter" || event.key === " ") {
            event.preventDefault();
            setOpen(true);
          }
        }}
      >
        {value.length === 0 ? (
          <span className="px-0.5 text-muted-foreground">{placeholder}</span>
        ) : (
          value.map((option) => (
            <span
              key={option}
              className="inline-flex items-center gap-1 rounded-sm bg-accent px-1.5 py-0.5 text-xs text-accent-foreground"
            >
              {option}
              <button
                type="button"
                aria-label={`移除 ${option}`}
                className="text-muted-foreground hover:text-foreground"
                onClick={(event) => {
                  event.stopPropagation();
                  onChange(value.filter((item) => item !== option));
                }}
              >
                <X className="size-3" />
              </button>
            </span>
          ))
        )}
        <ChevronDown className="ml-auto size-4 shrink-0 text-muted-foreground" />
      </div>
      {open ? (
        <div
          id={listboxId}
          role="listbox"
          aria-multiselectable="true"
          className="absolute top-full left-0 z-20 mt-1 flex max-h-64 w-full flex-col overflow-y-auto rounded-md border border-border bg-popover p-1 shadow-lg"
        >
          {options.map((option) => {
            const selected = value.includes(option);
            return (
              <button
                key={option}
                type="button"
                role="option"
                aria-selected={selected}
                className="flex w-full cursor-pointer items-center gap-2 rounded-sm px-2 py-1.5 text-left text-sm hover:bg-accent focus-visible:bg-accent focus-visible:outline-none"
                onClick={() => toggle(option)}
              >
                <span className="min-w-0 flex-1 truncate">{option}</span>
                {selected ? <Check className="size-4 shrink-0 text-primary" /> : null}
              </button>
            );
          })}
        </div>
      ) : null}
    </div>
  );
}
