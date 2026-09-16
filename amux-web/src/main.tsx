import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App";
import { Core } from "./core/core";
import { CoreProvider } from "./core/store";
import "./index.css";

const core = new Core();

const container = document.getElementById("root");
if (!container) throw new Error("缺少 #root 挂载点");

createRoot(container).render(
  <StrictMode>
    <CoreProvider core={core}>
      <App />
    </CoreProvider>
  </StrictMode>,
);
