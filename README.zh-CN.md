<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="./assets/hero-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="./assets/hero-light.svg">
  <img alt="AGY Hyper — Antigravity 原生 Rust 加速引擎与实时驾驶舱" src="./assets/hero-dark.svg" width="100%">
</picture>

<br/>

[![CI](https://img.shields.io/github/actions/workflow/status/BobbyCats/agy-hyper/ci.yml?branch=main&style=flat-square&label=CI&logo=githubactions&logoColor=white)](https://github.com/BobbyCats/agy-hyper/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-0EA5E9.svg?style=flat-square)](./LICENSE)
[![Rust](https://img.shields.io/badge/单文件引擎-Rust_~10MB-111827.svg?style=flat-square&logo=rust&logoColor=white)](#系统架构)
[![UDS RTT](https://img.shields.io/badge/守护进程延迟-0.12ms-16A34A.svg?style=flat-square)](#可复现实测数据)
[![Platform](https://img.shields.io/badge/支持平台-macOS_%C2%B7_Linux_%C2%B7_Windows-4B5563.svg?style=flat-square)](#一行安装装完即用)
[![Follow on X](https://img.shields.io/badge/X-@BobbyCat2025-000000.svg?style=flat-square&logo=x&logoColor=white)](https://x.com/BobbyCat2025)

[English](./README.md) · **简体中文** · [一行安装](#一行安装装完即用) · [AI 一键装机](#ai-一键安装与体检) · [侧边栏驾驶舱](#agy-hyper-deckantigravity-原生侧边栏驾驶舱) · [实测数据](#可复现实测数据) · [卸载说明](#卸载与保留说明) · [参与贡献](./CONTRIBUTING.md)

</div>

---

让 AI 查一处鉴权逻辑，它先翻目录，再搜关键词，读到一半超 10KB 被截断落盘，接着再猜下一个文件。来回 5 轮，38 秒过去了，业务代码一行没改。拖慢节奏的往往是每一轮重新排队等大模型吐出第一个字。

**AGY Hyper 把这些冤枉路直接砍掉。** 一个 ~10MB、无需额外环境的纯 Rust 常驻引擎，把多轮代码摸底压进一次 `14ms` 内存缓存打包，给每一次多文件修改加上原子备份与一键撤回，并在 Antigravity 右侧边栏嵌入实时遥测与用量成本驾驶舱。

> **100% 官方插件协议**：全程基于 Antigravity 官方 MCP、Lifecycle Hooks、Rules 与 `SIDECAR_UI_ENTRYPOINT_AUX_PANE` 构建。**不注入 Electron DOM，不开启 `--remote-debugging-port` 调试端口，不修改 `~/.gemini/config/config.json`**，可与各类主题、汉化插件零冲突共存。

---

## 10 秒看懂：装上之前 vs 装上之后

哪怕你平时不写底层代码，下面三组对比也能直接感受到差别。

### 1. 干活速度：来回跑五趟拿工具 vs 揣着户型图直接干
- **没装之前**：让 AI 修客厅漏水。它先跑一趟储藏室看眼管子（等 6 秒），回来问扳手放哪了（等 6 秒），再跑一趟发现工具箱太沉搬不动又扔在半路（输出超 10KB 截断落盘，再等 6 秒）。来回跑 5 趟，38 秒全耗在上下楼按电梯上。
- **装上之后**：出门前直接把整栋楼的户型图、水电表和关键源码切片一次塞进它手里。`14ms` 摸清全局，省掉 4 趟冤枉路，进门直接动手改代码。

### 2. 改文件安全：拿签字笔直接涂原件 vs 先锁保险柜再动笔
- **没装之前**：AI 一口气改 5 个文件，像拿不可擦的签字笔在合同原件上直接涂改。改到第 3 页突然报错崩掉，前两页已经面目全非。一不留神还顺手翻开你抽屉里的 `.env` 密钥文件。
- **装上之后**：动笔前先把原件逐字节锁进本地 WAL 保险柜，所有改动在临时草稿纸上写完、1 毫秒扫完语法错，再一整叠原子替换。改崩了？点一下侧边栏的 **一键撤回**，所有文件按字节恢复如初。抽屉里的密钥文件默认贴封条，AI 连瞄一眼都瞄不到。

### 3. 长对话与子代理：蒙眼开盲车 vs 装上实时遥测与成本驾驶舱
- **没装之前**：聊着聊着 AI 突然变慢还变贵，你不知道当前模型的缓存到底几分钟变冷，不知道窗口里是被哪些读文件或命令输出撑爆的，也不知道后台子代理是不是疑似卡死或悄悄触顶，更不知道近 7 天烧掉了多少等价算力。
- **装上之后**：Antigravity 右侧边栏长出一块实时驾驶舱。当前水位、**分模型实测校准缓存保鲜窗口**、**上下文构成 X-Ray 透视**、子代理树实时状态（运行中红线与 `STALLED` 疑似卡死告警）、`/compact` 与跨模型缓存重算提示，以及纯离线 `24H / 7D / 30D / 90D` 多币种用量账本一眼看穿；同时为 Antigravity CLI 提供 `<5ms` 原生状态栏与窗口标题接入。

---

## 一行安装，装完即用

不需要装 Rust，不需要装 `ripgrep` 或 Python，也不依赖本地 `git` 命令行。安装脚本强制通过 **SHA-256 校验** 后原子替换，绝不修改你的 `~/.gemini/config/config.json`。

```bash
# macOS & Linux（默认 Balanced 模式；追加 `-s -- --observe` 可开启零侵入纯观测模式）
curl -fsSL https://raw.githubusercontent.com/BobbyCats/agy-hyper/main/install.sh | bash
```

```powershell
# Windows (PowerShell；安装前设 `$env:AGY_HYPER_OBSERVE="1"` 可启零侵入纯观测模式)
irm https://raw.githubusercontent.com/BobbyCats/agy-hyper/main/install.ps1 | iex
```

```bash
# 从源码构建安装
git clone https://github.com/BobbyCats/agy-hyper.git && cd agy-hyper && ./install.sh
```

### AI 一键安装与体检

将下面这段提示词直接发给 **Antigravity / Claude Code / Codex / Cursor**，AI 会按仓库内置的 [SKILL.md](./SKILL.md) 自动完成安装并跑完 `doctor` 体检：

> 请读取 `https://raw.githubusercontent.com/BobbyCats/agy-hyper/main/SKILL.md`，按我当前系统执行 AGY Hyper 安装脚本（macOS/Linux 用 `install.sh`，Windows 用 `install.ps1`），装完后运行 `agy-hyper doctor` 并汇报体检结果。

装完切回 Antigravity 即可自动加载。随时也可以在终端体检、查用量账本，或者独立打开驾驶舱：

```bash
agy-hyper doctor                  # 检查 Tree-sitter 语法、gix、守护进程延迟与安全边界
agy-hyper audit --svg audit.svg   # 运行 14 天本机烧钱诊断并导出可分享 SVG 战报卡
agy-hyper usage --window 7d       # 查看近 24h / 7d / 30d / 90d Token 与多币种等价估值
agy-hyper bench --cache-ttl       # 查看各模型族实测 Prompt 缓存衰减曲线与 TTL 拐点
agy-hyper cli-setup               # 输出（或 --apply 写入）Antigravity CLI statusLine 与 title 配置
agy-hyper ui                      # 在浏览器独立打开 AGY Hyper Deck (127.0.0.1:47821)
```

---

## 本机烧钱诊断战报卡（`agy-hyper audit`）

运行 `agy-hyper audit --days 14 --svg audit-card.svg`，100% 本地离线扫描你自己的 Antigravity 历史日志（零云端上传），直接揪出各模型真实缓存掉崖时间、未命中输入成本、首次改代码前上下文膨胀与地毯式预读次数，并生成一张可直接分享的矢量 SVG 诊断卡：

<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="./assets/audit-card-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="./assets/audit-card-light.svg">
    <img src="./assets/audit-card-dark.svg" alt="AGY Hyper 本机遥测诊断战报卡" width="100%"/>
  </picture>
</div>

---

## AGY Hyper Deck（Antigravity 原生侧边栏驾驶舱）

通过官方 `SIDECAR_UI_ENTRYPOINT_AUX_PANE` 协议原生嵌入右侧边栏，100% 本地离线内置 **Apache ECharts 6** 与 **i18next**（`English / 简体中文 / 日本語`，零外网 CDN），自动同步宿主明暗主题。

- **分层零侵入架构**：支持先用 **纯观测（Observe）** 模式（只开侧边栏驾驶舱、烧钱诊断与被动 WAL 快照，完全不碰原生工具与 Diff 卡片），或切到 **安全稳健（Safe）**（开启打包读与同步校验，同时保留原生 `edit_file` 行内 Diff），也可以一键解锁 **均衡 / 狂暴** 全量 Rust MCP 加速，并在 **引擎** 页独立开关读/写/校验三个工具层。
- **简洁模式**面向日常使用：展示上下文水位、单轮 P50、本步与会话累计缓存命中、**子代理树状态条**（高亮运行中红线与静默 `>=90s` 的 `STALLED` 卡死子代理）、**本会话/整树等价估值缎带**（支持一键切货币）、**分模型自适应缓存保鲜条**（实测/默认 TTL 徽章与连续失败熔断预警）、四档分层模式切换，以及 **WAL 行级 Unified Diff 内联审阅 + 一键撤回**。
- **极客模式**展开五大深度视图：**耗时瀑布**（LLM / Hyper / 普通工具严格拆分）、**上下文轨迹**（双轴堆叠面积曲线 + **上下文 X-Ray 透视条**，拆分读文件/命令输出/Hyper 工具/思考回复并标注首次改动水位与地毯读次数）、**用量账本**（`24H / 7D / 30D / 90D` 增量统计 + 8 种货币 + 手动 **同步汇率** + **本机烧钱诊断与 TTL 断崖** 及一键导出 `.svg` 战报卡）、**引擎与缓存**（AST/打包命中 + 细粒度 MCP 开关）、**回滚与审计**。

<div align="center">
<table>
  <tr>
    <td align="center" width="50%">
      <strong>简洁模式 · 实测 TTL、四档模式与行级 WAL Diff（深色）</strong><br/>
      <sub>~180s 实测缓存保鲜 · Observe/Safe/Balanced/Hyper · 展开红绿 Diff 与一键撤回</sub><br/><br/>
      <img src="./assets/cockpit-simple-dark.png" alt="AGY Hyper Deck 简洁模式深色" width="100%"/>
    </td>
    <td align="center" width="50%">
      <strong>简洁模式 · 浅色主题与改动行数标记（浅色）</strong><br/>
      <sub>自适应宿主浅色主题 · 实测 TTL 徽章 · 原生 i18next 三语切换</sub><br/><br/>
      <img src="./assets/cockpit-simple-light.png" alt="AGY Hyper Deck 简洁模式浅色" width="100%"/>
    </td>
  </tr>
  <tr>
    <td align="center" width="50%">
      <strong>极客模式 · 用量账本、烧钱诊断与模型 TTL 断崖</strong><br/>
      <sub>多币种本地账本 · 实测断崖胶囊 · 核心发现 · 一键导出 SVG 战报卡</sub><br/><br/>
      <img src="./assets/cockpit-usage-dark.png" alt="AGY Hyper Deck 用量账本与诊断" width="100%"/>
    </td>
    <td align="center" width="50%">
      <strong>极客模式 · 双轴上下文曲线与上下文 X-Ray 透视</strong><br/>
      <sub>命中率双轴轨迹 · 压缩后窗口成分拆解 · 首改水位与地毯读计数</sub><br/><br/>
      <img src="./assets/cockpit-context-dark.png" alt="AGY Hyper Deck 上下文 X-Ray" width="100%"/>
    </td>
  </tr>
</table>
</div>

<details>
<summary><strong>更多视图：极客模式 · 逐轮耗时火焰图</strong></summary>
<br/>
<div align="center">
  <img src="./assets/cockpit-pro-dark.png" alt="AGY Hyper Deck 极客瀑布图" width="520"/>
</div>
</details>

---

## 工作原理与安全边界

<div align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="./assets/mechanics-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="./assets/mechanics-light.svg">
  <img alt="AGY Hyper 核心工作原理图解" src="./assets/mechanics-dark.svg" width="100%">
</picture>
</div>

<br/>

1. **单回合语义打包（`hyper_pack`）**：一次并行扫描完成内置 `ripgrep`、7 种 Tree-sitter 语法解析（Rust / TypeScript / TSX / JavaScript / Python / Go / JSON）、1-hop 导入图与 `gix` 热点排序，并内联高分符号切片。输出严格压在 `<= 8.5KB` 内，避开 Antigravity 10KB 落盘截断，守护进程真 LRU 热命中约 `14ms`。
2. **三阶段原子 WAL（`hyper_edit`）**：排他 `flock` 加 BLAKE3 ETag 乐观锁，先 `fsync` 写入修改前预像，再经同目录临时文件原子 `rename`。遇到磁盘满（`ENOSPC`）直接清理临时文件并中止，绝不破坏源文件。改完立刻做 `<1ms` 语法校验，原生 `edit_file` / `write_file` 验证后的修改前预像也会自动入账，统一支持一键撤回。
3. **编译期零信任沙箱与防卡死看门狗**：只读 `gix` 在编译阶段去掉写引用与网络代码，全程零 `git` 子进程。`realpath` 牢笼拦截 `../`、软链接逃逸及悬空目录软链，密钥盾默认封禁 `.env` / `*.pem` / `*.key` / `id_rsa`（放行 `.env.example`）。`hyper_check` 固定 `shell: false`，保留头 64KB 加尾 512KB 环形缓冲，为测试与脚本提供智能静默看门狗（`idle_timeout_secs`）与双条件崩溃断电（`fail_fast`），同时对编译与静态检查 Preset 默认免杀防误伤；超时、中止或取消均按进程组整树清理。
4. **无锁遥测、实测 TTL、上下文 X-Ray 与 CLI 状态栏**：驾驶舱轮询、CLI `StatuslinePulse`、回滚及字节偏移增量账本（`usage_ledger.json` v2，180 天滚动保留，8 种货币换算）均在守护进程全局锁外执行，绝不拖慢并发 MCP 请求。账本按模型族自动统计冷暖衰减拐点校准真实缓存 TTL（`agy-hyper bench --cache-ttl`）；单遍扫描产出 `/compact` 后的上下文 X-Ray 构成与首次改动水位；`PreInvocation` 在模型自适应水位线（`96k / 115k`、`140k / 175k`、`180k / 280k`）之外新增连续报错熔断（`[AGY Hyper Loop Guard]`）；`agy-hyper statusline` 支持 `<5ms` 输出 Antigravity CLI 原生状态栏与纯文本窗口标题。

---

## 系统架构

<div align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="./assets/architecture-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="./assets/architecture-light.svg">
  <img alt="AGY Hyper 系统架构" src="./assets/architecture-dark.svg" width="100%">
</picture>
</div>

### 5 个核心 MCP 工具（全员锁死 `<= 8.5KB` 内联）

压缩版参数 Schema 已预置在 `plugin/rules/AGENTS.md`，Agent 无需额外浪费回合去读工具定义即可直接调用。

| 工具 | 性质 | 能力 |
| :--- | :---: | :--- |
| `hyper_pack` | `只读` | 单遍 ripgrep + Tree-sitter 符号与核心切片 + 1-hop 导入图 + `gix` 热点 + BLAKE3 `etag`。 |
| `hyper_search` | `只读` | 多关键词并行纯 Rust 搜索，兼顾 `.gitignore` 文件列举与零子进程 `gix` 只读 Git 状态。 |
| `hyper_edit` | `原子 WAL` | 多文件精确替换或 Tree-sitter `symbol` 块级替换，含 ETag 乐观锁、BOM/CRLF 保护与 `<1ms` 语法校验。 |
| `hyper_check` | `沙箱验证` | 自动识别安全 Preset（`cargo:check` / `pkg:test` / `py:pytest` / `go:vet` 及 Monorepo 子包），内置智能静默/崩溃看门狗、头尾环形缓冲与 `h_xxxx` 钻取句柄。 |
| `hyper_tx` | `撤回 / 钻取` | 查看 WAL 修改账本，支持一键原子 `rollback` 任意 `tx_id`，或 `1ms` 切片过滤超长检查日志。 |

---

## 可复现实测数据

Apple Silicon 实机 Release（LTO 优化）通过 `./bench/run-bench.sh` 实测（含完整冷进程启动与 MCP 握手开销）；GitHub Actions CI 在 **Ubuntu / macOS / Windows** 上持续验证 Rust 单测、Sidecar 语法与多语言一致性以及 Release 构建：

| 测试项 | 实测耗时 | 说明 |
| :--- | ---: | :--- |
| 常驻守护进程 UDS 心跳（`agy-hyper daemon status`） | **0.12 ms** | 单例 Unix Domain Socket（Windows 自动切带 Token 回环 TCP） |
| `hyper_pack`（守护进程 AST + 打包热命中） | **14 ms** | 含完整 MCP stdio 冷启与握手返回 |
| `hyper_search` + `gix` 只读 Git（单次调用） | **20 ms** | 对比基线 `git status && git log && grep -rn` 耗时 **43 ms**（**2.2x 提速**） |
| `hyper_pack`（冷启单遍 ripgrep + AST + 切片 + `gix` + 1-hop） | **58 ms** | 零外部 `rg` 与 `git` 二进制依赖 |
| `agy-hyper statusline`（守护进程热脉冲 + CLI stdin） | **4.2 ms** | 含进程冷启、无锁 UDS 通信与渲染（任何异常保证 `exit 0`） |
| Rust 单元与安全混沌测试全集（`48/48`） | **2.1 s** | 覆盖磁盘满 staging、悬空软链、4 语言 1-hop、WAL 回滚、静默/崩溃看门狗、实测 TTL 迁移、上下文 X-Ray、循环熔断与 CLI 状态栏 |

在任意本地仓库一键复现：

```bash
./bench/run-bench.sh /path/to/your/repo
```

<details>
<summary><strong>展开查看：CLI 常用命令、工程口径与供应链校验</strong></summary>

```bash
agy-hyper doctor                                # 全链路 Tree-sitter、gix、守护进程与安全体检
agy-hyper daemon start|stop|status              # 管理本地共享 Rust 守护进程
agy-hyper ui [--port 47821]                     # 在浏览器独立打开 AGY Hyper Deck
agy-hyper audit [--days 14] [--sessions 80] [--svg card.svg] [--theme dark|light] [--json]
agy-hyper usage [--window 24h|7d|30d|90d] [--currency CNY] [--json]
agy-hyper bench [--sessions 10] [--svg waterfall.svg] [--cache-ttl] [--json]
agy-hyper statusline [--format compact|rich|json] [--title] [--no-color]
agy-hyper cli-setup [--apply] [--format compact|rich]  # 打印或显式写入 Antigravity CLI 状态栏与标题配置
agy-hyper tx list                               # 查看 BLAKE3 WAL 修改账本
agy-hyper tx diff <tx_id> [--json]              # 查看任意 WAL 事务的行级 Unified Diff
agy-hyper tx rollback <tx_id> [--force]         # 原子恢复文件到修改前字节状态
agy-hyper config show                           # 查看当前生效配置
agy-hyper config profile observe|safe|balanced|hyper    # 切换分层观测 / 加速模式
agy-hyper config toggle enable_mcp_read|enable_mcp_edit|enable_mcp_check true|false
agy-hyper config currency USD|CNY|JPY|EUR|GBP|HKD|SGD|KRW
agy-hyper config fx <货币代码> <对美元汇率>       # 自定义本地离线汇率
```

- **真实 Token 与压缩边界**：直读 Antigravity 最新 `PLANNER_RESPONSE` 的 `input_tokens + cache_read_tokens`，自动识别 `/compact` 重置步数、解释跨模型缓存重算、过滤用户主动取消，并单独标注瞬态网络抖动（`proxy_reset` / `stream_timeout`）。
- **诚实用量与多币种估值**：字节偏移增量扫描本地日志写入 `usage_ledger.json`，Rust 核心全程零外网请求。历史未标注模型的旧日志计入总 Token 但不虚估费用（面板透明展示归因占比）；汇率默认采用离线基准，仅在手动点击 **同步汇率** 时发起一次按需同步。
- **无水分瀑布归因**：`llm_inference_ms` 取规划开始到首个工具触发，`tool_exec_ms` 取首个工具触发到末个工具完成。剔除超 `180s` 人类停顿回合时，同步从分子剔除对应 `output_tokens`。
- **供应链纪律**：绝不在后台静默更新。每个发行版附带 `SHA256SUMS` 与 Sigstore/Cosign 构建来源证明。

</details>

---

## 卸载与保留说明

随时可以干净卸载，无残留：

```bash
# macOS & Linux（默认保留你的配置、用量账本与 WAL 撤回快照）
curl -fsSL https://raw.githubusercontent.com/BobbyCats/agy-hyper/main/install.sh | bash -s -- --uninstall

# macOS & Linux（彻底清空，包括配置与 WAL 快照）
curl -fsSL https://raw.githubusercontent.com/BobbyCats/agy-hyper/main/install.sh | bash -s -- --uninstall --purge
```

```powershell
# Windows PowerShell（默认保留配置、账本与 WAL 快照）
$env:AGY_HYPER_UNINSTALL = "1"; irm https://raw.githubusercontent.com/BobbyCats/agy-hyper/main/install.ps1 | iex

# Windows PowerShell（彻底清空）
$env:AGY_HYPER_UNINSTALL = "1"; $env:AGY_HYPER_PURGE = "1"; irm https://raw.githubusercontent.com/BobbyCats/agy-hyper/main/install.ps1 | iex
```

- **移除范围**：停止本地 Rust 守护进程与 Hyper Deck 侧边栏服务，删除 `~/.gemini/config/plugins/agy-hyper`，清理 `~/.local/bin/agy-hyper` 软链（Unix）或用户 `PATH` 中的插件目录（Windows）。
- **默认保留**：保留 `~/.gemini/antigravity/agy-hyper/`（`config.json`、`usage_ledger.json` 与 `wal/` 撤回快照），防止误删自定义单价或文件备份；如需彻底清空，追加 `--purge`（Windows 为 `-Purge`）即可。安装与卸载全程均不触碰 `~/.gemini/config/config.json`。

---

## 参与共建与开源致谢

- **欢迎提 PR**：本项目采用完全开放的 **MIT 协议**（个人与企业工作电脑均可免费使用）。无论是新增 Tree-sitter 语言语法（C/C++/Java/Swift），补充构建 Preset 和多语言翻译，还是在 Windows / Linux 上提交实测反馈，都欢迎直接提 Issue 或 Pull Request。起步指引见 [CONTRIBUTING.md](./CONTRIBUTING.md)。
- **开源致谢**：本项目的探索缘起于 [INK-Q/agy-fast](https://github.com/INK-Q/agy-fast) 对 Antigravity 延迟与工具链路加速的先行尝试，感谢开源社区提供的启发。
- **关注作者**：AGY Hyper 由 [@BobbyCat2025](https://x.com/BobbyCat2025) 持续开源迭代。如果你对 Rust 工程提效、Antigravity 实战玩法以及 AI 工具拆解感兴趣，欢迎在 X 上关注交流：[x.com/BobbyCat2025](https://x.com/BobbyCat2025)。

---

MIT © [BobbyCats](https://github.com/BobbyCats) & AGY Hyper Contributors.
