# Lite 模式（ONLY_ORDER_MONSTER）覆盖矩阵

> 批次 E1 产出。V2 用运行时开关 `is_lite_mode` 映射原工程的编译期宏 `ONLY_ORDER_MONSTER`
> （`#if !ONLY_ORDER_MONSTER` 排除 TTS/打卡/GM/AI）与 C# 侧运行时判断 `ToolsMain.IsOnlyOrderMonster`。

## 一、机制

| 层 | 机制 |
| --- | --- |
| 后端（Rust） | 统一守卫 `ensure_not_lite(&state, "模块名")?`（`lib.rs`），Lite 下返回 `Lite模式下XX已停用`；事件管道类在 `handle_incoming_*` 首部直接 return |
| 前端（React） | `isLite` 状态：导航项 `opacity-40` + 「停用」标签；控件 `disabled={isLite}`；视图内琥珀色横幅提示 |
| 判定边界 | 与原工程一致：**Lite = 仅点怪排队 + 悬浮窗 + B 站长连 + 基础设施（配置/身份码/日志）** |

## 二、覆盖矩阵

| 模块 | 功能 | 后端拦截点 | 前端处理 | Lite 支持 |
| --- | --- | --- | --- | --- |
| 点怪排队 | 点怪/优先/插队指令、队列增删改查、两段式置前排序、队列持久化与广播 | 无守卫（保留） | 队列视图正常可用 | ✅ 支持 |
| 悬浮窗 | 跑马灯、业务气泡、队列列表、透明度、锁定穿透、位置记忆、`Alt+,` 热键 | 无守卫（保留） | 正常可用 | ✅ 支持 |
| B 站长连 | 开播连接/断开、五态状态机、弹幕原始事件接收（点怪链路） | 无守卫（保留） | 长连面板正常 | ✅ 支持 |
| 身份码 | `save_id_code`（注册表持久化） | 无守卫（保留） | 长连面板可编辑 | ✅ 支持 |
| 配置 | `get_app_config` / `save_app_config` / `set_lite_mode` | 无守卫（保留） | 设置面板可用（TTS/打卡控件置灰） | ✅ 支持 |
| 运行日志 | 内存环 + `Logs/` 落盘、日志视图、资源缺失告警 | 无守卫（保留） | 日志视图正常 | ✅ 支持 |
| TTS 播报 | 普通弹幕朗读 `{uname} 说：{msg}` | `handle_incoming_danmu` 第 4 节 `!is_lite && enable_voice` | 置灰 | ❌ 停用 |
| TTS 播报 | 点餐指令播报（`点餐xxx`） | 同第 4 节 4.1 分支（位于 `!is_lite` 块内） | 置灰 | ❌ 停用 |
| TTS 播报 | 本地特殊音效（"曼波"等） | 同第 4 节 4.2；另 `play_sound_effect` 守卫 | 置灰 | ❌ 停用 |
| TTS 播报 | 礼物连击合并播报 | `handle_incoming_gift` 首部守卫；播报泵 `if is_lite { continue }` | 置灰 | ❌ 停用 |
| TTS 播报 | SC / 上舰 / 进场播报与气泡 | `handle_incoming_live_event` 首部守卫 | 无入口 | ❌ 停用 |
| TTS 播报 | 播报泵（超时连击结算 + 队列出队 + 引擎调用） | `run()` setup 内 150ms 泵 Lite 直通跳过 | — | ❌ 停用 |
| TTS 设置 | Manbo API Key 保存 | `save_manbo_api_key` 守卫 | 置灰 | ❌ 停用 |
| TTS 设置 | `get_current_tts_engine`（只读、无副作用） | 无守卫 | 「当前引擎」显示 | ⚪ 只读保留 |
| 点赞奖卡 | 点赞事件处理、30 赞/连续 7 天奖卡、`msg_id` 去重、播报 | `handle_incoming_like` 首部守卫 + `simulate_like` 守卫 | 置灰 | ❌ 停用 |
| 打卡 | 打卡指令（舰长/粉丝牌）、连续天数、落库 | `handle_incoming_danmu` 第 2 节（`!is_lite`）+ `record_checkin` / `get_checkin_profile` 守卫 | 横幅提示 + 按钮禁用 | ❌ 停用 |
| 打卡 | 关键词学习 + AI 个性化回复 | 同第 2 节（`!is_lite`） | 置灰 | ❌ 停用 |
| 补签 | 补签/查询指令、补签卡结算 | 第 2 节（`!is_lite`）+ `get_retroactive_cards` / `execute_retroactive_checkin` 守卫 | 横幅提示 + 按钮禁用 | ❌ 停用 |
| GM 运维 | 批量补签、用户搜索、手动发卡、记录导出 | `gm_batch_checkin` / `gm_search_users` / `gm_grant_card` / `gm_export_checkin_records` 守卫 | 横幅提示 + 按钮禁用 | ❌ 停用 |
| AI 思考 | `ask_ai_thinking` | `ask_ai_thinking` 守卫 | 横幅提示 + 按钮禁用 | ❌ 停用 |
| 调试通道 | `simulate_danmu` / `simulate_gift` / `simulate_live_event` | 经 `handle_incoming_*` 管道自然拦截（静默） | 调试入口 | ⚪ 静默空转 |
| 调试通道 | `simulate_like` | `simulate_like` 守卫（显式报错） | 调试入口 | ❌ 停用 |

## 三、新增功能规则（AGENTS.md）

1. **默认不支持 Lite**：任何新增的非排队功能必须在该命令入口第一行调用
   `ensure_not_lite(&state, "模块名")?`，模块名用于生成前端提示文案。
2. 事件管道类新增功能（无返回值的 `handle_incoming_*`）：在函数首部读取 `is_lite_mode`，
   Lite 下直接 `return`（对齐原工程 `#if !ONLY_ORDER_MONSTER` 的编译期排除）。
3. 前端：控件加 `disabled={isLite}` 与 `opacity-40`；视图级加琥珀色横幅提示。
4. 单测：新模块须加入 `test_ensure_not_lite_guard_blocks_non_lite_modules` 的断言列表。

## 四、验证

- `cargo test`：`test_ensure_not_lite_guard_blocks_non_lite_modules`（守卫文案与放行/拒绝矩阵）、
  `test_lite_mode_disables_non_queue_modules`（Lite 下排队仍可用）。
- 手工：界面左下角切换 Lite 开关 → 各视图控件禁用 + 后端命令返回 `Lite模式下XX已停用`；
  切换后无需重启（`set_lite_mode` 即时落盘并对所有守卫生效）。
- 有意差异：原工程为编译期宏（需重新编译出 Lite 版），V2 为运行时开关（同一二进制内切换），
  行为边界逐项对齐（见矩阵）。
