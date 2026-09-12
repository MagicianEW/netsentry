import { defineConfig } from "vite";
import { readFileSync } from "node:fs";
// @ts-expect-error type error without @types/node package
import process from "node:process";
const host = process.env.TAURI_DEV_HOST;

/**
 * 版本号只在 `package.json` 里维护一处。
 *
 * 读出来之后分别注入两个地方：
 *   - `define.__APP_VERSION__`  → 给 main.ts 用（关于对话框）
 *   - `transformIndexHtml`      → 替换 index.html 里的 `{{APP_VERSION}}`
 *
 * 改版之前 index.html 和 main.ts 各自硬编码了 "0.1.0"，改一次版本要动好几处，
 * 漏一个就会出现「界面显示旧版本」的诡异现象。现在只需要动 package.json。
 *
 * 注意：Rust 侧的版本是独立的，必须一起改，否则 exe 属性里还是旧号 ——
 *   - `src-tauri/Cargo.toml`      → 决定编译进 exe 的版本（属性面板可见）
 *   - `src-tauri/tauri.conf.json` → 决定打包产物（NSIS/MSI）的版本
 */
// @ts-expect-error type error without @types/node package
const pkg = JSON.parse(readFileSync(new URL("./package.json", import.meta.url), "utf-8")) as {
  version: string;
};
const appVersion = pkg.version;

// https://vite.dev/config/
export default defineConfig(() => ({
  define: {
    __APP_VERSION__: JSON.stringify(appVersion),
  },

  plugins: [
    {
      // 用 {{APP_VERSION}} 而不是 %APP_VERSION% —— 后者是 Vite 内建的 HTML
      // 环境变量替换语法（只认 VITE_ 前缀），容易互相干扰。
      name: "netsentry-html-version",
      transformIndexHtml(html: string) {
        return html.replace(/\{\{APP_VERSION\}\}/g, appVersion);
      },
    },
  ],

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
