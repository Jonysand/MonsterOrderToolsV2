# MonsterOrderWilds-Ascendance 系统架构设计说明书

本文档描述 `MonsterOrderWilds-Ascendance` 的总体架构组织、进程间通信模型、窗口机制及业务逻辑设计。

---

## 一、 系统架构组织图

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                            前端视图渲染层 (Renderer)                         │
│                                                                             │
│   ┌─────────────────────────┐  ┌─────────────────────────┐                   │
│   │  MainWindow (控制台)    │  │ OverlayWindow (悬浮窗)  │                   │
│   │  - 直播间长连配置       │  │ - 半透明置顶毛玻璃      │                   │
│   │  - 完整点单管理         │  │ - 自由拖拽 (drag region)│                   │
│   │  - Lite 模式切换开关    │  │ - 保序删除              │                   │
│   └───────────┬─────────────┘  └───────────┬─────────────┘                   │
│               │                            │                                │
│               └────────────────────────────┘                                │
│                                            │                                │
│                         Tauri IPC (invoke & event)                          │
└────────────────────────────────────────────┼────────────────────────────────┘
                                             │
┌────────────────────────────────────────────┼────────────────────────────────┐
│                          原生宿主内核层 (Rust Host)                          │
│                                            │                                │
│   ┌────────────────────────────────────────┴────────────────────────────┐   │
│   │                     Command Handler (路由与指令分发)                │   │
│   │  - get_queue / add_order / dequeue_by_user_id                       │   │
│   │  - get_lite_mode / set_lite_mode / clear_queue                      │   │
│   └────────────────────────────────────────┬────────────────────────────┘   │
│                                            │                                │
│   ┌────────────────────────────────────────┴────────────────────────────┐   │
│   │                  AppState (全局线程安全业务状态管理器)              │   │
│   │  - queue: Mutex<Vec<QueueItem>>                                     │   │
│   │  - is_lite_mode: Mutex<bool>                                        │   │
│   └───────────────────┬───────────────────────────────────┬─────────────┘   │
│                       │                                   │                 │
│         ┌─────────────┴─────────────┐       ┌─────────────┴─────────────┐   │
│         │   点怪排队内核 (保序算法)   │       │   Lite 模式冻结管理器     │   │
│         │   - 提权置前逻辑          │       │   - 冻结 TTS / AI / 打卡  │   │
│         │   - 保序精确移除          │       │   - 仅保留核心点怪排队    │   │
│         └───────────────────────────┘       └───────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 二、 核心通信与窗口架构

### 1. 多窗口配置与能力边界
在 `src-tauri/tauri.conf.json` 中明确声明双窗口：
* **`main` 窗口**：主控制台窗口，默认尺寸 1000×700，允许缩放，提供完整的房间管理和参数配置。
* **`overlay` 窗口**：
  - 属性：`transparent: true, decorations: false, alwaysOnTop: true, shadow: false`。
  - 路由：`index.html#/overlay`。
  - 交互：配置 `data-tauri-drag-region` 实现按住标题栏任意拖动，消除 Win32 `WM_NCHITTEST` 的复杂逻辑。
* **OBS 推流集成**：
  - 桌面点怪悬浮窗为标准置顶透明窗口，在 OBS 来源中使用【窗口捕获】选择该窗口即可推流，无需额外的本地 HTTP 服务或浏览器源。

### 2. 前后端数据协议 (Tauri Commands)
* `get_queue()`: 返回当前 `Vec<QueueItem>`。
* `add_order(user_id, user_name, monster_name, is_priority)`: 新增点单或对已在队用户提权。
* `dequeue_by_user_id(user_id)`: 按用户唯一 ID 精确删除条目，**严格保序**。
* `clear_queue()`: 清空队列。
* `get_lite_mode()` / `set_lite_mode(enabled)`: 查询与设置 Lite 模式状态。
* `get_bili_connection_state()`: 返回长连五态快照 `{state, reason, reason_text, attempt, display}`（D7）。
* `get_id_code()` / `save_id_code(id_code)`: 开播身份码读取（注册表优先，供前端输入框以密码形态回显）与保存（仅注册表，不落 JSON）。
* `set_overlay_locked(locked)` / `get_overlay_locked()`: 悬浮窗鼠标穿透 + 置顶（D2，运行时状态不持久化）。
* `save_overlay_position(x, y)`: 悬浮窗拖动位置防抖落盘（→ `top_pos_x/y`，D2）。
* `get_current_tts_engine()`: 当前实际引擎名 `manbo / xiaomi / sapi`（D1）。
* `save_manbo_api_key(key)`: Manbo Key 仅写注册表，不落 JSON、不回传（D1）。
* `get_manbo_voice_list()`: 185 项音色列表（D1）。
* `get_recent_logs(limit, minLevel)` / `clear_recent_logs()`: 运行日志内存环读取/清空（D5）。
* `end_app()`: 退出程序（E3）。与主窗口 `CloseRequested` 共用 `shutdown_app()` 清理链路：
  待写悬浮窗位置落盘（等价原工程 `WriteQueue::Flush`）→ 停止长连（`set_running(false)`，等价 `Disconnect/Destroy`）
  → 记录退出日志 → `app.exit(0)`。队列与配置本就变更即时落盘，退出仅做兜底；
  **有意差异**：不在退出路径阻塞调用 B 站下播接口，避免网络等待拖慢退出。

**事件（后端 → 前端）**

| 事件 | 载荷 | 消费方 |
| --- | --- | --- |
| `queue-updated` | `Vec<QueueItem>` | 主窗口 + 悬浮窗队列列表 |
| `order-placed` | `{user_id, user_name, monster_name, is_priority}` | 悬浮窗跑马灯（D3） |
| `checkin-recorded` | `UserProfile` | 主窗口「舰长打卡 & GM」页的「打卡动态」卡片 |
| `checkin-reply` | `{user_id, user_name, reply, is_ai}` | 悬浮窗打卡气泡 |
| `retroactive-checkin-recorded` / `retroactive-query` | `{user_id, user_name, reply, …}` | 悬浮窗补签气泡 |
| `like-reward-granted` | `{uid, user_name, likes, daily_total, replies}` | 悬浮窗奖卡气泡 |
| `gift-received` | `GiftEvent` | 悬浮窗礼物气泡 |
| `super-chat-received` / `guard-received` / `room-enter-received` | `LiveEvent`（外部标签枚举，如 `{"SuperChat": {…}}`） | 悬浮窗 SC/上舰气泡 |
| `config-changed` | 脱敏 `AppConfig` | 悬浮窗跑马灯/透明度热更新 |
| `overlay-lock-changed` | `bool` | 主窗口 + 悬浮窗锁定态同步（D2） |
| `connection-state-changed` | `ConnectionStatusPayload` | 主窗口直播连接面板（D7） |
| `resource-missing` | `string`（相对路径） | 主窗口资源缺失告警（D5） |

---

## 三、 业务保序算法与 Lite 模式设计

### 1. 保序删除算法 (Preserved-Order Deletion)
为防止条目删除后重排引发用户困惑：
* 删除操作直接定位用户所在索引（`position`）后执行 `remove(pos)`，不触发全量重排序；
* 新增元素遵循：优先元素插入在最后一个优先元素之后、首个普通元素之前；普通元素直接追加在末尾；
* 核心单元测试：`test_dequeue_by_user_id_preserves_order` 确保无论删除首项、中项还是尾项，剩余条目的前后相对位置均恒定不变。

### 2. Lite 模式 (ONLY_ORDER_MONSTER=1)
* **顶层开关**：系统维护全局 `is_lite_mode` 标识（运行时开关，替代原工程编译期宏 + C# 运行时判断）。
* **前端响应**：当切换为 Lite 模式时，界面上的“舰长周打卡”、“TTS 语音播报”等卡片自动进入冻结/禁用状态，页面标题旁显示琥珀色“Lite 纯排队模式已启用”徽章（注：原工程 Lite 下仅隐藏对应 Tab、不修改窗口标题；V2 以页内徽章指示，不调用 `setTitle`）。
* **后端响应（E1 统一守卫）**：非排队功能的命令入口第一行调用 `ensure_not_lite(&state, "模块名")?`（Lite 下返回 `Lite模式下XX已停用`）；
  事件管道类（`handle_incoming_danmu` / `_like` / `_gift` / `_live_event`）在函数首部判定 `is_lite_mode` 直接 return。
  Lite 下仅保留：点怪排队、悬浮窗、B 站长连、身份码、配置、日志落盘。
* **逐功能覆盖矩阵**：见 `docs/LITE_COVERAGE_MATRIX.md`（含新增功能必须显式声明是否支持 Lite 的规则）。

---

## 四、 资源与持久化策略（统一由 `src-tauri/src/paths.rs` 解析）

### 1. 可写数据目录 `config_dir()`（`MonsterOrderWilds_configs`）
解析顺序（存在即用）：
1. `cwd/MonsterOrderWilds_configs`（绿色版双击启动 / 仓库根目录运行）；
2. `cwd/../MonsterOrderWilds_configs`（`tauri dev` / `cargo test` 时 cwd = `src-tauri`）；
3. `exe 同级/MonsterOrderWilds_configs`（安装版随包资源目录，用户可编辑）；
4. 兜底：创建 exe 同级目录。

存放：`configs.json`（V2 配置）、`MainConfig.cfg`（原工程旧配置迁移源）、`captain_profiles.db`（历史打卡库，格式与原工程完全一致）、`order_list.json` / `OrderList.list`、`credentials.dat`（加密凭据）。

### 2. 资源文件 `find_resource(rel)`
顺序：`config_dir()/rel` → `resource_root()/MonsterOrderWilds_configs/rel` → `resource_root()/rel` → cwd 及仓库根回退。
其中 `resource_root()` 由 `setup()` 注入 Tauri `resource_dir()`，未注入时回退 exe 同级。

### 3. 随包分发（`tauri.conf.json` → `bundle.resources`）
* `MonsterOrderWilds_configs/monster_list.json`：怪物别名/图标数据（点怪匹配核心数据）；
* `MonsterOrderWilds_configs/voices/`：特殊音效（`manbo.mp3`、`duang.mp3` 等，散装目录回退）；
* `MonsterOrderWilds_configs/local_voices.zip`：本地语音包（zip 内存解压读取，与原工程一致）；
* `MonsterOrderWilds_configs/dict/stop_words.utf8`：分词停用词表（缺失时回退内置停用词）；
* `MonsterOrderWilds_configs/dict/user.dict.utf8`：用户自定义词典（主播黑话，支持「词语 [词频] [词性]」，词频缺省 10）；
* 首次运行缺失时由 `ensure_seeded()` 从资源根复制到可写数据目录；启动时若关键资源仍缺失，输出日志并广播 `resource-missing` 事件。

### 4. TTS 音频留档
* 目录：`config_dir()/TempAudio/YYYYMMDD/`（文件名 `{用户名_正文前5字}_{毫秒}.mp3`）；
* 启动时按 `tts_cache_days_to_keep` 清理过期日期目录（对齐原工程 `TTSCacheManager`）。

### 5. 打卡事件与关键词学习状态
* `user_profiles` 承载打卡与学习两类数据：打卡列 `last_checkin_date / continuous_days / cumulative_days`；学习列 `last_danmu_timestamp / keywords_json / danmu_history_json`（JSON 格式与原工程逐字一致，旧库可直接反序列化）；
* 打卡链路**只写打卡列**（不覆写 `last_danmu_timestamp`），学习链路**只写学习列**，避免互相污染；
* 打卡日期口径为弹幕服务器时间（`bilibili::server_date(ts)` = `localtime(serverTimestamp)`），时间戳缺失回退本机今天。

---

## 五、 多引擎 TTS 播放链路（`src-tauri/src/tts.rs`）

```
业务管道（lib.rs）
  handle_incoming_danmu ─┐
  handle_incoming_gift  ─┼─► TTSManager.enqueue_speak(text, uid, priority)
  handle_incoming_event ─┘         │  优先队列（礼物/SC/上舰/打卡/点餐）
                                   │  普通队列（"{uname} 说：{msg}" 朗读）
                                   ▼
                     播报泵（150ms 逐条出队，对齐原工程 Tick 语义）
                                   ▼
          speak_text(text, uid)  ├─► 本地语音包命中（zip 优先）→ 直接播放
                                 ├─► 特殊用户专属引擎 /apis/mbAIsc（3 次失败/30s 熔断）
                                 ├─► Manbo（曼波音色走 /apis/mbAIscvip，speed=rate×5）
                                 ├─► MiMo（小米开放平台，<style> 标签拼接）
                                 └─► Windows SAPI（中文音色 + SSML rate/volume/pitch）
                                   ▼
              AudioQueue（单播放线程 + mpsc，全部音频串行，防叠音）
                                   ▼
        rodio 播放（60s 超时保护） / SAPI 子进程（30s 超时强杀）
```

* **Lite 模式**：管道层拦截，不产生任何 TTS 队列任务与前端事件。

---

## 六、 打卡 / 补签 / AI 学习链路（`src-tauri/src/checkin.rs` + `checkin_ai.rs`）

```
弹幕到达（handle_incoming_danmu）
  │
  ├─ 舰长弹幕学习（guardLevel != 0 || hasMedal，且 enable_captain_checkin_ai 开启）
  │     CheckinLearner.learn（仅舰长；5s 窗口节流；历史上限 100）
  │        └─ jieba 分词（内嵌主词典 + user.dict.utf8 + HMM）
  │             → 过滤（字节长度 ≥2 / 停用词 / #标签# 词）→ 词频统计（上限 50，按频次降序）
  │        └─ 落库 user_profiles.{keywords_json, danmu_history_json, last_danmu_timestamp}
  │
  ├─ 同内容防刷屏（连续 3 条相同内容 → 跳过指令处理，但仍按普通弹幕朗读）
  │
  ├─ 打卡指令（trigger_words 配置 + 内置「打卡/签到」；舰长或佩戴粉丝牌）
  │     重复打卡 → "{name}今日已打卡，连续X天，累计Y天"（气泡 + 播报）
  │     首次打卡 → 即时落库 → 舰长且已配置 DeepSeek Key 时异步生成 AI 回复
  │                 ├─ 成功 → AI 文案（checkin-reply 事件 + 高优先播报）
  │                 └─ 失败/非舰长 → 兜底 "{name}连续第N天打卡！累计M天"
  │
  ├─ 补签指令（补签 / 补签卡）→ retro_command_outcome：
  │     无卡档案「系统错误」→ 卡数 0「你没有补签卡哦~」→ 满勤「无需补签哦~」
  │     → 无缺失日期「当前没有需要补签的日期。」→ 成功（扣卡+插记录+重算连续）
  │     → 失败「补签失败，请稍后再试。」
  │
  └─ 补签查询（补签查询 / 补签卡查询 / 查询补签 / 查询补签卡 / 我的补签卡）
        仅气泡（不朗读）：补签卡 N 张 + 连续点赞进度 + 每周点赞 30 进度

点赞事件（handle_incoming_like）
  → 与弹幕共用 msg_id 去重缓存 → add_likes 落库
  → 命中奖卡："{name}，恭喜！今日点赞突破30，获得1张补签卡！" / "…连续7天点赞…"
     （like-reward-granted 事件 + 高优先播报）
```

* **Lite 模式**：以上链路整体停用（打卡、学习、补签、查询、点赞奖卡均不处理）。

---

## 七、 长连状态机与运行日志（`bilibili.rs` + `logging.rs`）

### 1. 五态连接状态机（D7）

```
run_bili_live_loop
  ├─ 进入 → Connecting
  ├─ start_app
  │    ├─ 失败(网络类) → Reconnecting(NetworkError, N) → 指数退避重试（1s → 30s）
  │    └─ 失败(鉴权类) → ReconnectFailed(AuthFailed) → 停止重试（有意差异）
  ├─ WebSocket 连接成功 → Connected
  ├─ 心跳发送失败 → Reconnecting(HeartbeatTimeout, N)
  ├─ 服务端关闭帧 / INTERACTION_END → Reconnecting(ServerClose, N)
  ├─ 接收错误 → Reconnecting(NetworkError, N)
  └─ 用户断开 → end_app → Disconnected
```

* 状态载荷：`{state, reason, reason_text, attempt, display}`；`display` 已含「正在重连...(第N次)」「重连失败，原因: 鉴权失败」文案。
* 前端映射：侧栏状态点（绿 / 琥珀脉冲 / 红 / 灰）+ 按钮文案（断开连接 / 取消连接 / 开启直播连接）。

### 2. 运行日志（D5）

```
log_info!/log_warn!/log_error!/log_debug!
  ├─ 内存环（上限 500 条）── get_recent_logs / clear_recent_logs（IPC 保留，主窗口日志视图已移除）
  └─ Logs/YYYY-MM-DD.txt（UTF-8 BOM，行格式 [时间]:[LEVEL] 消息）
         └─ 仅非测试构建落盘；Debug 级别仅调试构建输出（对齐 Release 下 LOG_DEBUG 空宏）
```

* 全仓无裸 `eprintln!`；资源缺失额外广播 `resource-missing`，主窗口以即时 Toast 告警。
* 日志目录位于可写数据目录（安装版 exe 目录可能只读，属有意差异）。

### 3. 悬浮窗交互（`OverlayWindow.tsx`）

```
跑马灯（D3）
  默认文本：循环滚动（animate-marquee，浅黄）
  业务消息：pushMarquee → 空闲即播（10s 单次滚动，黄色）/ 忙则排队（+N 待播计数）
              └─ 结束：onAnimationEnd 或「时长+1s」兜底定时器（播放令牌去重）→ 取队首 / 回默认

气泡（D4）
  业务事件 → 堆叠气泡（上限 5 条，超出移除最旧，15s 自动退场，新消息置顶）

列表（D6/D8）
  虚拟化（VirtualList：仅渲染可视区 + overscan）
  长文本往返滚动（MarqueeText：超出容器时 marquee-x，悬停暂停）
  背景透明度：rgba(3,7,18, 锁定 ? penetrating_mode_opacity : opacity) —— 仅背景，文字不透明
```
