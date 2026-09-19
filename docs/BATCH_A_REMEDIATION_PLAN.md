# 批次 A 修复方案（核心链路 P0）

> 关联文档：`docs/MIGRATION_COMPLETION_PLAN.md` 第四章
> 状态：待评审 → 评审通过后按「附 B 提交切分」逐项实施
> 编制日期：2026-09-19

---

## 0. 实施总览

### 0.1 任务清单、依赖与顺序

| 顺序 | 任务 | 涉及文件 | 依赖 | 预估 |
| --- | --- | --- | --- | --- |
| 1 | A1 怪物别名匹配修复（已实施） | `monster.rs` | 无 | 0.5 人日 |
| 2 | A2 旧库格式兼容 + 修正假通过单测（已实施，不做 ALTER） | `checkin.rs` | 无 | 1 人日 |
| 3 | A6 配置字段级容错 | `config.rs` | 无 | 0.5 人日 |
| 4 | A5 凭据防明文 | `config.rs`、`lib.rs`、（`types.ts` 注释） | A6（同文件先改） | 1 人日 |
| 5 | A7 配置热更新 | `bilibili.rs`、`lib.rs`、`OverlayWindow.tsx`、`ObsView.tsx` | A5（`lib.rs` 同函数） | 1 人日 |
| 6 | A3 资源打包与路径统一 | `tauri.conf.json`、新增 `paths.rs`、`monster/tts/queue/config/checkin/lib.rs` | A7（`lib.rs` setup 改造） | 2 人日 |
| 7 | A4 OBS 浏览器源 | `App.tsx`（必做）+ 视决策 | 决策 + A3（资源清单） | 0.5~2 人日 |

**排序理由**：A6→A5→A7 均改 `config.rs / lib.rs`，串行避免冲突；A3 改动面最大放最后；A4 依赖用户决策。

### 0.2 统一验证命令（每个任务完成必跑）

```bash
cargo test --manifest-path src-tauri/Cargo.toml
npm run build
# A3 / A4 额外：
npm run tauri build
```

### 0.3 通用约束（AGENTS.md）

- 每次改动后执行 `git diff` 复查，**不自动提交**（提交信息模板见附 B）；
- 新增/修改业务功能必须同步单测，输出保留 `[PASS]` 标记；
- 新增功能默认**不支持 Lite 模式**（Lite 下仅保留点怪排队与悬浮窗）；本批次 A1/A2/A6/A3 与 Lite 无关，A5/A7 为全模式生效。

---

## A1 怪物别名匹配：改为「先精确、后剥离」

### A1.1 缺陷

`src-tauri/src/monster.rs:153-190`：先剥离 `历战王/歷戰王/AT`、`历战/歷戰`，再对 `^alias$` 全字匹配。但 `monster_list.json` 中 **44 条别称本身自带该前缀**，用于映射"历战/历战王专属条目 + 专属图标"。实测（按现逻辑遍历全部 44 条）：

| 输入 | 期望（原工程） | 现状 |
| --- | --- | --- |
| 历战王冰呪龙 | `雪花沉睡`（`MHWI_ArchTempered/MHWI-Arch_Tempered_Velkhana_Icon.png`） | `冰呪龙`（普通图标） |
| 历战钢龙 | `风暴的棺材` | `钢龙` |
| 历战王雷 / 历战王boy / 历战王冰 | 对应历战王条目 | 匹配失败 |

### A1.2 目标设计

```rust
pub fn match_monster(&self, input_text: &str) -> Option<MonsterMatchResult> {
    if !self.loaded || input_text.is_empty() { return None; }
    let trimmed = input_text.trim();

    // 阶段 1：原文精确匹配（优先命中自带历战/历战王前缀的专有别称）
    if let Some(res) = self.match_exact(trimmed) {
        return Some(res);
    }

    // 阶段 2：剥离修饰词后重试（保持现有语义，兼容“历战大胖虎”类无专属条目的写法）
    let (forced_tempered, cleaned) = Self::strip_tempered_modifiers(trimmed);
    if cleaned.is_empty() { return None; }
    self.match_exact(&cleaned).map(|mut res| {
        if forced_tempered > 0 { res.tempered_level = forced_tempered; }
        res
    })
}
```

- `match_exact(text)`：抽取现有 `patterns` 遍历逻辑（`monster.rs:173-187`），命中返回该条目的 `default_tempered` 与 `icon_url`；
- `strip_tempered_modifiers(text) -> (i32, String)`：抽取现有 `monster.rs:163-171` 逻辑，**保持判断顺序**（`历战王/歷戰王` → `历战/歷戰`；`AT` 沿用现有 `contains + replace` 行为），返回值 (强制等级, 清理后文本)。

### A1.3 改动清单

| 文件 | 位置 | 改法 |
| --- | --- | --- |
| `src-tauri/src/monster.rs` | `match_monster`（:153-190） | 拆为 `match_exact` + `strip_tempered_modifiers` + 两阶段入口 |

### A1.4 测试设计（新增 2 项 + 保留 3 项）

1. **`test_all_aliases_exact_match`（新增，数据驱动）**：读取 `monster_list.json`，对每个条目的每个别称调用 `match_monster(alias)`：
   - 断言必须 `Some`；
   - 断言返回 `monster_name` 属于该别称的**归属条目**（若数据存在跨条目重复别称，允许命中任一归属方，与加载器的字典序规则一致）；
   - 对含 `历战/历战王` 的别称，额外断言 `tempered_level == 该条目默认历战等级`（覆盖全部 44 条）。
2. **`test_tempered_strip_fallback`（新增）**：`历战大胖虎 → 嗟怨震天怨虎龙 (tempered=1)`、`历战王黑龙 → tempered=2`。
3. 保留并通过：`test_load_and_match_real_monster_data`、`test_monster_alias_deterministic_order`。

### A1.5 验收

> **状态（2026-09-19）：已实施并验证。**

- [x] 上述 44 条别名全部命中正确条目与等级（单测覆盖；数据含 42 个唯一别称，另 2 条为「零式游星欧米茄」重复项）
- [x] `cargo test` 全绿（47 项，含新增 `test_all_aliases_exact_match`、`test_tempered_strip_fallback`）

---

## A2 旧版 `captain_profiles.db` 兼容（不做任何 ALTER）

> **决策记录（2026-09-19）：采用「V2 完全适配旧库格式」方案，已实施。** 原 ALTER + 备份方案废弃 —— 不修改老库结构，V2 建表与全部读写改用旧库列 `monthly_first_claimed` 承载「本周首破领取日」标记。

### A2.1 缺陷（已实测）

`checkin.rs:88-93` 优先打开原工程库，但 `init_schema` 与全部 SQL 使用 `weekly_first_claimed` 列，真实旧库不存在该列。实测：

```
SELECT weekly_first_claimed FROM retroactive_cards        → no such column: weekly_first_claimed
INSERT INTO retroactive_cards (..., weekly_first_claimed) → table ... has no column named weekly_first_claimed
```

真实旧库列：`retroactive_cards(uid, card_count, total_earned, monthly_first_claimed, last_earned_date)`；旧库中亦无 V2 曾自建的 `weekly_likes` 表。
影响：`add_likes`（`checkin.rs:347,402`）报错被 `lib.rs:513` 静默吞掉 → 点赞与奖卡失效；`get_cards` 恒 0；`grant_card` 失败。
假通过单测：旧 `test_open_legacy_captain_profiles_db` 伪造的旧表**已含** `weekly_first_claimed`。

### A2.2 目标设计（已实施）

1. `init_schema` 建表语句与原工程 `ProfileManager.cpp:101-184` 逐列对齐：
   - `retroactive_cards` 使用 `monthly_first_claimed`（承载「本周首破领取日」）；
   - `user_profiles` 含 `keywords_json / danmu_history_json`；
   - `checkin_records.username TEXT`（可空）+ `UNIQUE(uid, checkin_date)`；
   - 删除 `weekly_likes` 表（原工程无此表且 V2 未使用）；
   - 可空性/默认值放宽为原工程口径（`DEFAULT 0`，不加 `NOT NULL`）。
2. 全部 SQL 改用 `monthly_first_claimed`：`add_likes` 连续 7 天发卡 INSERT、周首破 SELECT + UPSERT、`get_cards` SELECT、`grant_card` INSERT。
3. Rust 结构体字段 `RetroactiveCardData.weekly_first_claimed` 名称保持（前端 IPC 契约不变），仅加注释说明持久化列名。
4. 不引入任何 `ALTER TABLE` / 备份文件；打开旧库时 5 条 `CREATE TABLE IF NOT EXISTS` 均为无操作。

**语义说明**：旧列值（月度语义，如 `20260101`）与本周周起始日大概率不等 → 等价于「本周未领取」，首个当日 30 赞自然发卡；仅当旧值恰等于本周周一才会少发一次（概率极低，是不改格式的必然代价）。

### A2.3 改动清单

| 文件 | 位置 | 改法 |
| --- | --- | --- |
| `src-tauri/src/checkin.rs` | `init_schema` | 建表语句对齐原工程 DDL（含删 `weekly_likes`） |
| 同上 | `add_likes` / `get_cards` / `grant_card` | 4 处 `weekly_first_claimed` 列名替换为 `monthly_first_claimed` |
| 同上 | `RetroactiveCardData` | 字段注释说明列名映射（无类型/IPC 变化） |
| 同上 | 测试模块 | 重写旧库单测 + 新增 2 项测试（见 A2.4） |

### A2.4 测试设计（已实施）

1. **重写 `test_open_legacy_captain_profiles_db`**：伪造表结构与真实旧库逐列一致（`monthly_first_claimed`、含 `keywords_json/danmu_history_json`、无 `weekly_likes`）。断言：① 旧数据（含旧列月度语义值）可读；② `record_checkin` 成功；③ `add_likes(uid, 30, today) == true` 且 `card_count == 3`；④ `grant_card` 正常；⑤ **读写后 `sqlite_master` 表结构快照与打开前完全一致**。
2. **新增 `test_v2_schema_matches_legacy_format`**：V2 新建库 `retroactive_cards` 含 `monthly_first_claimed`、不含 `weekly_first_claimed`；无 `weekly_likes` 表；`user_profiles` 含原工程同构列。
3. **新增 `test_real_repo_legacy_db_copy_compatible`（集成）**：复制 `MonsterOrderWilds_configs/captain_profiles.db` 到临时目录（**源库只读**），执行 `record_checkin ×2 → add_likes(30) → get_cards → grant_card → find_last_missing → execute_retroactive_checkin` 全链路，断言表结构快照零变更；源库缺失时输出 `[SKIP]`。

### A2.5 验收

> **状态（2026-09-19）：已实施并验证。**

- [x] 对真实旧库副本，`add_likes / get_cards / grant_card / record_checkin / execute_retroactive_checkin` 全部成功
- [x] 旧库表结构零变更（无 ALTER、无新表）；仓库源库结构复核未变
- [x] 假通过单测已修正；`cargo test` 全绿（49 项）

---

## A3 生产安装包资源打包与路径统一

### A3.1 缺陷

`tauri.conf.json` 无 `bundle.resources`，而 Rust 侧按文件系统读取：`monster.rs:126-149`（monster_list.json）、`tts.rs:304-323`（voices/*.mp3）；`config.rs / queue.rs / checkin.rs` 各自实现路径探测且规则不一致（cwd 优先 vs exe 优先）。安装版后果：**点怪匹配整体失效、特殊音效静默失败**。

### A3.2 目标设计

**① 资源随包**（`tauri.conf.json`）

```json
"bundle": {
  "active": true,
  "targets": "all",
  "resources": {
    "../MonsterOrderWilds_configs/monster_list.json": "MonsterOrderWilds_configs/monster_list.json",
    "voices": "voices"
  },
  "icon": ["icons/32x32.png", "icons/128x128.png", "icons/128x128@2x.png", "icons/icon.icns", "icons/icon.ico"]
}
```

> 单一数据源：映射源路径保留仓库内唯一副本，避免复制维护。C1/B10 落地时再追加 `dict/`、`local_voices.zip`。
> 实施时验证映射语法与落盘位置（Windows 上资源位于 exe 同级或 `exe/resources/`，候选列表需同时覆盖）。

**② 新增 `src-tauri/src/paths.rs`（统一路径解析）**

```rust
pub fn init_resource_dir(dir: Option<PathBuf>);        // setup() 注入 app.path().resource_dir()
pub fn resource_root() -> Option<PathBuf>;             // OnceLock → exe/resources → exe 同级
pub fn find_resource(rel: &str) -> Option<PathBuf>;    // 可写目录 → 资源根 → 开发目录回退
pub fn config_dir() -> PathBuf;                        // 可写目录（见下）
pub fn ensure_seeded();                                // 首次运行播种（见 ③）
```

`config_dir()` 解析顺序（统一 config/queue/checkin 三处现状）：
1. `exe 同级/MonsterOrderWilds_configs`（绿色版，用户可编辑）—— 存在即用；
2. `cwd/MonsterOrderWilds_configs` —— 存在即用（保持 dev 体验）；
3. 兜底 `exe 同级/MonsterOrderWilds_configs`（创建）。

`find_resource(rel)` 解析顺序：`config_dir()` → `resource_root()/rel` → `resource_root()/resources/rel` → `cwd/rel` → `cwd/../rel`（dev 回退，保留现有体验）。

**③ 首次运行播种**：`ensure_seeded()` 在 exe 同级配置目录缺失 `monster_list.json` / `voices/` 时，从资源根复制（**不播种** DB 与用户配置）。

**④ 启动时序改造**（`lib.rs run()`）：资源目录需在 `AppState::default()` 之前确定，故状态创建移入 `setup`：

```rust
.setup(|app| {
    paths::init_resource_dir(app.path().resource_dir().ok());
    paths::ensure_seeded();
    app.manage(AppState::default());     // 由 .manage(AppState::default()) 迁移至此
    // …现有礼物连击定时任务…
    Ok(())
})
```

**⑤ 落点替换**：`monster.rs find_monster_list_path`、`tts.rs play_special_sound` 的候选列表、`config.rs get_config_path/get_legacy_config_path`、`queue.rs get_order_list_path`、`checkin.rs get_default_db_path` 全部改为调用 `paths.rs`；旧路径作为兼容候选保留在 `paths.rs` 内部。

**⑥ 资源缺失可见**：查找失败时输出日志 + emit `resource-missing` 事件（前端提示，配合 D5）。

### A3.3 改动清单

| 文件 | 改法 |
| --- | --- |
| `src-tauri/tauri.conf.json` | 新增 `bundle.resources` 映射 |
| `src-tauri/src/paths.rs` | **新增**（上述 API + 单测） |
| `src-tauri/src/lib.rs` | 模块声明；`run()` 的 setup/manage 时序改造 |
| `src-tauri/src/{monster,tts,queue,config,checkin}.rs` | 路径探测替换为 `paths::*` |

### A3.4 测试设计

- `paths.rs` 单测：`config_dir` 三级回退、`find_resource` 五级候选（用临时目录构造优先级场景）；
- 保留并通过现有 `test_load_and_match_real_monster_data`（改为经 `find_resource` 定位）。

### A3.5 验收

> **状态（2026-09-19）：已实施；`cargo test` 58 项全绿，`npm run tauri build` 打包验证见下。**

- [x] `paths.rs` 单测：`config_dir` 回退、`find_resource` 真实资源解析、`resource_root` 回退、目录递归复制
- [x] 绿色版优先使用 exe 同级 `MonsterOrderWilds_configs`（`config_dir()` 第一优先级，存在即用）
- [x] 各模块路径探测统一为 `paths::*`（monster / tts / config / queue / checkin / credentials）
- [x] `cargo test` 全绿（58 项）
- [x] `npm run tauri build` 成功（MSI + NSIS）；安装脚本 `nsis/x64/installer.nsi` 已确认资源文件写入 `$INSTDIR\MonsterOrderWilds_configs\`（monster_list.json + voices 全树，卸载同步清理）。B 批次追加 `local_voices.zip`、C 批次追加 `dict/stop_words.utf8` 与 `dict/user.dict.utf8` 后，资源清单为 16 项（2026-09-19 重新打包复核）
- [ ] 干净目录运行安装产物实测点怪匹配与特殊音效（需人工：安装后启动 → 模拟"点怪火龙" → 播放"曼波"）

**实施补充说明**
- `config_dir()` 候选顺序修正为：**cwd 下 → cwd/.. 下（dev）→ exe 同级（安装版兜底）→ 兜底创建 exe 同级**。原因：`tauri dev` / `cargo test` 的 cwd = `src-tauri`，若 exe 同级优先会命中 `target/debug/MonsterOrderWilds_configs` 资源副本，导致开发态丢失 `credentials.dat`、历史库与 `MainConfig.cfg`；
- `find_resource(rel)` 候选顺序：`config_dir()/rel` → `resource_root()/MonsterOrderWilds_configs/rel` → `resource_root()/rel` → `cwd` 及仓库根回退（Windows 下 Tauri 资源直接落在 resource_dir，无 `resources/` 子层，故未列入）；
- 启动时序：`run()` 的 setup 中先 `init_resource_dir` + `ensure_seeded`，再 `app.manage(AppState::default())`（原本在 Builder 链上提前创建，会导致路径解析拿不到资源目录）；
- 资源缺失可见：启动时对 `monster_list.json` / `voices` 做一次存在性检查，缺失则输出 `[Paths]` 日志并 emit `resource-missing` 事件（前端提示并入 D5）；
- `credentials.dat` 路径探测一并统一（原第 5 处独立实现）。

---

## A4 OBS 浏览器源（含决策点）

> **决策记录（2026-09-19）：选择变体 2「移除误导」，已实施。**

### A4.1 缺陷

`MainWindow.tsx:279` 与 README 宣称 `http://localhost:1420/#/obs`，生产版无本地 HTTP 服务；`ObsView.tsx` 为死代码（`App.tsx:15-19` 将 `#/obs` 渲染为 `OverlayWindow`，其 `hide_window("obs")` 指向不存在的窗口）。

### A4.2 变体 1（推荐）：内置轻量 HTTP 服务，保留卖点

- 依赖：`axum` + `tower-http`（静态文件）；监听 `127.0.0.1`，端口 1420（占用则 +1 并回传实际端口）；
- 路由：`GET /` → `dist/` 静态资源（`index.html#/obs` 由前端路由渲染）；`GET /api/queue` → 队列 JSON；可选 `GET /api/events`（SSE）替代轮询；
- 资源：`tauri.conf.json` 追加 `"../dist": "web"`（依赖 A3）；
- UI：`MainWindow.tsx` 复制按钮改为显示**实际监听地址**（由新命令 `get_obs_url` 返回）。
- 工作量约 1.5~2 人日；需评估对"4MB 单文件"目标的体积影响（axum 增量约 1~1.5MB，需实测）。

### A4.3 变体 2：移除误导

- 删除 `ObsView.tsx`；`App.tsx` 只保留 `#/overlay`；
- README 与 `MainWindow.tsx:279` 文案改为"OBS 使用窗口捕获"；移除未使用的 `hide_window("obs")` 调用。
- 工作量约 0.5 人日。

### A4.4 共同必做项（与决策无关）

- [x] `App.tsx` 路由修复：`#/obs` 分支已移除，仅保留 `#/overlay`（变体 2 已删除 `ObsView.tsx` 及路由分支）
- [x] 移除失效的 `hide_window("obs")` 调用（随 ObsView 删除）
- [x] README 与 `ARCHITECTURE_DESIGN.md` 已改为【窗口捕获】说明；`MainWindow` 侧边栏改为非误导提示

### A4.5 决策点

**已决策：变体 2（2026-09-19）**。原文如下，保留备查：

~~需用户确认选型~~（见《MIGRATION_COMPLETION_PLAN》待决策事项 #1）。变体 1 若后续需要，可基于 `axum` + `tower-http` 重新引入。

---

## A5 敏感凭据不得明文落盘 / 返回前端

### A5.1 缺陷

`lib.rs:281-294` 用 `credentials.dat` 真实值覆盖 `new_cfg` 后，`config.rs:279` 将完整 `AppConfig`（含 `app_id / access_key_id / access_key_secret / chat_api_key / mimo_api_key / manbo_api_key`）**明文写入 configs.json**；`get_app_config` 还会把上述字段返回 WebView（`types.ts:32-67`）。与 AGENTS.md"禁止明文设置或泄露敏感凭据"冲突。

### A5.2 目标设计

**① 序列化排除**（`config.rs`）：对 `id_code`、`app_id`、`access_key_id`、`access_key_secret`、`manbo_api_key`、`mimo_api_key`、`deepseek_api_key` 增加 `#[serde(skip_serializing)]`。
- 保留 `Deserialize`（旧 `MainConfig.cfg` 的 `MIMO_API_KEY` 等历史键仍需可读，`from_legacy` 不受影响）；
- `id_code` 一并排除，落实 AGENTS.md"身份码与 JSON 配置解耦、仅注册表持久化"。

**② 保存侧保护**（`lib.rs save_app_config`）：
```rust
// 身份码：仅非空才写注册表，避免前端空值清空注册表
if !new_cfg.id_code.trim().is_empty() {
    registry::write_id_code(&new_cfg.id_code)?;   // 失败不再吞错（E2）
}
// 敏感字段：逐字段以 credentials 为准；credentials 为空则保持现有 cfg 值
macro_rules! keep_cred { ($field:ident, $cred:expr) => {
    if !$cred.is_empty() { new_cfg.$field = $cred.clone(); }
    else { new_cfg.$field = state.config.lock().unwrap().$field.clone(); }
}}
```
> 修复现状"仅凭 `app_id` 非空就统一切换三个字段"的粒度问题。

**③ 读取侧脱敏**（`lib.rs get_app_config`）：返回前清空敏感字段（保留 `id_code`，前端身份码输入框依赖它；`MainWindow.tsx:1153-1163` 的凭据展示走 `get_credentials_status` 掩码，不依赖 config 字段）。
- 实现为 `AppConfig::sanitized(&self) -> AppConfig`（清空敏感字段），复用于 `get_app_config` 与 `config-changed` 事件 payload。

**④ 前端**：`types.ts` 敏感字段保留（运行时为空串），加注"敏感字段恒为空，请以 credentials.dat 为权威来源"；无需 UI 改动。

### A5.3 改动清单

| 文件 | 改法 |
| --- | --- |
| `src-tauri/src/config.rs` | 7 个字段加 `skip_serializing`；新增 `sanitized()`；测试改造（A5.4） |
| `src-tauri/src/lib.rs` | `save_app_config` 身份码/敏感字段逐字段保护；`get_app_config` 返回 `sanitized()` |
| `src/types.ts` | 注释说明（无类型变化） |

### A5.4 测试设计

1. **`test_config_serialization_excludes_secrets`（新增）**：构造含真实值的 cfg → `save(Some(tmp))` → 读取文件文本，断言**不含** `"app_id"`、`"access_key_secret"`、`"manbo_api_key"`、`"mimo_api_key"`、`"deepseek_api_key"`、`"id_code"` 键，且仍含 `opacity` 等常规键；
2. **改造 `test_app_config_persistence_and_defaults`**：删除 `id_code` 经 JSON 往返的断言（其值应来自注册表），保留并补充 `sanitized()` 断言；
3. 保留现有 credentials 兼容性测试（`credentials.rs`）全部通过。

### A5.5 验收

> **状态（2026-09-19）：已实施并验证。**

- [x] configs.json 文本中无任何明文密钥/身份码（`test_config_serialization_excludes_secrets` 覆盖序列化与落盘两条路径）
- [x] 前端保存配置后，注册表与 credentials.dat 中已有值不被清空（含前端回传空值场景；`AppConfig::save()` 亦改为仅非空写注册表）
- [x] 反序列化兼容：历史配置中的敏感键仍可读取（用例内断言）

---

## A6 配置字段级容错

### A6.1 缺陷

`config.rs:7-49` 的 `AppConfig` 无 `#[serde(default)]`；`config.rs:148` 的 `from_value(val).unwrap_or_default()` 导致**缺任一键即整份配置静默重置**。

### A6.2 目标设计

1. `AppConfig` 增加**容器级** `#[serde(default)]`（已实现 `Default`，缺失字段逐字段取默认）；
2. `parse_content` 解析失败分支：保留原文件、额外写出 `<file>.invalid` 副本供诊断，并输出日志（配合 D5）；
3. `from_legacy` 分支与旧格式探测逻辑不变。

### A6.3 测试设计

1. **`test_partial_config_uses_field_defaults`（新增）**：写入 `{"opacity": 80}` → 加载后 `opacity == 80` 且其余字段 == `AppConfig::default()` 对应值；
2. **`test_empty_object_config_is_all_defaults`（新增）**：写入 `{}` → 加载成功、不 panic、全默认；
3. 保留 `test_load_legacy_mainconfig_format`。

### A6.4 验收

> **状态（2026-09-19）：已实施并验证。**

- [x] 任一字段缺失不再触发整份重置；`cargo test` 全绿（52 项）
- [x] 解析失败生成 `<file>.invalid` 诊断副本且不覆盖原文件（补充用例）

---

## A7 配置热更新

### A7.1 缺陷

`lib.rs:99-102` 仅启动时注入过滤字段；`save_app_config`（`lib.rs:276-320`）不回写、不发事件；`OverlayWindow.tsx:25-37` 仅挂载时读配置。后果：过滤开关需重启；跑马灯/透明度改动不生效。

### A7.2 目标设计

**① `DanmuProcessor` 过滤字段改为原子类型**（`bilibili.rs:386-393`）：
```rust
pub only_medal_order: AtomicBool,
pub only_speek_wearing_medal: AtomicBool,
pub only_speek_guard_level: AtomicI32,

pub fn update_filters(&self, only_medal_order: bool, only_speek_wearing_medal: bool, only_speek_guard_level: i32);
```
- `process_danmu`（`bilibili.rs:527`）读取改为 `.load(Ordering::Relaxed)`；
- 触点同步：`lib.rs:99-102`（构造改 `AtomicBool::new(..)`）、`bilibili.rs:423-425`（`Default`）。

**② `save_app_config` 热更新**：更新 filters 后 emit `config-changed`（payload = `sanitized()` 配置，复用 A5）。

**③ 前端订阅**：`OverlayWindow.tsx` 在现有 `useEffect` 中增加 `listen("config-changed", () => fetchConfig())`，并清理监听（`ObsView.tsx` 已随 A4 删除，无需处理）；`MainWindow` 保存成功 Toast 文案已为"全局配置已保存并即时生效！"（无需改动）。

> 边界说明：**原生窗口**透明度（`set_opacity`）与位置记忆（`top_pos_x/y`）属于 D2 范畴，本任务不含；本任务保证前端 CSS 透明度与跑马灯文本即时刷新。

### A7.3 测试设计

- **`test_update_filters_takes_effect_immediately`（新增）**：构造 `DanmuProcessor`，先设置 `only_medal_order = true` → 无粉丝牌弹幕 `<0x01>` 不入队；调用 `update_filters(false, ..)` → 同一弹幕可入队（覆盖运行期切换）；
- 保留 `test_danmu_processor_flow`、`test_non_guard_cannot_claim_priority`。

### A7.4 验收

> **状态（2026-09-19）：已实施并验证（单测 + 构建；悬浮窗手测并入批次 A 整体回归）。**

- [x] 修改"仅粉丝牌可点怪 / 仅播报佩戴粉丝牌 / 舰长等级"后无需重启即生效（`test_update_filters_takes_effect_immediately`；`only_speek_*` 两项经配置快照天然即时）
- [x] 修改跑马灯文本、透明度后悬浮窗即时更新（`config-changed` 事件 + 悬浮窗订阅，`npm run build` 通过）
- [x] `cargo test` 全绿（54 项）

---

## 附 A：批次 A 完成后的整体回归清单

| # | 场景 | 命令/操作 | 期望 |
| --- | --- | --- | --- |
| 1 | Rust 单测 | `cargo test --manifest-path src-tauri/Cargo.toml` | 全绿，新增用例均输出 `[PASS]` |
| 2 | 前端构建 | `npm run build` | 通过 |
| 3 | 生产打包 | `npm run tauri build` | 成功；exe 复制到空目录可运行 |
| 4 | 空目录首启 | 模拟弹幕"点怪历战王冰呪龙" | 入队 monster_name = `雪花沉睡`，图标为 AT 图标 |
| 5 | 特殊音效 | `play_sound_effect("曼波")` | 可播放 |
| 6 | 旧库兼容 | 用真实 `captain_profiles.db` 副本启动 | 点赞/补签卡/发卡正常，且表结构零变更（无 ALTER） |
| 7 | 配置容错 | 手工删除 configs.json 中 `opacity` 键后启动 | 其余设置保持，opacity 取默认 |
| 8 | 凭据安全 | 保存配置后检查 configs.json | 无明文密钥与身份码 |
| 9 | 热更新 | 勾选"仅粉丝牌可点怪"→ 发送普通用户点怪弹幕（不重启） | 不入队 |
| 10 | Lite 回归 | 开启 Lite 模式 | 仅点怪排队与悬浮窗可用，其余模块拦截（不受本批次影响） |

---

## 附 B：提交切分建议（由用户执行，助手不自动提交）

| # | 提交范围 | 建议信息 |
| --- | --- | --- |
| 1 | A1 | `fix(monster): 别名匹配改为先精确后剥离，修复历战/历战王专有别称错配` |
| 2 | A2 | `fix(checkin): V2 完全适配旧库格式（monthly_first_claimed），不做 ALTER，修正假通过单测` |
| 3 | A6 | `fix(config): 配置解析按字段容错，缺键不再整体重置` |
| 4 | A5 | `fix(security): 敏感凭据不再明文落盘与回传前端，身份码仅注册表持久化` |
| 5 | A7 | `feat(config): 配置热更新（过滤器即时生效 + config-changed 事件）` |
| 6 | A3 | `fix(bundle): 资源随包分发与统一路径解析，修复安装版点怪与音效失效` |
| 7 | A4 | 视决策：`feat(obs): 内置本地 HTTP 服务支持 OBS 浏览器源` 或 `chore(obs): 移除不可用的 OBS 网页源说明与死代码` |

每个提交前：`git diff` 复查范围 → `cargo test` → `npm run build`。
