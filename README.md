# NetSentry（网哨）

轻量级本机网络连接监控工具。常驻系统托盘，周期性检查网卡状态与外网连通性，
记录每一次断网的起止时间与持续时长，并提供一个可选的网页界面查看实时状态。

- 桌面端：Tauri 2（Rust + WebView2）
- 界面：原生 TypeScript + Vite
- 内嵌 Web 服务：axum

---

## 功能

| 功能 | 说明 |
|---|---|
| 网络状态监控 | 每 5 秒检查一次，兼顾"网卡是否拿到可用 IPv4"和"能否 ping 通外网" |
| 断网记录 | 记录每次断网的开始时间、结束时间与持续秒数，重启后仍然保留 |
| 断网提醒 | 断网时主界面切换到告警状态，实时显示已持续时长 |
| 双语界面 | 中文 / English，日志会跟着界面语言一起切换 |
| 常驻托盘 | 关闭窗口只是隐藏，不影响后台监控；托盘菜单可打开设置、日志、关于 |
| 内嵌 Web 界面 | 默认仅本机可访问，可选开放给局域网（需要访问令牌） |
| 开机自启 | 写入当前用户的 `Run` 注册表项，可随时关闭 |

## 环境要求

| 组件 | 版本 |
|---|---|
| Node.js | `^20.19.0` 或 `>=22.12.0`（vite 8 的最低要求） |
| Rust | stable（`x86_64-pc-windows-msvc`） |
| MSVC | Visual Studio Build Tools，含 C++ 生成工具与 Windows SDK |

## 构建

```bash
npm install

# 开发（热更新）
npm run tauri dev

# 打包安装程序（NSIS + MSI）
npm run tauri build
```

产物位于 `src-tauri/target/release/bundle/`。

只做类型检查与前端构建：

```bash
npm run build          # tsc && vite build
cargo check --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings
```

### 关于 `custom-protocol` feature

`src-tauri/Cargo.toml` 里声明了：

```toml
[features]
custom-protocol = ["tauri/custom-protocol"]
```

**生产构建必须启用它。** Tauri 内部用 `dev = !custom_protocol` 决定前端资源是从磁盘读
还是从编进二进制的资源里读（见 `tauri/build.rs`）。如果没启用，程序会去读工作目录下的
`dist/`，装到别的机器上只会得到 404。

`npm run tauri build` 会自动启用。如果你绕过 CLI 直接用 cargo：

```bash
# 正确 —— 资源嵌入二进制
cargo build --release --features custom-protocol

# 错误 —— 产物依赖运行目录下存在 dist/
cargo build --release
```

## 版本号维护

版本号**只在 `package.json` 的 `version` 字段里维护一处**，其余位置全部由它派生：

| 位置 | 怎么拿到版本 | 要手工改吗 |
|---|---|---|
| 界面（关于对话框） | `vite.config.ts` 读 `package.json`，经 `define.__APP_VERSION__` 注入 JS，再用 `transformIndexHtml` 替换 HTML 里的 `{{APP_VERSION}}` | 不用 |
| `src-tauri/tauri.conf.json` | 值写成 `"version": "../package.json"`，Tauri 解析配置时自行读取 | 不用 |
| `src-tauri/Cargo.toml` | 由 `scripts/sync-version.mjs` 写入 | 不用，跑同步 |
| `src-tauri/Cargo.lock` | 同上（`netsentry` 是本地包，条目无 checksum，可安全改写） | 不用，跑同步 |
| `package-lock.json` | 同上（根 `version` 两处） | 不用，跑同步 |

exe 的 `FileVersion` / `ProductVersion` 以及安装包版本，都取自 `tauri.conf.json` 解析出来的
那个 `version`（`tauri-build` 用它生成 Windows 版本资源）。所以只要 `package.json` 对了，
exe 属性就是对的。

### 改版本的正确姿势

```bash
# 推荐：改 package.json 并自动触发同步，最后一起提交
npm version 0.2.0          # 或 patch / minor / major

# 手工改了 package.json 的 version 之后
npm run version:sync

# 只校验不修改（CI 里跑的就是这个，有偏差时退出码为 1）
npm run version:check
```

`npm version` 会把 `Cargo.toml` / `Cargo.lock` 一并 stage 进这次版本提交。

有三道保险挡住版本号漂移：

1. `tauri dev` / `tauri build` 的 hook 里带了 `npm run version:sync`，构建前先对齐；
2. `src-tauri/build.rs` 在编译期比对 `package.json` 与 `Cargo.toml`，不一致就直接让构建
   失败并打印修复命令——覆盖"直接用 `cargo build`"的情况；
3. CI 的 `Check version consistency` 步骤跑 `npm run version:check`。

## 目录结构

```
.
├── i18n.json            # 语言资源，前端 import、Rust include_str! 共用同一份
├── index.html
├── vite.config.ts       # 注入 __APP_VERSION__、替换 HTML 里的 {{APP_VERSION}}
├── scripts/
│   └── sync-version.mjs # 版本号同步 / 校验，唯一来源是 package.json
├── src/
│   ├── main.ts          # 界面逻辑
│   ├── env.d.ts         # __APP_VERSION__ 的类型声明
│   └── styles.css
└── src-tauri/
    ├── build.rs         # 编译期校验版本号一致性
    ├── Cargo.toml
    ├── tauri.conf.json  # 窗口、CSP、打包配置
    └── src/
        ├── main.rs
        └── lib.rs       # 监控循环、托盘、内嵌 Web 服务、设置读写
```

## 配置与数据文件

| 内容 | 位置 |
|---|---|
| 设置 | `%APPDATA%\NetSentry\settings.json` |
| 断网历史 | `%APPDATA%\NetSentry\history.json` |
| 访问令牌 | `%APPDATA%\NetSentry\token.txt` |
| 日志 | `%LOCALAPPDATA%\NetSentry\logs\netsentry.log` |

日志超过 128 KB 会自动轮转为 `netsentry_<时间戳>.log`。

## 内嵌 Web 界面

启动后访问 `http://127.0.0.1:<端口>`（默认端口 8080）。

### 访问范围

默认**只监听 127.0.0.1**，局域网内的其它设备访问不到，也不会触发 Windows 防火墙弹窗。

如果需要在手机或另一台电脑上查看，在设置里打开 **允许局域网访问**，服务会改为监听
所有网卡。此时：

- 静态界面（HTML/CSS/JS）任何人都能打开；
- 所有 `/api/*` 接口，凡是非本机来源的请求都必须携带令牌。

### 访问令牌

令牌在首次运行时随机生成，写入 `%APPDATA%\NetSentry\token.txt`。两种传法都支持：

```bash
# 方式一：请求头
curl -H "Authorization: Bearer <token>" http://<本机IP>:8080/api/status

# 方式二：查询参数（浏览器里方便）
http://<本机IP>:8080/?token=<token>
```

浏览器打开带 `?token=` 的地址后会把令牌记在 `localStorage`，后续请求自动带上。

> 本机（127.0.0.1）访问不需要令牌，直接打开 `http://127.0.0.1:8080` 即可。

### HTTP 接口

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/status` | 当前网络状态与断网历史 |
| GET | `/api/settings` | 当前设置 |
| GET | `/api/logs` | 已翻译的日志（结构与 Tauri 命令一致） |
| POST | `/api/save-settings` | 保存设置（JSON 体，字段同 `settings.json`） |

## 检测逻辑说明

"是否在线" = **网卡状态正常** 且 **能 ping 通任一目标**（`8.8.8.8` / `1.1.1.1` / `114.114.114.114`）。

网卡状态的判据是"存在任意一张网卡拿到了可用的 IPv4 地址"，其中排除了：

- `127.0.0.0/8` 回环地址
- `169.254.0.0/16` 链路本地地址（APIPA）——Windows 在拿不到 DHCP 时的自动分配段，
  拔网线、WiFi 未连接、虚拟网卡都会落到这个段
- `0.0.0.0` 与广播地址

之所以不按网卡名称判断，是因为中文 Windows 上"以太网""本地连接"这类友好名称
匹配不上任何英文关键字。

## 已知限制

- 目前只在 Windows 上完整验证过；非 Windows 平台的 ping 参数已按平台区分（`-c 1 -W 1`），
  但开机自启尚未实现。
- 断网记录最多保留 500 条，超出后丢弃最旧的。
- 局域网访问的令牌是明文比对，适用于可信的家庭 / 办公内网，不建议直接暴露到公网。

## 许可证

MIT，见 [LICENSE](LICENSE)。
