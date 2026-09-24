# Agent Rules for MonsterOrderWilds-Ascendance

## 项目来源与迁移对照规则
- **项目前身**：本项目（发布名 `MonsterOrderWilds-Ascendance`，仓库工程代号 `MHDanmuToolsV2`，Rust crate 名 `mhdanmutoolsv2`）是由原 C++/C# 工程 `D:\VisualStudioProjects\JonysandMHDanmuTools` 完整重构迁移而来。
- **架构映射**：
  - 原工程后端：C++ (`MonsterOrderWilds`) -> 现迁移为 Rust (`src-tauri/src/`)
  - 原工程前端：C# WPF (`JonysandMHDanmuTools` / `MonsterOrderWildsGUI`) -> 现迁移为 React + TypeScript + Tailwind CSS (`src/`)
- **行为与规范一致性准则**：
  - **原工程参考**：新增、修改或排查任何功能时，必须对照原项目 `D:\VisualStudioProjects\JonysandMHDanmuTools`（特别是其设计规格 `docs/superpowers/` 与核心源码实现），确保业务逻辑与交互行为与原工程对齐。
  - **敏感凭据安全托管**：严格遵循原工程加密配置文件 `credentials.dat`（Base64 + `@MonsterOrderSecret@` + HMAC-SHA256 签名）的加密托管机制，禁止手动明文设置或泄露敏感凭据。
  - **身份码注册表存储**：开播身份码 `IdCode` 必须严格参考原项目规范，独立持久化在 Windows 注册表 `HKEY_CURRENT_USER\Software\MonsterOrderWilds\IdCode`，实现与常规 JSON 配置文件的解耦与安全保护。
  - **业务算法一致性**：排队两段式置前算法（优先 > 舰长等级 1/2/3 > 首次入队时间）、怪物别名多重模糊匹配字典、SQLite 舰长打卡与自然周 30 赞奖卡算法、连续打卡天数倒推防误差算法、多引擎 TTS 熔断降级等，均以原工程权威实现为准。

## 语言规则
- 所有回答、解释、注释均使用中文
- 代码中的变量名、函数名遵循 Rust 与 TypeScript 惯例命名规范

## 代码复查与 Git 规则
- **每次代码修改后必须执行 `git diff` 复查**
- 确认修改范围正确，无多余改动
- **永远不要自动提交（commit）和推送（push）**，所有提交和推送必须由用户明确要求

## 单元测试规则
- **新增或修改任何业务功能时必须建立或同步更新单元测试**
- **单元测试在代码修改后立即运行验证**（`cargo test`）
- 测试输出使用 `[PASS]` 标记

## ONLY_ORDER_MONSTER 功能询问与设计规则
- **新增任何功能时，必须明确考虑并询问用户：此功能是否需要在 `ONLY_ORDER_MONSTER=1`（Lite 模式）下支持**
- 默认为不支持，即非排队功能代码默认受 `is_lite_mode` 控制
- Lite 模式下仅保留核心点怪排队队列和悬浮窗，停用 TTS、打卡和 AI 模块

## 文件编码规则
- **所有源代码文件统一使用 UTF-8 with BOM 编码**
- Rust 文件 (.rs): UTF-8 with BOM
- TypeScript/TSX 文件 (.ts/.tsx): UTF-8 with BOM
- CSS 文件 (.css): UTF-8 with BOM
- JSON/TOML/YAML 配置文件: UTF-8 无 BOM

## 编译验证规则
- **前端类型与打包检查**：
  ```bash
  npm run build
  ```
- **Rust 后端单元测试**：
  ```bash
  cargo test --manifest-path src-tauri/Cargo.toml
  ```
- **生产打包验证**：
  ```bash
  npm run tauri build
  ```