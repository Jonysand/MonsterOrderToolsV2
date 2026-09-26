# 点怪、签到、打卡、补签与日志功能复核报告

> 复核时间：2026-09-25。当前仓库 HEAD：`cb0e2c56418b5f12228ea4922de5759900db5ee2`。上一轮代码审查基线为 `221cd86e212e744fbd6d671bbabaafe69cac7b72`；两者之间仅修改 `scripts/check_orphan_fields.py` 的 Python 2 兼容逻辑，本文所查业务源码没有变化。
> 迁移前对照：公开仓库 `Jonysand/MonsterOrderTools` 的固定提交 `40bca8c9af3ad68c1f8c4acd77c96ac725c826be`。本机不存在 AGENTS.md 指定的旧工程本地路径。`D:\publish_v2` 含旧版 `MonsterOrderWilds.exe`、WPF DLL 与历史文件，**不是 V2 运行包**。
> 这是只读复核，**未修改或修复业务功能**。对应的完整代码级修复设计在 `docs/CORE_FLOW_REMEDIATION_PLAN_2026-09-25.md`。两份文件各自自洽；修复设计不是已实施结果。

## 1. 问题定义、数据流与证据等级

直播弹幕、主播拖拽、500ms 队列保存任务、数据库写入、两个 WebView 会同时作用于同一业务状态。正确性的基本条件是：**不能让陈旧前端覆盖新订单，不能在写盘失败时谎报成功，不能把数据库错误变成内存中的“成功打卡”，不能让语音开关意外改变旧版的文字留档。**

```text
B 站 WebSocket → bilibili.rs 解析 → lib.rs::handle_incoming_danmu
                                     ├─ checkin_ai.rs → checkin.rs → SQLite 档案／明细／卡片
                                     ├─ bilibili.rs::process_danmu → QueueManager → order_list.json
                                     ├─ TTSManager（完整版） → 播报泵
                                     └─ logging.rs → Logs／History
主窗口／悬浮窗 ─Tauri IPC→ add_order、reorder_queue、GM、凭据导入
                              └─ 后端事件 → 两个 WebView
```

`QueueManager` 在 `Arc<Mutex<_>>` 下处理内存变更，磁盘写在锁外；`CheckinManager` 以 `Mutex<Connection>` 串行 SQLite 调用，但**单连接锁不等于 SQL 事务**；`MonsterRoster` 的 `RwLock` 当前没有包住整段“读、计算、写盘、提交”。这三条是判定下列交错的关键。

- **源码证明**：当前实现与固定版本旧源码对照，给出最短可达调用链或线程交错。不是新版 GUI 实测。
- **故障注入 SQL 复演**：D02／D04／D05 只在空的 `sqlite3.connect(':memory:')` 里创建虚构用户和 `RAISE(ABORT)` trigger，再按 Rust 使用的 SQL 顺序复演；**没有运行 Rust 函数**。
- **正常 SQL 链复演**：曾以 SQLite `mode=ro` 打开旧库，并通过 backup API 把完整旧库复制到**内存**，在该内存快照新增虚构 UID 演算正常打卡→奖卡→补签。内存快照包含真实用户明细，但全程未查询或展示个人行、未写到磁盘；这与故障注入用的空白库是不同证据。该操作可能更新了原库的 `.db-shm` 元数据，详见第 6 节。
- **旧包有限核验**：只输出了旧版包体、怪物公共词库和数据库的表结构／聚合总数，未展示密钥或个人明细；不能说“完全没有读到用户数据”，因为内存 backup 确曾加载原库全部页。

`P1` 为丢单、错单、资产／记录失真或关键审计缺失；`P2` 为条件性体验错误或较窄的状态不一致；`P3` 为低影响错误。标记“故障注入”的问题不能写成日常操作必现。对尚未证明生产线程可并发的路径，明确标注为“API 层并发风险”。

## 2. P1：点怪、持久化和名单

### Q01｜拖拽旧快照删除直播新单、复活完成单

`src/views/MainWindow.tsx` 第403—418行以组件里的 `queue` 构造完整 `QueueItem[]`；悬浮窗 `src/views/OverlayWindow.tsx` 第507—519行也发送整表。本地成员检查只比对前端缓存，不比对 Rust 当前队列。`src-tauri/src/lib.rs` 第287—299行把该快照直接交给 `src-tauri/src/queue.rs` 第175—179行 `reorder` 整体替换，并重建 user_id 索引。

最短交错：后端 `[A,B]`；前端准备 `[B,A]`；弹幕让后端变为 `[A,B,C]`；旧重排到达，后端变 `[B,A]`。**C 在内存和立即保存的文件中消失**。若中间操作改为“完成 B”，旧快照会让 B 复活；改为“A 优先置前”，旧对象 `A(is_priority=false)` 会撤回提权。旧 WPF 在 [OrderedMonsterWindow.xaml.cs 第421—446行](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/JonysandMHDanmuTools/OrderedMonsterWindow.xaml.cs#L421-L446) 是移动单个对象，而非提交整表副本。当前两个前端都有轮询与事件，旧响应晚于新事件还可扩大旧快照窗口。

### Q02｜后台快照与命令立即落盘倒序；失败后丢脏标记

`src-tauri/src/lib.rs` 第167—199行的 `flush_queue` 在队列锁内序列化后**先**置 `dirty=false`，随后解锁写文件。500ms 后台任务第2058—2068行与完成、清空、重排命令的 `save_queue_now(force=true)` 可在执行上交错。

例：后台取旧 `[A,B]` 并暂停；主播完成 B 并强制保存 `[A]`；后台最后把旧 `[A,B]` 写回。内存 `[A]`、文件 `[A,B]`、`dirty=false`；下一个定时 tick 不会纠正。异常退出再打开时 B 复活；**正常退出**第1878—1879行再强制刷盘可能补救，所以不称“每次正常退出必丢”。如果写盘直接失败，也因过早清脏而不会自动重试。旧 [PriorityQueueManager.cpp 第237—304行](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/PriorityQueueManager.cpp#L237-L304) 只在成功后清脏；旧版也可能有锁外快照竞态，不能声称这一类风险全部首次由 V2 引入。

### Q03｜怪物原名碰撞会覆盖词库

`src/views/MonsterListTab.tsx` 第156—196行允许“新增自定义怪物”填写已有“黑龙”；`src-tauri/src/lib.rs` 第342—355行直接 `raw.insert(name, config)`，`src-tauri/src/monster.rs` 第180—195行随即写盘热重载。原黑龙的 6 个别称和图标可被空草稿取代；改原名为另一已有名还会删除一个条目。`docs/design/monster_list_editor_spec.md` 第44行另要求改原名二次确认，当前未做。编辑器是 V2 新能力，此问题不归因于旧工程。

### Q04｜手动选怪重跑别名匹配，可点错怪且绕禁点

通过正常编辑给“雌火龙”添加别称“黑龙”，再把雌火龙列入禁点名单。选怪面板展示未禁点的原名“黑龙”，但 `src-tauri/src/lib.rs` 第223—245行重新按别名匹配该字符串，可命中排在前面的雌火龙。**手动 `add_order` 未验证 `roster.is_blocked(final_monster)`，因此错误怪物可入队**，前端第363行仍提示“黑龙已入队”。弹幕路径 `src-tauri/src/bilibili.rs` 第857—866行却会拦截。出厂词库没有“原名被其他怪别称抢先命中”的现成样本；该用例要求先经合法编辑产生冲突，不能写成开箱即错。

## 3. P1：打卡、补签、奖卡、凭据与 Lite

| 编号 | 触发与结果 | 当前证据及旧版对照 |
|---|---|---|
| D01 | **数据库打开、建表或迁移失败**：启动无告警切成内存库；打卡与卡片界面上成功，重启后消失。属故障条件。 | `src-tauri/src/lib.rs` 第88—91行 `unwrap_or_else(new_in_memory)`；旧 [ProfileManager.cpp 第86—120行](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/ProfileManager.cpp#L86-L120) 记录错误并返回失败。 |
| D02 | **打卡明细 INSERT 出错**仍建立 profile 并回复成功。纯内存 SQL trigger 复演：记录 0 条，档案 `continuous_days=1, cumulative_days=0`。仅故障注入稳定复现。 | `src-tauri/src/checkin.rs` 第365—403行吞掉执行结果，`lib.rs` 第861—896行返回成功；旧 [ProfileManager.cpp 第608—671行](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/ProfileManager.cpp#L608-L671) 会传播插入失败。 |
| D03 | 同一用户有多张卡和多个缺日，连续发**不同 `msg_id`** 的三条“补签”：第 3 条被打卡防刷屏跳过；语音关闭时无提示，打开时可能只朗读原文。普通用户可达。 | `src-tauri/src/checkin_ai.rs` 第203—225行阈值 3；`lib.rs` 第821—969行拿该值阻断整个补签分支；旧版 [DanmuProcessor.cpp 第413—423行](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/DanmuProcessor.cpp#L413-L423) 独立通知补签模块。 |
| D04 | **GM 批量补签的 INSERT／UPDATE 错误**被忽略，返回成功、统计新增记录，但实际明细缺失、档案可能已改。纯内存故障复演为“报告 1 条、实际 0 条”。 | `src-tauri/src/checkin.rs` 第1153—1177行；旧 [ProfileManager.cpp 第1732—1848行](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/ProfileManager.cpp#L1732-L1848) 遇 SQL 错回滚。仅故障注入稳定复现。 |
| D05 | 第 7 天点赞发卡成功，**连赞领取标记 UPSERT 失败**，再收到另一条不同 `msg_id` 的赞；可再发一张卡。纯内存故障复演两次后卡数 2、连赞仍 6 天、领取标记 0。 | `src-tauri/src/checkin.rs` 第571—597行先加卡、后存标记且无事务，`lib.rs` 第1091—1093行静默吞错；旧 [ProfileManager.cpp 第1222—1300行](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/ProfileManager.cpp#L1222-L1300) 同一事务写标记和卡数。**若仅每日点赞插入失败，标记已保存，不会重复发卡**。 |
| K01 | 启动时已有凭据 A，导入文件 B 后，连接、重连与 2.5 秒状态轮询仍读取 A；B 只更新配置镜像／AI／TTS。首次无凭据导入 B 时可用 B 建连，但状态仍误报未加载。 | `src-tauri/src/lib.rs` 第37—50、63—69、1215—1229、1488—1517、1907—1911行：`Arc<Credentials>` 启动后从未更换，连接优先其非空字段。若已连接，后台持有旧 A 的**值快照**，替换共享状态也不能自动更新旧会话。 |
| L01 | 完整版默认 `enable_voice=false` 时普通弹幕、打卡与补签回复等文字 History 缺失；补签查询只 emit，不记录回复。 | `src-tauri/src/lib.rs` 第946—967、1035—1061、2041—2051行仅在 TTS 出队处记历史；旧完整版 [TextToSpeech.cpp 第181—216行](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/TextToSpeech.cpp#L181-L216) 在语音过滤前单独留原弹幕，补签查询回复也在 `SendReply(..., false)` 中留档。**Lite 不留这些历史与旧版一致**。 |
| L02 | HTTP 200 的心跳为 `{}`、`{"code":"bad"}`、或 `code` 超出 `i32` 时可能被误判为成功，隐藏会话失效。 | `src-tauri/src/bilibili.rs` 第199—203行 `unwrap_or(0) as i32`，第1260—1278行把 0 视正常；旧 [BliveManager.cpp 第410—459行](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/BliveManager.cpp#L410-L459) 缺 `code` 默认为 -1。未调用真实心跳。 |
| M01 | Lite 构建**只启动**，目录有缺列的历史打卡库时也会建表／执行 `ALTER TABLE`。 | `src-tauri/src/lib.rs` 第88—91行无条件创建 manager，`checkin.rs` 第224—305行初始化库；旧 [DataBridgeExports.cpp 第479—503行](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/DataBridgeExports.cpp#L479-L503) 在 `#if !ONLY_ORDER_MONSTER` 内才初始化。该条是源码推导，**没有**让 Lite 打开真实库。 |

## 4. P2／P3：其他确定问题及适用范围

| 编号 | 等级 | 内容及证据 |
|---|---|---|
| D06 | P2 | 新安装先在 `checkin.db` 积累打卡／卡数，之后把旧 `captain_profiles.db` 放到同目录并重启，`checkin.rs` 第212—222行突然优先改读旧库。先前数据仍留在 `checkin.db`，但从界面消失，**不是物理删除**；旧版恒定使用 `captain_profiles.db`。 |
| D07 | P2 | 新用户卡片行不存在时“补签”和“我的补签卡”回复“系统错误”。`checkin.rs` 第679—713、780—802行将无行视故障；旧 [ProfileManager.cpp 第1006—1034、1057—1086行](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/ProfileManager.cpp#L1057-L1086) 无行会置零并返回成功。当前 `checkin.rs` 第1873—1877、1908—1911行测试还固化了错误答案。普通新用户可达。 |
| Q05 | P2／API 线程风险 | `roster.rs` 第143—148行先写盘后拿 `RwLock` 写锁。若**两个 Rust 线程**同时调用 `replace`，A 写 `[甲]` 后暂停，B 写盘并提交 `[乙]`，A 再提交 `[甲]`，则磁盘乙、内存甲，双方 `Ok`；共用 `.tmp` 还可能错写。`add_all/remove/rename_item` 第193—229行的读－改－写亦非原子。**当前默认同步 Tauri 命令的后端并行执行尚未实机确认**，此条不能宣称正常 UI 两次同步 IPC 一定可交错；库 API 的直接线程调用有明确风险。 |
| Q06 | P2 | `src/views/MonsterListTab.tsx` 第230—243、624—636行只检测**修改前已冲突**的词。给“黑龙”新增仅属“雌火龙”的别称“太太”时不提示，保存后造成冲突；规格第147—150行要求草稿期间提示真实命中者。 |
| Q07 | P2 | `src/components/MonsterPickerPanel.tsx` 第43—78行先选“黑龙”，再筛选不含黑龙的作品或关键词，确认卡仍可提交黑龙；选中项后被禁点时，又静默换成首个可点怪。两种情况都会使实际提交目标与当下可见选择不一致。 |
| Q08 | P2 | 主界面 `MainWindow.tsx` 第393—418行按旧 `draggedIndex` 取新数组元素；若拖 B 时优先单插入队首，放开时可能实际搬动 A。此条是不丢单的**误排**，独立于 Q01。 |
| Q09 | P2 | `MainWindow.tsx` 第278—303行取消的只是尚未触发的名单定时器；已经发出的旧整表保存可能晚于后续字典改名／名单保存执行。`lib.rs` 第409—413行整表命令没有 revision／CAS，前端失败时又直接回滚旧快照，可能覆盖新名单。这一问题即使后端同步命令顺序执行也可因**前端旧快照**发生；当前主要入口是主窗口与后端改名／删除，不声称已有两个名单编辑窗口。 |
| L03 | P2 | `bilibili.rs` 第232—234行 `end_app` 忽略网络结果永远 `Ok`；第1164—1173行可假记“重连前已关闭上一场”。主动断开时 `lib.rs` 第1325—1360行和后台第1406—1412行还可能向同一 game_id 双发 end。 |
| L04 | P2 | setup 第1958—1968行只 emit 一次 `resource-missing`；前端 `MainWindow.tsx` 第228—231行在挂载后才 listen，没有重放或快照补偿。若 emit 先于 listener，唯一 Toast 丢失。**与窗口加载时序有关，不能称每次启动必丢。** |
| L05 | P2 | 正常连接成功、收到普通直播事件时 `lib.rs` 第633—645行及 `bilibili.rs` 第1223—1228、1316—1355行无 INFO 级状态／事件类型锚点，Release 日志难分“未收到”与“已过滤”。旧版记录状态及整包；不应照搬整包以免泄露个人内容。 |
| Q10 | P3 | 撤销完成时，同一用户已再次入队：`queue.rs` 第185—193行拒绝恢复，`lib.rs` 第310—317行却忽略布尔结果，`OverlayWindow.tsx` 第701—715行还消耗撤销入口。队列不产生重复，但 UI 无失败反馈。 |
| L06 | P3 | `logging.rs` 第143—164行先将日志入内存环、释放锁后另拿文件写锁，两线程可令内存与文件的事件顺序不同；当前无并发测试。其文件行尾为 LF，旧 Windows CRT 产物是 CRLF；行头与 BOM 语义对齐，只有按字节解析 CRLF 的下游需要兼容。 |

## 5. 旧问题、待产品确认的行为

- **I01，旧问题继承，不算迁移回归；修复方案 D5 一并处理（需用户核准新增 V2 去重表）：**相同 `msg_id` 的“补签”弹幕重投，在 `bilibili.rs` 第808—814行点怪去重前，已于 `lib.rs` 第912—943行处理并 return；如果有两张卡、两个缺日，重投可扣两次。旧补签入口也没有 DM ID 去重。修复不能简单在 `handle_incoming_danmu` 顶部调用现有缓存，否则第一次点怪在 `process_danmu` 又被判重。
- 旧版及新版周奖卡均只比较 `weekly_first_claimed != week_start`；上周迟到赞可能使标记回退，但没有 B 站实际乱序证据。旧日期打卡覆写 `last_checkin_date` 亦为旧问题，不作为本轮迁移回归。
- **待确认：**手动拖拽后新弹幕入队又触发 `queue.rs` 第114—119、196—199行全量排序，是否允许抹去主播人工顺序；悬浮窗当前整行点击即完成，旧工程亦如此，但历史审计记录称用户确认改为“显式完成按钮”；TTS 积压超过 15 秒时气泡可在出声前消失，是故意“即时气泡”还是应与旧版播音回调同步。完整方案给出默认建议，不在未确认前把这些取舍称为修复完成。

## 6. 已核对正确点、测试结果与安全边界

- 怪物表：旧包和项目各 175 个条目，原名／别称／默认历战等级一致；当前词库有 42 个唯一历战别称，175 个非空图标路径均存在于前端静态资源。当前源码／既有测试覆盖历战专名优先、剥离回退，不等于本轮 `cargo test` 已执行。
- 排队比较器与保序删除、两段式舰长提权、正常弹幕禁点、Lite 下仍可点怪均与旧版和设计核对。签到默认 `打卡,签到`、自定义中英文逗号分割与粉丝牌权限正常；自然周周一、当日累计 30 点赞周内一张、单笔补签事务写入等核心算法均已定位。
- 日志级别、日期文件名、UTF-8 BOM、主要行头、进场 History 与 Release 不输出 Debug 的源码路径存在。原主窗口日志视图在当前代码中已移除，`get_recent_logs` 仅保留 IPC；不应拿早期 UI 报告当现状。

| 检查 | 本轮结果及其界限 |
|---|---|
| `npm run build`、`npm run build:lite` | `[PASS]`，上一轮在 `221cd86` 运行；当前 HEAD 仅改过配置字段检查脚本。仅证明前端类型与打包，不证明 Rust 业务正确。 |
| `npm run check:encoding` | `[PASS]`，上一轮存量文件通过；本文和修复方案落盘后须重新检查。 |
| `npm run check:fields` | `[PASS]`，本轮当前 HEAD 运行，列出 30 个配置字段的代码引用；脚本即使发现 `ORPHAN/WEAK` 也只打印、不以非零退出，**不是可靠发布门禁**，更不能验证凭据热替换。 |
| 内存 SQLite SQL 复演 | `[PASS]`，虚构用户的正常打卡→点赞周卡→补签及 D02／D04／D05 的失败路径均按上述结果重现；**不是 Rust 函数或发布 EXE 测试**。 |
| `cargo test`、`cargo test --features lite`、`npm run tauri build`、V2 GUI 和真实开播 | **未执行。** 本轮未找到 Cargo，也没有获取或运行与当前 HEAD 对应的新版 EXE；没有触发线上直播。 |

**测试隔离：**当前 `lib.rs` 第2185行测试构造器可能读取默认路径的真实 `credentials.dat`，`credentials.rs` 第163—179行有真实凭据测试；`registry.rs` 第373—392、469—487行测试会短暂写 HKCU；`logging.rs` 第477—491行测试会向数据目录写崩溃报告。开发者不应在未隔离数据目录、凭据和注册表的机器上直接运行全套测试。现有 `checkin.rs` 第2255—2261行只复制主 `.db` 的发布库测试，也不代表包含 WAL 的完整快照。

**包体副作用如实记录：**上一轮 SQLite `mode=ro` 查询旧包数据库后，观察到 `D:\publish_v2\MonsterOrderWilds_configs\captain_profiles.db-shm` 的修改时间变为 2026-09-25 20:28:42；主 `.db`／`-wal` 的时间仍为 20:07:40。SQLite 只读连接可能写共享内存 sidecar，故不能声称旧包完全未触动。发现后停止所有旧库 SQLite 访问，未擅自恢复 sidecar，未启动旧 EXE，未展示真实用户行或密钥。

## 7. 固定旧源码的对照入口

- [旧队列持久化／500ms 节流](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/PriorityQueueManager.cpp#L237-L304)；[旧 WPF 对象拖拽](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/JonysandMHDanmuTools/OrderedMonsterWindow.xaml.cs#L421-L446)。
- [旧数据库固定路径／初始化失败](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/ProfileManager.cpp#L74-L120)；[无卡行置零返回成功](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/ProfileManager.cpp#L1057-L1086)；[连赞标记／卡数同事务](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/ProfileManager.cpp#L1222-L1300)；[批量补签失败回滚](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/ProfileManager.cpp#L1732-L1848)。
- [旧打卡防刷屏只拦打卡模块](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/CaptainCheckInModule.cpp#L237-L304)；[补签独立分发](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/DanmuProcessor.cpp#L413-L423)；[旧补签和查询回复](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/RetroactiveCheckInModule.cpp#L293-L390)。
- [旧完整版弹幕在播音过滤前留档](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/TextToSpeech.cpp#L181-L216)；[旧心跳缺 code 的错误处理](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/BliveManager.cpp#L410-L459)；[旧 Lite 不初始化数据库](https://github.com/Jonysand/MonsterOrderTools/blob/40bca8c9af3ad68c1f8c4acd77c96ac725c826be/MonsterOrderWilds/DataBridgeExports.cpp#L479-L503)。
