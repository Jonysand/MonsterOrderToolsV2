# MonsterOrderWilds-Ascendance（怪猎荒野 弹幕点怪互动工具）

[![Tauri v2](https://img.shields.io/badge/Tauri-v2.11-blue.svg)](https://v2.tauri.app/)
[![React 19](https://img.shields.io/badge/React-19.1-cyan.svg)](https://react.dev/)
[![Tailwind CSS 4](https://img.shields.io/badge/TailwindCSS-v4.0-38bdf8.svg)](https://tailwindcss.com/)
[![License](https://img.shields.io/badge/License-Apache--2.0-green.svg)](LICENSE)

基于 **Tauri v2 + React 19 + TypeScript + Tailwind CSS 4 + Rust** 构建的下一代跨平台怪猎直播弹幕互动工具。

> **命名说明**：对外发布产物统一使用品牌名 `MonsterOrderWilds-Ascendance`（Windows exe、macOS `.app` / `.dmg`、NSIS / MSI 安装包与开始菜单显示名均取此名）；Rust crate 名 `mhdanmutoolsv2`、bundle identifier `com.jonysand.danmutools` 与数据目录 `MonsterOrderWilds_configs` 属技术标识，保持不变。

---

## 🌟 核心特性与优势

1. **极致轻量，原生 Standalone 单文件 EXE**：
   - 依赖 Windows 10/11 内置的 WebView2 运行时，无需捆绑庞大的 Chromium 引擎和 Node.js。
   - 最终输出的生产独立可执行文件 `MonsterOrderWilds-Ascendance.exe` **体积仅约 4 MB**。
   - **真单文件**：零解压、瞬时毫秒级启动、不写 `%TEMP%`，极大降低直播 3A 大作（如《怪物猎人：荒野》）时的 CPU 与内存消耗。
2. **多窗口协同架构**：
   - **主控制台 (`main`)**：设置直播间参数、实时管理排队队列、手动点单、各功能模块开关与状态监控。
   - **桌面置顶透明悬浮窗 (`overlay`)**：无边框半透明背景（透明度仅作用于背景，文字保持不透明），支持任意拖拽移动（`data-tauri-drag-region`）、`Alt+,` 全局热键一键锁定穿透、位置记忆，主播点击条目上的「完成」按钮保序出队。
3. **OBS 推流兼容**：
   - 桌面点怪悬浮窗为标准置顶透明窗口，在 OBS 中使用【窗口捕获】直接捕获即可推流，无需额外本地服务或浏览器源。
4. **业务保序算法与 Lite 模式 (ONLY_ORDER_MONSTER)**：
   - 保留核心排队优势：条目完成删除后，队列剩余元素相对顺序保持严格不变。
   - 运行时开关一键切换纯排队 Lite 模式，彻底停用打卡、TTS、点赞奖卡、GM 与 AI 等非排队模块（后端统一守卫 `ensure_not_lite` + 前端置灰降级）。
   - 逐功能覆盖矩阵见 [docs/LITE_COVERAGE_MATRIX.md](docs/LITE_COVERAGE_MATRIX.md)。
5. **首次使用：导入凭据文件**：
   - 出于安全考虑，安装包**不随包分发** B 站开放平台凭据 `credentials.dat`。
   - 请在「设置」页的「敏感凭据加密托管」卡片点击 **导入凭据文件**，选择由原工程生成（或随原始发行包提供）的 `credentials.dat`；
     程序会先做 Base64 + HMAC-SHA256 校验，再复制到数据目录并**即时生效**（无需重启）。
   - 凭据文件格式与原工程**双向兼容**（已用真实文件复算 HMAC 验证）。

---

## 📂 目录结构

```
MonsterOrderWilds-Ascendance/
├── src-tauri/                         # Tauri v2 原生宿主 (Rust)
│   ├── Cargo.toml                     # 依赖与 Release 优化 (LTO, Strip)
│   ├── tauri.conf.json                # 多窗口、权限、资源打包与构建策略
│   ├── capabilities/                  # 窗口能力与权限安全白名单
│   └── src/
│       ├── lib.rs                     # 核心状态、命令注册、Lite 守卫、业务总线与单元测试
│       ├── bilibili.rs                # B 站开放平台长连、弹幕解析、五态连接状态机
│       ├── tts.rs                     # 多引擎 TTS（Manbo/MiMo/SAPI 自动级联）、音频串行队列
│       ├── checkin.rs / checkin_ai.rs # 打卡 / 补签 / 点赞奖卡 / 关键词学习与 AI 回复
│       ├── config.rs / credentials.rs / registry.rs / paths.rs  # 配置、凭据、注册表与资源路径
│       ├── queue.rs / monster.rs      # 保序排队算法 / 怪物别名匹配
│       ├── logging.rs                 # 运行日志（内存环 + Logs/ 落盘）
│       └── main.rs                    # 应用启动入口
├── src/                               # 现代化前端界面 (React 19 + TypeScript)
│   ├── views/
│   │   ├── MainWindow.tsx             # 主控制台视图
│   │   └── OverlayWindow.tsx          # 桌面置顶透明悬浮窗视图
│   ├── components/                    # 虚拟列表 / 长文本往返滚动等复用组件
│   ├── types.ts                       # 全局数据模型声明
│   ├── App.tsx                        # 路由与窗口视图动态分发
│   ├── App.css                        # Tailwind CSS 配置
│   └── main.tsx                       # React 渲染入口
├── scripts/                           # 提交门禁脚本（编码核查 / 配置字段接线核查）
├── docs/                              # 架构、迁移与发版文档
│   ├── MIGRATION_COMPLETION_PLAN.md   # 迁移完整性审计与批次 A~E 修复计划（含实施状态）
│   ├── LITE_COVERAGE_MATRIX.md        # Lite 模式逐功能覆盖矩阵与新增功能规则
│   ├── ARCHITECTURE_DESIGN.md         # 总体系统架构设计与通信模型
│   ├── MIGRATION_ANALYSIS.md          # 技术栈剖析与迁移评估报告
│   └── DEVELOPMENT_GUIDE.md           # 本地开发、提交门禁与单元测试指南
├── AGENTS.md                          # AI 代理开发规范与编码约束
├── package.json                       # 前端依赖与脚本配置
├── vite.config.ts                     # Vite 构建配置
└── tsconfig.json                      # TypeScript 严格模式配置
```

---

## 🛠️ 常用开发命令

在当前项目根目录下：

### 1. 启动桌面端开发联调（支持前端热更新 + Rust 增量编译）
```bash
npm run tauri dev
```

### 2. 运行单元测试
```bash
cargo test --manifest-path src-tauri/Cargo.toml
```
> 输出包含 `[PASS]` 标记，验证保序删除算法、别名匹配、打卡算法与 Lite 模式守卫等。

### 3. 提交门禁一键验证（构建 + 单测 + 编码核查）
```bash
npm run verify
```

### 4. 构建发布生产包（Standalone 单文件 EXE + 安装包）
```bash
npm run tauri build
```
编译产物位于：
* **独立单文件 EXE**：`src-tauri/target/release/MonsterOrderWilds-Ascendance.exe` (~4MB)
* **标准 NSIS 安装包**：`src-tauri/target/release/bundle/nsis/MonsterOrderWilds-Ascendance_0.1.3_x64-setup.exe` (~3.3MB)
* **MSI 安装包**：`src-tauri/target/release/bundle/msi/MonsterOrderWilds-Ascendance_0.1.3_x64_en-US.msi` (~4.0MB)

---

## 📖 详细文档导航

* 架构与方案：请参阅 [docs/ARCHITECTURE_DESIGN.md](docs/ARCHITECTURE_DESIGN.md)
* **审计与修复记录**：请参阅 [docs/AUDIT_FIX_REPORT.md](docs/AUDIT_FIX_REPORT.md)（原工程全量交叉审计发现的 P0/P1 缺陷、修复方式与复验证据）
* 迁移与修复计划：请参阅 [docs/MIGRATION_COMPLETION_PLAN.md](docs/MIGRATION_COMPLETION_PLAN.md)
* Lite 模式覆盖矩阵：请参阅 [docs/LITE_COVERAGE_MATRIX.md](docs/LITE_COVERAGE_MATRIX.md)
* 调研与对比：请参阅 [docs/MIGRATION_ANALYSIS.md](docs/MIGRATION_ANALYSIS.md)
* 开发与测试：请参阅 [docs/DEVELOPMENT_GUIDE.md](docs/DEVELOPMENT_GUIDE.md)
* 规范与规则：请参阅 [AGENTS.md](AGENTS.md)

---

## 🎨 素材与致谢

* **应用图标**：原创作者为 [B 站空间 20253814](https://space.bilibili.com/20253814)，图标由 **ChatGPT Image** 在其原创图基础上风格化修改而来，用作 `MonsterOrderWilds-Ascendance` 的应用图标。