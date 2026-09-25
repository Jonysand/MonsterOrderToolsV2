# MonsterOrderWilds-Ascendance 开发与维护指南

本文档为开发者提供本地环境配置、调试开发、单元测试与构建发布的完整操作指引。

---

## 一、 环境依赖

* **操作系统**：Windows 10 / 11 (x64)；macOS（Apple Silicon，环境与产物见第五节）
* **Node.js**：>= 20.x (当前验证：Windows `v24.14.0` / macOS `v24.14.1`，npm `11.9.0`)
* **Rust**：稳定版 (Windows 实测 `1.98.1` x86_64-pc-windows-msvc；macOS 实测 `1.98.1` aarch64-apple-darwin)
* **C++ 编译环境**：Windows 需 Visual Studio 2022 Community (包含 C++ 桌面开发工作负载)；macOS 需 Xcode 或 Command Line Tools（提供 clang 链接器，`xcode-select -p` 应返回开发者目录）

---

## 二、 常用脚本与命令

在项目根目录下执行（Windows：`D:\VisualStudioProjects\MHDanmuToolsV2`；macOS：`~/Documents/MonsterOrderToolsV2`）：

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
Windows 下执行后会在 `src-tauri/target/release/` 下生成：
* `MonsterOrderWilds-Ascendance.exe`：独立的单个 EXE 文件（~4MB），可直接拷贝分发；
* `bundle/nsis/`：标准 Windows 安装向导安装包。

macOS 下产出 `.app` 与 `.dmg`，路径与核验方法见第五节。

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

---

## 五、 macOS 本地构建与验证

平台差异只有产物形态：Windows 出 `.exe + MSI + NSIS`，macOS 出 `.app + .dmg`；命令与门禁口径完全一致。CI 双平台同口径执行（`.github/workflows/ci.yml` 的 `verify-macos`），Release 由 `.github/workflows/release.yml` 的 `bundle-macos` 产出 `.dmg`。

### 1. 环境准备

* **操作系统**：macOS（Apple Silicon，本机实测 `aarch64-apple-darwin`）；
* **Rust 工具链不在非交互 shell 的 PATH 中**（rustup 默认装到 `~/.cargo/bin`），执行 cargo 前先加：
  ```bash
  export PATH="$HOME/.cargo/bin:$PATH"
  ```
* 其余依赖（Node.js、Xcode / Command Line Tools）见第一节。

### 2. 构建与验证命令（与 Windows 相同）

```bash
npm run build                      # 图标清单 + tsc 类型检查 + vite 构建
cargo test --manifest-path src-tauri/Cargo.toml   # Rust 单测（亦可用 npm run test:rust）
npm run verify                     # 提交门禁：build + 单测 + 编码核查
npm run tauri build                # 生产打包（内部先跑 beforeBuildCommand 的 npm run build）
./build-macos.sh                   # 上述生产打包的一键封装：自动补 cargo PATH、校验依赖并打印产物路径
build-windows.bat                  # Windows 端同口径一键封装（双击或 cmd 执行，产出裸 exe + NSIS + MSI；文件须保持 GBK/ANSI 编码与 CRLF 换行，勿另存为 UTF-8）
```

判定成败时**不要**把命令接到 `| tail` 再看退出码 —— 管道退出码取自最右侧命令，cargo 根本不存在时也会返回 0，会造成「构建通过」的假象。应重定向日志后单独取退出码：

```bash
npm run tauri build > /tmp/tauri_build.log 2>&1; echo "EXIT=$?"; tail -30 /tmp/tauri_build.log
```

### 3. 产物路径（本机实测 0.1.4 / aarch64）

| 产物 | 路径 |
| --- | --- |
| 可执行文件 | `src-tauri/target/release/MonsterOrderWilds-Ascendance`（~18 MB） |
| App Bundle | `src-tauri/target/release/bundle/macos/MonsterOrderWilds-Ascendance.app` |
| 安装包 | `src-tauri/target/release/bundle/dmg/MonsterOrderWilds-Ascendance_<version>_aarch64.dmg`（~19 MB） |

* 版本号取自 `src-tauri/tauri.conf.json` 的 `version`，改版本后重新打包即得到新文件名；
* 产物为 **adhoc 签名、未公证**（既有策略），换机首次打开需右键「打开」放行；
* 单独执行 `cargo build --release` **不能**代替 `npm run tauri build`：前者产出指向 `devUrl`（localhost:1420）的开发态二进制，启动后 WebView 报 `ERR_CONNECTION_REFUSED`。

### 4. 产物核验（无需启动应用）

前端资源被压缩内嵌进二进制，`grep` 二进制搜**前端** UI 文案会 0 命中（假阴性，别据此判断产物过期）；**Rust 侧字符串是明文可搜的**，可用于证明产物确由本次改动构建：

```bash
BIN=src-tauri/target/release/MonsterOrderWilds-Ascendance
grep -c -- "deepseek-flash" "$BIN"   # 新串命中即新产物；被删除的旧串应命中 0
```

### 5. macOS 专属注意事项

* **`python` 是 2.7**（`/usr/local/bin/python`），`python3` 为 3.12：`package.json` 中的 python 脚本必须保持 py2/py3 双兼容；`npm run check:fields` 在本机必失败（既有问题，与改动无关，可用 `python3 scripts/check_orphan_fields.py` 复核）。
* **不要为验证界面而启动打包后的应用**：它会占用 `Alt+,` 全局热键并把置顶悬浮窗弹到屏幕上；纯前端改动优先在浏览器里用 IPC 桩页面验证，或走 `npm run dev`。
* **密码型输入框无法自动化**：macOS 安全输入模式会拦截合成键盘事件，身份码 / API Key 相关流程只能靠单测或人工点击。
* 打包态数据走 macOS 用户数据目录（脱离 `.app` 包体），`MonsterOrderWilds_configs` 资源由 `paths::ensure_seeded` 播种。