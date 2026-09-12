/**
 * 构建期注入的全局量。
 *
 * `__APP_VERSION__` 由 `vite.config.ts` 的 `define` 提供，值取自 `package.json`
 * 的 `version` 字段（构建时被替换成字符串字面量，不是运行时查找）。
 * 改版本号请只改 `package.json`。
 */
declare const __APP_VERSION__: string;
