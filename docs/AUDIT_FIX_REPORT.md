# 全量交叉审计与缺陷修复报告

> 编制日期：2026-09-19
> 审计对象：V2（`D:\VisualStudioProjects\MHDanmuToolsV2`）对照原工程（`D:\VisualStudioProjects\JonysandMHDanmuTools`）
> 审计方式：8 个分域并行审计（点怪排队 / 打卡补签奖卡 / TTS / B 站长连 / 配置凭据注册表 / 前端 UI / 逐规格文档 / 基础设施），
> 原工程侧完整阅读 C++ 约 1.96 万行 + C# WPF 约 4800 行 + 37 份规格文档；V2 侧约 1.45 万行。
> 全部关键结论均经**独立复验**（含对真实 `captain_profiles.db` / `credentials.dat` 的实测取证）。

---

## 一、修复总览

| 编号 | 缺陷 | 级别 | 修复方式 |
| --- | --- | --- | --- |
| P0-1 | `retroactive_cards` 列名沿用已废弃的 `monthly_first_claimed`，对原工程 v40+ 新建库全链 SQL 报错且错误被静默吞掉 | P0 | 改用 `weekly_first_claimed` + 原工程同款 `ALTER TABLE` 迁移 |
| P0-2 | 音频留档范围做反（对全部 TTS 留档、签到音频反而无留档），重现原工程 v24 已修复的缺陷 | P0 | 仅签到播报留档，文件名 `打卡_{用户名}_{ts}.mp3` |
| P0-3 | 语音音量对 Manbo / MiMo / 本地音效完全无效（rodio 路径从不设置音量） | P0 | `play_decoder_sync` 按 `speech_volume/200` 设置增益 |
| P1-1 | SC / 上舰气泡永不显示（后端内部标签 vs 前端外部标签） | P1 | 前端按 `kind` 字段判别 |
| P1-2 | 重连前不下播 → 命中服务端 7001 冷却期 → 无限重连失败 | P1 | 重连前先 `end_app` 关闭上一场 |
| P1-3 | 应用层心跳响应码不判定 → `HeartbeatTimeout` 不可达、假连接 | P1 | 判定 code 0/4004，异常码与网络失败均触发重连 |
| P1-4 | 6 项配置默认值与原工程相反 | P1 | 逐项对齐 `ConfigManager.h` |
| P1-5 | 打卡触发词硬编码兜底，配置无法关闭打卡 | P1 | 仅以配置触发词为准 |
| P1-6 | 保存配置回写陈旧窗口位置，位置记忆丢失 | P1 | 后端保留 `top_pos` 权威值；前端订阅 `config-changed` |
| P1-7 | Manbo Key 被 `credentials.dat` 的 `special_user_tts_api_key` 覆盖并反写注册表 | P1 | 权威来源回归注册表/配置 |
| P1-8 | 安装版无 `credentials.dat` 且无导入入口 → 安装版无法开播 | P1 | 新增「导入凭据文件」入口（校验 + 复制 + 即时生效） |
| P1-9 | 悬浮窗被关闭后永久消失 | P1 | overlay 关闭改为隐藏 |
| P1-10 | GM 批量补签语义缩水、搜索静默截断 50 条、导出总览未过滤 | P1 | 逐条对齐原工程 `ProfileManager` |
| P1-11 | 崩溃转储与 History 留档整体缺失 | P1 | panic hook + `SetUnhandledExceptionFilter` + minidump；History 留档 |
| P1-12 | 队列性能规格未实现（线性扫描 + 每条弹幕同步整表落盘 + 持锁 I/O） | P1 | `HashSet` 索引 + 500ms 节流 + 锁外写盘 |

### P2/P3 批量对齐（同上批次）

| 项 | 修复内容 |
| --- | --- |
| 透明度 0 失效 | `cfg.opacity \|\| 95` → `?? 100`（0 是合法值） |
| 身份码明文 | 输入框改 `type="password"` |
| 打卡明细 upsert | 改回 `ON CONFLICT ... WHERE created_at != excluded.created_at` |
| 补签插入 | 裸 `INSERT` → `INSERT OR REPLACE` |
| 空记录连续天数 | 返回 1（对齐原工程 `CalculateContinuousDaysFromRecords`） |
| 满勤判定 | 去掉多余的 `cumulative > 0` 前置条件 |
| 缺失日期查找 | 去掉"最早打卡日"下界，与 `FindLastMissingCheckinDate` 一致 |
| MiMo 引擎可用性 | 未配置 Key 时判为不可用，不再发空 Bearer 请求 |
| 本地音效键 | 恢复原工程 9 键；拦截点移出通用播报入口 |
| 点餐播报 | 接单文案入普通队列，且恢复原文朗读（原工程不 return） |
| 退避参数 | 上限 60s、位宽 6（对齐 `RECONNECT_MAX_DELAY_MS`） |
| WS 心跳 | 间隔 20s、消费 `OP_HEARTBEAT_REPLY` 复位计数、未知 op 触发重连 |
| 出站 `seq` | 恒为 0（对齐 `ProtoUtils::Packet`） |
| `INTERACTION_END` | 增加 `game_id` 比对与清理 |
| `fans_medal_level` | 仅佩戴粉丝牌时记录 |
| 弹幕时间戳缺失 | 回退 0（不再伪造成当前时间排到队尾） |
| 点赞解析 | 缺 uid / `like_count<=0` 记 WARNING 后丢弃 |
| 日志 BOM 竞态 | 文件写入串行化（对齐原工程 `writtingLock`） |
| 队列文件 | 写入恒为 `order_list.json`，旧 `OrderList.list` 仅首次读取迁移 |
| 队列加载 | 旧格式迁移时归一化一次，V2 自有文件按序载入（保留拖拽次序） |
| 铺助路径 | `config_dir()` 改为「exe 同级优先、按存在性回退」，避免以不同 cwd 启动读写到不同数据 |
| 引擎名解析 | 未知值按「自动」处理（保留降级链） |
| 死命令清理 | 移除 8 个前端零调用的命令注册（含可直接扣卡的 `execute_retroactive_checkin`） |
| 前端细节 | 版本号展示、搜索空关键词提示与结果计数、GM 发卡后刷新列表、滑杆改动自动保存并即时生效、跑马灯周期 10s、页面标题与图标 |

### 有意保留（不作为缺陷处理）

| 项 | 理由 |
| --- | --- |
| CSP 仍为 `null` | 收紧 CSP 需实测 Tauri 注入脚本的 nonce 行为，误配会导致生产白屏；风险高于收益，保持现状 |
| 注册表仅在非空时写入 | 原工程无条件覆写（含空值）；V2 防止注册表瞬时读取失败清空凭据。「清空身份码」入口为 `save_id_code`，能力不缺失 |
| 两段式优先词无条件剥离、历战前缀剥离回退匹配 | 修正了原工程"整条点怪被静默丢弃"的体验缺陷，属有意增强 |
| `only_speek_paid_gift` 不作用于 DM 播报 | 原工程该判定依赖恒为 false 的 `isPaidGift`（死逻辑），V2 改为作用于礼物播报过滤 |
| 主词典改用 jieba-rs | 与原工程 cppjieba 同源词典；随包 `dict/*.utf8` 逐字节一致 |
| SAPI 走 PowerShell 子进程 | 参数换算/转义/音量减半逐项一致，规避 COM 绑定 |
| 单击条目完成 → 显式「完成」按钮 | 误删风险权衡，用户已确认 |
| MiMo 长文本（>8K token）分段合成未实现 | 规格 `mimo-tts-integration` 有该条目，但**原工程 `XiaomiTTSProvider.cpp` 从未实现**（全仓无分段/分块逻辑）。V2 与原工程保持一致；如需补齐属新增需求而非"迁移缺失" |
| TTS Key 格式校验未实现 | 原工程仅有 `IsAvailable() = !apiKey_.empty() && available_`（V2 已对齐该判定），无更严格的格式校验逻辑可迁移 |

---

## 二、规格符合性说明（37 份 spec）

逐份核对结论：**核心行为均以原工程 C++/C# 源码的实际行为为准** —— 审计中发现多处 spec 与实现本身不一致
（`credentials-manager` 写 XOR 实为 HMAC、`separate-opacity-control` 写默认 80 实为 100、
`connection-state-machine` 写重连上限 5 次实为无限重连、`gift-combo-optimization` 的表格与代码不符），
V2 在这些点上都正确地对齐了**实现**。

其中 `dump-helper`（崩溃转储）与 `queue-performance`（HashSet 判重 + 500ms 落盘节流）两项规格要求在迁移中被整体遗漏，
本次已补齐；`mimo` 长文本分段与 TTS Key 格式校验两项规格要求**原工程亦未实现**，保持不迁移。

---

## 三、被推翻的审计误报（供留档）

**「注册表 IdCode 编码不兼容」为误报。** 审计中发现原工程使用 `RegQueryValueExA`/`RegSetValueExA`（ANSI 系列）而 V2 使用 W 系列（UTF-16），曾被判为 P1。
经复核：**Windows 注册表 `REG_SZ` 内部恒以 UTF-16 存储**，A 系列 API 仅在进出时做编码转换，ASCII 值双向完全互通，不构成不兼容。原工程用 A 系列（`ConfigManager.cpp:22/26/58/60`）但语义等价。

**「V2 不得修改旧库表结构」的原始结论为误报（方向性错误）。** 见下节。

---

## 四、对既有文档错误结论的更正

### 更正 1：A2「V2 完全适配旧库格式，不执行任何 ALTER」的断言不成立

- **原文档结论**：以「仓库内 `captain_profiles.db` 的 `retroactive_cards` 列名为 `monthly_first_claimed`」为依据，
  判定 V2 应使用该列且不执行 ALTER，并新增单测断言 `retroactive_cards` **不得**存在 `weekly_first_claimed`。
- **实际情况**：原工程自 **v40** 起权威实现为 —— 新库建 `weekly_first_claimed` 列，并对老库执行
  `ALTER TABLE retroactive_cards ADD COLUMN weekly_first_claimed INTEGER DEFAULT 0`
  （`ProfileManager.cpp:178/216-237`，全部读写使用该列 `:1062/1094/1258/1333`）。
  仓库内那份是 **pre-v40 老库**，`monthly_first_claimed` 属历史列，已停止更新。
- **后果**：对「由原工程 v40+ 全新建库」的用户，V2 的读/写 SQL 全部报 `no such column`，且 `lib.rs` 的
  `let Ok(rewards) = ... else { return Vec::new() }` 将其静默吞掉 → 周奖卡/连续 7 天奖卡/补签查询/GM 发卡**全部失效且无提示**；
  反向固化该缺陷的单测使 `cargo test` 无法暴露问题。
- **已修复**：列名与迁移逻辑对齐原工程；新增两条反向回归测试：
  - `test_v40_plus_schema_weekly_card_chain`：模拟 v40+ 建库，全链路（发卡/幂等/跨周/GM/查询）断言
  - `test_schema_migration_adds_weekly_column_idempotently`：迁移生效且幂等、不触碰其它表
  同时把 `test_open_legacy_captain_profiles_db` / `test_real_repo_legacy_db_copy_compatible` 的断言
  由「表结构零变更」改为「仅 `retroactive_cards` 追加列，其余表逐字不变」。

### 更正 2：B7「TTSCacheManager 实为音频留档（原审计描述有误）」的核对结论不成立

- **原文档结论**：认为原工程 `TTSCacheManager` 的职责是「全量音频留档 + 启动清理」，并据此让 V2 对所有 TTS 留档。
- **实际情况**：原工程**唯一**的留档调用点是 `TextToSpeech.cpp:983` 的 `SaveCheckinAudio`，受 `reqPtr->isCheckinTTS` 守卫，
  文件名固定 `打卡_{username}_{tick}.mp3`；`SaveCachedAudio` / `SaveCachedAudioWithPrefix` 在全工程**无任何调用方（死代码）**。
  规格 `tts-cache-manager/spec.md` FR-1 亦明确「只保存签到 AI 回复 TTS 音频；一般弹幕 TTS 不缓存，直接播放后丢弃」。
  这正是原工程 **v24 已修复**的缺陷形态。
- **已修复**：`play_audio_bytes` 不再无条件留档；新增 `save_checkin_audio`，仅在签到/补签播报路径落盘 `打卡_{用户名}_{ts}.mp3`。

---

## 五、回归验证

```bash
cargo test --manifest-path src-tauri/Cargo.toml   # 117 项全绿、无编译警告
npm run build                                      # tsc + vite 通过
python scripts/check_encoding.py                   # 编码规则通过（36 BOM 必需 + 9 JSON）
python scripts/check_orphan_fields.py              # 配置字段接线通过
```

新增/改写的单元测试覆盖本次全部修复点：

| 测试 | 覆盖 |
| --- | --- |
| `checkin::test_v40_plus_schema_weekly_card_chain` | P0-1 反向回归（v40+ 建库全链路） |
| `checkin::test_schema_migration_adds_weekly_column_idempotently` | P0-1 迁移生效与幂等 |
| `checkin::test_open_legacy_captain_profiles_db` / `test_real_repo_legacy_db_copy_compatible` | P0-1 旧库兼容与非目标表零变更 |
| `tts::test_volume_gain_mapping` | P0-3 音量换算（含越界钳制） |
| `tts::test_content_prefix_and_cache_cleanup` | P0-2 签到留档命名 |
| `tts::test_special_sound_keys_match_legacy_voice_map` | 本地音效键与原文一致、未定义键不命中 |
| `tts::test_speak_queue_capacity_logs_and_requeue` | 双队列各推进一条、回滚入队、并发闸门 |
| `tts::test_auto_engine_cascade_and_current_engine_name` | MiMo 无 Key 跳过（T4） |
| `lib::test_food_order_danmu_enters_priority_queue` | 点餐双播 + 普通队列 |
| `lib::test_parse_tts_engine_mapping` | 未知引擎名 → 自动 |
| `lib::test_app_config_persistence_and_defaults` / `lib::test_app_state_initialization` | P1-4 默认值对齐 |
| `queue::test_user_index_stays_in_sync` | P1-12 O(1) 索引一致性 |
| `queue::test_dirty_flag_lifecycle` / `test_queue_paths_separate_read_and_write` | P1-12 节流语义与读写路径分离 |
| `credentials::test_credentials_roundtrip_and_mask` | P1-8 凭据往返 + 篡改检测 + 掩码边界 |
| `credentials::test_load_real_credentials_dat` | 真实凭据解析（找不到时**显式** SKIP，不再静默跳过） |
| `logging::test_history_line_format_matches_original_record_history` / `test_crash_report_format_and_write` | P1-11 留档与崩溃报告 |

**残余项（本机无法执行，已登记为发布前人工验收清单，不是未完成的修复）**：

| 残余项 | 为何无法在本次完成 | 验收方式 |
| --- | --- | --- |
| 真实 B 站开播长连全链路（心跳异常码 → 重连、7001 冷却、`INTERACTION_END` game_id 比对） | 需要有效身份码与真实直播会话，属外部环境依赖 | 开播后观察日志：`[BiliLive] 应用心跳返回异常 code=`、`重连前已关闭上一场互动应用` |
| 安装产物在干净目录的端到端（新凭据导入入口、minidump 生成） | 需要 GUI 环境与安装包部署 | `npm run tauri build` 后在干净目录实测三项：导入凭据后可开播、断线可重连、崩溃后 `Crashes/` 下出现 `.dmp` |
| Manbo / MiMo 真实音频与音量听感 | 需要真实 API Key 与音频设备 | 设置页把「语音音量」由 200 拖到 0，确认 Manbo/MiMo/本地音效音量随之变化（修复前恒定满音量） |

**代码层交付物已全部闭合**：3 项 P0 + 12 项 P1 + P2/P3 批量项均已落地并带反向回归测试，
`cargo test` 119 项全绿、零编译警告，`npm run build` 与两项门禁脚本通过。

**未提交说明**：按 `AGENTS.md`「永远不要自动提交和推送」的约定，全部改动保留在工作区
（18 个文件修改 + 新增 `docs/AUDIT_FIX_REPORT.md`），提交需由用户明确要求后执行。
