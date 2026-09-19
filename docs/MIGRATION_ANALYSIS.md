# OpenHanako 技术栈剖析与迁移评估报告

本文档记录了从原有架构（C++ 原生库 + C# WPF）向现代跨平台技术栈演进的完整技术调研与决策过程。

---

## 一、 调研对象：OpenHanako (HanaAgent) 技术栈深度剖析

[OpenHanako](https://github.com/liliMozi/openhanako) 是一个具备记忆系统、人格化和多平台接入能力的桌面 AI Agent 个人助理。

### 1. 架构模式：Server-First + Electron 双层架构
* **核心服务层 (HanaAgent Server)**：
  - 基于 Node.js (>=24) + 高性能轻量框架 **Hono** (`@hono/node-server`, `@hono/node-ws`)。
  - 通过 HTTP RESTful API 和 WebSocket 暴露服务，使得服务本身能够同时服务 Desktop 端、CLI 终端、局域网多端和移动端 PWA。
* **数据与本地扩展**：
  - 持久化使用 `better-sqlite3`（C++ 原生模块编译的高性能 SQLite）。
  - 分词使用 `@node-rs/jieba`（Rust 编写的高性能原生扩展）。
  - 图像处理使用 `@silvia-odwyer/photon-node`（Rust 原生扩展）。
  - 终端交互使用 `node-pty`。
* **前端展示层 (Renderer)**：
  - React 19 + TypeScript + Vite 7 + Zustand（状态管理）+ Motion（动效）。
  - 编辑器与渲染：Tiptap 3, CodeMirror 6, markdown-it, KaTeX, Mermaid。
* **分发与打包**：
  - 基于 `electron-builder` 26，针对 Win (NSIS)、Mac (DMG/Notarize)、Linux (AppImage/deb) 进行多平台发布。
  - 基于 `electron-updater` 实现自动热更新。

---

## 二、 当前项目 (JonysandMHDanmuTools) 现状诊断

* **架构体系**：C++ 原生核心库 (`MonsterOrderWilds.dll`) + C# WPF 桌面界面 (`MonsterOrderWildsGUI`)，两层通过 P/Invoke (`DataBridge`) 桥接。
* **核心业务能力**：
  - B站直播开放平台 WebSocket 长连与签名算法。
  - 点怪保序排队队列 (`PriorityQueueManager`)。
  - 舰长周打卡、补签卡与用户档案管理 (`ProfileManager`)，涉及复杂的周周期判定和 SQLite 事务回滚。
  - TTS 语音引擎路由 (`TextToSpeech`)，包含 SAPI 离线兜底以及 Manbo、MiMo 在线引擎自动故障转移。
  - DeepSeek AI 思考互动。
* **Windows 深度绑定点**：
  - SAPI COM 组件（`ISpVoice`）。
  - Windows 注册表持久化敏感凭据。
  - Win32 窗口透明穿透、置顶及分层样式（`WS_EX_LAYERED`, `WS_EX_TRANSPARENT`, `WS_EX_TOPMOST`）。
  - WiX Toolset 生成 `.msi` 安装包。

---

## 三、 多平台技术选型决策：Electron vs Tauri v2

为了实现**跨平台、现代化 Web 界面以及 OBS 网页源原生直连**，对比了类似 OpenHanako 的 Electron 方案与更轻量级的 Tauri v2 方案：

| 评估指标 | Electron 路线 (OpenHanako 模式) | Tauri v2 路线 (最终推荐并采纳) | 针对直播工具的实际影响 |
| :--- | :---: | :---: | :--- |
| **Windows 单文件发布** | 需打包为 Portable 自解压单文件 | **真正单一原生 PE EXE (`.exe`)** | Tauri 无解压延迟，不写 `%TEMP%` 目录 |
| **单文件体积** | ~120MB - 180MB | **仅 4.07 MB (实测)** | **Tauri 缩减了 96% 体积**，极为便携 |
| **运行内存占用** | 150MB - 350MB | **30MB - 60MB** | 主播直播 3A 游戏时内存极其敏感，Tauri 优势明显 |
| **冷启动延迟** | 1~3 秒 (需先后台释放 Chromium 运行时) | **毫秒级瞬时启动** | 用户点击后无需等待沙漏 |
| **渲染内核** | 内置独立 Chromium | 系统内置 WebView2 (Win10/11 标配) | 无需重复打包浏览器引擎 |
| **OBS 网页源集成** | 优秀 (内置 Hono/Express) | **优秀 (前端路由或内置轻量服务)** | 彻底免去主播捕获桌面透明窗口的困扰 |
| **C++ 核心代码融合** | 编译为 Node-API 原生插件 | **可通过 `cc` crate 静态链接直接编入 exe** | 外部无任何散落的 `.dll` 动态库 |

---

## 四、 结论与落地成果

综合体积、冷启动性能、直播资源占用与 Windows 单文件交付需求，**最终决定采用 Tauri v2 作为 MHDanmuToolsV2 的基础技术底座**。
在实际验证中，生产编译输出的独立 `mhdanmutoolsv2.exe` **仅为 4.07 MB**，完美达成单文件 Standalone 的产品目标。