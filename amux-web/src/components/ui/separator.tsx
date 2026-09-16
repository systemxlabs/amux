import * as React from "react";

import { cn } from "../../lib/utils";

type SeparatorProps = React.ComponentProps<"div"> & {
  orientation?: "horizontal" | "vertical";
};

function Separator({ className, orientation = "horizontal", ...props }: SeparatorProps) {
  return (
    <div
      data-slot="separator"
      role="separator"
      aria-orientation={orientation}
      className={cn(
        "shrink-0 border-border",
        orientation === "horizontal" ? "w-full border-t" : "h-full border-l",
        className,
      )}
      {...props}
    />
  );
}

export { Separator };
