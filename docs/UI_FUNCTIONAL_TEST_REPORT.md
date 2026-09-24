# 生产版全功能实测报告（UI Functional Test Report）

- **被测产物**：`src-tauri/target/release/MonsterOrderWilds-Ascendance.exe`（批次 A~E 全部完成后构建，17.79 MB；安装包 MSI 11.65 MB / NSIS 9.96 MB）
- **对应提交**：`f32e310`（标签 `v0.1.0`）
- **测试日期**：2026-09-19
- **测试方式**：computer-use 驱动真实桌面 UI（主窗口 + 悬浮窗），以「界面截图 + 落盘文件 + SQLite + 日志 + 注册表 + 子进程探针」多路交叉取证
- **约束**：全程未新增或修改任何代码与文档（测试结束后 `git status` 干净）

---

## 一、结论摘要

| 维度 | 结果 |
|---|---|
| 通过功能点 | 28 项（点怪排队、悬浮窗、打卡/补签/点赞、TTS 降级链、配置持久化与热更新、Lite 守卫、GM、日志、退出） |
| 缺陷 | **P1 × 1**（二次确认在打包版静默放行）、P2 × 2、P3 × 3 |
| 无法通过 UI 验证 | 真实 B 站长连五态、打卡 AI、Manbo/MiMo 真实音质、500 条虚拟列表（均需凭据或批量造数） |
| 与原工程行为一致性 | 排队比较算法、去重语义、旧库表结构、打卡/补签/点赞文案、Lite 边界、退出流程均一致 |

**上线判定**：核心点怪链路（迁移审计时的 P0 重灾区）实测全部可用，可进入灰度。**2026-09-19 已完成修复轮**：本报告 6 项缺陷（P1×1、P2×2、P3×3）全部处置，并在重打包产物上回归通过（含 F1「二次确认」的是/否双分支、F2/F3 跨链路档案联动、F4 三事件按钮与 Lite 置灰行为），详见 `docs/UI_FUNCTIONAL_TEST_FIX_PLAN.md`。

---

## 二、通过项明细（附实测证据）

### 1. 启动、资源与路径（A3）

| 检查点 | 证据 |
|---|---|
| 数据目录解析 | 双击启动后落到 exe 同级 `target/release/MonsterOrderWilds_configs`（安装版语义），未污染仓库根目录的真实数据 |
| 首运行播种 | `monster_list.json`(36 KB)、`local_voices.zip`(267 KB)、`dict/`、`voices/` 自动就位 |
| 缺凭据容错 | 日志 `[WARNING] 加载 credentials.dat 失败: 凭证文件不存在` —— 仅告警，不阻断启动 |
| 热键注册 | 日志 `[INFO] [Hotkey] 已注册 Alt+, 锁定/解锁悬浮窗`（仅生产构建注册） |
| 怪物图标 | 主窗口与悬浮窗图标均正常渲染，历战条目自动切 `MHWorld_Tempered/...` |

### 2. 点怪排队核心链路（A1 + 排队算法）

| 检查点 | 证据 |
|---|---|
| 别名「先精确后剥离」 | 输入 **`历战钢龙` → 命中 `风暴的棺材`**（修复前会剥离「历战」误配 `钢龙`）；`阿爆`→爆鳞龙、`太太`→雌火龙、`霸主太太`→霸主雌火龙 |
| 非优先条目排序 | 普通水友(先) → 舰长(后) 保持 FIFO，舰长等级**不参与**非优先排序 |
| 「优先」提权 | 已在队用户发 `优先` → 提权至第 1 位，日志 `[Queue] 测试水友 优先置前 成功（优先=true）` |
| 重复点单去重 | 已在队用户再发 `点怪 霸主太太` → 不入队、不改怪（与原工程 `Enqueue` 命中即 return false 一致） |
| 历战难度联动 | 命中「风暴的棺材」后「历战难度」自动置为「历战 (紫色)」，落库 `tempered_level=1` |
| 队列落盘 | `order_list.json` 实时写入，字段完整（id/user_id/monster_name/is_priority/guard_level/tempered_level/timestamp/icon_url） |

**与原工程算法逐行核对结论**：V2 `QueueItem::compare_priority` 与原工程 `PriorityQueueManager.h:22-33`、`PriorityQueue.cs:250-261` 语义一致；设计意图见原工程 `docs/superpowers/specs/captain-checkin-ai-reply/spec.md`「舰长点怪按时间顺序排列，只有明确说『优先/插队』才会插队」。原工程插入方式为 `push_back + stable_sort`，V2 为 `push + sort_queue`，同构。

### 3. 悬浮窗（D3 / D4 / D6 / D8 / D2）

| 检查点 | 证据 |
|---|---|
| 跑马灯默认文本 | 「欢迎来到直播间！发送"点怪+怪物名"即可加入排队。」循环滚动，与设置项文案一致 |
| 跑马灯业务消息 | 点怪成功后单次插播「猎人B 点怪 霸主雌火龙 成功!」 |
| 气泡 | 补签查询、舰长打卡气泡置顶出现、多行渲染正常，15 s 退场 |
| 行内样式 | 优先=红底、历战=紫框、舰长/优先徽章、拖拽柄、图标 |
| 完成出队（D8 已确认保留按钮） | 点「完成」→ 该单移除、其余保序、`order_list.json` 同步 |
| 锁定 | 设置页「锁定窗口」→ 悬浮窗出现「已锁定」徽章 + 背景切穿透透明度、鼠标穿透 |
| 全局热键 | `Alt+,` 解锁生效，且主窗口按钮文案经 `overlay-lock-changed` 事件同步 |
| 透明度语义 | 95%→19% 仅背景变透，文字/徽章/图标仍清晰（D8「仅作用于背景」） |
| 位置记忆 | 写 `top_pos=(640,280)` 重启后悬浮窗精确落位 (640,280) |

### 4. 打卡 / 补签 / 点赞（C1~C5）

| 检查点 | 证据 |
|---|---|
| 首次打卡落库 | `user_profiles` 建行（`last_checkin_date=20260919`、`continuous_days=1`、`cumulative_days=1`）+ `checkin_records` 明细 |
| 重复打卡文案 | 气泡「测试水友今日已打卡，连续1天，累计1天」 |
| 关键词学习 | `keywords_json` 累积 jieba 分词（点怪/太太/优先/点餐/红烧肉/打卡/霸主/补签/查询）带 freq+ts；`danmu_history_json` 全量留痕 |
| 补签查询（无数据） | 「猎人B，系统错误，请稍后再试。」—— 三份数据全无时的原工程兜底 |
| 补签查询（有数据） | 三行完整文案：「补签卡1张 / 连续点赞7天：已1天，差6天 / 每周点赞30：已领取」 |
| 点赞奖卡 | 30 赞 → `retroactive_cards.card_count=1`、`total_earned=1`、`monthly_first_claimed=20260914`；`user_like_streaks.current_streak=1` |
| 打卡动态卡片 | 主窗口「打卡动态(最近10)」实时展示「连续 1 天 / 累计 1 天」 |
| 最近弹幕动态 | 主窗口实时展示最近弹幕（昵称 + 舰长等级 + 原文，新消息置顶） |
| 旧库兼容（A2） | 建表为 `user_profiles / checkin_records / retroactive_cards / user_daily_likes / user_like_streaks`，无 `weekly_likes`，列名沿用 `monthly_first_claimed`；`captain_profiles.db` 存在时优先沿用，否则新建 `checkin.db` |

### 5. 语音（B1~B7 / D1）

| 检查点 | 证据 |
|---|---|
| 引擎降级链 | 无 Manbo Key → 设置页徽章显示「**当前引擎: Windows SAPI**」；子进程探针捕获**由应用进程派生的 `powershell.exe` 存活约 3.5 s**，即 SAPI 实际发声 |
| 播放串行化 | 多条播报排队依次播出（同一时刻仅 1 个 powershell 子进程） |
| 普通弹幕朗读 | 「测试水友 说：大家好…」入普通队列 |
| 点餐指令 | 「点餐 红烧肉/可乐」入高优先队列，无气泡（与原工程一致） |
| 设置区完整度 | 引擎下拉(Manbo/Auto/…)、185 音色下拉、缓存天数、MiMo 角色/风格、语速/音量/音调三滑块、总开关、仅粉丝牌、仅付费礼物、打卡 AI 开关、打卡触发词、默认跑马灯文案 |

### 6. 配置与安全（A5 / A6 / A7 / E2）

| 检查点 | 证据 |
|---|---|
| 凭据防明文 | 保存后 `configs.json` 22 个字段中**不含任何** key/secret/token/password/appid/access 字段 |
| 注册表托管 | `HKCU\Software\MonsterOrderWilds` 下 `IdCode`、`ManboApiKey` 独立存储；设置页明示「仅写入注册表，不落 JSON / 不回显」 |
| 热更新 | 透明度保存后悬浮窗**免重启**即时生效（`config-changed` 事件链） |
| 字段级容错 | 全默认值启动（无 `configs.json`）正常，未整份重置 |

### 7. Lite 模式守卫（E1）

| 检查点 | 证据 |
|---|---|
| 前端冻结 | 导航「舰长打卡 & GM / AI 思考互动」置灰 + 「停用」标签；设置页卡片显示「Lite 模式下已停用」；GM 搜索框为真 `disabled`（实测无法输入） |
| 头部指示 | 页面标题旁琥珀徽章「Lite 纯排队模式已启用」；开关说明文案切换 |
| 后端拦截 | Lite 下发「打卡」→ `checkin_records` 仍为 1 条、`user_profiles.updated_at` 未变（学习链路同步停用） |
| 核心保留 | Lite 下「点怪 阿爆」→ 正常入队为第 4 位，日志 `[Queue] Lite水友 点怪 成功` |
| 状态可观测 | 日志 `[Config] Lite 模式已开启` / `Lite 模式已关闭` |

### 8. GM 运维（C6 / C7）

| 检查点 | 证据 |
|---|---|
| 模糊搜索 | 关键字「测试」→ 命中 2 行（`sim-测试水友`、`sim-like-测试水友`），列含 UID/昵称/连续天数/累计天数/补签卡数；「猎人B」正确不匹配 |
| 一键黑幕批量补签 | 执行后结果卡片：覆盖总用户 0 / 补签修复用户 0 / 插入明细记录 0 条 |
| 导出对话框 | 弹出**系统原生「另存为」**：默认名 `checkin_records_20260919_154539.csv`、保存类型 `CSV (*.csv)`；取消无残留文件 |

### 9. 可观测与生命周期（D5 / E3）

| 检查点 | 证据 |
|---|---|
| 文件日志 | `Logs/2026-09-19.txt`，UTF-8 BOM + `[时间]:[LEVEL]`；Release 下无 Debug 行 |
| 前端日志视图 | 标题「运行日志（内存环最近 500 条）」、文件路径提示、级别过滤「INFO 及以上」、刷新/清空视图按钮、8 条日志全量回显、WARNING 黄色高亮、无「资源缺失」徽章 |
| 退出流程 | 关闭主窗口 → 进程自然结束（无需 kill），日志 `[App] 退出程序：队列与配置已落盘`，`configs.json` 落盘 |
| 悬浮窗独立关闭 | 关闭悬浮窗不退出应用（正确设计） |

---

## 三、缺陷清单

### P1｜二次确认在打包版完全失效（建议先修）

- **现象**：点击「清空队列」「一键黑幕批量补签」「赠送 N 张补签卡」时**不弹任何确认框，操作直接执行**。实测点「清空队列」后队列立刻清空（`order_list.json` → `[]`），点「一键黑幕批量补签」直接出现结果卡片。
- **位置**：`src/views/MainWindow.tsx:338`、`:524`、`:536` 三处 `window.confirm()`
- **根因（依赖源码级核对）**：`wry 0.55.1` 仅在 Android 侧实现 JS 对话框（`src/android/kotlin/RustWebChromeClient.kt`），`tauri-runtime-wry 2.11.4` 未处理 WebView2 的 `DialogRequested` 事件；Chromium 在无 dialog delegate 时默认放行 → `confirm()` 直接返回 `true`。
- **影响**：C7 设计意图（防误删/防误发）失效；直播中误点「清空队列」将直接清空全部排队且无挽回。
- **建议**：改用项目已在后端使用的 `tauri-plugin-dialog`（导出功能正是走它，实测正常）——前端引入 `@tauri-apps/plugin-dialog` 的 `async confirm()`，或把确认动作下沉到 Rust 侧。
- **注**：本次未修改任何代码，仅记录。
- **修复状态（2026-09-19）**：✅ 已修复（F1，Rust 侧 `confirm_action` 命令 + 前端 `askConfirm`）并打包回归通过——确认框弹出、点「否」不执行（队列保留）、点「是」执行（`order_list.json` → `[]`），对话框打开期间应用健康。详见 `docs/UI_FUNCTIONAL_TEST_FIX_PLAN.md`。

### P2｜模拟测试通道 uid 前缀不一致

- 弹幕通道 `user_id = sim-{昵称}`，点赞通道 `uid = sim-like-{昵称}`（`MainWindow.tsx` `handleSimDanmu` / `handleSimLike`）。
- 同一昵称在两条链路落到**不同档案**，导致「点赞得卡 → 补签查询看到卡」这类联动无法自然验证（本次靠把昵称改成 `like-测试水友` 才打通正向文案）。
- 建议：两条通道统一为同一 `sim-{昵称}` 口径，或在 UI 上明示 uid 生成规则。
- **修复状态（2026-09-19）**：✅ 已修复（F2，点赞通道 uid 统一为 `sim-{昵称}`）并回归通过——点赞 30 次后 `retroactive_cards` / `user_daily_likes` / `user_like_streaks` 全部落在 `sim-回归甲`，与弹幕通道共用同一档案。

### P2｜模拟弹幕身份字段硬编码

- `handleSimDanmu` 固定 `has_medal: true / medal_level: 10 / guard_level: 3`。
- 导致「仅粉丝牌（含舰长）可点怪」「仅播报佩戴粉丝牌弹幕」「播报至少等级」等过滤在 UI 层不可验证，舰长分层排序也无法经该通道验证。
- 建议：模拟通道增加 粉丝牌/舰长等级 选择项。
- **修复状态（2026-09-19）**：✅ 已修复（F3，新增舰长等级下拉 + 粉丝牌佩戴/等级控件，默认沿用旧硬编码值）并回归通过——`回归甲` 无舰长/未佩戴时发「打卡」不落库；改为舰长+佩戴后 `sim-回归甲` 正常落库（连续 1 天 / 累计 1 天）。

### P3｜`simulate_gift` / `simulate_live_event` 无前端入口

- 两个调试命令已在 `lib.rs` 注册（`#[cfg]` 生产可用），但前端无任何按钮（全仓 grep 无引用）。
- 后果：**B8 礼物连击**（双池/冷却/≥3 首报/超时尾报）与 **B2 SC/上舰播报** 无法人工实测，目前仅单测覆盖。
- 建议：在「本地弹幕模拟测试通道」补 3 个事件模拟按钮（礼物 / SC / 上舰）。
- **修复状态（2026-09-19）**：✅ 已修复（F4，新增「直播间事件模拟」子区块：礼物 / SC / 上舰）并回归通过——礼物 ×3 出 3 条悬浮窗气泡 + SAPI 播报、SC 与上舰分别 SAPI 播报；Lite 下三按钮置灰且点击无事件（`LITE_COVERAGE_MATRIX.md` 已同步声明不支持 Lite）。

### P3｜手动点单路径偏离原工程（复核降级：不可达的防御逻辑）

- `queue.rs::add_or_update` 对已在队用户会**改怪物 / 提升舰长等级**；原工程 `PriorityQueueManager::Enqueue` 命中即 `return false`（不改怪、不更新等级），仅 `UpdateNodePriority` 可提权。
- **复核结论（2026-09-19 修复期）**：该 upsert 分支在两条实际链路上均**不可达**——「快速手动点怪」表单每次生成唯一 uid `manual-{时间戳}-{随机}`（`MainWindow.tsx` `handleAddOrder`），不存在重复 uid；弹幕路径在 `bilibili.rs:806-820` 入队前已拦截重复。且 `add_or_update` 全流程处于同一把队列锁内，无竞态窗口。
- 处置：不改代码，按「不可达的防御逻辑」记录（`docs/UI_FUNCTIONAL_TEST_FIX_PLAN.md` F5）；`add_or_update` 语义与原工程一致性维持现状。
- **修复状态（2026-09-19）**：✅ 已按「仅澄清」处置（F5），无代码改动。

### P3｜文档措辞与实现不符

- `docs/ARCHITECTURE_DESIGN.md:111` 称 Lite 下「控制台标题指示器变更为金黄色『Lite 纯排队模式』」。实际实现为**页面内琥珀徽章**，OS 窗口标题栏始终为「MonsterOrderWilds-Ascendance - 控制台」（前端无 `setTitle` 调用）。
- 建议：把该句改为「页面标题栏指示徽章」，或补 `setTitle` 实现以贴合原工程。
- **修复状态（2026-09-19）**：✅ 已修订（F6）——`ARCHITECTURE_DESIGN.md` 改为描述实际实现（页内琥珀徽章 + 导航停用标签），并注明原工程 Lite 下不修改窗口标题、V2 不调用 `setTitle`。

---

## 四、本次无法验证的项及原因

| 项 | 原因 | 现有保障 |
|---|---|---|
| B 站长连五态（D7） | 无 `credentials.dat` / 无有效开播身份码；空身份码时前端直接 toast 拦截（实测），不触达状态机 | 105 项单测覆盖状态迁移与鉴权失败收敛 |
| 打卡 AI 个性化回复（C1） | 未配置 DeepSeek Key | 单测覆盖「无 Key → 兜底文案入高优先队列」 |
| Manbo / MiMo 真实音质、185 音色可用性 | 无 API Key | SAPI 兜底链已实测发声；端点/参数映射经单测 |
| 播报听感与串行无叠音（B3） | 无音频回采手段 | 子进程探针间接确证串行；单测覆盖队列 |
| 500 条虚拟列表（D6） | UI 无法批量造数 | 单测实测 500 条仅渲染 29 行 DOM |

---

## 五、环境与仓库状态

- **代码/文档零改动**：测试结束 `git status` 干净，`HEAD = f32e310`，`v0.1.0` 指向未变。
- **真实用户数据未被触碰**：仓库根 `MonsterOrderWilds_configs/captain_profiles.db`（mtime 5/9）、`order_list.json`（mtime 15:08）均早于本次测试，未被写入。
- **测试写入位置**：全部落在 `src-tauri/target/release/MonsterOrderWilds_configs/`（`target/` 已被 gitignore）。其中残留：
  - `configs.json`：`top_pos=(640,280)`、`opacity=95`、`is_lite_mode=false`
  - `checkin.db`：本次测试档案（测试水友 / 猎人B / like-测试水友）
  - 如需复原，可整目录删除，程序下次启动会重新播种。
- **注册表既有测试残留**（非本次写入）：`IdCode=SECRET_ID_CODE`、`ManboApiKey=SECRET_MANBO`。

---

## 六、可复用的取证方法（供后续回归）

1. **UIA 不可用**：WebView2 内容在 Windows 无障碍树中只有嵌套「区域」，无文本/控件节点 → 只能靠截图 + 坐标点击。
2. **坐标偏差**：主窗口截图 1052×768 对应窗口 1015×737，工具按 ~0.96 缩放；实测落点比图像坐标**偏低约 12 px**，高度 <20 px 的小控件（如 Lite 开关）需上调 y 才点得中。
3. **输入免点击**：中文可用 `type_text`（SendInput unicode）直发；字段跳转用 `Tab`，表单提交在文本框内用 `Enter`（`<select>` 上 Enter 不提交，需点按钮）。
4. **TTS 是否真发声**：轮询 `Get-CimInstance Win32_Process -Filter "ParentProcessId=<app_pid> AND Name='powershell.exe'"`，命中即 SAPI 在播。
5. **退出链路**：用 `Alt+F4`（等价 WM_CLOSE）关闭主窗口；对非前台窗口的标题栏按钮用 UIA `Invoke` 不生效，坐标点击易误命中「最大化」。
6. **数据落点判定**：双击启动时 `config_dir()` 解析到 exe 同级目录，与仓库根数据目录天然隔离，可安全做破坏性试验。
7. **务必钉住启动工作目录**：用 `Start-Process` 启动 exe 时必须显式传 `-WorkingDirectory <exe 目录>`；否则 `config_dir()` 按调用方 cwd 解析，可能落到仓库根真实数据目录（修复期回归曾误触发一次，已清理，真实数据零改动）。
8. **坐标空间钉死**：自动化脚本先调用 `SetProcessDpiAwarenessContext(PerMonitorV2)`，让 `GetWindowRect` / `SetCursorPos` / 截图统一在物理像素空间；否则在 150% 缩放屏上落点会偏 1.5 倍。
9. **确认框打开期间勿对主窗口做跨进程 UIA 查询**：会让主线程卡在模态等待（表现为假死、对话框无法关闭）；对 `#32770` 对话框本身查询/Invoke 是安全的。真实用户交互不受影响（修复期回归实测：对话框打开期间应用健康，鼠标点击「否/是」均正常返回）。
10. **生产包必须用 `npm run tauri build`**：`cargo build --release` 单独执行会产出**指向 `devUrl`（localhost:1420）的开发态二进制**（体积 14.6 MB / 生产 18.5 MB），启动后 WebView 报 `ERR_CONNECTION_REFUSED`。
11. **无开播环境下的真实弹幕来源**：房间 `live_status=0` 时 B 站页面**仍可发送弹幕**（进入 `#chat-items` 列表），可据此驱动点怪 / 打卡全链路；判定是否真的送达应以程序侧落盘（`order_list.json`、打卡库）为准，页面输入框清空不等于服务端受理。

---

## 七、第三轮：真实直播间弹幕回归（2026-09-21）

前置缺陷修复（L1~L4）与逐条证据见 `docs/UI_FUNCTIONAL_TEST_FIX_PLAN.md` 第五节、第六节。本轮在**真实直播间**（房间 `1570807`，未开播；弹幕经 Chrome 真实页面发送，非模拟通道）完成：

| 用例 | 结果 | 关键证据 |
|---|---|---|
| 长连建立（身份码留空 → 读注册表） | ✅ | 状态稳定「已连接」；日志零 `未知操作码 8`（修复前 1~2 秒一次无限重连） |
| `点怪霸主太太` | ✅ | 别名模糊匹配为 `霸主雌火龙` 入队；`order_list.json` 落盘；跑马灯「鬼酒時雨 点怪 霸主雌火龙 成功！」 |
| `优先`（已在队中） | ✅ | 两段式提权：`is_priority=true` + 日志 `优先置前` + 队列「优先」徽章 |
| `点怪优先黑蚀龙`（清空队列后） | ✅ | `黑蚀龙` 入队且 `is_priority=true` |
| `打卡` | ✅ | `checkin_records` 新增 `checkin_date=20260921` 记录 |
| 重复点怪 | ✅ | 同账号在队时静默拦截（对齐原工程 `DanmuProcessor.cpp:116-125`） |

本轮结论修正了第二节「B 站长连五态无法验证」的前置条件：注册表内有效 `IdCode` 已具备，长连、弹幕、打卡全链路均已在真实环境跑通。新增缺陷 L5（日志误报优先状态）已随本轮修复，见修复方案文档 6.4。**遗留待验**：TTS 实际发声需人耳确认（注册表已配置 Manbo Key、播报入队无告警）；非舰长带优先词的降级行为需第二个账号才能覆盖，当前由单测保障。
