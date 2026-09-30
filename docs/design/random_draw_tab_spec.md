# 随机抽选武器与怪物 · 实施规格

> 设计稿：`docs/design/random_draw_tab.html`（可点击运行的真实原型，已用浏览器逐项验证）
> 状态：**已实施**（2026-09-30）。实施记录与开发中实际踩到的坑见 §十一。
> 任何与设计稿冲突之处以本文件为准，并回写设计稿。

## 一、目标

主窗口新增第 4 个页签「随机抽选」：从选定作品的池子里随机抽 1 把武器 + 1 只怪物，带悬念演出。

- 支持**只抽武器 / 只抽怪物 / 两个都抽**
- 支持**按作品筛选**怪物池（多选，多选即并集，天然覆盖「只选某一作」）
- 支持**手选排除名单**（怪物与武器都可逐个剔出）
- 抽选是**纯本地、零副作用**的行为：不写队列、不占弹幕额度、不发弹幕

## 二、决策表

| # | 决策 | 状态 |
|---|---|---|
| 1 | 入口 = 主窗口新增独立页签「随机抽选」，导航第 4 项（「怪物名单」之后、「舰长打卡 & GM」之前） | **已实施** |
| 2 | 抽选结果**先定后演**：真随机只摇一次，滚动帧全部是演出，绝不「边滚边随机」 | 已定（设计稿已如此实现） |
| 3 | 等概率，不加权重 | 已定 |
| 4 | 排除名单与「怪物名单」页签的**禁点名单相互独立**，另存一个文件；提供一次性「同步自禁点名单」 | **已实施** |
| 5 | 抽选记录只存内存（会话内最近 12 条），不落盘 | **已实施** |
| 6 | **Lite 形态支持** —— 用户 2026-09-30 明确要求，故不加 `ensure_not_lite` 守卫 | **已实施** |

## 三、资源

### 3.1 武器图标（已就位）

| 项 | 内容 |
|---|---|
| 来源 | `monsterhunterwiki.org/wiki/Category:MHWilds_Equipment_Icons` 下的 `MHWA-<Weapon> Icon Base.webp` |
| 原始规格 | 100×100 webp，约 4–6 KB，**纯灰阶**（max RGB = 255 的白色线稿，带 alpha） |
| 落地（随包入库） | `public/weapon_icons/MHWilds/MHWilds-<Weapon>_Icon_Base.png`，14 张，100×100 PNG |
| 打包产物 | `MonsterOrderWilds_configs/weapon_icons.zip`（约 117 KB） |
| 命名 | 与 `monster_icons.zip` 同构：`<作品>/<作品>-<英文名>_Icon_Base.png`，目录项 `MHWilds/` |

14 把武器（游戏内武器栏顺序，官方简中名）：

| 序 | 文件名 | 简中 | 英文 |
|---|---|---|---|
| 1 | `MHWilds-Great_Sword_Icon_Base.png` | 大剑 | Great Sword |
| 2 | `MHWilds-Long_Sword_Icon_Base.png` | 太刀 | Long Sword |
| 3 | `MHWilds-Sword_and_Shield_Icon_Base.png` | 单手剑 | Sword & Shield |
| 4 | `MHWilds-Dual_Blades_Icon_Base.png` | 双剑 | Dual Blades |
| 5 | `MHWilds-Hammer_Icon_Base.png` | 大锤 | Hammer |
| 6 | `MHWilds-Hunting_Horn_Icon_Base.png` | 狩猎笛 | Hunting Horn |
| 7 | `MHWilds-Lance_Icon_Base.png` | 长枪 | Lance |
| 8 | `MHWilds-Gunlance_Icon_Base.png` | 铳枪 | Gunlance |
| 9 | `MHWilds-Switch_Axe_Icon_Base.png` | 斩击斧 | Switch Axe |
| 10 | `MHWilds-Charge_Blade_Icon_Base.png` | 盾斧 | Charge Blade |
| 11 | `MHWilds-Insect_Glaive_Icon_Base.png` | 操虫棍 | Insect Glaive |
| 12 | `MHWilds-Light_Bowgun_Icon_Base.png` | 轻弩炮 | Light Bowgun |
| 13 | `MHWilds-Heavy_Bowgun_Icon_Base.png` | 重弩炮 | Heavy Bowgun |
| 14 | `MHWilds-Bow_Icon_Base.png` | 弓 | Bow |

**抓取注意事项（复现时必读）**：

- 站点挂在 Cloudflare 挑战后，命令行 `curl` / `WebFetch` 一律 403（`Just a moment...`），**必须走真实浏览器**；放行后再用同源 `fetch` 取图（图片域名与页面同源可用，`monsterhunterwiki.org/images/...` 跨子域会被 CORS 拦）。
- 站点原图是 webp。转成 PNG 是为了两点：与 `monster_icons` 全 PNG 的约定对齐；规避不同 WebView 的 webp 支持差异。
- 该分类下同时有甲具/护石/随从等 34 个 `Icon Base`，**只有正好 14 个是武器**（Great Sword / Long Sword / Sword and Shield / Dual Blades / Hammer / Hunting Horn / Lance / Gunlance / Switch Axe / Charge Blade / Insect Glaive / Light Bowgun / Heavy Bowgun / Bow）。`Kinsect`、`Palico *`、`Relic *`、`Bowgun Mod` 都不要。

### 3.2 怪物图标（复用现有）

- 复用 `public/monster_icons/**`（432 张，已入库），按 `src/generated/iconManifest.ts` 的 `iconGroup()` 分作品
- 字典分布：荒野 39 / 世界 39 / 冰原 40 / 崛起 30 / 曙光 27 = **175 条**

> ⚠ **尺寸约束（直接影响排版）**：怪物图标源图**只有 60×60**，武器是 100×100。
> 抽选盘上怪物图标显示 ≤100px、武器 ≤150px，把放大倍率压在 ~1.6× 内；
> 再大就会糊，白色线稿的锐利轮廓会先垮掉。

### 3.3 清单生成

`scripts/gen_icon_manifest.py` 扩展为同时扫描两个目录：

- `public/monster_icons/**/*.png` → `src/generated/iconManifest.ts`（现状，不动）
- `public/weapon_icons/**/*.png` → `src/generated/weaponIconManifest.ts`（新增，导出 `WEAPON_ICON_LIST: string[]`）

脚本已挂在 `npm run gen:icons`，并被 `dev` / `build` 前置，无需改 `package.json`。

## 四、数据设计

### 4.1 抽选设置文件（新增）

`MonsterOrderWilds_configs/draw_settings.json`（UTF-8 无 BOM，经 `paths::config_dir()` 解析，与 `monster_list.json` 同目录）

```json
{
  "games": ["MHWilds", "MHWorld", "MHWI", "MHRise", "MHRS"],
  "excluded_monsters": ["黑龙"],
  "excluded_weapons": ["MHWilds-Bow_Icon_Base.png"],
  "mode": "both",
  "pace": "normal"
}
```

- `games`：参与抽取的作品；空数组视为全选（防呆，避免用户把自己锁在空池里）
- `excluded_monsters`：按怪物**原名**（与 `monster_list.json` 的键一致）
- `excluded_weapons`：按**文件名**（稳定，不随中文名调整而失效）
- `mode`：`both` | `weapon` | `monster`
- `pace`：`fast` | `normal` | `long`
- 文件缺失 / 解析失败 / 结构非法：回退全默认并记 `log_warn`，**不阻断启动**（对齐 `roster.rs` 的处理姿态）
- 落盘时机：改动后 500ms 防抖；写到临时文件再 rename（对齐 `roster` 的原子写）

### 4.2 武器表（前端编译期常量，无后端）

新增 `src/lib/weaponList.ts`：

```ts
export const GAME_WEAPON_ORDER = [...] as const;   // 14 条，顺序即游戏内武器栏顺序
export interface WeaponEntry { id: string; cn: string; en: string; file: string; }
export const WEAPONS: WeaponEntry[] = [...];
export const weaponSrc = (file: string) => `/weapon_icons/MHWilds/${file}`;
```

不落后端：14 条常量口径固定，后端不需要认识武器。

### 4.3 抽选记录

只存内存（组件 state），会话内最近 12 条。**不落盘**（P2 再议）。理由：重启后记录失去意义，落盘会引入新的失败面。

## 五、前端设计

### 5.1 落点

- 新增 `src/views/RandomDrawTab.tsx`
- `src/views/MainWindow.tsx`：`activeTab` 联合类型加 `"draw"`；nav 加一项（`Dices` 图标，lucide）；顶部标题加一条；渲染分支加一段

> 若决定不支持 Lite：nav 项与渲染分支一并挂在 `{!isLite && ...}` 里（与「舰长打卡 & GM」同款处理）。

### 5.2 骨架

```
┌ 左栏 236px ─┬──────────── 舞台 flex:1 ────────────┐
│ 抽什么       │  本次有效池 武器 n 把 · 怪物 m 只  [阶段 chip]│
│ 节奏         │  ┌ 武器盘 ┐  ×  ┌ 怪物盘 ┐        │
│ 武器池 14    │  │  图标  │     │  图标  │        │
│ 怪物池 175   │  └ 名字   ┘     └ 名字   ┘        │
│ [管理排除名单]│  节奏演出时间轴 ▬▬▬▬▬▬▬▬▬▬▬        │
│             │        [ 抽  选 ]                  │
│             │  [再抽一次] [送去点单] [复制结果]    │
├─────────────┴────────────────────────────────────┤
│ 抽选记录  （横向滚动卡条，最近 12 条）              │
└──────────────────────────────────────────────────┘
```

- 抽选记录放**底部横条**而不是右侧竖栏：1000px 窗口下左栏 224 + 配置 236 之后只剩 ~500px，
  再切一列会让两个抽选盘窄到看不清图标。
- 抽屉（`DrawPoolDrawer`）：从右侧滑出 520px，含作品多选 chips、搜索框、按作品分组的怪物网格、底部统计与操作。

### 5.3 抽选算法

```ts
const poolW = () => WEAPONS.filter(w => !excludedWeapons.has(w.file));
const poolM = () => entries.filter(e => games.has(e.game) && !excludedMonsters.has(e.name));
const pick = <T,>(arr: T[]) => arr[Math.floor(Math.random() * arr.length)];
```

- **结果先定**：`draw()` 一进入就算出 `wWin` / `mWin`，后面所有滚动帧都由这两个结果驱动（最后一帧落位到中奖项）
- 不加权重；不做保底；不做「N 次内不重复」（可作为 P2 开关，但会破坏「每次独立等概率」的直觉）

### 5.4 空池守卫（分况提示，别笼统说"不可抽"）

对齐 `MonsterPickerPanel` 的失效分况提示风格：

| 情形 | 表现 |
|---|---|
| 仅武器 + 武器池空 | 按钮禁用 + 「14 把武器全部被排除 —— 至少放回 1 把」 |
| 仅怪物 + 怪物池空 | 按钮禁用 + 「当前作品筛选下没有可抽的怪物 —— 放宽作品或从排除名单放回几只」 |
| 双抽 + 任一池空 | 按钮禁用 + 指明是哪一个空了 |
| 作品筛选只剩 1 个 | 允许，但移除最后一个作品时拒绝并提示「至少要保留一个作品」 |
| 排除到池子只剩 1 个 | 允许剔除失败并提示「至少要保留 1 只」 |

**不允许**通过 UI 把某个池子清成 0：最后一项不可被排除。这样"空池"只可能来自配置文件的非法值，而那条路径在加载时已经回退默认。

## 六、动效规格

### 6.1 时间轴（标准档）

| 段 | 帧间隔 | 段时长 | 帧数 | 作用 |
|---|---|---|---|---|
| 起手 | 90 ms | 340 ms | 4 | 卡片抖动 + 按钮锁定，建立"要开始了"的预期 |
| 高速 | 46 ms | 900 ms | 20 | 图标完全糊成残影，看不出内容 |
| 中速 | 62 ms | 900 ms | 15 | 能看清轮廓但来不及认，开始"猜" |
| 减速 | 110 ms | 500 ms | 5 | 明显变慢，观众以为要停了 |
| **伪停** | 200 ms | 300 ms | 2 | **悬念钩子**：慢到几乎停住，但没停 |
| **再冲** | 70 ms | 200 ms | 3 | 突然又加速一次，把刚放下的心再提起来 |
| 落位 | 160 ms | 200 ms | 1 | 真正停下 |
| 滚动合计 | — | **3000 ms** | 50 | |
| 锁定 | — | 200 ms × 抽几项 + 120 ms | | 先武器后怪物，各响一下：回弹 + 光环 + 白光 |
| 揭晓 | — | 760 ms | | 金光涌起 + 结果名点亮 + 按钮解锁 |

**端到端 ≈ 4.1 s**（设计稿标题里的「3.0s」指滚动段，不是端到端）。

档位：快 ×0.40 ≈ **1.7 s** ｜ 标准 ×1.0 ≈ **4.1 s** ｜ 拖长 ×1.60 ≈ **6.1 s**

> 悬念来自**速度的两次转折**（减速→伪停→再冲），而不是把总时长拉长。
> 拉长只是让人等，转折才让人揪心。拖长档拉的是每一段的间隔，转折形状保持不变。

### 6.2 关键帧

| 名称 | 用途 |
|---|---|
| `drawShiver` | 起手：卡片 ±2px 抖动 |
| 滚动帧（WAAPI） | 旧图 `translateY(-58%) scale(.82) blur(4px)` 淡出；新图自下反向进入；`linear`，时长 = 该帧间隔 |
| `drawKick` | 锁定回弹 `1.055 → .985 → 1`，`cubic-bezier(.2,1.5,.4,1)` |
| `reelRingOut` | 锁定：琥珀光环 `.9 → 1.28` 扩散淡出 |
| `reelFlash` | 锁定：中心白光一闪 |
| `drawBurst` | 揭晓：金色径向光 `.5 → 2.05` 扩散 |
| `compassSpin` | 背景罗盘双环反向旋转，**仅滚动期间可见**（opacity 0 → .5） |
| `tlRevealPulse` | 时间轴「揭晓」段呼吸 |

### 6.3 降级与无障碍

- `prefers-reduced-motion: reduce` 或页内「减少动态」→ 跳过滚动（只 1 帧），保留短锁定与揭晓，总时长 < 0.6 s
- 阶段文字（待机 / 蓄力 / 滚动中 / 锁定 / 揭晓 / 已出结果）独立可读，**不依赖动画传达状态**
- 时间轴每一段都带 `title` 说明（段名 · 帧数 × 帧间隔 · 该段作用），鼠标悬停可读设计意图

### 6.4 并行约束（实测踩过的坑）

两个抽选盘必须**并行**推进：

```ts
const jobs = [];
if (needW()) jobs.push(reelW.step(..., f.gap));
if (needM()) jobs.push(reelM.step(..., f.gap));
await Promise.all(jobs);
```

串行 `await` 会让实际滚动时长**翻倍**（设计 3.0 s 跑成 6.0 s），时间轴播放头也会在半程停住，
和眼睛看到的动画完全对不上。设计稿初版就是这个错。

## 七、工程注意（实现时按这条自查）

### 7.1 `overflow-hidden` + 屏外抽屉 = 整个界面会横向错位

抽屉用 `transform: translateX(102%)` 藏在屏外时，父容器的 `overflow: hidden` 会把它算进 `scrollWidth`，
容器因此变成**可编程滚动容器**。此后任何 `scrollIntoView` / 焦点滚动都会把它横向滚偏：

- 自动化点击前的 scroll-into-view
- Tab 键把焦点移进屏外抽屉
- `input.focus()`、锚点跳转

现象是"整个应用框里的内容错位了 200 多像素"，而根容器自己的 `getBoundingClientRect()` 看起来完全正常。

**处置：这类容器一律用 `overflow: clip`**（不建立滚动容器，屏外内容既不可见也不可能被滚出来）。
现网 `MainWindow` 的 `.main` / 窗口根容器在加抽屉、下拉、Toast 时要按这条检查。

### 7.2 图标放大倍率

怪物 60×60 / 武器 100×100 是硬上限。抽选盘按 §3.2 的尺寸约束给，别为了"大气"把图标拉到 200px。

### 7.3 动画验证要看前台标签

后台标签 / 后台窗口下 CSS 动画与 rAF 会被节流，验证动画必须让页面在前台，否则测出来的时长和观感都不对。

## 八、落地清单

| 文件 | 动作 |
|---|---|
| `src-tauri/src/draw.rs` | **新增** `DrawSettings` 读写 + `get_draw_settings` / `set_draw_settings` 命令 |
| `src-tauri/src/lib.rs` | 注册上述命令（Lite 下按 §九 决策决定是否加 `ensure_not_lite`） |
| `src/views/RandomDrawTab.tsx` | **新增** 页签主体 |
| `src/components/DrawReel.tsx` | **新增** 单个抽选盘（滚动/锁定/揭晓三态） |
| `src/components/DrawPoolDrawer.tsx` | **新增** 池子配置抽屉（作品多选 + 搜索 + 怪物网格） |
| `src/lib/weaponList.ts` | **新增** 14 把武器常量 |
| `src/generated/weaponIconManifest.ts` | **生成**（`gen_icon_manifest.py` 扩展） |
| `src/views/MainWindow.tsx` | nav 加项 + 标题 + 渲染分支 |
| `src/App.css` | 新增 §6.2 的关键帧与抽选盘样式 |
| `scripts/gen_icon_manifest.py` | 扩展扫描 `public/weapon_icons` |
| `src-tauri/src/draw.rs` 单测 | 默认值 / 损坏回退 / 往返序列化 / 排除过滤 |
| `public/weapon_icons/MHWilds/*.png` | 已就位（14 张，入库） |
| `MonsterOrderWilds_configs/weapon_icons.zip` | 已就位（入库） |

前端搜索复用 `src/lib/monsterList.ts` 的 `matchKeyword`（命中原名或任一别称），
作品标签复用 `GAME_ORDER` / `GAME_LABEL`，别新造一套。

## 九、决策落定（原「待确认」，2026-09-30 已定）

1. **Lite 形态支持** —— 用户明确要求，已按支持实施：命令入口**不加** `ensure_not_lite`，
   页签入口不加 `{!isLite && ...}`，并在 `docs/LITE_COVERAGE_MATRIX.md` 登记了归属决策与理由。

2. 排除名单**独立**，另存 `draw_settings.json`；「同步自禁点名单」按钮把 roster 并进来。

3. 「送去点单」= 切到「排队管理」并把怪物预填进选怪面板（不直接入队：入队仍需昵称等字段）。
   实现落在 `MainWindow.pickerPreset` + `MonsterPickerPanel.presetMonster`；
   预选同样过一遍"可见且未禁点"的校验，命中禁点名单时只提示、不选中。

4. 抽选记录**只在内存**（会话内最近 12 条）。

## 十、验证方式

设计稿本身是**可点击运行**的原型，其 `window.__layoutCheck()` 自检会报告"容器是否被滚过、
左栏是否与应用框对齐"。改动设计稿后按下面三步复验：

1. `python -m http.server` 起在仓库根，打开 `docs/design/random_draw_tab.html`
2. 依次切 1000×700 / 1280×860 / 800×550 三档，确认 `__layoutCheck().ok === true`
3. 点「抽选」，确认标准档端到端 ≈ 4.1 s、时间轴播放头与动画同步、揭晓后记录 +1

实装后另需：

- `cargo test --manifest-path src-tauri/Cargo.toml`（draw.rs 单测）
- `npm run build`（含 `gen:icons` 与类型检查）

## 十一、实施记录（2026-09-30）

### 11.1 落点对照

| 文件 | 实际动作 |
|---|---|
| `src-tauri/src/draw.rs` | 新增：`DrawSettings` + `DrawSettingsManager` + 9 条单测 |
| `src-tauri/src/paths.rs` | 新增 `write_json_atomic`（通用原子写 JSON 配置，draw.rs 复用）+ 1 条单测 |
| `src-tauri/src/lib.rs` | 模块声明、`AppState.draw_settings`、两处构造、`get/set_draw_settings` 命令与注册、Lite 双形态测试 |
| `src/views/RandomDrawTab.tsx` | 新增：抽选编排 / 时间轴 / 记录 / 池子守卫 |
| `src/components/DrawReel.tsx` | 新增：抽选盘（滚动 / 锁定 / 揭晓） |
| `src/components/DrawPoolDrawer.tsx` | 新增：池子配置抽屉 |
| `src/lib/weaponList.ts` | 新增：14 把武器的口径表 + 清单一致性自检 |
| `src/generated/weaponIconManifest.ts` | 生成（`gen_icon_manifest.py` 扩展为双清单） |
| `src/types.ts` | 新增 `DrawMode` / `DrawPace` / `DrawSettings` |
| `src/views/MainWindow.tsx` | 导航项 + 顶栏标题 + 渲染分支 + `pickerPreset` |
| `src/components/MonsterPickerPanel.tsx` | 新增 `presetMonster` / `onPresetConsumed` |
| `src/App.css` | `.rd-scope` 段（约 870 行，含全部关键帧） |
| `docs/LITE_COVERAGE_MATRIX.md` | 登记「随机抽选 = Lite 保留」及其理由 |

### 11.2 开发中实际踩到并修掉的问题

设计稿验证阶段没暴露、写进 React/Tauri 之后才现形的四个问题，都已修并留下注释：

1. **页面不可见时抽选永久卡死**（最严重）。
   `Animation.finished` 依赖 `document.timeline`，而页面一旦不可见（切标签、窗口最小化/被遮挡），
   时间轴整个冻结、`finished` **永不 resolve**，`await` 挂死、按钮永久禁用，只能切页签自救。
   三层修复：① 每帧与 `setTimeout(dur+60)` 竞速兜底；② 页面 `hidden` 时直接走定格路径
   （后台标签的定时器被节流到 ≥1s，50 帧会拖成近一分钟，且此刻也没人看得见演出）；
   ③ `draw()` 整体 `try/catch`，任何异常都必须 `settle()` 解锁。

2. **同一 tick 内的连续变更互相覆盖**。
   各切换函数直接读闭包里的 `settings`，同一 tick 内连点 13 个武器格只生效最后 1 个。
   真人逐次点击因中间有重渲染而不暴露，但快速连点是真实场景。
   改为 `settingsRef` 同步镜像 + `mutate(fn)` 单一变更入口，落盘仍由单写者合并。

3. **类名错配**：组件输出 `rd-reel-weapon` / `rd-reel-monster`，CSS 写的是 `.rd-reel-w` / `.rd-reel-m`，
   导致「武器 ≤120px / 怪物 ≤98px」两条图标尺寸规则完全没生效 —— 图标退回按源图原始像素渲染，
   怪物盘明显偏小偏暗。顺带把纯百分比改成「百分比 + px 上限」，
   避免框变小时图标跟着缩（窄窗降级后怪物只剩 74px，比设计意图小一圈）。

4. **默认窗口下高度溢出**：`.ml-scope` 的高度交给内容会让 1000×700 下多出约 60px 滚动。
   压紧卡片内边距、记录行高、抽选盘外边距后落到 618px，可用区 602px —— 仍差 16px，
   权衡后保留这点滚动：主视图区本来就是 `overflow-y-auto`，且窗口最大化后不存在。

另有一条**未改**的观察：`draw()` 的演出时长（`paceEta`）与时间轴按**设计档位**渲染，
页面不可见时走的定格路径不会改写它们。理由是这个时间轴描述的是"设计好的那场演出"，
而定格路径是没人看得见时的兜底，不该反向改写设计参数。

### 11.3 试运行后的调整（2026-09-30）

产品试跑后按反馈做了两处修改：

1. **节奏示意条不进产物**（用户决定）。舞台上那条按段着色的时间轴是设计评审辅助，
   对主播是冗余信息，还挤占舞台高度 —— 已从 `RandomDrawTab` 与 `App.css` 中整体移除
   （连同播放头、分段着色、`rdTlPulse` 关键帧），零残留。
   「节奏」档位选择器与卡片上的端到端时长读数**保留**：那是控件与读数，不是示意条。
   设计稿里那条仍保留，但已在页首标注「设计说明用，最终产物不显示」。

   **同批一并去掉的还有滚动中浮在舞台左上角的「阶段气泡」**（"中速：看得清轮廓，但还来不及认" 那行）。
   它和示意条是同一类东西 —— 把设计意图讲给评审听，对主播是噪音，还压住了有效池文字。
   随之删除只服务它的 `Segment.tip` 字段、段位按钮上的悬停说明、以及只为"段变了才更新气泡"
   而存在的 `curSeg` 空转变量。`Segment.label` 保留，作为读代码时辨认每一拍用途的注记。

2. **帧间隔改为段内均分**。原先每帧取段落的标称 `gap`，`round(dur / gap)` 的取整让每段都偏长：
   标准档标称滚 3.0s，实际滚出 3.53s —— 于是档位标签（"3.0s 滚"）和时长读数全是假的。
   改成 `gap = dur / n` 后每段总长恰好等于设计值，档位标签与读数都从真实帧表算出。
   同时把时长读数补上「起手」那 260ms（此前系统性偏小半拍，实测 4.93s 却报 4.6s）。

3. **结果区不再摆「再抽一次」**（用户决定）。它与中间那颗大按钮功能完全重复 ——
   同一屏里两个入口做同一件事，只会让人多想一秒该点哪个。
   结果区现在只放"把结果用起来"的动作：「送去点单」「复制结果」；重抽走中间的大按钮。
   `.rd-btn.pri` 样式保留（池子配置抽屉的「完成」还在用）。

顺带把配置列又压了 12px：默认 1000×700 窗口下从"溢出约 60px"变为**溢出 2px**，基本一屏放得下。
抽完之后结果名与结果按钮出现，会再多占几十像素 —— 那属于结果态的正常增高。

### 11.4 定格被父渲染冲掉（2026-09-30 反馈修复）

**现象**：抽完之后盘上图标与下方结果名对不上 —— 名字写着「狩猎笛」，盘上却是别的武器。

**根因**：`MainWindow` 的 `showToast` 是内联箭头函数，**父组件每渲染一次就是新引用**
（队列轮询、连接状态、实时队列快照、拖拽等都会触发）。而「随机抽选」里
「加载设置」的 effect 把 `toast` 放进了依赖数组，于是它从"只跑一次"变成"每次父渲染都重跑"：

1. 重跑 → `invoke("get_draw_settings")` → `setSettings(后端返回值)`；
2. 后端返回的是**反序列化出的全新对象**，所有设置数组都换了引用；
3. `poolW` / `poolM` 这些挂在 `settings.xxx` 上的 `useMemo` 随之重算；
4. 「首屏给两个盘铺一张初始图」的 effect 依赖 `poolW` / `poolM`，被触发 → `show(poolW[0])`
   → **把已经定格的盘面冲回池子第一项**。

名字与记录读的是本地的 `last` 状态，不受影响，所以只有盘上的图标错位。

**修复**（两处，缺一不可）：

- 加载设置的 effect 改为**只跑一次**（依赖数组清空），`toast` 改经 `toastRef` 取最新值；
  `saveSettings` 同样不再依赖 `toast`，连带让所有设置变更回调的身份稳定下来。
- 铺初始图的 effect 加 `seededRef` 守卫，**每个盘只铺一次**；
  铺完之后显示权归抽选编排独占，池子引用再变也不动盘面。

**验证**：用带周期性重渲染的核对壳（模拟 MainWindow 的轮询）复现了错位，
修复后连抽带等，盘上图标、结果名、历史记录三者始终一致，父组件继续重渲染也不再漂移。

### 11.5 验证

- `npm run build`（含 `gen:icons` 与 `tsc`）通过
- `cargo test`（完整版）**279 passed**；`cargo test --features lite` **249 passed**
- `npm run check:encoding` / `check:fields` 通过
- 真实浏览器渲染核对（临时 Vite 壳 + 真实组件 + 真实 App.css，核对后已删除）：
  14 武器格 / 175 怪物格 / 9 段时间轴 / 193 张图标零破图；
  抽选一次得到「轻弩炮 × 搔鸟」并生成记录、按钮正常解锁；
  抽屉 5 组分组与计数正确；连点排除 13 把武器全部生效；排到只剩 1 把时被正确拒绝
