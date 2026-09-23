# 点怪名单编辑器（禁点名单）· 实施规格

> 设计稿：`docs/design/monster_list_editor.html`（三视图可交互原型，已用浏览器逐项验证）
> 分支：`feat/monster-list-editor`　状态：**已实施**（2026-09-23，落点对照见第十章）
> 本文件为实施依据，任何与设计稿冲突处以本文件为准并回写设计稿。

> **⚠ 语义变更（2026-09-23 二次修订）**：名单由「白名单（名单内才可点）」翻转为**禁点名单 / 黑名单（名单内不可点）**，
> 并移除「弹幕白名单」总开关 —— 名单内的怪弹幕与选怪面板都点不了，空名单 = 不限制。详见 §一 决策 1 与 §十差异 2。

## 一、目标与已确认决策

| # | 决策 | 说明 |
|---|---|---|
| 1 | 名单语义 = **禁点名单（黑名单）**（2026-09-23 二次修订） | 名单内的怪物**不可被点单**：弹幕点怪与选怪面板同时生效；空名单 = 不限制任何点怪；**没有总开关**。原「白名单 + 开关」模型已废弃 |
| 2 | 编辑深度 = **完整字典编辑** | 可新增/删除怪物条目、改别称、默认历战等级、更换图标，写回 `monster_list.json` |
| 3 | 入口 = 主窗口新增独立 tab「怪物名单」 | 导航第 3 项，位于「点单排队管理」之后 |
| 4 | **支持 Lite 模式** | `ONLY_ORDER_MONSTER=1` 下名单与选怪面板均可用（属核心点怪能力） |
| 5 | 选怪面板落点（2026-09-23 修订） | 放在「点单排队管理」tab 左侧，**整体替换原「快速手动点怪」文字表单**；悬浮窗回归纯队列、不改动 |
| 6 | 拦截次数不统计（2026-09-23） | 界面不展示任何拦截次数/计数，拦截发生时就地提示原因即可 |

## 二、数据设计

### 2.1 名单文件（新增）

`MonsterOrderWilds_configs/monster_roster.json`（UTF-8 无 BOM，与 `monster_list.json` 同目录，经 `paths::config_dir()` 解析）

```json
{
  "items": ["嗟怨震天怨虎龙", "黑龙", "风暴的棺材"]
}
```

- `items`：禁点怪物原名数组（去重、按加入顺序保存，**顺序无功能含义**）
- 空数组 / 文件缺失 = **不限制任何点怪**（默认状态，无需开关）
- 旧版白名单文件（含 `enabled` 字段）语义相反，沿用会把全部怪物误判为禁点：加载时判定为旧格式、丢弃内容、迁移落盘为空名单并记 `log_warn`
- 解析失败 / 结构非法：回退空名单并记 `log_warn`，不阻断启动
- 落地路径与 `monster_list.json` 一致：`config_dir()` → 打包态为 exe 同级或 macOS 用户数据目录

### 2.2 字典编辑写回

- 写回目标：`MonsterDataManager::find_monster_list_path()` 解析出的同一条 `monster_list.json`
- 保持现有格式：`serde_json::to_string_pretty` + 2 空格缩进、字段名沿用中文键（`默认历战等级`/`图标地址`/`别称`）、UTF-8 无 BOM
- 写入后**热重载**匹配器（`load_from_file` 重新编译正则），失败则回滚内存数据并返回错误
- 破坏性操作（删除条目、改原名）由前端二次确认；运行中的队列不回溯修改

### 2.3 图标清单（图标选择器用）

432 张图标位于前端静态目录 `public/monster_icons/`，后端在打包态无法枚举该目录，故清单在**构建期生成**：

- 新增脚本 `scripts/gen_icon_manifest.py`：扫描 `public/monster_icons/**/*.png` → 生成 `src/generated/iconManifest.ts`（UTF-8 with BOM，导出 `ICON_LIST: string[]` 与 `iconGroup(path)`）
- `package.json`：`"gen:icons": "python scripts/gen_icon_manifest.py"`，并挂到 `dev` / `build` 前置（`"build": "npm run gen:icons && tsc && vite build"`）
- 生成物**提交入库**，保证 CI 与本地一致；`check:encoding` 已覆盖 `.ts`

## 三、后端设计

### 3.1 新模块 `src-tauri/src/roster.rs`

```rust
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct RosterData { pub items: Vec<String> }

pub struct MonsterRoster { data: RwLock<RosterData>, path: PathBuf }

impl MonsterRoster {
    pub fn load(path: Option<&Path>) -> Self;          // 缺失/损坏 → 空名单 + 告警；旧白名单文件 → 清空并迁移
    pub fn save(&self) -> Result<(), String>;          // 原子写（temp + rename）
    pub fn is_blocked(&self, monster: &str) -> bool;   // 命中禁点名单即 true（空名单恒 false）
    pub fn snapshot(&self) -> RosterData;              // {items}
    pub fn replace(&self, data: RosterData) -> Result<(), String>;
    pub fn add_all(&self, names: Vec<String>) -> Result<usize, String>;  // 追加去重，返回新增数
    pub fn remove(&self, name: &str) -> Result<(), String>;
    pub fn clear(&self) -> Result<(), String>;
    pub fn rename_item(&self, old: &str, new: &str) -> Result<bool, String>;  // 改原名同步名单
}
```

- 并发模型：`RwLock`（读多写少；弹幕热路径只读 `is_blocked`）
- 单测守护：空名单缺省、往返持久化、去重追加、`is_blocked` 仅命中名单内、损坏回退、**旧白名单文件被清空并迁移**

### 3.2 AppState 扩展（`lib.rs`）

```rust
pub struct AppState {
    pub monster_mgr: Arc<MonsterDataManager>,
    pub roster: Arc<MonsterRoster>,      // 新增
    // ... 其余不变
}
```

### 3.3 命令清单（新增 7 个）

| 命令 | 签名 | 用途 |
|---|---|---|
| `get_monster_roster` | `() -> RosterData` | 读取禁点名单 |
| `set_monster_roster` | `(data: RosterData) -> RosterData` | 整表保存（编辑器批量操作后落盘） |
| `get_monster_dict` | `() -> HashMap<String, MonsterConfig>` | 全量字典（图鉴库 + 别称冲突检测）；`MonsterConfig` 需补 `Serialize`（已有） |
| `save_monster_entry` | `(name: String, config: MonsterConfig, original: Option<String>) -> usize` | 新增/更新条目并热重载，返回条目总数 |
| `delete_monster_entry` | `(name: String) -> usize` | 删除条目并热重载（同一次调用内移出禁点名单中的同名项） |
| `export_monster_roster` | `(app: AppHandle) -> Result<Option<String>, String>` | 弹保存对话框 → 写 `{items}` JSON → 返回路径（取消返回 `None`） |
| `import_monster_roster` | `(app: AppHandle) -> Result<Option<RosterData>, String>` | 弹打开对话框 → 读取校验 → **仅返回解析结果**，由前端选定「覆盖 / 合并」后经 `set_monster_roster` 统一落盘（取消返回 `None`） |

导入形态：`{"items": [...]}` 对象，或裸字符串数组；文件含 BOM 时自动容忍；**含 `enabled` 的旧白名单文件直接拒绝并提示改写格式**。解析函数 `parse_roster_json` 为纯函数，已有单测覆盖。

导入导出实现：用已引入的 `tauri-plugin-dialog`（`lib.rs:1790` 已注册）Rust 侧 API + `tauri::async_runtime` 的 oneshot 回调桥接，**不新增前端依赖、不需改 capabilities**（Rust 侧调用不受 ACL 约束）。

### 3.4 弹幕禁点拦截

接入点：`src-tauri/src/bilibili.rs`（`process_danmu` 步骤 5，`match_monster` 命中分支内）

```rust
if let Some(match_res) = monster_matcher.match_monster(&substring) {
    if roster.is_blocked(&match_res.monster_name) {   // 该怪在禁点名单内
        result.matched = true;
        result.monster_name = match_res.monster_name.clone();
        result.blocked_by_roster = true;
        return result;                                // 不入队，交由调用方提示
    }
    // ... 原有入队逻辑不变
}
```

- `process_danmu` 签名追加 `roster: &MonsterRoster`（与 `monster_matcher` 同风格传参，便于单测）
- `DanmuProcessResult` 新增 `blocked_by_roster: bool`（`Default` 派生不受影响）
- `lib.rs` 调用处传 `&state.roster`；`res.blocked_by_roster` 时记 `log_info` 并 emit **`order-blocked`** 事件（`{user_name, monster_name}`）
- 悬浮窗（`OverlayWindow.tsx`）监听 `order-blocked` → 复用 `pushMarquee` 提示「xxx 点怪 yyy 未生效（已在禁点名单）」，仅此一处悬浮窗改动

### 3.5 Lite 模式

- 名单与选怪面板均在核心点怪路径上，**不加 `is_lite_mode` 门控**
- 字典编辑（增删条目）同样可用；`get_monster_dict` / `save_monster_entry` 等命令不判断 Lite
- 复核现有 Lite 门控点（`lib.rs:586/838/910/945`）确认不误伤新命令

## 四、前端设计

### 4.1 导航

`MainWindow.tsx` 的 tab 联合类型追加 `"monster"`，导航按钮插入「怪物名单」（第 3 位），副标题：`怪物名单与禁点配置`。

### 4.2 「怪物名单」tab（设计稿视图 01 / 02）

- 布局：`col-span-4`（图鉴库） + `col-span-3`？→ **实际按设计稿：左库 flex-1、右名单固定 308px**（不走 12 栅格，用 `flex` 容器，见设计稿 `.editor-main`）
- 页头：标题 + 副标题 + 导入/导出按钮；**无任何白名单开关**（禁点名单默认生效，空名单即不限制）
- 统计条（3 张卡）：怪物总数 / 禁点名单内怪物数 / **别称冲突数（可点击，打开冲突明细浮层）**—— **不统计拦截次数**，拦截发生时就地提示即可
- 图鉴库：搜索（原名 + 别称全字包含）、作品分段（5 部）、禁点状态分段（已禁点 / 未禁点）、「全部禁点」按钮（把当前筛选结果批量加入禁点名单）；卡片 `title` 列出该条目的冲突别称
- 名单面板：**只读展示**（图标 + 名称 + 别称数）+「新增自定义怪物」；**不做**拖拽排序 / 全选 / 清空 / 单条移除，加入与移出一律由左库卡片点击完成，列表顺序无功能含义
- 卡片：点击切换禁点（乐观更新 + `set_monster_roster` 落盘）、悬停铅笔打开编辑抽屉
- 编辑抽屉：原名 / 图标（打开图标选择器，数据源 `ICON_LIST`，可留空并可一键清除）/ 默认历战等级三选一 / 别称增删（Enter 添加、× 删除）/ 冲突实时校验（给出「本条目生效 / 对方生效」的确定结论）/ 删除条目 / 保存（`save_monster_entry`）
- 冲突判定：前端持有全量条目（`get_monster_dict`）自建「别称 → 归属条目」表；与运行时一致地按「名称排序靠前者命中」给出结论
- 文字规范（2026-09-23）：**界面文案不得出现「字典」字样**（含 hover `title`、下拉选项、toast 与命令错误串）；冲突规则的表述统一为「按名称排序靠前者优先命中」，不再用「字典序」
- 冲突明细浮层：列出每条冲突的**别称 + 全部归属条目 + 生效/不生效标记**，点条目直接打开该条编辑抽屉（`conflictWinner` 判定，见 `src/lib/monsterList.ts`）

### 4.3 「点单排队管理」tab 改造（设计稿视图 03）

- 左栏 `col-span-5`：原「快速手动点怪」表单单整体删除，替换为选怪面板
  - 搜索框（怪物名 / 别称）、作品分段、4 列图标网格（`get_monster_dict` 驱动，顺序即字典顺序）
  - 禁点名单内的怪物：置灰 + 锁标记，点击只提示拦截（`#opFootTip`）；**其余全部可选**（候选池 = 全量字典）
  - 选中确认卡：水友昵称 / 身份（普通·舰长·提督·总督）/ 难度（跟随字典或强制 0/1/2）/ 优先插队 → 「加入排队」调 `add_order`
- 右栏 `col-span-7`：现有队列列表保持不变（拖拽排序 / 完成并出队 / 清空队列）
- 队列数据继续走 `get_queue` + `queue-updated`，无需新接口

### 4.4 类型与样式

- `src/types.ts`：新增 `RosterData`、`MonsterConfig`、`MonsterIconInfo`（如需）、`OrderBlockedPayload`
- `src/App.css`：沿用设计稿的令牌体系（`--w-*` / `--amber-*` / `.monster-grid` / `.mcard` / `.oc` / `.q-row` 等已在原型中定稿的样式），按现网变量命名改写并整体追加
- 字体现有子集已覆盖怪物名与常用汉字，新增 UI 文案若含冷僻字需重跑 `scripts/fonts/subset_fonts.py`

### 4.5 状态与落盘时机

| 操作 | 落盘策略 |
|---|---|
| 左库卡片点击（加入 / 移出禁点名单） | 本地乐观更新 + **300ms 防抖** 调 `set_monster_roster` 整表落盘；失败回滚并 toast |
| 字典编辑（抽屉） | 抽屉内为草稿态，仅点「保存」时调 `save_monster_entry`；取消则丢弃 |
| 字典删除条目 | `delete_monster_entry` 同一次调用内同步移出禁点名单中的同名项（名单只读展示，无手动清理入口） |
| 编辑抽屉的别称 | 原名在抽屉中作为独立「原名」芯片展示，**不再重复写入 `别称` 数组**（加载器本就会把原名纳入匹配，去重后语义等价且数据更整洁） |
| 别称冲突检测 | 基于 `get_monster_dict` 的本地快照计算，不额外请求 |

「全部禁点」= 当前筛选命中的怪批量加入禁点名单（不重复添加已有项）。

### 4.6 前后端字段约定

- `MonsterConfig` 序列化沿用现有**中文键**（`默认历战等级` / `图标地址` / `别称`），前端 TS 类型按同名键声明，不做字段映射
- `RosterData` 使用英文键：`{ items: string[] }`

## 五、设计稿对照

| 设计稿元素（`monster_list_editor.html`） | 实现落点 |
|---|---|
| 视图 01 图鉴库网格 `.monster-grid/.mcard` | `MainWindow.tsx` monster tab 左栏 |
| 视图 01 可选名单 `.rrow`（设计稿含拖拽排序 / 移除 / 全选 / 清空） | 同 tab 右栏，**改为只读展示**（图标 / 名称 / 别称数），增删走左库卡片点击 |
| 视图 02 编辑抽屉 + 图标选择器 `.drawer/.icon-picker` | 同 tab 内抽屉组件（非独立窗口） |
| 视图 03 选怪面板 `.oc/.op-confirm/.op-foot` | queue tab 左栏（替换原手动表单），置灰对象由「名单外」改为「禁点名单内」 |
| 视图 03 队列 `.q-row` 系列 | queue tab 右栏（沿用现有实现，不改） |
| `toast()` / `footTip()` | 前端临时提示（局部 state + 定时器） |

设计稿中已定稿的文案（如「点图标即点怪」）按原样落地；涉及名单语义的文案按禁点（黑名单）重写。

## 六、边界与异常

| 场景 | 处理 |
|---|---|
| 禁点名单为空 | 不限制任何点怪（默认状态）；界面不再有「名单为空」红色警示（该警示随白名单模型一并移除） |
| 旧版白名单文件（带 `enabled`） | 加载时判定为旧格式 → 丢弃内容、迁移落盘为空名单并记 `log_warn`；导入时直接拒绝并提示改写为 `{"items": [...]}` |
| 字典条目被删除 | `delete_monster_entry` 同一次调用内把禁点名单中的同名项一并移出（名单为只读展示，无手动移除入口） |
| 名单含字典外的名字（外部导入） | 右栏显示为「字典缺失 · ?」但不清除；选怪面板不受影响（该名字本就不在字典中），需要彻底清理可导出后编辑再导入 |
| 字典编辑改原名 | 同步把禁点名单中旧名替换为新名（同一次保存内完成） |
| 字典文件损坏 / 写入失败 | 内存数据回滚，命令返回错误，前端 toast 提示且不更新本地状态 |
| 别名冲突（同一别称多归属） | 前端提示字典序实际命中结论；运行期行为不变 |
| 图标未设置（字典允许空 `图标地址`） | 图鉴库 / 名单 / 选怪面板 / 抽屉统一渲染盾牌占位（`.icon-ph`）；队列沿用既有 `icon_url` 空值兜底 |
| 图标文件缺失 | 卡片 `onError` 隐藏图片（现有实现） |
| 并发：编辑保存 vs 弹幕点怪 | `RwLock` 读写隔离；落盘采用「临时文件 + rename」原子替换 |
| 打包态路径 | 名单与字典均经 `paths::config_dir()`；macOS 走用户数据目录，`ensure_seeded` 仅播种字典（名单不播种，空名单即无限制） |

## 七、测试计划

**后端（`cargo test`，输出 `[PASS]` 标记）**
1. `roster.rs`：空名单缺省（不限制）、序列化往返、`add_all` 去重、`is_blocked` 仅命中名单内、损坏文件回退、**旧白名单文件被清空并迁移**
2. `bilibili.rs`：空名单→全部照常入队；名单内→`blocked_by_roster=true` 且队列不变；名单外→入队；优先词 / 繁体前缀组合
3. `monster.rs`：`save_monster_entry` 后新别称可匹配、删条目后不再匹配、改名不破坏原匹配
4. 命令层：`set_monster_roster` → `get_monster_roster` 往返一致；删除字典条目同步清理名单

**前端 / 工程**
- `npm run build`（tsc + vite，含 `gen:icons` 前置）
- `npm run check:encoding`（新增 `.rs/.ts/.tsx/.css/.md` 必须 BOM，JSON 无 BOM）
- 设计稿交互断言：改完前端后以同等方式回归（浏览器断言 + 控制台零报错）

## 八、实施顺序

| 步骤 | 内容 | 验证 |
|---|---|---|
| 1 | `roster.rs` + AppState + 名单 3 命令 + 单测 | `cargo test` |
| 2 | 弹幕拦截改造 + `order-blocked` 事件 + 单测 | `cargo test` |
| 3 | 字典编辑命令（读全量 / 保存 / 删除）+ 热重载 + 单测 | `cargo test` |
| 4 | `gen:icons` 脚本 + 「怪物名单」tab（图鉴库 / 名单 / 抽屉 / 图标选择器） | `npm run build` + 界面验证 |
| 5 | queue tab 改造（删手动表单、接选怪面板）+ 悬浮窗 `order-blocked` 跑马灯 | 界面验证（含 Lite 模式） |
| 6 | 导入导出 + 编码检查 + 全量 `npm run verify` | 全绿 |

每步完成后 `git diff` 复查，不自动提交。

## 九、待确认项（已确认，2026-09-23）

1. **名单为空时是否允许开启白名单** —— ~~允许 + 红色警示~~ **已随二次修订作废**：改为黑名单后不存在「开启」动作，空名单即不限制
2. **导入策略** —— **导入时弹二次选择**。已实现：选择文件 → 弹「合并 / 覆盖」；选择「覆盖」再弹一次破坏性确认，两次均拒绝则中止（Esc 关窗不再等同于覆盖）
3. **capabilities** —— **维持 Rust 侧 dialog**，未改动权限文件、未新增前端依赖

## 十、实施落点（2026-09-23）

| 规格条目 | 实际落点 |
|---|---|
| `roster.rs` 名单模块 | `src-tauri/src/roster.rs`（`MonsterRoster` + `RosterData`，RwLock、临时文件+rename 原子写、7 项单测含旧格式迁移） |
| AppState 名单注入 | `lib.rs` AppState 新增 `roster: Arc<MonsterRoster>`（生产注入 `load(None)`，单测指向临时目录） |
| 名单 3 命令 + 字典 3 命令 + 导入导出 | `get_monster_roster` / `set_monster_roster` / `export_monster_roster` / `import_monster_roster` / `get_monster_dict` / `save_monster_entry` / `delete_monster_entry`（共 7 个，已注册 `generate_handler!`；`set_roster_enabled` 随二次修订删除） |
| 弹幕禁点拦截 | `bilibili.rs::process_danmu` 命中分支（`roster.is_blocked` 为真即 `blocked_by_roster = true` 并 break）；`lib.rs` 侧 emit `order-blocked`，悬浮窗订阅后跑马灯提示「已在禁点名单」 |
| 字典热重载 | `MonsterDataManager` 内部状态改为 `RwLock<MonsterInner>`；新增 `edit_and_save`（保序读 → 闭包改 → 校验 → 原子写 → 整体换入），失败不改内存 |
| 字典写回保序 | `Cargo.toml` 为 `serde_json` 开启 `preserve_order`，避免默认 BTreeMap 按码点重排整份 `monster_list.json` |
| `add_order` 难度语义 | `tempered_level = None` → 跟随字典默认等级；`Some(v)` → 强制该等级（选怪面板「难度：默认 / 普通 / 历战 / 历战王」） |
| 图标清单 | `scripts/gen_icon_manifest.py` → `src/generated/iconManifest.ts`（432 张，UTF-8 with BOM，提交入库）；挂在 `dev` / `build` 前置 |
| 「怪物名单」tab | `src/views/MonsterListTab.tsx`（`MainWindow.tsx` 新增 `monster` tab，导航第 2 项「怪物名单」） |
| 选怪面板 | `src/components/MonsterPickerPanel.tsx`（queue tab 左栏，替换原「快速手动点怪」文字表单） |
| 样式命名空间 | 设计稿样式整体移植进 `src/App.css`，统一限定在 `.ml-scope` 内（新增 `--w-*` / `--amber*` 令牌），避免与悬浮窗主题互扰 |
| 主窗口外壳 | 现网为**左侧导航**（非设计稿的顶栏），故页面标题栏与外壳沿用现网结构，面板与抽屉等组件与设计稿一致 |

与设计稿的有意差异：
1. **名单语义翻转为禁点（黑名单）**（2026-09-23 二次修订）：设计稿与初版实现都是「白名单（名单内才可点）+ 总开关」；现改为**名单内不可点**、**取消总开关**，选怪面板的置灰对象与两 tab 页头文案随之翻转，后端拦截改为 `is_blocked`；
2. **「新增自定义怪物」打开空白抽屉**（设计稿演示为打开已有条目），新建时原名/别称为空、**图标留空**（字典允许空 `图标地址`，展示位用盾牌占位）、等级 0；编辑态另有「清除图标」按钮可把图标重新置空；
3. **禁点名单面板改为只读展示**（2026-09-23 修订）：去掉拖拽排序 / 全选 / 清空 / 单条移除，仅保留名单展示与「新增自定义怪物」按钮，加入与移出一律走左库卡片点击；作为配套，删除字典条目时后端在**同一次调用**内同步移出名单中的同名项，避免出现无法清理的残留条目；
4. **冲突统计卡可点开明细浮层**（2026-09-23 新增，设计稿只有一个数字）：浮层内按「别称 / 归属条目 / 生效·不生效」列出，点条目直接跳到编辑抽屉；
5. **队列卡片头部新增「配置怪物名单」按钮**（一键跳到名单 tab），设计稿未画。

**已修的实现缺陷**（2026-09-23）
- 工具栏第二行（作品分段 + 禁点状态分段）在默认 1000px 窗口下总宽超出卡片内宽 24px，导致状态分段那一组溢出卡片右边框：`.tb-row` 允许换行 + `.seg` 不参与收缩；
- `.drawer-body` 的样式在从设计稿移植时遗漏（设计稿为 `flex:1;min-height:0;overflow-y:auto;padding:14px`），抽屉内容贴边且超高时无法滚动，已补回。
