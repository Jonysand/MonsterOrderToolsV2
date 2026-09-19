# 生产版实测缺陷修复方案（UI Functional Test Fix Plan）

- **依据**：`docs/UI_FUNCTIONAL_TEST_REPORT.md`（2026-09-19 生产版全功能实测报告）
- **被测基线**：`f32e310`（标签 `v0.1.0`）
- **决策记录**：2026-09-19 用户确认「全部按推荐执行」——F1 走 Rust 侧 `confirm_action`、F5 仅澄清不改代码、F4 新按钮 Lite 下禁用
- **约束**：所有回答/注释使用中文；每次修改后 `git diff` 复查；不自动提交

---

## 一、范围总览

| 缺陷（报告等级） | 处置 | 涉及文件 | 状态 |
|---|---|---|---|
| P1 二次确认在打包版静默放行 | 代码修复（F1） | `src-tauri/src/lib.rs`、`src/views/MainWindow.tsx` | ☑ 已修复 + 回归通过（是/否双分支） |
| P2 模拟通道 uid 前缀不一致 | 代码修复（F2） | `src/views/MainWindow.tsx` | ☑ 已修复 + 回归通过（单一档案 `sim-回归甲`） |
| P2 模拟弹幕身份字段硬编码 | 代码修复（F3） | `src/views/MainWindow.tsx` | ☑ 已修复 + 回归通过（正向落库/负向拦截） |
| P3 礼物/SC/上舰无前端入口 | 代码修复（F4） | `src/views/MainWindow.tsx`、`docs/LITE_COVERAGE_MATRIX.md` | ☑ 已修复 + 回归通过（气泡 + SAPI；Lite 置灰无事件） |
| P3 手动点单路径偏离原工程 | 仅澄清，不改代码（F5） | `docs/UI_FUNCTIONAL_TEST_REPORT.md` | ☑ 澄清已写入报告 |
| P3 文档措辞与实现不符 | 文档修订（F6） | `docs/ARCHITECTURE_DESIGN.md` | ☑ 已修订 |

---

## 二、逐项方案

### F1（P1）二次确认 → 原生对话框（Rust 侧下沉）

**根因**：`wry 0.55.1` 仅在 Android 侧实现 JS 对话框，`tauri-runtime-wry 2.11.4` 未处理 WebView2 的 `DialogRequested`；Chromium 在无 dialog delegate 时默认放行，`window.confirm()` 直接返回 `true`。

**方案**：新增 Rust 命令 `confirm_action`，复用导出功能已在生产版实测可用的 `tauri-plugin-dialog` 阻塞模式（`save_text_file_with_dialog` 同构）。

**线程安全依据**：`run_on_main_thread` 只向事件循环排队（`tauri-runtime-wry/src/lib.rs:1604`），Tauri v2 命令在非主线程执行——这正是现有导出对话框能正常弹出的原因，同一机制可安全复用于消息框。

**改动**：

1. `src-tauri/src/lib.rs` 新增 `cfg(not(test))` / `cfg(test)` 双分支 `confirm_with_dialog` 与 `confirm_action` 命令（必须保留 test 分支：rfd 静态导入 comctl32 v6 的 `TaskDialogIndirect`，测试二进制无清单会在加载期以 `STATUS_ENTRYPOINT_NOT_FOUND` 失败），并注册进 `invoke_handler`：

```rust
#[cfg(not(test))]
fn confirm_with_dialog(app_handle: &AppHandle, title: &str, message: &str) -> Result<bool, String> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    Ok(app_handle.dialog().message(message).title(title)
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::YesNo)
        .blocking_show())
}
#[cfg(test)]
fn confirm_with_dialog(_: &AppHandle, _: &str, _: &str) -> Result<bool, String> {
    Err("测试构建不弹出确认对话框".into())
}

#[tauri::command]
fn confirm_action(title: String, message: String, app_handle: AppHandle) -> Result<bool, String> {
    confirm_with_dialog(&app_handle, &title, &message)
}
```

2. `MainWindow.tsx` 新增 helper（失败按「未确认」处理，安全优先）：

```ts
const askConfirm = async (message: string, title = "确认操作") => {
  try { return await invoke<boolean>("confirm_action", { title, message }); }
  catch { return false; }
};
```

3. 替换 3 处 `window.confirm`（所在方法均已是 `async`，无需改签名）：
   - `MainWindow.tsx:338` 清空队列 → 标题「清空队列」
   - `MainWindow.tsx:524` 一键黑幕批量补签 → 标题「一键黑幕」
   - `MainWindow.tsx:536` 发卡 → 文案对齐原工程 `GMRetroactiveCardDialog.xaml.cs:220-228`「确认为 {昵称} 发放 {N} 张补签卡？」，标题「确认发放」

**备注**：原工程仅「发卡」有二次确认（清空队列、批量补签在原工程均无确认）；V2 保留三处确认属安全增强，不算偏离。

**验证**：类型检查 → 重打包 → 手工回归 3 处（「否」不执行、「是」执行）。

### F2（P2）uid 前缀统一

- `MainWindow.tsx:566`：`sim-like-${simLikeUser}` → `sim-${simLikeUser}`，与弹幕通道（`:489`）同口径。真实场景中弹幕与点赞本就同一 uid，统一后「点赞得卡 → 补签查询」可直接连通验证。
- 测试残留档案（`sim-like-测试水友`）位于 `target/release/...` 测试目录，整目录删除即可，无需迁移。

### F3（P2）模拟通道身份字段可配

- 新增状态：`simGuardLevel`（0/1/2/3）、`simHasMedal`、`simMedalLevel`。
- 模拟弹幕表单新增一行：舰长等级下拉（无/总督/提督/舰长）+ 粉丝牌复选 + 等级输入。
- `handleSimDanmu` 的 `has_medal / medal_level / guard_level` 改为读取状态（原 `:493-495` 硬编码）；未佩戴粉丝牌时 `medal_level` 发 0。
- 默认值沿用现值（舰长 3、佩戴粉丝牌 10 级），不改动既有验证习惯。
- **Lite 说明**：本项仅构造载荷、不新增模块入口，点怪在 Lite 下为核心功能，故不加 Lite 禁用。

### F4（P3）礼物 / SC / 上舰模拟入口

在模拟通道卡片内新增「直播间事件模拟」子区块，复用「模拟昵称」作为 uname（uid = `sim-${昵称}`）：

| 按钮 | 参数 | 调用载荷 |
|---|---|---|
| 模拟礼物 | 礼物名（默认「小心心」）、数量、付费复选 | `simulate_gift { event: { open_id, gift_id, uname, gift_name, gift_num, paid, combo: null } }`（`GiftEvent` 全字段 `serde(default)`，combo 空走动态连击跟踪） |
| 模拟 SC | 金额、内容 | `simulate_live_event { event: { kind: "SuperChat", user_id, uname, rmb, message } }` |
| 模拟上舰 | 等级下拉(1/2/3)、数量、单位(月) | `simulate_live_event { event: { kind: "Guard", user_id, uname, guard_level, guard_num, guard_unit } }` |

- `LiveEvent` 为 `#[serde(tag = "kind")]`（`bilibili.rs:343-362`），JSON 形态已核对。
- **Lite 处置（按 AGENTS 默认规则：不支持）**：三按钮 `disabled={isLite}`；后端 `handle_incoming_gift`（`lib.rs:840-847`）与 live_event 管道在 Lite 下本就静默丢弃。
- 备注：播报受「语音总开关」与付费过滤影响——关闭开关时仅发前端气泡。

### F5（P3）手动点单 upsert —— 澄清，不改代码

**实测新证据（修正报告口径）**：手动表单每次生成唯一 uid `manual-{时间戳}-{随机}`（`MainWindow.tsx:306`）；弹幕路径在 `bilibili.rs:806-820` 入队前已拦截重复。因此 `queue.rs:70-94` 的 upsert 分支在 UI 与弹幕两条链路**均不可达**（单锁保护无竞态）。

- 处置：不改代码；在报告与本文档中按上述口径澄清该分支为「不可达的防御逻辑」，`add_or_update` 语义与单测现状均不动。

### F6（P3）文档措辞修正

- `docs/ARCHITECTURE_DESIGN.md:111`：「控制台标题指示器变更为金黄色『Lite 纯排队模式』」改为描述实际实现：页面内琥珀色徽章 + 导航停用标签 + 卡片冻结。
- 依据：原工程 Lite 下既不改窗口标题、也无金色标签（仅隐藏两个 Tab，`ConfigWindow.xaml.cs:21-25`）；该句与两端实现均不符，属设计稿遗留措辞——修订文档而非实现 `setTitle`（实现反而偏离原工程）。

---

## 三、执行顺序与验证门禁

1. 顺序：F1 → F2 → F3 → F4 → F6 → F5（文档澄清收尾）。
2. 编码：`.tsx/.md` 保持 UTF-8 with BOM，完成后 `npm run check:encoding`。
3. 每次改动后 `git diff` 复查（AGENTS 规则）；不自动提交。
4. 门禁：`npm run build` + `cargo test --manifest-path src-tauri/Cargo.toml`（105 项应保持全绿）+ `npm run tauri build` 重打包。
5. 回归：computer-use 实测 3 处确认框（是/否两分支）、统一 uid 后点赞→补签查询联动、身份选择器对三条过滤链的验证、三个事件按钮（Lite 开/关各一次）。
6. 单测说明：本次改动为 UI 胶水层 + 调试通道，不涉及业务算法，无新增 Rust 单测；`confirm_with_dialog` 的 test 分支无法单测（无法构造 AppHandle），以打包版手工回归为准。
7. 收尾：在 `UI_FUNCTIONAL_TEST_REPORT.md` 各缺陷条目追加「已修复」状态。

---

## 四、执行记录

| 日期 | 步骤 | 结果 |
|---|---|---|
| 2026-09-19 | F1~F4 代码修复 + F6 文档修订 + F5 报告澄清 + Lite 覆盖矩阵同步 | 完成，`git diff` 逐项复查通过 |
| 2026-09-19 | `npm run build`（tsc + vite） | ✅ 通过（1888 modules，0 错误） |
| 2026-09-19 | `cargo test --manifest-path src-tauri/Cargo.toml` | ✅ 105 passed; 0 failed |
| 2026-09-19 | `npm run check:encoding` | ✅ OK（bom-required=36） |
| 2026-09-19 | `npm run tauri build` 重打包（exe + MSI + NSIS） | ✅ 通过（exe 17.8 MB，17:0x 时间戳） |
| 2026-09-19 | 回归 F1：清空队列 → 原生确认框弹出；点「否」队列保留（`order_list.json` 不变）；点「是」队列清空（→ `[]`） | ✅ 通过（对话框打开期间主窗口 `IsHungAppWindow=False`，应用健康） |
| 2026-09-19 | 回归 F3：`回归甲` 无舰长/未佩戴粉丝牌发「打卡」→ 不落库；改为舰长+佩戴后 → `user_profiles(sim-回归甲, 连续1天/累计1天)` + `checkin_records` 落库 | ✅ 通过（正向/负向双向验证） |
| 2026-09-19 | 回归 F2：点赞 30 次（昵称 `回归甲`）→ `retroactive_cards/user_daily_likes/user_like_streaks` 全部落在 `sim-回归甲`，与弹幕通道同档案 | ✅ 通过（旧 `sim-like-` 仅存于上一轮历史数据） |
| 2026-09-19 | 回归 F4：模拟礼物 ×3 → 悬浮窗 3 条气泡 + SAPI「感谢 回归甲 赠送的1个小心心」；模拟 SC → SAPI「感谢 回归甲 赠送的30元SC：加油！」；模拟上舰 → SAPI「感谢 回归甲 上船1个月的舰长」 | ✅ 通过（气泡 + 子进程双通道取证） |
| 2026-09-19 | 回归 F4-Lite：开启 Lite 后三按钮置灰（截图），点击置灰的「模拟 SC」无 SAPI 子进程、无日志、无提示 | ✅ 通过 |
| 2026-09-19 | 环境复位：测试数据目录 `is_lite_mode=false` 复位；真实数据目录零改动（`captain_profiles.db` 5/9、`order_list.json` 15:08） | ✅ |
| 2026-09-19 | 收尾：`UI_FUNCTIONAL_TEST_REPORT.md` 各缺陷条目追加「已修复」状态 | ✅ |

### 回归方法学注意事项（供后续复用）

1. **钉住测试数据目录**：用 `Start-Process` 启动 exe 时必须显式指定 `-WorkingDirectory` 为 exe 所在目录，否则 `config_dir()` 会按调用方 cwd 解析，可能落到仓库根真实数据目录（本次已排查并清理，真实数据零改动）。
2. **坐标空间**：自动化脚本须先 `SetProcessDpiAwarenessContext(PerMonitorV2)`，使 `GetWindowRect`/`SetCursorPos`/截图统一在物理像素空间；否则 150% 缩放下落点偏差约 1.5 倍。
3. **勿在确认框打开时对主窗口做跨进程 UIA 查询**：会让主线程卡在模态等待（表现为应用"假死"、对话框无法关闭）；确认识别用 `#32770` 窗口 + UIA 焦点在第 3/4 号按钮（是/否），或以真实鼠标点击按钮坐标。真实用户交互不受此影响（已实测：对话框打开期间应用健康、点击「否/是」均正常返回）。
4. **键盘路径**：WebView2 中 `<select>` 获得焦点后 `Home/End` 可直接改选项；复选框用 `Space`；表单提交按钮用 `Space`（`Enter` 在部分控件不提交）。
