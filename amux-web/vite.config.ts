import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { fileURLToPath, URL } from "node:url";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
  server: {
    // 开发时把 API 前缀转发到本机 Server，生产由 Server 直接托管构建产物。
    proxy: {
      "/machines": "http://127.0.0.1:34567",
      "/sessions": "http://127.0.0.1:34567",
      "/workflows": "http://127.0.0.1:34567",
      "/config": "http://127.0.0.1:34567",
    },
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
