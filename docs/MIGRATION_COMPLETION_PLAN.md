# MonsterOrderWilds-Ascendance 迁移完整性修复计划

> 编制日期：2026-09-19
> 依据：对原工程 `D:\VisualStudioProjects\JonysandMHDanmuTools`（C++ `MonsterOrderWilds` + C# WPF，约 2.4 万行）与 V2（Rust + Tauri v2 + React，约 8 千行）的全量交叉审计
> 审计基线：`cargo test` 45 项全绿、`npm run build` 通过（注意：部分单测为"假通过"，见 A2）

---

## 一、背景与目标

原工程经 v44 个版本迭代，积累了完整的直播互动能力；V2 目前完成了**骨架级迁移**（保序排队、配置/凭据加载、Lite 模式、基础打卡/补签/GM），但存在 7 项已实测确认的 P0 缺陷与成片的功能缺失。

本计划目标：**在不破坏既有保序排队与 Lite 模式设计的前提下，按批次补齐能力、修复缺陷**，最终达到"V2 可完全替代原工程开播使用"。

---

## 二、现状基线（实测结论）

| 验证项 | 方法 | 结果 |
| --- | --- | --- |
| Rust 单测 | `cargo test` | 45 通过 0 失败（但 A2 项为假通过） |
| 前端构建 | `npm run build` | 通过 |
| 旧库兼容 | 对仓库内真实 `captain_profiles.db` 执行 V2 SQL | **失败**：`no such column: weekly_first_claimed` |
| 怪物匹配 | 按 `monster.rs` 逻辑模拟全部 44 条历战别名 | **失败**：41 条错怪、3 条匹配失败 |
| 资源打包 | 检查 `tauri.conf.json` + `build.rs` + `package.json` | **无任何 `resources` 配置**，Rust 侧按文件系统读取 |
| 事件消费 | 前端 `listen` 调用 vs Rust `emit` | 6 个事件零监听 |
| 配置文件容错 | 阅读 `config.rs:148` | 缺任一键 → 整份配置重置默认 |

---

## 三、批次总览

| 批次 | 主题 | 任务数 | 预估工作量 | 优先级 | 状态 |
| --- | --- | --- | --- | --- | --- |
| A | 核心链路缺陷修复 | 7 | 6~9 人日 | P0（阻断开播） | ✅ 已完成（2026-09-19） |
| B | TTS 语音域补齐 | 11 | 8~12 人日 | P1 | ✅ 已完成（2026-09-19） |
| C | 打卡 / 补签 / AI 域补齐 | 7 | 6~9 人日 | P1 | ✅ 已完成（2026-09-19） |
| D | UI 交互域补齐 | 8 | 6~10 人日 | P1~P2 | ✅ 已完成（2026-09-19） |
| E | 工程化与一致性 | 6 | 2~3 人日 | P2 | ✅ 已完成（2026-09-19） |

> **总体状态（2026-09-19）：批次 A~E 全部完成、D8 交互已确认、首次提交与标签 v0.1.0 已生成；剩余人工待办为安装产物端到端实测。**
>
> ⚠️ **后续进展（2026-09-19）：完成一轮原工程全量交叉审计（8 分域并行 + 独立复验），发现并修复 3 项 P0、12 项 P1
> 及一批 P2/P3 缺陷，同时更正了本文档中两条错误结论（A2、B7）。详细清单、复验证据与新增回归测试见
> [AUDIT_FIX_REPORT.md](AUDIT_FIX_REPORT.md)。当前基线：`cargo test` 117 项全绿、无编译警告，`npm run build` 通过。**

**建议里程碑**
- M1 = 批次 A（可开播的最低可用版本）
- M2 = A + B1~B5 + C3/C4（直播体验主体）
- M3 = 全量（B/C/D/E）

---

## 四、批次 A：核心链路缺陷修复（P0）

### A1 怪物别名匹配：改为"先精确、后剥离"

**问题**：`src-tauri/src/monster.rs:163-169` 在匹配前无条件剥离 `历战/历战王/AT` 前缀。而 `monster_list.json` 中 **44 条别称本身自带该前缀**，用于映射到"历战/历战王专属条目 + 专属图标"。

**实测结果**（按 V2 现有逻辑遍历全部 44 条别名）：

| 弹幕 | 原工程（精确匹配） | V2 现状 |
| --- | --- | --- |
| 历战王冰呪龙 | 雪花沉睡（图标 `MHWI_ArchTempered/MHWI-Arch_Tempered_Velkhana_Icon.png`） | 冰呪龙（普通图标 `MHWI/MHWI-Velkhana_Icon.png`） |
| 历战钢龙 | 风暴的棺材（`MHWorld_Tempered/...`） | 钢龙（错怪） |
| 历战王雷 / 历战王boy / 历战王冰 | 对应历战王条目 | **匹配失败，点怪丢失** |

**修复方案**
1. 匹配顺序改为：**① 原文精确匹配**（含前缀别名，命中即返回条目的 `默认历战等级`）→ **② 剥离前缀后重试**（仅当①未命中）→ ③ 返回 None。
2. 剥离优先级：`历战王/歷戰王` → `历战/歷戰`；`AT` 仅在作为独立前缀且非别名命中时剥离。
3. `monster.rs:230` 现有"历战大胖虎"用例属于②的场景（无专属条目），保留。

**涉及文件**：`src-tauri/src/monster.rs`

**验收标准**
- [x] 新增数据驱动单测：遍历 `monster_list.json` 中所有含 `历战/历战王` 的别称，断言匹配结果为**所属 key** 且 `tempered_level == 该条目默认历战等级`（已实施：`test_all_aliases_exact_match`，42 个唯一历战别称全覆盖）
- [x] 保留并通过：`历战大胖虎 → 嗟怨震天怨虎龙 (tempered=1)`、`历战王黑龙 → tempered=2`
- [x] `cargo test` 全绿

**Lite 模式**：属于核心点怪链路，Lite 下同样生效（无需条件编译）。

---

### A2 旧版 `captain_profiles.db` 兼容（含修正假通过单测）

> ⚠️ **本节的原始结论已于 2026-09-19 全量审计中更正，见 [AUDIT_FIX_REPORT.md](AUDIT_FIX_REPORT.md) 第四节「更正 1」。**
> 原结论「真实旧库列名为 `monthly_first_claimed`，V2 不得执行任何 ALTER」只对**仓库内那份 pre-v40 老库**成立：
> 原工程自 v40 起权威结构为 `weekly_first_claimed`，并对老库执行 `ALTER TABLE ... ADD COLUMN weekly_first_claimed`
> （`ProfileManager.cpp:178/216-237`）。原实现导致「由原工程 v40+ 新建库」的用户奖卡/补签/GM 发卡全链静默失效。
> 现已改为对齐原工程列名 + 同款迁移，并新增反向回归测试 `test_v40_plus_schema_weekly_card_chain`。

> **决策记录（2026-09-19）：采用「V2 完全适配旧库格式」方案 —— 不修改旧库结构（无 ALTER、无迁移、无备份文件），V2 建表与读写全部使用旧库格式（列名 `monthly_first_claimed`）。已实施并验证。**

**问题**：`checkin.rs:88-93` 优先打开原工程数据库，但 `init_schema` 与全部 SQL 使用 `weekly_first_claimed` 列，而仓库内真实旧库的 `retroactive_cards` 列名为 `monthly_first_claimed`。

**实测证据**（对真实库副本执行旧版 V2 的 SQL）：
```
SELECT weekly_first_claimed FROM retroactive_cards        → no such column: weekly_first_claimed
INSERT INTO retroactive_cards (..., weekly_first_claimed) → table ... has no column named weekly_first_claimed
```

**影响链**：`add_likes` 报错并被 `lib.rs:513` 静默吞掉 → 点赞统计与奖卡全部失效；`get_cards` 恒返回 0 张；`grant_card` GM 发卡失败。

**假通过单测**：`checkin.rs test_open_legacy_captain_profiles_db` 伪造的旧表结构**已含** `weekly_first_claimed`，与真实旧库不符。

**修复方案（已实施）**
1. `init_schema` 建表语句与原工程 `ProfileManager.cpp` 完全对齐：`retroactive_cards` 使用 `monthly_first_claimed` 列承载「本周首破领取日」标记；`user_profiles` 补全 `keywords_json / danmu_history_json`；`checkin_records.username` 可空；删除原工程没有的 `weekly_likes` 表；可空性与默认值逐一对齐。
2. 全部读写 SQL（`add_likes` 两处、`get_cards`、`grant_card`）改用 `monthly_first_claimed`；V2 不执行任何 ALTER。
3. 重写 `test_open_legacy_captain_profiles_db`：伪造表结构与真实旧库逐列一致，并断言 V2 读写后**表结构完全不变**。
4. 新增 `test_v2_schema_matches_legacy_format`：断言 V2 新建库的列名/表集合与旧库格式一致。
5. 新增 `test_real_repo_legacy_db_copy_compatible`：对真实旧库副本走全链路（打卡/点赞/发卡/补签），断言表结构零变更。

**涉及文件**：`src-tauri/src/checkin.rs`

**验收标准**
- [x] 对真实旧库副本执行 `add_likes / get_cards / grant_card / record_checkin / execute_retroactive_checkin` 全部成功
- [x] V2 打开并读写后旧库表结构零变更（无 ALTER、无新表，单测断言）
- [x] 假通过单测已修正为真实结构
- [x] `cargo test` 全绿（49 项）

**Lite 模式**：打卡属非排队模块，Lite 下沿用既有拦截策略，与本次改动无关。

**已知边界**：比当前旧库格式更早的库（如缺 `cumulative_days` 列）不再自动升级 —— 遵循「不修改老库」约束；如遇此类库需用户手工提供新格式库。

---

### A3 生产安装包资源打包与路径统一

**问题**：`tauri.conf.json` 未配置 `bundle.resources`，但 Rust 侧依赖文件系统：
- `monster.rs:126-149` 读 `MonsterOrderWilds_configs/monster_list.json` → 安装版不存在 → **核心点怪整体失效**
- `tts.rs:304-323` 读 `voices/*.mp3` → 特殊音效静默失败
- `config.rs:93-109 / queue.rs:259-288 / checkin.rs:88-114` 各自实现路径探测，规则互不一致（cwd 优先 vs exe 优先）

**修复方案**
1. 新增 `src-tauri/src/paths.rs` 统一资源定位，解析顺序固定为：
   **运行目录（cwd / 仓库根，绿色版与 dev）→ exe 同级 `MonsterOrderWilds_configs`（安装版随包资源）→ 兜底创建**；
   资源文件查找再叠加 Tauri `resource_dir()`（安装版随包资源）与开发目录回退
2. `tauri.conf.json` 增加 `bundle.resources`，随包分发：`monster_list.json`、`voices/`、`dict/`（C1 需要）、`local_voices.zip`（B10 需要）、空白配置模板。
3. 首次运行：若 exe 同级无 `MonsterOrderWilds_configs`，从 `resource_dir` 复制一份（保证"配置在本地目录、用户可改"的原工程语义）。
4. 资源缺失时不得静默：写日志 + 向前端 emit `resource-missing` 事件（配合 D5）。

**涉及文件**：`src-tauri/tauri.conf.json`、`src-tauri/src/paths.rs`（新增）、`monster.rs`、`tts.rs`、`queue.rs`、`config.rs`、`checkin.rs`、`credentials.rs`、`lib.rs`

**验收标准**
- [x] `cargo test` 全绿（58 项）；`paths.rs` 单测覆盖 `config_dir` 回退、`find_resource` 真实解析、`resource_root` 回退、目录递归复制
- [x] 绿色版（exe 同级放 `MonsterOrderWilds_configs`）优先使用本地目录（`config_dir()` 第一优先级）
- [x] 全部 6 处路径探测统一为 `paths::*`；启动缺失资源告警（日志 + `resource-missing` 事件）
- [ ] `npm run tauri build` 后在**干净目录**运行安装产物：可匹配怪物、可播特殊音效、可读写配置与数据库

> **状态（2026-09-19）：已实施（打包产物实测见 BATCH_A 文档 A3.5）。** 随包资源为 `monster_list.json` + `voices/` + `local_voices.zip`（后者由 B10 落地追加，安装脚本资源清单共 14 项）；`dict/` 待 C1 分词方案落地时追加。

**Lite 模式**：资源打包对所有模式生效。

---

### A4 OBS 浏览器源可用性 ✅ 已决策（方案 2，已实施）

> 2026-09-19 决策：选择方案 2「移除误导」。已删除 `ObsView.tsx` 与 `#/obs` 路由、移除失效的 `hide_window("obs")` 调用，README / `ARCHITECTURE_DESIGN.md` / `MainWindow` 文案改为【窗口捕获】说明。

**问题**：README 与 `MainWindow.tsx:279` 宣称 OBS 浏览器源使用 `http://localhost:1420/#/obs`，但生产构建无任何本地 HTTP 服务（`Cargo.toml` 无 web server 依赖，1420 仅存在于 `devUrl`）；且 `ObsView.tsx` 为**死代码**（`App.tsx:15-19` 把 `#/obs` 也渲染为 `OverlayWindow`，其 `hide_window("obs")` 指向不存在的窗口）。

**方案（需用户决策）**
- **方案 1（推荐）**：内置轻量 HTTP 服务（`axum` 或 `tiny_http`）监听本机端口，提供 `/#/obs` 静态页与 `/api/queue`（SSE 或轮询）→ 保留 OBS 卖点，约 1.5~2 人日。
- **方案 2**：移除误导，README/UI 改为"OBS 使用窗口捕获"，删除死代码 `ObsView.tsx`，约 0.5 人日。

**涉及文件**：`src/App.tsx`（路由修复，二选一都必须做）、`src/views/ObsView.tsx`、`src-tauri/src/lib.rs`、`src-tauri/Cargo.toml`、`README.md`

**验收标准**
- [x] `#/obs` 路由渲染正确组件，无死代码（已删除）
- [ ] 方案 1：生产构建后 OBS 浏览器源可加载并实时刷新队列（未采用）
- [x] 方案 2：文档与 UI 文案一致，无失效地址

---

### A5 敏感凭据不得明文落盘 / 返回前端

**问题**：`lib.rs:281-294` 先用 `credentials.dat` 真实值覆盖 `new_cfg`，随后 `config.rs:279` 将**完整 AppConfig（含 `app_id / access_key_id / access_key_secret / chat_api_key / mimo_api_key / manbo_api_key`）明文写入 configs.json**；`get_app_config` 还会把上述字段原样返回 WebView。与 `AGENTS.md`"禁止明文设置或泄露敏感凭据"及原工程 credentials.dat 托管设计冲突。

**修复方案**
1. 敏感字段增加 `#[serde(skip_serializing)]`（或引入独立 DTO 拆分"可落盘配置"与"运行时配置"）。
2. `get_app_config` 返回脱敏视图（保留 `has_*` 布尔与掩码，参考现有 `credentials.rs to_status`）。
3. `save_app_config` 一律忽略前端传入的敏感字段，权威来源仅为 credentials.dat + 注册表。
4. 前端 `types.ts` 同步调整类型；`MainWindow.tsx` 的凭据展示区改为只读状态。

**涉及文件**：`src-tauri/src/config.rs`、`src-tauri/src/lib.rs`、`src-tauri/src/credentials.rs`、`src/types.ts`、`src/views/MainWindow.tsx`

**验收标准**
- [x] 新增单测：`save()` 后 configs.json 文本中**不含** `access_key_secret` / `chat_api_key` 等明文（`test_config_serialization_excludes_secrets`）
- [x] `get_app_config` 返回体经单测断言不含明文密钥（`sanitized()` 单测覆盖；`get_app_config` 已改为返回脱敏副本）
- [x] 现有 `credentials.dat` 读写兼容性测试保持通过
- [x] `cargo test` 全绿（53 项）

**实现补充说明**
- 7 个敏感字段统一 `#[serde(skip_serializing)]`（仍可反序列化，历史配置可读）；
- `save_app_config` 逐字段保护：credentials.dat 有值以其为准 → 否则保留前端非空输入 → 再否则维持现有内存值（修复原"仅凭 `app_id` 非空即统一切换三字段"的粒度问题）；
- `AppConfig::save()` 注册表写入改为仅非空值写入，避免空值清空 IdCode / ManboApiKey；
- 落盘用例使用空 id_code / manbo_api_key，避免与 `registry` 测试并行写 HKCU 产生竞争。

**Lite 模式**：安全修复，全模式生效。

---

### A6 配置字段级容错

**问题**：`config.rs:7-49` 的 `AppConfig` 无 `#[serde(default)]`；`config.rs:148` 解析失败即 `unwrap_or_default()` → **缺一个键，用户全部设置静默重置**。

**修复方案**
1. `AppConfig` 增加容器级/字段级 `#[serde(default)]`（注意与 `PartialEq` 断言、`Default` 实现配套）。
2. 未知字段继续忽略；缺失字段取默认值，不整体重置。
3. 解析失败时写日志（配合 D5），并保留原文件不覆盖。

**涉及文件**：`src-tauri/src/config.rs`

**验收标准**
- [x] 单测：删除 configs.json 中任意一个键后加载，其余字段保持原值（`test_partial_config_uses_field_defaults`）
- [x] 单测：`{}` 空对象加载得到全默认值且不 panic（`test_empty_object_config_is_all_defaults`）
- [x] 解析失败保留原文件并生成 `.invalid` 诊断副本（`test_invalid_config_dumps_diagnostic_copy`）
- [x] `cargo test` 全绿（52 项）

> **状态（2026-09-19）：已实施并验证。**

---

### A7 配置热更新

**问题**：`lib.rs:99-102` 仅在启动时把 `only_medal_order / only_speek_wearing_medal / only_speek_guard_level` 注入 `DanmuProcessor`；`save_app_config`（`lib.rs:276-320`）不回写、不发事件；`OverlayWindow.tsx:25-37` 仅挂载时读一次配置。后果：过滤开关需重启；改跑马灯/透明度悬浮窗不生效。

**修复方案**
1. `save_app_config` 内同步更新 `state.danmu_processor` 的过滤字段（改为 `Mutex`/`ArcSwap` 保护，或提供 `update_filters()`）。
2. 广播 `config-changed` 事件；`OverlayWindow/ObsView` 监听后刷新透明度、跑马灯文本、排序样式。
3. 与 D2 联动：透明度/窗口位置在保存与窗口移动时即时应用（`set_opacity` / `set_position`）。

**涉及文件**：`src-tauri/src/lib.rs`、`src-tauri/src/bilibili.rs`、`src/views/OverlayWindow.tsx`、`src/views/ObsView.tsx`

**验收标准**
- [x] 修改"仅粉丝牌可点怪 / 仅播报佩戴粉丝牌 / 舰长等级"后，无需重启即生效
- [x] 修改透明度、跑马灯文本后悬浮窗即时更新（`config-changed` 事件 + OverlayWindow 订阅）
- [x] 单测：`update_filters` 后 `process_danmu` 行为随之改变（`test_update_filters_takes_effect_immediately`）

> **状态（2026-09-19）：已实施并验证，`cargo test` 54 项全绿。**

**Lite 模式**：过滤开关属点怪链路，Lite 下同样生效。

---

## 五、批次 B：TTS 语音域补齐（P1）✅ 已实施（2026-09-19）

> 对照实现：`TextToSpeech.cpp`（1433 行）、`TTSCacheManager.cpp`、`AudioPlayer.cpp`、`LocalVoiceManager.cpp`、`ManboTTSProvider.cpp`、`SpecialManboTTSProvider.cpp`、`XiaomiTTSProvider.cpp`
> **实施结论：B1~B11 全部落地；`cargo test` 78 项全绿（批次前 67 项）、无编译警告；`npm run build` 通过。**

### B1 普通弹幕朗读 ✅
- **原实现**：`TextToSpeech.cpp:180-267`（`HandleSpeekDm`），过滤链"仅粉丝牌 → 仅舰长等级 → enableVoice"后入 `NormalMsgQueue` 朗读 `"{uname} 说：{msg}"`；打卡/补签/查询指令跳过；本地语音命中则直接播放。
- **实施**：`lib.rs handle_incoming_danmu` 第 4 节重写为完整过滤链 → 点餐判定 → 本地音效命中 → 普通朗读入 `TTSManager.normal_queue`；由后台播报泵（150ms）逐条取出执行（对齐原工程 Tick 每周期出队一条的限速语义）。
- **核对结论（重要）**：原工程 `DanmuProcessor.shouldSpeak/speakText`（"XX点怪YY"）**在全工程无任何消费方（死字段）**；点怪弹幕的真实播报就是普通朗读原文。V2 已按原工程行为对齐（点怪弹幕朗读 `"{uname} 说：点怪XX"`，不再播报"XX点怪YY"）。
- **验收**：`test_normal_danmu_read_aloud_queue`（普通/点怪弹幕入队格式）、`test_read_aloud_respects_speak_filters`（粉丝牌过滤 + Lite 拦截）、`test_local_sound_danmu_bypasses_read_aloud`。
- **Lite**：不支持（Lite 停用 TTS）。

### B2 SC / 上舰 / 进场事件 ✅
- **原实现**：`BliveManager.cpp:561-570` 分发 `LIVE_OPEN_PLATFORM_SUPER_CHAT / GUARD / LIVE_ROOM_ENTER`；文案 `TextToSpeech.cpp:377-453`。
- **实施**：`bilibili.rs` 新增 `LiveEvent` 枚举与 `parse_super_chat / parse_guard / parse_room_enter`（open_id 优先、回退 uid）；cmd 分支补齐三类事件；`lib.rs handle_incoming_live_event` 播报并 emit 前端事件（`super-chat-received` / `guard-received` / `room-enter-received`）。
- **文案对齐**：SC `"感谢 {uname} 赠送的{rmb}元SC：{message}"`；上舰 `"感谢 {uname} 上船{num}{unit}的{总督|提督|舰长}"`；进场**不播报**（仅前端提示，与原工程一致）。
- **验收**：`test_parse_live_events_and_tts_text`、`test_live_event_pipeline_and_lite_interception`。
- **新增调试通道**：`simulate_live_event` 命令（与真实长连同一管道，便于无直播间时手工验证）。
- **Lite**：不支持。

### B3 音频播放串行化（防叠音）✅
- **原实现**：`AudioPlayer` 单实例串行播放；`MAX_CONCURRENT_TTS=2`（API 并发请求）、播放串行。
- **实施**：`tts.rs` 新增 `AudioQueue`（单播放线程 + mpsc），全部播放入口（`play_audio_bytes / play_special_sound / SAPI`）统一入队；rodio 播放带 60s 超时保护（`PLAYBACK_TIMEOUT`），SAPI 带 30s 超时强杀（`SAPI_PLAYBACK_TIMEOUT`）。
- **验收**：`test_audio_queue_serializes_jobs`（注入 mock 播放器，断言并发峰值恒为 1、3 条任务全部串行完成）；`test_speak_queue_priority_and_fifo`（播报队列优先/FIFO/容量上限）。
- **Lite**：不支持。

### B4 SAPI 参数与音色 ✅
- **原实现**：`SetupSapiVoiceParams`（`SpFindBestToken(Language=804)` 中文音色、`SetRate(speechRate)`、`SetVolume(speechVolume/2)`）+ `BuildSapiSsml`（`<prosody pitch="±Nst">`）。
- **实施**：`build_sapi_ssml`（pitch semitone + XML 转义）与 `build_sapi_command`（PowerShell `SpeechSynthesizer`：自动选择 `zh*` 文化音色、`$s.Rate` 直传并钳制 ±10、`$s.Volume = speechVolume/2`、`SpeakSsml`）。
- **有意差异**：原工程使用 COM `ISpVoice`（含 pVoice 重建逻辑）；V2 走 PowerShell `System.Speech`（避免引入 COM 绑定），参数与音色语义一致，重建需求由子进程每次新建天然规避。
- **验收**：`test_sapi_ssml_and_command`（SSML 转义/符号、rate 钳制、volume 减半、中文音色与 SpeakSsml 调用）。
- **Lite**：不支持。

### B5 Manbo 默认音色与端点映射 ✅
- **实施**：默认音色改回 `"曼波"`（`TTSConfig::default` 与 `AppConfig::default` 同步）；MiMo 默认值对齐原工程（`mimo_default` / 空 style）；`build_manbo_url` 端点选择与参数对齐原工程（`曼波` → `/apis/mbAIscvip`，`speed = speech_rate × 5`，`key` 参数 + `Authorization: Bearer`；其它音色 → `/apis/AIvoice`）。
- **音色列表**：新增 `src-tauri/src/manbo_voices.rs`（原工程 `ToolsMain.ManboVoiceList` 全量 185 项，已校验无重复），新增 `get_manbo_voice_list` 命令供前端下拉（UI 落地见 D1）。
- **验收**：`test_manbo_url_building`、`test_manbo_voice_list_data`、`test_tts_circuit_breaker_and_cooldown_recovery`。
- **Lite**：不支持。

### B6 特殊用户专属 TTS 引擎 ✅
- **原实现**：`SpecialManboTTSProvider`（`/apis/mbAIsc`，无 key/无 Authorization）+ 3 次失败 / 30s 熔断 + 失败后回落通用链。
- **实施**：`speak_text_for_user(text, user_id)` 命中 `SPECIAL_OPEN_ID` 且未熔断时优先专属引擎；失败累计 3 次触发 30s 冷却（`special_engine_available / special_mark_failure / special_mark_success`），冷却期内回落通用链（Manbo → MiMo → SAPI），成功即清零并解除冷却。
- **验收**：`test_special_engine_circuit_breaker`、`build_special_manbo_url` 断言（无 key 参数）。
- **Lite**：不支持。

### B7 TTS 音频留档与清理 ✅
> ⚠️ **本条的「原实现修正」结论已于 2026-09-19 全量审计中更正，见 [AUDIT_FIX_REPORT.md](AUDIT_FIX_REPORT.md) 第四节「更正 2」。**
> 原工程 `TTSCacheManager` 的 `SaveCachedAudio` / `SaveCachedAudioWithPrefix` 为**死代码（零调用方）**，
> 唯一留档入口是受 `isCheckinTTS` 守卫的 `SaveCheckinAudio`（文件名 `打卡_{username}_{tick}.mp3`），
> 且规格 FR-1 明确「一般弹幕 TTS 不缓存、播完即丢」—— 全量留档正是原工程 v24 已修复的缺陷形态。
> 现已改为仅签到/补签播报留档。
- **原实现修正**：`TTSCacheManager` 实为**音频留档 + 启动清理**（`{exe}/TempAudio/YYYYMMDD/{前缀}_{tick}.mp3`，按目录创建时间清理超过 `ttsCacheDaysToKeep` 天的目录），**并非"命中复用缓存"**（原审计描述有误，此处按原工程权威实现落地）。
- **实施**：`save_cached_audio`（前缀规则对齐 `GetContentPrefix`：含" 说："时取 `用户名_正文前5字`，否则全文前 5 字；非法文件名字符替换为 `_`）、`cleanup_old_cache`（启动时按 `tts_cache_days_to_keep` 清理）；留档目录统一为 `{数据目录}/TempAudio`（跟随 paths.rs 数据目录，见有意差异）。
- **验收**：`test_content_prefix_and_cache_cleanup`。
- **有意差异**：原工程缓存在 exe 同级；V2 放在数据目录（`MonsterOrderWilds_configs/TempAudio`），保证绿色版/安装版统一且可写。
- **Lite**：不支持。

### B8 礼物连击完整语义 ✅
- **原实现**：`TextToSpeech.cpp:270-380`（`HandleSpeekSendGift`）—— 键为 `open_id + gift_id`；5s 冷却（`GIFT_COOLDOWN_SECONDS`）内仅累加；`paid + combo_info` 走官方准备池（`gift_num = combo_base_num × combo_count`，按 `combo_timeout` 秒结算）；普通礼物走动态池（10s 窗口，累加 ≥3 首次播报"感谢 X 开始赠送Y"，超时尾报合并数量）。
- **实施**：`GiftComboTracker` 重写为双池结构（`dynamic` / `prepare`）+ 冷却表（60s 清理）；时间基准改为绝对 `Instant` 截止时间（等价原工程 `deltaTime` 递减，且与外部 tick 频率解耦）；`bilibili.rs parse_gift_event` 解析 `paid / gift_id / combo_info`；lib.rs 播报泵每 tick 结算超时连击并入优先队列（播放由 B3 队列保证串行，不再阻塞后续 flush）。
- **验收**：`test_gift_combo_merging`（首报 + 冷却 + 尾报合并）、`test_gift_combo_first_report_and_cooldown`（≥3"开始赠送"）、`test_gift_official_combo_and_paid_filter`（combo 合并数量与付费过滤）、`test_parse_gift_event_with_combo_and_paid`。
- **Lite**：不支持。

### B9 "点餐"指令 ✅
- **原实现**：`TextToSpeech.cpp:1424-1433`（`HandleDmOrderFood`），文案 `"{uname} 下单的 {xxx} 已接单，预计{n}分钟后送达！"`（n ∈ [0,60] 随机）。
- **实施**：`build_food_order_text`（`strip_prefix("点餐")` 且后续非空方触发，`rand 0..=60`），经优先队列播报。
- **验收**：`test_food_order_text_generation`、`test_food_order_danmu_enters_priority_queue`。
- **Lite**：不支持。

### B10 本地语音包 zip ✅
- **原实现**：`LocalVoiceManager.cpp:56-95`（`local_voices.zip` 内存解压 + 字典精确匹配，zip 内路径含 `manbo/`、`mho/` 前缀）。
- **实施**：新增 `zip` 依赖；`load_local_voice`（zip 内查找 → 散装 `voices/{manbo,mho}/` 回退）；关键字映射改回 zip 内路径（如 `曼波 → manbo/manbo.mp3`）；`local_voices.zip` 加入 `tauri.conf.json bundle.resources` 与 `paths::ensure_seeded()` 播种清单。
- **验收**：`test_local_voice_zip_loading`（zip 读取 `manbo/manbo.mp3`、自动补前缀、缺失返回 None）。
- **Lite**：不支持。

### B11 `only_speek_paid_gift` 生效 ✅
- **原实现核查**：该开关有两个作用点 —— ① 礼物官方连击结算（`TextToSpeech.cpp:137`：`(!onlySpeekPaidGift || paid)`）；② `ShouldSpeak` 的 DM 路径（`DanmuProcessor.cpp:317`），但其依赖的 `isPaidGift` 字段**在原工程从未被赋值（恒 false）**，开启会静音全部 DM 播报，属死逻辑。
- **实施**：V2 将该开关接入**礼物播报过滤**（连击结算处生效，`flush_gift_combos(only_paid)`）；`DanmuData.is_paid_gift` 字段已补齐（DM 通道恒 false），**不复刻**原工程 DM 侧死逻辑（已在代码注释与本文档标注为有意差异）。
- **验收**：`test_gift_pipeline_only_paid_filter`（免费连击静默、付费连击正常结算）、`test_parse_gift_event_with_combo_and_paid`。
- **Lite**：不支持。

**批次 B 涉及文件**：`src-tauri/src/tts.rs`（重写）、`src-tauri/src/manbo_voices.rs`（新增）、`src-tauri/src/bilibili.rs`、`src-tauri/src/lib.rs`、`src-tauri/src/config.rs`、`src-tauri/src/paths.rs`、`src-tauri/Cargo.toml`（zip 依赖）、`src-tauri/tauri.conf.json`（资源）、`src/views/MainWindow.tsx`（模拟弹幕 payload 对齐）。

**手工验证入口**（无真实直播间时）：`simulate_danmu`（普通朗读/点餐/本地音效/打卡）、`simulate_gift`（连击/付费过滤）、`simulate_live_event`（SC/上舰/进场），三者均走与真实长连完全一致的业务管道并受 Lite 拦截。

---

## 六、批次 C：打卡 / 补签 / AI 域补齐（P1）✅ 已实施（2026-09-19）

> 对照实现：`CaptainCheckInModule.cpp`（819 行）、`RetroactiveCheckInModule.cpp`、`ProfileManager.cpp`（1862 行）、`DataBridgeExports.cpp`、`DeepSeekAIChatProvider.cpp`
> **决策记录（2026-09-19）**：C1 采用 `jieba-rs`（对齐原工程 cppjieba 同源词典 + HMM 新词识别）；C5 保留即时落库（原工程为「AI/TTS 成功后落库」，差异标注为有意差异）。
> **实施结论：C1~C7 全部落地；`cargo test` 94 项全绿（批次前 78 项）、无编译警告；`npm run build` 通过。**

### C1 打卡 AI 个性化回复（含关键词学习）✅
- **原实现**：`DanmuProcessor.cpp:67-89` 以 `guardLevel != 0 || hasMedal` 推送船长事件 → `CaptainCheckInModule::PushDanmuEvent` 学习段（`ShouldLearn` 仅舰长 + 5s 节流 → `ExtractKeywords` jieba 分词/停用词/#标签# 排除/词频上限 50/按频次降序）+ `ShouldSkipDuplicateContent` 同内容防刷屏 → `BuildPrompt` → DeepSeek `CallAPI`（仅 user 消息、无 system prompt）→ 失败回退 `GetFallbackAnswer`。
- **实施**：新增 `src-tauri/src/checkin_ai.rs`（`CheckinLearner`：内嵌 jieba 主词典 + `dict/user.dict.utf8` 自定义词 + `dict/stop_words.utf8` 停用词；`learn / should_skip_duplicate / extract_keywords / build_prompt / fallback_answer`）；`checkin.rs` 新增 `KeywordRecord / LearningProfile` 与 `load_learning / save_learning`（`keywords_json` 为 `[{"word","freq","ts"}]`、`danmu_history_json` 为 `[[ts,"内容"]]`，与原工程 `ProfileManager::KeywordsToJson / DanmuHistoryToJson` 逐字对齐，旧库历史学习数据可直接反序列化）；`lib.rs` 接入学习与打卡回复链路。
- **素材随包**：`MonsterOrderWilds_configs/dict/{stop_words.utf8,user.dict.utf8}` 随安装包分发（`paths::ensure_seeded` 播种到可写数据目录，A3 资源清单 14 → 16 项）。
- **词典容错**：用户词典按「词语 [词频] [词性]」解析，词频缺省/非法回退 10（对齐原工程《弹幕习惯词黑白名单配置.txt》约定）、词频钳制最小 1（0 在动态规划分词中等价禁用）、兼容 CRLF 与 BOM、逐词 `add_word` 不受单行格式错误中断。
- **AI 调用**：复用 `DeepSeekAIChatProvider`（思考模式 `deepseek-v4-flash`），仅舰长且已配置 API Key 时发起；异步 `tauri::async_runtime::spawn` 生成回复，失败即回退兜底文案；未启用语音时不影响气泡事件。
- **开关语义核查**：原工程 `enableCaptainCheckinAI` 实际控制 `CaptainCheckInModule::SetEnabled`（**打卡模块总开关**：关闭后打卡指令与弹幕学习全部停用，补签模块不受影响）；V2 按此语义接入，不再是孤儿字段。
- **有意差异**：① 原 `ShouldLearn` 用「本地毫秒 − 服务器秒」比较导致 5s 节流实际永不生效，V2 统一为弹幕时间戳秒级比较，实现该常量的既定语义；② 原 `BuildPrompt` 在跨月且非 1 日时不显示间隔天数（缺陷），V2 用日期差精确计算（覆盖跨月/跨年）；③ 原 `IsAvailable()` 首次恒为 false 导致 AI 实际从不被调用（隐患），V2 改为「已配置 Key 即尝试，失败回退」；④ 新增 `checkin-reply` 事件（回复文本 + `is_ai`）供气泡展示（D4 消费）。
- **验收**：`test_learn_gate_window_and_history_cap`、`test_stopword_min_length_and_hashtag_exclusion`、`test_same_content_skip`、`test_build_prompt_and_fallback`、`test_learning_profile_json_format_matches_legacy`、`test_danmu_learning_pipeline_and_duplicate_skip`。
- **Lite**：不支持。

### C2 补签查询回复 ✅
- **原实现**：`RetroactiveCheckInModule.cpp:350-390`（`HandleQueryCommand`）：`{user}，补签卡N张` + `\n连续点赞7天：已满足，下次领取 | 已X天，差Y天` + `\n每周点赞30：已领取 | A/30，差B | 已满足，可领取`；三份数据（卡档案/连续点赞/当日点赞）全无时返回「系统错误，请稍后再试。」；`SendReply(..., false)` 仅气泡不朗读。
- **实施**：`checkin.rs` 新增 `query_reply`、`get_like_streak`、`get_daily_like_total`、`load_cards`；`lib.rs` 查询分支广播 `retroactive-query`（含 `reply` 全文与 `card_count`），不朗读。
- **验收**：`test_retro_permission_and_query_words`（查询不朗读）+ `query_reply` 文案随 `retro_command_outcome` 覆盖。
- **Lite**：不支持。

### C3 点赞奖卡播报 + 点赞去重 ✅
- **原实现**：`DanmuProcessor::NotifyLikeEvent` 用与弹幕共用的 10 万条 LRU（`IsDuplicateMsgId`）去重；`RetroactiveCheckInModule::ProcessLike` 落库后按「今日突破30 → 连续7天」顺序 `SendReply`（默认朗读）。
- **实施**：`bilibili.rs` 新增 `LikeEvent`（uid/uname/msg_id/like_count/timestamp）与 `parse_like_event`（open_id 优先、uid 回退、`like_count` 截断 10000），长连回调改为 `Box` 语义的 `Fn(LikeEvent)`；`DanmuProcessor` 新增 `is_duplicate_msg_id` 复用同一缓存；`checkin.rs::add_likes` 返回 `LikeRewards{streak_reward, weekly_reward, daily_total}`；`lib.rs::handle_incoming_like` 组装两条播报文案（`{name}，恭喜！今日点赞突破30，获得1张补签卡！` / `{name}，恭喜！连续7天点赞，获得1张补签卡！`）→ 广播 `like-reward-granted` + 高优先播报；新增 `simulate_like` 调试命令与前端模拟入口。
- **验收**：`test_parse_like_event`、`test_like_pipeline_dedup_and_rewards`（同 msg_id 只计一次、同周不重复发卡、第 7 天连赞发卡、Lite 拦截）。
- **Lite**：不支持。

### C4 补签词表与权限对齐 ✅
- **词表**（原 `RetroactiveCheckInModule::Init` 的 `SetTriggerWords` 原文）：操作词 `补签,补签卡`；查询词 `补签查询,补签卡查询,查询补签,查询补签卡,我的补签卡`。V2 以常量 `RETRO_TRIGGER_WORDS` + `parse_retro_trigger_words`（支持 `;` 分组与「不含分号时按词内含『查询』归类」的旧格式兼容）实现，替代原先自拟的 `补签/补卡`、`补签查询/查补签`。
- **权限**：补签与查询统一为「舰长或佩戴粉丝牌」（原 `NotifyCaptainDanmu` 门槛）；无权用户的消息按普通弹幕继续处理（不被指令吞噬）。
- **附带对齐**：补签指令回复文案改为原工程全文 —— 无卡档案「系统错误，请稍后再试。」、卡数为 0「你没有补签卡哦~」、满勤「当前连续打卡X天、累计Y天，无需补签哦~」、无缺失日期「当前没有需要补签的日期。」、成功「已成功补签X月X日，剩余补签卡N张，连续打卡恢复为M天！」、失败「补签失败，请稍后再试。」（`retro_command_outcome` 统一实现）。
- **验收**：`test_retro_permission_and_query_words`（粉丝牌用户可补签、普通用户不响应）、`test_simulate_danmu_retroactive_flow`（成功文案）。
- **Lite**：不支持。

### C5 打卡落库语义修正 ✅
- **移除覆写**：`record_checkin` 不再写 `last_danmu_timestamp`（该列改由学习链路独占写入），INSERT 列集同步收窄。
- **回读真实值**：返回值改为落库后回读 `user_profiles` 行（`created_at / last_danmu_timestamp` 等以库内既有值为准）。
- **重复打卡文案**：「{name}今日已打卡，连续X天，累计Y天」（对齐原工程 `repeatedAnswer`），不再重复播报首次文案。
- **打卡日期口径**：改用弹幕服务器时间（原工程 `sendDate` = `localtime(serverTimestamp)`，`bilibili::server_date`），时间戳缺失时回退本机今天；点赞日期同口径。
- **落库时机**：**保留即时落库**（用户决策）——AI/TTS 失败不丢打卡记录；与原工程「AI/TTS 成功后落库」的差异在本文档标注为有意差异。
- **验收**：`test_simulate_danmu_checkin_flow`（`last_danmu_timestamp == 0` 断言 + 重复打卡文案）。
- **Lite**：不支持。

### C6 GM 导出增强 ✅
- **原实现**：`DataBridgeExports.cpp:590-690` —— 昵称部分匹配取 UID（无命中报 `User not found`）、日期范围过滤、日期降序、UTF-8 BOM、CSV 表头 `uid,username,checkin_date,created_at`、JSON 为 `{uid, username, checkinDate, createdAt}` 数组、格式非法报 `Unsupported format. Use 'csv' or 'json'`；文件路径由系统保存对话框选择。
- **实施**：`checkin.rs::export_records_content`（格式/昵称/日期范围 + BOM + 原格式逐字对齐，LIKE 通配符转义）；`lib.rs::gm_export_checkin_records` 经 `tauri-plugin-dialog` 弹出**系统保存对话框**并写入文件，返回保存路径；前端「打卡数据导出」卡片补齐格式下拉、昵称筛选、开始/结束日期与路径回显。
- **替代说明**：移除前端 Blob 下载方案（不再受浏览器下载目录限制），BOM 由后端写入，效果与原工程一致。
- **验收**：`test_simulate_danmu_retroactive_flow`（导出内容包含补签日期）等；导出内容构造逻辑由 `export_records_content` 单测路径覆盖。
- **Lite**：不支持。

### C7 GM 对话框增强 ✅
- **搜索结果补 `card_count`**：`search_users` 返回 `UserSearchItem`（`#[serde(flatten)]` 档案 + `COALESCE(card_count,0)` LEFT JOIN），前端新增「补签卡」列。
- **发卡二次确认**：前端 `confirm("确定给「X」发放 N 张补签卡吗？")`，与 `GMRetroactiveCardDialog` 的确认语义一致。
- **文案修正**：按钮由固定「赠送 1 张补签卡」改为「赠送 {grantCardAmount} 张补签卡」，toast 同步显示实际数量与被发卡人昵称。
- **参数校验**：`grant_card` 拒绝 `count <= 0`；`search_users` 转义 `LIKE` 通配符（`%`/`_`/`\`，`ESCAPE '\'`），避免关键字造成全表匹配。
- **验收**：`test_stopword_min_length_and_hashtag_exclusion` 同批次的 `checkin.rs` 单测覆盖 `grant_card` 校验与转义路径（`test_search_users_card_count_and_like_escaping`）。
- **Lite**：不支持。

**批次 C 涉及文件**：`src-tauri/src/checkin_ai.rs`（新增）、`src-tauri/src/checkin.rs`、`src-tauri/src/lib.rs`、`src-tauri/src/bilibili.rs`、`src-tauri/src/ai.rs`、`src-tauri/src/paths.rs`、`src-tauri/Cargo.toml`（`jieba-rs`、`tauri-plugin-dialog`；tauri 显式特性清单排除 `common-controls-v6`）、`src-tauri/tauri.conf.json`（dict 资源）、`MonsterOrderWilds_configs/dict/{stop_words.utf8,user.dict.utf8}`（新增随包资源）、`src/types.ts`、`src/views/MainWindow.tsx`（GM 导出/发卡/点赞模拟 UI）。

**批次 C 验证与手工测试入口**
- `cargo test`：94 项全绿（批次前 78 项），无编译警告。
- `npm run build`：通过。
- `npm run tauri build`：成功产出 MSI（11.8 MB）+ NSIS（10.2 MB）；安装脚本资源清单 16 项（含 `MonsterOrderWilds_configs/dict/*` 两项）。
- 手工验证：① 主播/舰长发送「打卡」→ 首次回复兜底或 AI 文案（配置 DeepSeek Key 后为 AI 生成），同日再发 → 「今日已打卡」文案；② 发送「我的补签卡」→ 气泡完整文案；③ 前端 B站 TAB「发射模拟点赞」30 次 → 奖卡播报与气泡；④ GM TAB 导出 CSV/JSON → 系统保存对话框并落盘（Excel 打开中文不乱码）。
- 已知平台事项：`tauri` 显式关闭 `common-controls-v6` —— 该特性会让 `muda` 链接 comctl32 v6 的 `TaskDialogIndirect`，而 `cargo test` 生成的测试二进制没有 Common-Controls 清单，会在加载期以 `STATUS_ENTRYPOINT_NOT_FOUND` 失败（该代码仅服务于 About/消息对话框，本工程未使用）。

---

## 七、批次 D：UI 交互域补齐（P1~P2）✅ 已完成（2026-09-19）

> 对照实现：`ConfigWindow.xaml(.cs)`、`OrderedMonsterWindow.xaml(.cs)`、`AIBubbleControl.xaml`、`DanmuManager.cs:48-63`、`BliveManager.h/.cpp`（连接状态机）、`WriteLog.cpp`（日志）

### D1 语音设置区补全 ✅
- **引擎「自动」**：`TTSEngineType::Auto`（`tts.rs`）+ `select_active_engine` 级联 Manbo → MiMo → SAPI，对齐原工程 `TTSProviderFactory::Create` 的 AUTO 模式；`parse_tts_engine` 统一字符串映射（未知值按原工程默认 Manbo）。
- **当前实际引擎实时显示**：`TTSManager.active_engine` 在每次播报成功路径记录；命令 `get_current_tts_engine` 返回 `manbo/xiaomi/sapi`（对齐 `TTSManager_GetCurrentProviderName` 命名），前端每 2.5s 刷新并映射为 `Manbo / 小米MiMo / Windows SAPI / 未知`。
- **Manbo API Key**：密码框 + `save_manbo_api_key` 命令（**仅写注册表，不落 JSON、不回传明文**；空值不覆盖）；占位文案随凭据状态切换。
- **184 音色下拉**：`get_manbo_voice_list`（185 项）驱动，保留历史配置中不在列表内的音色。
- **语速/音量/音高**：语音音量范围由 `0~100` 修正为 **`0~200`**（对齐原工程 `VoiceVolumeSlider`，SAPI 侧 `Volume 减半` 后仍在 0~100）；音高、语速 ±10。
- **MiMo 角色/风格**：`mimo_voice`（默认/中文/英文）、`mimo_style`（默认 + 5 种）下拉，与原工程 `MimoVoiceComboBox / MimoStyleComboBox` 选项逐项一致。
- **缓存天数 / 过滤开关 / 总开关**：`tts_cache_days_to_keep`（1~365 钳制）、`enable_voice` 总开关、`only_speek_wearing_medal`、`only_speek_paid_gift`、`only_speek_guard_level`（所有人/舰长/提督/总督）、舰长打卡 AI 开关与触发词。
- **验收**：`test_auto_engine_cascade_and_current_engine_name`（Auto 级联 + 引擎名映射）、`test_parse_tts_engine_mapping`。

### D2 点怪设置与窗口控制 ✅
- **仅粉丝牌可点怪入口**：设置面板复选框（→ `only_medal_order`，运行期由 `DanmuProcessor.update_filters` 热生效）。
- **穿透模式透明度**：`penetrating_mode_opacity` 滑杆（0~100）；悬浮窗锁定时背景 alpha 取该值（原 `RefreshWindow` 语义）。
- **窗口锁定/穿透**：命令 `set_overlay_locked` → `set_ignore_cursor_events(true)` + `set_always_on_top(true)`，广播 `overlay-lock-changed`；锁定态为**运行时状态不持久化**（与原工程 `mIsLocked` 一致）。
- **全局热键 `Alt+,`**：`tauri-plugin-global-shortcut`（仅生产构建注册；测试构建不注册，避免抢占系统热键），行为与命令共用 `apply_overlay_lock`。
- **窗口位置记忆**：前端 `onMoved` → `save_overlay_position`（除以 `scaleFactor` 换算逻辑坐标）→ `AppState.pending_pos`，后台任务每 3s 防抖写回 `top_pos_x/y`；启动时 `setup()` 恢复记忆位置（实测旧 `MainConfig.cfg` 的 `TopPos(913,105)` 已生效）。
- **验收**：`test_overlay_lock_and_position_runtime_state`；实测热键注册日志 + 窗口落位 913,105。

### D3 动态跑马灯 ✅
- **后端**：`added_to_queue || priority_updated` 时 `emit("order-placed", {user_id, user_name, monster_name, is_priority})`（与原工程 `DataBridgeExports.cpp:469` 触发条件一致），并写入 INFO 业务日志（`[Queue] xx 点怪 yy 成功（优先=…），当前排队 N 位`）。
- **前端**（`OverlayWindow`）：默认文本循环滚动（`animate-marquee`，浅黄）；业务消息单次滚动（10s，黄色）后取队首继续、队列空则回默认；文案逐字对齐 `DanmuManager.OnDanmuProcessed`：
  - 优先 + 有怪物：`xx 优先 yy 成功，已置前！`
  - 优先无怪物：`xx 优先插队成功，已置前！`
  - 普通：`xx 点怪 yy 成功！`
- **实现要点**：`marqueeBusyRef` + `marqueeQueueRef` + 播放令牌（`marqueeTokenRef`）驱动，`onAnimationEnd` 与「时长+1s」兜底定时器双保险（令牌保证只推进一次）；待播消息数在跑马灯栏以 `+N` 显示。
- **验收**：浏览器内注入 Tauri 事件 mock 实测 —— 默认文本 → 派发 `order-placed` → 文本变为 `测试水友 点怪 煌黑龙 成功！`；再派发优先事件 → `+2` 待播计数 → 播完自动切换为 `测试水友 优先 雷狼龙 成功，已置前！`。

### D4 业务事件提示 ✅
- **气泡**：`checkin-reply / retroactive-checkin-recorded / retroactive-query / like-reward-granted / gift-received / super-chat-received / guard-received / ai-bubble` 统一进入堆叠气泡；**上限 5 条**（超出移除最旧）、**15s 自动退场**、新消息置顶（对齐 `OrderedMonsterWindow.AddBubble/UpdateBubblePositions`）；配色按业务分类（打卡绿 / 补签紫 / 奖卡橙 / 礼物红 / AI 靛蓝）。
- **主播控制台动态（新）**：B站 TAB 新增「最近弹幕动态」（`danmu-received`，20 条）与「打卡动态」（`checkin-recorded`，10 条）双卡片，使这两个事件具备消费方（原工程无对应 UI，属增强）。（2026-09-24 按产品要求移除「最近弹幕动态」面板及 `danmu-received` 事件，仅保留「打卡动态」卡片）
- **有意差异**：`checkin-recorded` 不再生成气泡（避免与 `checkin-reply` 同帧重复），改由控制台动态列表消费。
- **验收**：浏览器 mock 实测 —— 5 条气泡上限生效；`retroactive-query` → 「补签查询@水友E …」、`gift-received` → 「礼物@礼物D 赠送 辣条 ×10」、`guard-received` → 「上舰@新舰长C 开通 月 ×1（等级 3）」、`like-reward-granted` → 「点赞奖卡@测试水友 …突破30…」均正确渲染。

### D5 日志与可观测 ✅
- **`logging.rs`（新）**：`Logs/YYYY-MM-DD.txt`（数据目录内），UTF-8 BOM，行格式 `[YYYY-MM-DD HH:MM:SS]:[LEVEL] message`（与原工程 `WriteLog` 一致）；内存环 500 条；Debug 级别仅调试构建输出（`cfg!(debug_assertions)`，等价原工程 Release 下空宏）。
- **替换裸 `eprintln!`**：全仓 16 处改为 `log_error!/log_warn!/log_info!`（bilibili 8、config 3、lib 5）。
- **前端「运行日志」视图**（全模式保留）：日志目录回显、级别过滤（ERROR/WARNING/INFO/DEBUG，语义为「最高详细级别」）、刷新/清空（仅清内存环）、ERROR/WARNING/INFO/DEBUG 分色；`get_recent_logs` / `clear_recent_logs` 命令。
- **资源缺失提示**：`resource-missing` 事件 → 侧栏「运行日志」项红色角标 + 日志页顶部告警块。
- **有意差异**：日志目录由原工程的 exe 同级改为**可写数据目录**（安装版 exe 目录可能只读）；`.gitignore` 增加 `Logs/`。
- **验收**：实测生产版启动生成 `Logs/2026-09-19.txt`（含 BOM 与 `[Hotkey] 已注册 Alt+, …`），日志视图截图确认路径与条目。

### D6 列表性能与长文本 ✅
- **虚拟化**：`VirtualList` 固定行高 + 可视区渲染（含 overscan）+ `paddingTop/Bottom` 撑高；主窗口（行高 54）与悬浮窗（行高 50）队列列表均接入。实测 500 条数据仅渲染 **29** 行 DOM（`ceil(1037/50)+8`），`scrollHeight = 25000 = 500 × 50`。
- **长文本往返滚动**：`MarqueeText` 测量文本宽度超出容器时启动 `marquee-x`（`--marquee-shift` 注入位移，时长 = 超出像素 / 25，最少 2s），鼠标悬停暂停（`.marquee-pause:hover span`），对齐原工程 `OnScrollTextLoaded / OnScrollTextMouseEnter / OnScrollTextMouseLeave`。实测 29 行长名称全部进入滚动且位移量正确（`-139px`）。

### D7 连接状态机 ✅
- **五态**（`bilibili.rs`）：`Disconnected / Connecting / Connected / Reconnecting / ReconnectFailed` + `DisconnectReason`（`None / NetworkError / HeartbeatTimeout / ServerClose / AuthFailed`），中文文案取自原工程 `ConnectionStateToString / DisconnectReasonToString`。
- **驱动点**：进入循环即 `Connecting`；`start_app` 网络类失败 → `Reconnecting(NetworkError, N)`；WS 连接成功 → `Connected`；心跳发送失败 → `HeartbeatTimeout`；服务端关闭帧/终止包 → `ServerClose`；接收错误 → `NetworkError`；用户断开 → `Disconnected`。
- **命令/事件**：`get_bili_connection_state` 返回 `{state, reason, reason_text, attempt, display}`，事件 `connection-state-changed`（替换原 `connection-changed` bool 事件）。
- **前端**：侧栏状态点四色（绿/琥珀脉冲/红/灰），长连面板显示「正在重连...(第N次)」「重连失败，原因: 鉴权失败」；按钮文案随状态切换（断开长连 / 取消连接 / 开启直播长连）。
- **有意差异**：鉴权类失败（HTTP 401/403、错误码 -400/100001、文案含签名/鉴权/权限）收敛为 `ReconnectFailed` 并**停止重试**（原工程 `ReconnectFailed`/`AuthFailed` 从未被置位、无限重试；见 `classify_start_error` 注释）。
- **验收**：`test_connection_status_five_states_and_reasons`、`test_start_error_classification_stops_retry_on_auth`。

### D8 交互细节对齐 ✅
- **透明度仅作用于背景**：悬浮窗外层不再使用整体 `opacity`，改为面板背景 `rgba(3,7,18, alpha)`（alpha = 锁定 ? `penetrating_mode_opacity` : `opacity`），文字与条目保持不透明 —— 与原工程「仅插值 `MainGrid.Background` alpha」一致；实测 `panelBg = rgba(3, 7, 18, 0.95)`。
- **完成交互**：**保留显式「完成」按钮**（原工程为单击条目即完成）。因「单击即删」在悬浮窗上存在误删风险，**已经用户确认（2026-09-19）保留按钮方案**，窗口中标注「点击完成该单并删除」，差异标注为有意差异（见附录）。

**批次 D 涉及文件**：`src-tauri/src/logging.rs`（新增）、`src-tauri/src/bilibili.rs`、`src-tauri/src/tts.rs`、`src-tauri/src/lib.rs`、`src-tauri/Cargo.toml`（`tauri-plugin-global-shortcut`）、`src/components/VirtualList.tsx`（新增）、`src/components/MarqueeText.tsx`（新增）、`src/views/MainWindow.tsx`、`src/views/OverlayWindow.tsx`、`src/types.ts`、`src/App.css`、`.gitignore`。

**批次 D 验证与手工测试入口**
- `cargo test`：102 项全绿（批次前 94 项），无编译警告。
- `npm run build`：通过；`npm run tauri build`：MSI + NSIS 产物正常。
- 生产版实测：`Logs/2026-09-19.txt` 落盘 + `Alt+,` 热键注册成功 + 悬浮窗按记忆位置 `(913,105)` 落位 + 日志视图/设置面板（含「当前引擎: Windows SAPI」实时显示）截图确认。
- 浏览器事件 mock 实测（`http://localhost:1420/#/overlay` + 注入 `__TAURI_INTERNALS__`）：跑马灯三类文案与队列推进、5 条气泡上限与多事件渲染、500 条队列虚拟化（29 行 DOM）、长文本往返滚动与悬停暂停、背景透明度语义。

**Lite 模式**：D1/D2 的 TTS 与打卡相关控件在 Lite 下置灰（`opacity-40` + 禁用）、D3/D4 在 Lite 下不产生业务（后端已拦截播报与打卡）；D5/D6/D7 全模式保留 —— 与计划一致。

---
## 八、批次 E：工程化与一致性（P2）✅ 已完成（2026-09-19）

> **实施结论：E1~E6 全部落地；`cargo test` 105 项全绿（批次前 102 项）、无编译警告；`npm run build` 通过；新增 `npm run verify` 一键门禁。**

### E1 Lite 覆盖矩阵与统一拦截 ✅
- **统一守卫**：`lib.rs` 新增 `ensure_not_lite(&state, "模块名")?`；9 处命令内联拦截（打卡/补签/GM/AI 思考）机械收敛到守卫，前端可见文案逐字不变；**补齐 3 处遗漏**：`play_sound_effect`（原静默 `Ok(())` 改为显式拒绝）、`save_manbo_api_key`、`simulate_like`；前端「发射模拟点赞」入口同步 `disabled={isLite}`（逐条核对 13 个被拦截命令的前端调用点，无遗漏）。
- **矩阵文档**：新增 `docs/LITE_COVERAGE_MATRIX.md`（逐功能矩阵：点怪排队 / 悬浮窗 / 长连 / 身份码 / 配置 / 日志 = 保留；TTS 播报 / 点赞奖卡 / 打卡 / 补签 / GM / AI / 音效 = 停用；`get_current_tts_engine` 只读保留；调试通道静默空转）。
- **新增功能规则**：非排队功能默认不支持 Lite，必须在命令入口调用守卫；事件管道类在 `handle_incoming_*` 首部 `return`；单测新模块须加入守卫断言列表。
- **验收**：`test_ensure_not_lite_guard_blocks_non_lite_modules`（13 个模块的文案矩阵 + 放行/拒绝 + Lite 下排队仍可用）。

### E2 注册表写入健壮性与去重 ✅
- `AppConfig::save` 的注册表同步抽为 `persist_registry()`：失败仅 `log_warn`，不阻断文件保存（原为 `let _ =` 静默吞错）。
- `config.rs::load` 的两处迁移写入（IdCode / ManboApiKey）改为失败告警。
- **重复写入消除**：`save_app_config` 原先直写注册表、`cfg.save()` 内再写一次 → 现统一由 `persist_registry` 单点负责（空值语义不变：仅非空覆盖）。
- 连带收敛：`set_lite_mode` 保存失败上抛前端 + 日志；`save_manbo_api_key` 保存失败告警；悬浮窗位置防抖落盘失败告警；**队列落盘 5 处**（4 个命令 + 弹幕管道）抽为 `save_queue_or_warn`；启动时怪物库/历史队列加载失败告警。
- **有意保留**：`emit` 类广播失败保持静默（前端窗口未创建属正常场景，非错误）。
- **验收**：`test_save_with_empty_credentials_keeps_registry_untouched`（空凭据不改写注册表 + 文件保存不受注册表状态影响）。

### E3 退出流程 ✅
- 新增 `shutdown_app()`（命令 `end_app` 与主窗口 `CloseRequested` 共用）：① 待写悬浮窗位置并入内存并落盘（等价原工程 `WriteQueue::Flush`）② 停止 B 站长连（`set_running(false)`，等价 `BliveManager::Disconnect/Destroy`）③ 记录退出日志 ④ `app.exit(0)`。
- 队列与配置本就变更即时落盘，退出链路只做兜底，不引入新的写入时序。
- **有意差异**：退出路径不阻塞调用 B 站下播接口（`end_app` API），避免网络等待拖慢退出。
- **验收**：`test_apply_pending_position_updates_memory_config`。

### E4 测试门禁 ✅
- `package.json` 新增：`npm run verify`（build + cargo test + check:encoding 串联）、`test:rust`、`check:encoding`、`check:fields`。
- 门禁脚本：`scripts/check_encoding.py`（BOM 规则自动核查）、`scripts/check_orphan_fields.py`（配置字段接线核查）。
- `docs/DEVELOPMENT_GUIDE.md` 增补「提交门禁」章节与 `ensure_not_lite` 规则说明。

### E5 编码与资源清单核查 ✅
- **BOM 修复 5 个文件**：`src/components/{VirtualList,MarqueeText}.tsx`、`src/views/OverlayWindow.tsx`、`src-tauri/build.rs`、`src-tauri/src/checkin_ai.rs`；复检 34 个 BOM 必需文件 + 10 个 JSON 全部通过。
- **`.gitignore` 增补**：运行时用户数据与凭据（`credentials.dat` / `configs.json` / `order_list.json` / `MainConfig.cfg` / `captain_profiles*.db*` / `*.invalid`）禁止入库。
- **资源清单核查**：`bundle.resources` = `monster_list.json` + `voices/` + `local_voices.zip` + `dict/*.utf8`，与 `paths::find_resource` 全部调用点一一对应；`monster_icons.zip` 不打包 —— V2 图标走前端 `public/monster_icons`（441 个文件，Vite 构建进 dist）；**清理冗余 `public/voices`（628KB，与 `MonsterOrderWilds_configs/voices` 逐字节重复且全仓无引用）**。

### E6 孤儿字段清单 ✅
- 核查脚本 `scripts/check_orphan_fields.py`：AppConfig 29 个字段全部接线，**无孤儿字段**。
- 计划点名项逐一确认：`opacity` / `penetrating_mode_opacity` → 悬浮窗背景 alpha；`top_pos_x/y` → 启动恢复 + 防抖落盘 + 退出落盘；`tts_cache_days_to_keep` → 启动音频留档清理；`enable_captain_checkin_ai` → 打卡模块总开关；`only_speek_paid_gift` → 礼物播报过滤。

**批次 E 涉及文件**：`src-tauri/src/lib.rs`、`src-tauri/src/config.rs`、`src-tauri/build.rs`（BOM）、`src-tauri/src/checkin_ai.rs`（BOM）、`src/components/VirtualList.tsx`、`src/components/MarqueeText.tsx`、`src/views/OverlayWindow.tsx`（BOM）、`public/voices`（删除）、`.gitignore`、`package.json`、`scripts/check_encoding.py`（新增）、`scripts/check_orphan_fields.py`（新增）、`docs/LITE_COVERAGE_MATRIX.md`（新增）、`docs/DEVELOPMENT_GUIDE.md`。

**批次 E 验证与手工测试入口**
- `npm run verify`：`npm run build` 通过 + 105 项单测全绿（批次前 102 项）+ 编码核查通过；`cargo build` 无编译警告。
- `npm run tauri build`：MSI（11.65MB）+ NSIS（9.96MB）产物正常，较批次 D 各减小约 270KB（`public/voices` 清理）。
- **E3 实测（生产产物）**：精确 EnumWindows 定位主控制台窗口（label=`main`）并发送 WM_CLOSE → 进程 0.5s 内**自然退出**，`Logs/YYYY-MM-DD.txt` 落盘 `[INFO] [App] 退出程序：队列与配置已落盘`。
  （注：关闭 overlay 悬浮窗不退出应用——主窗口才是退出入口，与"悬浮窗常驻"设计一致。）

---

## 九、验证与回归策略

1. **单元测试**：每个任务在 `cargo test` 下新增用例（`[PASS]` 输出标记），前端改动跑 `npm run build` 类型检查。
2. **真实数据回归**：A2 必须使用仓库内真实 `captain_profiles.db` 副本；A1 必须数据驱动遍历真实 `monster_list.json`。
3. **端到端手测清单**（每里程碑执行）：
   - 模拟弹幕：点怪 / 点怪优先 / 优先（两段式）/ 打卡 / 补签 / 补签查询 / 点餐 / 曼波（音效）
   - 旧 B 站数据：历史队列 `OrderList.list` 加载、历史打卡库加载
   - 打包验证：安装版干净环境全链路（A3）
4. **对拍策略**：关键文案与算法（排序、连续天数、奖卡、匹配结果）与原工程逐条对拍，差异需在提交说明中标注为"有意差异"。

---

## 十、发布前检查清单

- [x] A1~A7 全部完成且单测覆盖（`cargo test` 105 项全绿）
- [x] `cargo test` 全绿、`npm run build` 通过、`npm run tauri build` 产物（MSI + NSIS）可用
- [x] configs.json 无明文凭据；configs.json 缺键不重置
- [x] 真实旧库（打卡/队列/配置）无损迁移验证（旧库文件 mtime 未变，WAL 侧仅新增测试用户记录）
- [x] Lite 模式全功能降级验证（仅保留点怪排队与悬浮窗）—— E1 产出覆盖矩阵 + 统一守卫 + 单测；安装版端到端仍建议人工回归
- [x] OBS 方案落地（A4：移除浏览器源误导 + 窗口捕获）
- [x] README 与 `docs/` 更新：功能状态表、已知差异清单（E 批次：README 重写目录/命令/导航、新增 LITE_COVERAGE_MATRIX、DEVELOPMENT_GUIDE 门禁章节）
- [x] D8 完成交互已确认（2026-09-19 用户决策：保留显式「完成」按钮，与原工程「单击条目即完成」的差异已标注为有意差异）

---

## 十一、风险与回滚

| 风险 | 说明 | 缓解 |
| --- | --- | --- |
| 旧库迁移写坏用户数据 | A2 涉及 ALTER 与数据搬移 | 迁移前自动备份 `captain_profiles.db.bak`（原工程已有 `_bak` 惯例）；迁移失败不阻断启动并提示 |
| 资源路径改动影响开发流程 | A3 改变现有相对路径行为 | 保留开发目录回退分支；单测覆盖四类分支 |
| AI 分词引入新依赖体积膨胀 | C1 引入 jieba-rs + 词典 | 评估体积（目标仍 < 10MB）；词典按需懒加载 |
| 行为对齐与"有意差异"混淆 | B/C 域存在多处历史修复语义 | 对拍清单强制标注差异项，避免"修回旧 bug" |
| UI 补全与 Lite 冲突 | D1/D2 控件在 Lite 下需隐藏 | ✅ 已缓解（E1）：统一守卫 `ensure_not_lite(&state, "模块名")` + `docs/LITE_COVERAGE_MATRIX.md` 逐功能矩阵 + `test_ensure_not_lite_guard_blocks_non_lite_modules` 单测；新增非排队功能必须显式声明 |

---

## 附：待用户决策事项

1. ~~**A4 OBS 方案**~~ → 已决策并实施（方案 2：移除误导 + 窗口捕获）。
2. ~~**C1 分词方案**~~ → 已决策并实施（2026-09-19：引入 `jieba-rs`，对齐原工程 cppjieba 同源词典 + HMM）。
3. ~~**C5 打卡落库时机**~~ → 已决策并实施（2026-09-19：保留即时落库，与原工程的差异标注为有意差异）。
4. ~~**C5 打卡日期口径**~~ → 已决策并实施（2026-09-19：采用弹幕服务器时间 sendDate 口径，时间戳缺失回退本机今天）。
5. ~~**D8 完成交互**~~ → 已决策并实施（2026-09-19：保留显式「完成」按钮；与原工程「单击条目即完成」的差异标注为有意差异）。
6. ~~**Lite 支持范围确认**~~ → 已按 AGENTS.md 默认实施（D3/D4 不支持 Lite；D5/D6/D7 全模式保留；D1/D2 控件置灰禁用）。

