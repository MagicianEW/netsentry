#!/usr/bin/env node
/**
 * 版本号同步 / 校验。
 *
 * ## 单一来源
 *
 * 版本号只在 `package.json` 的 `version` 字段里维护。其余出现版本号的地方都是**派生**的：
 *
 * | 位置                        | 怎么来的                                              |
 * |-----------------------------|-------------------------------------------------------|
 * | `src-tauri/tauri.conf.json` | 值写成 `"../package.json"`，由 Tauri 解析配置时读取     |
 * | `src-tauri/Cargo.toml`      | 本脚本写入（cargo 要求 `[package] version` 是字面量）   |
 * | `src-tauri/Cargo.lock`      | 本脚本写入（`netsentry` 包条目）                       |
 * | `package-lock.json`         | 本脚本写入（根 `version` 两处）                        |
 * | 前端界面                     | `vite.config.ts` 读 package.json 后注入                |
 *
 * 所以改版本只要动 `package.json`，然后跑一次 `npm run version:sync`。
 * 更省事的方式是 `npm version 0.2.0`，它会更新 package.json 并自动触发本脚本。
 *
 * ## 用法
 *
 *     node scripts/sync-version.mjs            # 同步（幂等，值没变就不动文件）
 *     node scripts/sync-version.mjs --check    # 只校验，有偏差则退出码 1
 */

import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join, relative } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const checkOnly = process.argv.includes("--check");
const rel = (p) => relative(root, p).split("\\").join("/");

/** tauri.conf.json 的 version 必须指向 package.json，而不是写死的版本号 */
const VERSION_REF = "../package.json";

/** 需要人工处理、脚本不代劳的问题（同步模式下也只会报出来） */
const manual = [];
/** 本次实际改掉的地方（同步模式） */
const applied = [];
/** 校验模式发现的不一致 */
const drift = [];

function bail(message) {
  console.error(`\n[version] ${message}\n`);
  process.exit(1);
}

function readFile(path) {
  try {
    return readFileSync(path, "utf8");
  } catch (error) {
    bail(`读不到 ${rel(path)}：${error.message}`);
  }
}

function eolOf(text) {
  return text.includes("\r\n") ? "\r\n" : "\n";
}

/**
 * 登记一处版本号出现位置。
 *
 * 值已经一致 → 什么都不做（**不重写文件**，否则每次构建都会刷新 mtime，
 * 白白触发一次 Cargo 重新编译）。
 * 值不一致 → 校验模式记入 drift，同步模式记入 applied 并返回 true，由调用方写回文件。
 */
function note(path, current) {
  if (current === version) return false;
  if (checkOnly) {
    drift.push(`${rel(path)} 里是 "${current}"，应为 "${version}"`);
    return false;
  }
  applied.push(`${rel(path)}: "${current}" -> "${version}"`);
  return true;
}

// ---------------------------------------------------------------------------
// 1. 读唯一来源
// ---------------------------------------------------------------------------

const pkg = JSON.parse(readFile(join(root, "package.json")));
const version = pkg.version;

if (typeof version !== "string" || !/^\d+\.\d+\.\d+/.test(version)) {
  bail(`package.json 的 version 不是合法的 semver："${version}"`);
}

// ---------------------------------------------------------------------------
// 2. src-tauri/tauri.conf.json —— 只校验引用形式
// ---------------------------------------------------------------------------

const confPath = join(root, "src-tauri", "tauri.conf.json");
const conf = JSON.parse(readFile(confPath));

if (conf.version !== VERSION_REF) {
  manual.push(
    `src-tauri/tauri.conf.json 的 version 应为 "${VERSION_REF}"，实际是 "${conf.version}"。\n` +
      `     它必须指向 package.json，不能写死版本号，否则又多出一处要手工同步的地方。`,
  );
}

// ---------------------------------------------------------------------------
// 3. src-tauri/Cargo.toml —— [package] 段的 version
// ---------------------------------------------------------------------------

const cargoPath = join(root, "src-tauri", "Cargo.toml");
{
  const text = readFile(cargoPath);
  const eol = eolOf(text);
  const lines = text.split(eol);
  let inPackage = false;
  let current = null;
  let targetIndex = -1;

  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    if (/^\s*\[/.test(line)) {
      inPackage = /^\s*\[package\]\s*$/.test(line);
      continue;
    }
    if (!inPackage) continue;
    const match = line.match(/^(\s*version\s*=\s*")([^"]*)(".*)$/);
    if (match) {
      current = match[2];
      targetIndex = i;
      lines[i] = match[1] + version + match[3];
      break;
    }
  }

  if (current === null) {
    bail("src-tauri/Cargo.toml 的 [package] 段里没找到 version 字段");
  }
  if (note(cargoPath, current)) {
    writeFileSync(cargoPath, lines.join(eol), "utf8");
  }
}

// ---------------------------------------------------------------------------
// 4. src-tauri/Cargo.lock —— netsentry 包条目
//
// netsentry 是本地包，条目里没有 checksum，直接改版本号是安全的。
// ---------------------------------------------------------------------------

const lockPath = join(root, "src-tauri", "Cargo.lock");
{
  const text = readFile(lockPath);
  const match = text.match(/(\[\[package\]\]\r?\nname = "netsentry"\r?\nversion = ")([^"]*)(")/);
  if (!match) {
    manual.push(`${rel(lockPath)} 里没找到 netsentry 的包条目（跑一次 cargo build 就会生成）`);
  } else if (note(lockPath, match[2])) {
    writeFileSync(lockPath, text.replace(match[0], match[1] + version + match[3]), "utf8");
  }
}

// ---------------------------------------------------------------------------
// 5. package-lock.json —— 根 version 两处
//
// 文件里后面那些 "version" 都是各个依赖自己的版本，所以只取前两处：
//   1) 根对象的 version
//   2) packages."" （项目自身）的 version
// ---------------------------------------------------------------------------

const npmLockPath = join(root, "package-lock.json");
{
  const text = readFile(npmLockPath);
  let seen = 0;
  let first = null;

  const patched = text.replace(/("version"\s*:\s*")([^"]*)(")/g, (whole, head, value, tail) => {
    seen += 1;
    if (seen > 2) return whole;
    if (seen === 1) first = value;
    return head + version + tail;
  });

  if (seen < 2) {
    bail(`${rel(npmLockPath)} 里没找到预期的两处 version 字段（锁文件格式可能变了）`);
  }
  if (note(npmLockPath, first)) {
    writeFileSync(npmLockPath, patched, "utf8");
  }
}

// ---------------------------------------------------------------------------
// 6. 汇报
// ---------------------------------------------------------------------------

if (checkOnly) {
  if (drift.length > 0 || manual.length > 0) {
    console.error("\n[version] 版本号不一致：\n");
    for (const item of [...drift, ...manual]) console.error(`  - ${item}`);
    console.error(`\n  版本号只在 package.json 里维护（当前 ${version}）。修一次就好：\n`);
    console.error("      npm run version:sync\n");
    process.exit(1);
  }
  console.log(`[version] 一致，全部为 ${version}`);
  process.exit(0);
}

if (applied.length > 0) {
  console.log(`[version] 已同步到 ${version}：`);
  for (const item of applied) console.log(`  - ${item}`);
} else {
  console.log(`[version] 已是最新（${version}）`);
}

if (manual.length > 0) {
  console.error("\n[version] 另有需要人工处理的问题：\n");
  for (const item of manual) console.error(`  - ${item}`);
  process.exit(1);
}
