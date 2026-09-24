# MonsterOrderWilds-Ascendance 开发与维护指南

本文档为开发者提供本地环境配置、调试开发、单元测试与构建发布的完整操作指引。

---

## 一、 环境依赖

* **操作系统**：Windows 10 / 11 (x64)
* **Node.js**：>= 20.x (当前验证：`v24.14.0`, npm `11.9.0`)
* **Rust**：稳定版 (当前验证：`1.98.1` x86_64-pc-windows-msvc)
* **C++ 编译环境**：Visual Studio 2022 Community (包含 C++ 桌面开发工作负载)

---

## 二、 常用脚本与命令

在项目根目录 `D:\VisualStudioProjects\MHDanmuToolsV2` 下执行：

### 1. 桌面端开发与热重载
```bash
npm run tauri dev
```
* 前端支持 Vite HMR（代码修改即时在窗口内生效）；
* Rust 侧代码修改后会自动增量重新编译。

### 2. 仅调试 Web 前端 (浏览器模式)
```bash
npm run dev
```
启动本地开发服务器 `http://localhost:1420`，可通过 Chrome/Edge 快速审查页面布局。

### 3. 执行单元测试
```bash
cargo test --manifest-path src-tauri/Cargo.toml
```
* 测试用例必须使用 `[PASS]` 标记输出；
* 每次增加或修改核心功能（如排队、打卡、配置）后，必须第一时间运行单元测试验证。

### 4. 提交门禁（一键验证，E4）
```bash
npm run verify
```
等价于依次执行：`npm run build`（TS 类型检查 + Vite 构建）→ `cargo test`（Rust 单测，全部用例输出 `[PASS]`）→ `npm run check:encoding`（编码规范核查）。
任何提交前必须全绿；单独执行可用：
* `npm run test:rust`：仅跑 Rust 单测；
* `npm run check:encoding`：编码规范核查（`.rs/.ts/.tsx/.css/.md` 需 UTF-8 with BOM，`.json` 需无 BOM）；
* `npm run check:fields`：配置字段接线核查（孤儿字段检测）。

### 5. 生产构建打包 (发布 Standalone 单文件)
```bash
npm run tauri build
```
执行后会在 `src-tauri/target/release/` 下生成：
* `MonsterOrderWilds-Ascendance.exe`：独立的单个 EXE 文件（~4MB），可直接拷贝分发；
* `bundle/nsis/`：标准 Windows 安装向导安装包。

---

## 三、 代码与文件规范

1. **文件编码规则**：
   - 所有的源代码与文档文件（`.rs`, `.ts`, `.tsx`, `.css`, `.md`）**必须统一使用 UTF-8 with BOM 编码**；
   - 配置文件（`package.json`, `tauri.conf.json`, `tsconfig.json`）使用 UTF-8 无 BOM 编码；
   - 提交前用 `npm run check:encoding` 自动核查。
2. **新增功能与 ONLY_ORDER_MONSTER 规则**：
   - 新增任何业务功能时，必须明确考虑是否需要在 `ONLY_ORDER_MONSTER=1`（Lite 纯排队模式）下支持；
   - 非排队功能（如 TTS、打卡、AI 互动）必须在命令入口调用统一守卫 `ensure_not_lite(&state, "模块名")?`；
   - 完整覆盖矩阵与新增功能声明规则见 `docs/LITE_COVERAGE_MATRIX.md`。
3. **版本控制规则**：
   - 每次代码修改后必须执行 `git diff` 严格复查改动；
   - 不得未经用户明确许可自动提交（commit）或推送（push）。

---

## 四、 如何添加新的前后端通信接口 (Tauri Command)

以添加 `save_live_room_id` 为例：

1. **Rust 侧（`src-tauri/src/lib.rs`）**：
   ```rust
   #[tauri::command]
   fn save_live_room_id(room_id: String, state: State<'_, AppState>) -> Result<(), String> {
       // 处理业务逻辑
       Ok(())
   }
   ```
2. **注册到 `generate_handler!`**：
   ```rust
   .invoke_handler(tauri::generate_handler![
       // ...原有命令,
       save_live_room_id
   ])
   ```
3. **前端调用（`src/views/MainWindow.tsx` 等）**：
   ```typescript
   import { invoke } from "@tauri-apps/api/core";

   await invoke("save_live_room_id", { roomId: "123456" });
   ```