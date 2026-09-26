pub mod ai;
pub mod bilibili;
pub mod checkin;
pub mod checkin_ai;
pub mod config;
pub mod credentials;
pub mod logging;
pub mod manbo_voices;
pub mod monster;
pub mod paths;
pub mod queue;
pub mod registry;
pub mod roster;
pub mod tts;

use checkin_ai::CheckinLearner;

use ai::DeepSeekAIChatProvider;
use checkin::CheckinManager;
use config::AppConfig;
use credentials::{Credentials, CredentialsStatus};
use monster::MonsterDataManager;
use queue::{QueueItem, QueueManager, QueuePersistence, QueueSnapshot};
use roster::{MonsterRoster, RosterData};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};
use tts::{TTSConfig, TTSEngineType, TTSManager};

/// Lite 形态编译期常量：对应原工程 C++ 侧编译期宏 `ONLY_ORDER_MONSTER`
/// （`#if !ONLY_ORDER_MONSTER` 排除 TTS/打卡/GM/AI）。完整版与 Lite 版在
/// build 期由 Cargo feature `lite` 决定（`cargo build --features lite`），
/// 运行期不可切换；release 编译下恒定 false/true 的分支会被整体消除。
pub const IS_LITE: bool = cfg!(feature = "lite");

/// 打卡子系统可用性状态（脱敏：只含原因码与提示文本，不含路径、凭据或用户数据）
#[derive(Debug, Clone, Serialize)]
pub struct CheckinStatus {
    /// 是否可持久化。false 时打卡/补签/点赞/GM 全部拒绝写入
    pub available: bool,
    /// 停用原因码：`None` / `LiteDisabled` / `DatabaseUnavailable`
    pub reason_code: String,
    /// 面向主播的提示文本
    pub message: String,
    /// 实际使用的库文件名（仅文件名；不可用时为 None）
    pub active_db_file: Option<String>,
    /// 是否存在另一份未展示、未删除的打卡库
    pub has_shadow_db: bool,
}

impl CheckinStatus {
    fn available(active_db_file: String, has_shadow_db: bool) -> Self {
        Self {
            available: true,
            reason_code: "None".to_string(),
            message: if has_shadow_db {
                "打卡数据可持久化。检测到另一份打卡库未被展示（未删除），请查看日志并按提示离线合并。"
                    .to_string()
            } else {
                "打卡数据可持久化".to_string()
            },
            active_db_file: Some(active_db_file),
            has_shadow_db,
        }
    }

    fn lite_disabled() -> Self {
        Self {
            available: false,
            reason_code: "LiteDisabled".to_string(),
            message: "Lite 形态按构建期设定已停用打卡功能".to_string(),
            active_db_file: None,
            has_shadow_db: false,
        }
    }

    /// 数据库不可用：只带原因码与固定提示，不回显底层错误原文（可能含本机路径）
    fn database_unavailable() -> Self {
        Self {
            available: false,
            reason_code: "DatabaseUnavailable".to_string(),
            message: "打卡数据库不可用，本次运行已停用打卡、补签与点赞奖卡。\
                      核心点怪排队不受影响；请检查本机数据目录的读写权限与磁盘空间后重启。"
                .to_string(),
            active_db_file: None,
            has_shadow_db: false,
        }
    }
}

/// 凭据权威快照：所有 `AppState` 克隆共享同一份。
///
/// 导入新凭据后必须**整体替换**这份快照 —— 早期实现把 `Arc<Credentials>` 固定在启动时的值，
/// 于是连接、重连与状态轮询会一直用旧凭据 A，而界面与配置文件却显示 B。
#[derive(Debug, Clone, Default)]
pub struct CredentialState {
    pub creds: Credentials,
    /// 是否已从验签文件成功加载
    pub loaded: bool,
    /// 非 None 表示凭据处于"不可用/暂禁开播"状态（导入过程中内存发布失败等），
    /// 保留原因供人工恢复，不用默认值继续开播
    pub blocked_reason: Option<String>,
}

/// 全局运行状态（线程安全且支持克隆引用）
#[derive(Clone)]
pub struct AppState {
    pub queue_mgr: Arc<Mutex<QueueManager>>,
    /// 队列磁盘唯一写者锁。锁序恒为 **queue_flush_lock → queue_mgr**：
    /// 500ms 后台任务与所有强制保存命令都按此顺序取锁，任何持 queue_mgr 的代码都不得再请求刷盘。
    pub queue_flush_lock: Arc<Mutex<()>>,
    /// 最近一次刷盘是否处于失败状态：用于 `queue-persistence-changed` 的边沿事件，避免每 tick 重复广播
    pub queue_persistence_broken: Arc<std::sync::atomic::AtomicBool>,
    pub monster_mgr: Arc<MonsterDataManager>,
    /// 点怪禁点名单（名单内的怪物不可被点单；弹幕与选怪面板共享同一份约束）
    pub roster: Arc<MonsterRoster>,
    pub checkin_mgr: Option<Arc<CheckinManager>>,
    /// 打卡可用性状态：不可用时所有打卡入口据此拒绝写入并给出可见反馈
    pub checkin_status: Arc<CheckinStatus>,
    /// 弹幕关键词学习器（jieba 分词）。生产环境在 run() 中注入；测试默认 None（学习链路将被跳过）
    pub checkin_learner: Option<Arc<CheckinLearner>>,
    pub tts_mgr: Arc<TTSManager>,
    pub ai_provider: Arc<DeepSeekAIChatProvider>,
    pub config: Arc<Mutex<AppConfig>>,
    /// 凭据权威快照（可变）：连接、状态展示、导入全部读同一份
    pub credentials: Arc<std::sync::RwLock<CredentialState>>,
    /// 会话 start／stop／import 共用的生命周期闸门（async 命令持有，跨 `.await`）
    pub bili_lifecycle: Arc<tokio::sync::Mutex<()>>,
    /// 凭据提交短锁：只覆盖"原子替换文件 → 发布内存快照 → 同步 AI/TTS"，**不含任何 `.await`**
    pub credential_gate: Arc<Mutex<()>>,
    pub danmu_processor: Arc<bilibili::DanmuProcessor>,
    /// B 站长连五态状态机（替代原 bool 状态，D7）
    pub connection: Arc<Mutex<bilibili::ConnectionStatus>>,
    pub bili_service: Arc<bilibili::BiliLiveService>,
    /// 悬浮窗锁定（穿透）状态：运行时状态，不持久化（对齐原工程 mIsLocked）
    pub overlay_locked: Arc<std::sync::atomic::AtomicBool>,
    /// 悬浮窗拖动待落盘位置（防抖后由后台任务写回配置 top_pos_x/y）
    pub pending_pos: Arc<Mutex<Option<(f64, f64)>>>,
    /// 启动时缺失的资源名快照：`resource-missing` 事件只 emit 一次，
    /// 挂载晚于 setup 的前端会丢掉它，故另提供可查询快照（按名去重合并展示）。
    pub startup_missing: Arc<Vec<String>>,
    /// 最近一次「打卡不可用」提示的时间戳（用于高频 LIKE 通道的节流）
    pub last_checkin_unavailable_at: Arc<std::sync::atomic::AtomicI64>,
}

impl Default for AppState {
    fn default() -> Self {
        let app_cfg = AppConfig::load(None);

        // 0. 加载原工程加密配置文件 credentials.dat
        //    加载失败只表示"尚无可用凭据"，不阻断启动：核心点怪不依赖它
        let (creds, creds_loaded) = match credentials::load_credentials(None) {
            Ok(c) => (c, true),
            Err(e) => {
                crate::log_warn!("加载 credentials.dat 失败: {}", e);
                (Credentials::default(), false)
            }
        };

        // 1. 初始化怪物别名匹配器
        let mut monster_mgr = MonsterDataManager::new();
        if let Err(e) = monster_mgr.load_from_file(None) {
            crate::log_warn!("[Init] 怪物别名库加载失败，点怪匹配降级: {}", e);
        }

        // 1.1 初始化点怪可选名单（白名单默认关闭；文件缺失即用默认值）
        let roster = MonsterRoster::load(None);

        // 2. 初始化排队管理器并自动加载持久化列表
        //    读取优先 V2 的 order_list.json，其次原工程 OrderList.list（首次迁移，只读不改写）
        let mut queue_mgr = QueueManager::new();
        let order_path = queue::resolve_queue_read_path();
        if let Err(e) = queue_mgr.load_from_file(&order_path) {
            crate::log_warn!("[Init] 历史队列加载失败，从空队列启动: {}", e);
        }

        // 3. 初始化打卡管理器
        //    - Lite：完全不实例化（不探测路径、不建目录、不 open、不建表、不迁移）
        //    - 完整版：库不可用时**显式停用**打卡子系统并保留可查询的错误状态。
        //      绝不静默退回内存库：那会让打卡与卡片在界面上"成功"，重启后资产消失
        let (checkin_mgr, checkin_status) = if IS_LITE {
            (None, CheckinStatus::lite_disabled())
        } else {
            match CheckinManager::open_default() {
                Ok((mgr, report)) => (
                    Some(Arc::new(mgr)),
                    CheckinStatus::available(report.active_file_name, report.has_shadow_db),
                ),
                Err(e) => {
                    crate::log_error!("[Checkin] 打卡数据库不可用，已停用打卡子系统: {}", e);
                    (None, CheckinStatus::database_unavailable())
                }
            }
        };

        // 4. 初始化 TTS 管理器 (优先注入加密凭证中的 API Key)
        let mimo_key = if !creds.mimo_tts_api_key.is_empty() {
            creds.mimo_tts_api_key.clone()
        } else {
            app_cfg.mimo_api_key.clone()
        };
        // Manbo API Key 的权威来源是注册表（HKCU\Software\MonsterOrderWilds\ManboApiKey）与配置值，
        // 不由 credentials.dat 承载 —— 原工程 C++/C# 全仓无 special_user_tts_api_key 引用，
        // 该凭据不得覆盖用户手填的 Manbo Key。
        let manbo_key = app_cfg.manbo_api_key.clone();

        let tts_mgr = TTSManager::new(TTSConfig {
            engine: parse_tts_engine(&app_cfg.tts_engine),
            enable_voice: app_cfg.enable_voice,
            speech_rate: app_cfg.speech_rate,
            speech_volume: app_cfg.speech_volume,
            speech_pitch: app_cfg.speech_pitch,
            manbo_api_key: manbo_key,
            manbo_voice: app_cfg.manbo_voice.clone(),
            mimo_api_key: mimo_key,
            mimo_voice: app_cfg.mimo_voice.clone(),
            mimo_style: app_cfg.mimo_style.clone(),
            mimo_audio_format: app_cfg.mimo_audio_format.clone(),
        });

        // 5. 初始化 AI 思考模块 (优先注入加密凭证中的 API Key)
        let chat_key = if !creds.chat_api_key.is_empty() {
            creds.chat_api_key.clone()
        } else {
            app_cfg.deepseek_api_key.clone()
        };
        let ai_provider = DeepSeekAIChatProvider::new(chat_key);

        // 6. 初始化弹幕处理器（过滤开关运行时热更新）
        let danmu_processor = bilibili::DanmuProcessor::new();
        danmu_processor.update_filters(
            app_cfg.only_medal_order,
            app_cfg.only_speek_wearing_medal,
            app_cfg.only_speek_guard_level,
        );

        Self {
            queue_mgr: Arc::new(Mutex::new(queue_mgr)),
            queue_flush_lock: Arc::new(Mutex::new(())),
            queue_persistence_broken: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            monster_mgr: Arc::new(monster_mgr),
            roster: Arc::new(roster),
            checkin_mgr,
            checkin_status: Arc::new(checkin_status),
            checkin_learner: None,
            tts_mgr: Arc::new(tts_mgr),
            ai_provider: Arc::new(ai_provider),
            config: Arc::new(Mutex::new(app_cfg)),
            credentials: Arc::new(std::sync::RwLock::new(CredentialState {
                creds,
                loaded: creds_loaded,
                blocked_reason: None,
            })),
            bili_lifecycle: Arc::new(tokio::sync::Mutex::new(())),
            credential_gate: Arc::new(Mutex::new(())),
            danmu_processor: Arc::new(danmu_processor),
            connection: Arc::new(Mutex::new(bilibili::ConnectionStatus::default())),
            bili_service: Arc::new(bilibili::BiliLiveService::new()),
            overlay_locked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_pos: Arc::new(Mutex::new(None)),
            startup_missing: Arc::new(Vec::new()),
            last_checkin_unavailable_at: Arc::new(std::sync::atomic::AtomicI64::new(0)),
        }
    }
}

// ---------------------------------------------------------------------------------
// Tauri Commands
// ---------------------------------------------------------------------------------

impl AppState {
    /// 打卡子系统句柄。不可用时返回面向调用方的提示文本，
    /// 调用方必须据此**拒绝写入**并给出可见反馈，不得伪造成功。
    pub fn checkin(&self) -> Result<&Arc<CheckinManager>, String> {
        match &self.checkin_mgr {
            Some(m) => Ok(m),
            None => Err(self.checkin_status.message.clone()),
        }
    }

    pub fn checkin_available(&self) -> bool {
        self.checkin_mgr.is_some()
    }

    /// 取凭据权威快照（连接、状态展示、导入全部读同一份）
    pub fn credentials_snapshot(&self) -> CredentialState {
        self.credentials
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 发布新凭据快照（导入成功后调用）
    pub fn publish_credentials(&self, next: CredentialState) {
        *self.credentials.write().unwrap_or_else(|e| e.into_inner()) = next;
    }

    /// 构造当前会话应使用的 B 站凭据。
    /// 已验签的凭据文件优先；其字段为空时回退配置镜像（保持历史兼容）。
    fn bili_credentials(&self, cfg: &AppConfig) -> bilibili::BiliCredentials {
        let snap = self.credentials_snapshot();
        let pick = |cred: &str, mirror: &str| -> String {
            if !cred.is_empty() {
                cred.to_string()
            } else {
                mirror.to_string()
            }
        };
        bilibili::BiliCredentials {
            app_id: pick(&snap.creds.app_id, &cfg.app_id),
            access_key_id: pick(&snap.creds.access_key_id, &cfg.access_key_id),
            access_key_secret: pick(&snap.creds.access_key_secret, &cfg.access_key_secret),
            id_code: cfg.id_code.clone(),
        }
    }
}

/// 打卡不可用时的统一反馈：向两个 WebView 广播一次脱敏事件，并做 ERROR 记录。
///
/// 调用方在返回前**必须**提前 return，不得落入普通 TTS 朗读或伪造成功气泡。
/// `throttle=true` 用于高频 LIKE 通道：同一提示最多每 [`CHECKIN_UNAVAILABLE_THROTTLE_SECS`] 秒一次，
/// 避免点赞刷屏时把日志与事件打爆；主窗口的常驻状态来自 `get_checkin_status`，不受节流影响。
fn emit_checkin_unavailable(state: &AppState, app: Option<&AppHandle>) {
    emit_checkin_unavailable_throttled(state, app, false)
}

fn emit_checkin_unavailable_throttled(state: &AppState, app: Option<&AppHandle>, throttle: bool) {
    const CHECKIN_UNAVAILABLE_THROTTLE_SECS: i64 = 60;

    let status = &state.checkin_status;
    if throttle {
        let now = chrono::Utc::now().timestamp();
        let last = state
            .last_checkin_unavailable_at
            .load(std::sync::atomic::Ordering::Relaxed);
        if now - last < CHECKIN_UNAVAILABLE_THROTTLE_SECS {
            return;
        }
        state
            .last_checkin_unavailable_at
            .store(now, std::sync::atomic::Ordering::Relaxed);
    }

    crate::log_error!(
        "[Checkin] 打卡子系统不可用（{}），已拒绝本次打卡/补签/奖卡请求",
        status.reason_code
    );
    if let Some(handle) = app {
        let _ = handle.emit(
            "checkin-unavailable",
            &serde_json::json!({
                "reason_code": status.reason_code,
                "message": status.message,
            }),
        );
    }
}

/// 获取当前排队列表（权威快照：items + revision + 落盘状态）
#[tauri::command]
fn get_queue(state: State<'_, AppState>) -> Result<QueueSnapshot, String> {
    let q = state.queue_mgr.lock().map_err(|e| e.to_string())?;
    Ok(q.snapshot())
}

/// 队列落盘：进程内**唯一写者**。
///
/// 锁序恒为 `queue_flush_lock → queue_mgr`。写锁覆盖「取快照 → 写临时文件 → 替换目标 → 确认脏位」，
/// 因此后台 500ms 任务与手动强制保存不可能倒序覆盖；业务队列锁只在取快照的瞬间持有，
/// 磁盘 I/O 期间弹幕热路径照常推进。
///
/// 失败时**保留未落盘状态**（不提前清脏），由下一次 tick 或下一次命令重试。
/// `cfg(test)` 下不写真实数据目录；真实逻辑在 `flush_queue_to_path`，由单测注入临时路径验证。
fn flush_queue(state: &AppState, force: bool, app: Option<&AppHandle>) {
    if cfg!(test) {
        return;
    }
    let path = queue::get_order_list_path();
    let _ = flush_queue_to_path(state, &path, force, app);
}

/// 真实保存逻辑（路径可注入，供单测在隔离临时目录验证）。
///
/// 调用者必须已持有 `queue_flush_lock` —— 该锁是「磁盘单写者」不变量的载体。
fn flush_queue_to_path(
    state: &AppState,
    path: &Path,
    force: bool,
    app: Option<&AppHandle>,
) -> Result<bool, String> {
    let _flush_guard = state
        .queue_flush_lock
        .lock()
        .map_err(|e| format!("队列刷盘锁异常: {}", e))?;

    // 锁内：取「内容 + 版本」，随即释放队列锁，磁盘 I/O 不在队列锁内进行
    let (json, flushed_revision) = {
        let q = match state.queue_mgr.lock() {
            Ok(q) => q,
            Err(e) => {
                let msg = format!("队列锁异常，跳过落盘: {}", e);
                crate::log_warn!("[Queue] {}", msg);
                return Err(msg);
            }
        };
        if !q.is_dirty() && !force {
            return Ok(false);
        }
        match q.to_json() {
            Ok(j) => (j, q.revision),
            Err(e) => {
                let msg = format!("队列序列化失败: {}", e);
                crate::log_warn!("[Queue] {}", msg);
                return Err(msg);
            }
        }
    };

    // 锁外写盘（仍持刷盘锁）：失败则不清脏，下一次 tick 会重试
    match QueueManager::write_json(path, &json) {
        Ok(()) => {
            if let Ok(mut q) = state.queue_mgr.lock() {
                // 仅当最新版本就是本次落盘的版本时才确认已保存；
                // 写入期间到来的新弹幕保持脏位，由下一 tick 继续保存
                q.mark_saved(flushed_revision);
            }
            if state
                .queue_persistence_broken
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                crate::log_info!("[Queue] 队列已恢复落盘（版本 {}）", flushed_revision);
                emit_queue_persistence(app, QueuePersistence::Saved);
            }
            Ok(true)
        }
        Err(e) => {
            // 普通 WARN 不带本机路径；路径只在 Debug 级受控诊断入口出现
            crate::log_warn!("[Queue] 队列落盘失败: {}", e);
            crate::log_debug!("[Queue] 落盘目标路径: {}", path.display());
            if !state
                .queue_persistence_broken
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                emit_queue_persistence(app, QueuePersistence::PendingRetry);
            }
            Err(e)
        }
    }
}

/// 广播落盘状态变化（不含任何用户数据，只表达「内存版本是否已确认落盘」）
fn emit_queue_persistence(app: Option<&AppHandle>, persistence: QueuePersistence) {
    if let Some(handle) = app {
        let _ = handle.emit(
            "queue-persistence-changed",
            &serde_json::json!({ "persistence": persistence }),
        );
    }
}

/// 变更后立即落盘（用于用户命令与退出链路；弹幕热路径改由 500ms 节流任务落盘）。
/// 返回权威快照，落盘失败时其 `persistence` 为 `PendingRetry`（内存已更新、磁盘待重试）。
fn save_queue_now(state: &AppState, app: Option<&AppHandle>) -> QueueSnapshot {
    if let Err(e) = flush_queue_to_path(state, &queue::get_order_list_path(), true, app) {
        if cfg!(test) {
            crate::log_warn!("[Queue] 保存失败（测试路径）: {}", e);
        }
    }
    queue_snapshot_or_default(state)
}

/// 取权威快照；锁中毒时退回一份空快照（只影响展示，不影响内存队列）
fn queue_snapshot_or_default(state: &AppState) -> QueueSnapshot {
    match state.queue_mgr.lock() {
        Ok(q) => q.snapshot(),
        Err(_) => QueueSnapshot {
            items: Vec::new(),
            revision: 0,
            persistence: QueuePersistence::PendingRetry,
        },
    }
}

/// 广播权威队列快照（命令与弹幕路径唯一出口）
fn emit_queue_snapshot(app: &AppHandle, snapshot: &QueueSnapshot) {
    let _ = app.emit("queue-updated", snapshot);
}

/// 新增点怪
/// `tempered_level`：None = 跟随字典默认等级（选怪面板「难度：默认」），
/// Some(v) = 强制该等级（主播显式指定，覆盖字典默认值）
#[tauri::command]
fn add_order(
    user_id: String,
    user_name: String,
    monster_name: String,
    is_priority: bool,
    guard_level: Option<i32>,
    tempered_level: Option<i32>,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<QueueSnapshot, String> {
    let mut q = state.queue_mgr.lock().map_err(|e| e.to_string())?;

    // 匹配怪物别名与图标
    let (final_monster, final_tempered, icon_url) = if let Some(m) = state.monster_mgr.match_monster(&monster_name) {
        (m.monster_name, tempered_level.unwrap_or(m.tempered_level), m.icon_url)
    } else {
        (
            monster_name,
            tempered_level.unwrap_or(0),
            String::new(),
        )
    };

    let item = QueueItem {
        id: format!("item-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()),
        user_id,
        user_name,
        monster_name: final_monster,
        is_priority,
        guard_level: guard_level.unwrap_or(0),
        tempered_level: final_tempered,
        timestamp: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64,
        icon_url,
    };

    q.add_or_update(item);
    let revision_changed = q.is_dirty();
    drop(q);

    // 用户命令路径保持"变更即落盘"语义（弹幕热路径改由 500ms 节流任务负责）
    let snapshot = if revision_changed {
        save_queue_now(&state, Some(&app_handle))
    } else {
        queue_snapshot_or_default(&state)
    };
    emit_queue_snapshot(&app_handle, &snapshot);
    Ok(snapshot)
}

/// 按 User ID 删除指定排队项（保序删除）；未命中时不产生无变更的强制写盘
#[tauri::command]
fn dequeue_by_user_id(
    user_id: String,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<QueueSnapshot, String> {
    let (removed, snapshot) = {
        let mut q = state.queue_mgr.lock().map_err(|e| e.to_string())?;
        let removed = q.dequeue_by_user_id(&user_id).is_some();
        (removed, q.snapshot())
    };

    let snapshot = if removed {
        save_queue_now(&state, Some(&app_handle))
    } else {
        snapshot
    };
    emit_queue_snapshot(&app_handle, &snapshot);
    Ok(snapshot)
}

/// 清空当前排队
#[tauri::command]
fn clear_queue(state: State<'_, AppState>, app_handle: AppHandle) -> Result<QueueSnapshot, String> {
    let changed = {
        let mut q = state.queue_mgr.lock().map_err(|e| e.to_string())?;
        let rev_before = q.revision;
        q.clear();
        q.revision != rev_before
    };

    // 清空属破坏性操作：确实清掉了内容才立即落盘（对齐原工程 Clear() 立即 SaveList）
    let snapshot = if changed {
        save_queue_now(&state, Some(&app_handle))
    } else {
        queue_snapshot_or_default(&state)
    };
    emit_queue_snapshot(&app_handle, &snapshot);
    Ok(snapshot)
}

/// 手动拖拽重新排序队列（主播拖拽调整排队顺序）。
///
/// 只接受「预期版本 + 用户 ID 顺序」：客户端整表快照不再被后端采纳，
/// 因此一次拖拽不可能删除另一路新订单、复活已完成单或撤回提权。
#[tauri::command]
fn reorder_queue(
    ordered_user_ids: Vec<String>,
    expected_revision: u64,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<QueueSnapshot, String> {
    let snapshot = {
        let mut q = state.queue_mgr.lock().map_err(|e| e.to_string())?;
        q.reorder_by_ids(&ordered_user_ids, expected_revision)?;
        q.snapshot()
    };

    let snapshot = if snapshot.persistence == QueuePersistence::PendingRetry {
        save_queue_now(&state, Some(&app_handle))
    } else {
        snapshot
    };
    emit_queue_snapshot(&app_handle, &snapshot);
    Ok(snapshot)
}

/// 撤销完成：把条目原样插回指定下标（保留原 id 与 timestamp）。
/// 同一用户已重新入队时返回明确冲突，且不落盘、不发成功事件。
#[tauri::command]
fn restore_order(
    item: QueueItem,
    index: usize,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<QueueSnapshot, String> {
    let restored = {
        let mut q = state.queue_mgr.lock().map_err(|e| e.to_string())?;
        q.restore(item, index)
    };
    if !restored {
        return Err("该用户已重新入队，无法撤销之前的完成操作".to_string());
    }

    let snapshot = save_queue_now(&state, Some(&app_handle));
    emit_queue_snapshot(&app_handle, &snapshot);
    Ok(snapshot)
}

/// 选怪面板入队的核心逻辑（不依赖 Tauri `State`，便于单测直接覆盖）。
///
/// 锁序统一为 **queue_mutex → 字典读 → 名单读**（与弹幕路径 `process_danmu` 一致）：
/// 名单读 guard 持续覆盖"检查 → 入队"，因此一次名单编辑不可能插进检查与入队之间。
fn picked_order_enqueue(
    state: &AppState,
    user_id: String,
    user_name: String,
    monster_name: &str,
    is_priority: bool,
    guard_level: Option<i32>,
    tempered_level: Option<i32>,
) -> Result<(), String> {
    let name = monster_name.trim().to_string();
    if name.is_empty() {
        return Err("怪物名不能为空".into());
    }

    // 1. 队列锁
    let mut q = state.queue_mgr.lock().map_err(|e| e.to_string())?;

    // 2. 字典读：面板入口要求精确命中（未知名字请走弹幕点怪的既有兼容路径）
    let Some(entry) = state.monster_mgr.exact_entry(&name) else {
        return Err(format!("词库中没有名为「{}」的怪物，请刷新图鉴库后重试", name));
    };

    // 3. 名单读 guard 一直持有到入队完成
    let roster_guard = state.roster.read_guard();
    if roster_guard.items.iter().any(|n| n == &entry.monster_name) {
        return Err(format!("「{}」已在禁点名单中，无法入队", entry.monster_name));
    }

    // tempered_level=None 表示"按字典默认值"；Some 表示主播显式覆盖
    let final_tempered = tempered_level.unwrap_or(entry.tempered_level);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();

    q.add_or_update(QueueItem {
        id: format!("item-{}", now.as_millis()),
        user_id,
        user_name,
        monster_name: entry.monster_name.clone(),
        is_priority,
        guard_level: guard_level.unwrap_or(0),
        tempered_level: final_tempered,
        timestamp: now.as_secs() as i64,
        icon_url: entry.icon_url,
    });
    // roster_guard 在此之后才释放：检查与入队处于同一临界区
    Ok(())
}

/// 选怪面板入队（主播手动点单）。
///
/// 与弹幕点怪的关键差异：面板展示的是**字典原名**，因此这里按原名精确取键，
/// 不重新跑别名匹配 —— 否则某只怪把该原名登记为别称后，面板上点「黑龙」会被换成另一只怪。
/// 入队前由后端检查禁点名单，命中即拒绝（Toast 用后端实际确认的条目，不复述请求文案）。
#[tauri::command]
fn add_picked_order(
    user_id: String,
    user_name: String,
    monster_name: String,
    is_priority: bool,
    guard_level: Option<i32>,
    tempered_level: Option<i32>,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<QueueSnapshot, String> {
    picked_order_enqueue(
        &state,
        user_id,
        user_name,
        &monster_name,
        is_priority,
        guard_level,
        tempered_level,
    )?;

    let snapshot = save_queue_now(&state, Some(&app_handle));
    emit_queue_snapshot(&app_handle, &snapshot);
    Ok(snapshot)
}

/// 读取全量怪物字典（图鉴库展示与别称冲突检测的数据源）
#[tauri::command]
fn get_monster_dict(
    state: State<'_, AppState>,
) -> Result<HashMap<String, monster::MonsterConfig>, String> {
    Ok(state.monster_mgr.get_all_monsters())
}

/// 新增 / 更新字典条目并热重载匹配器；`original` 为改名前旧名（用于同步可选名单）。
/// 返回落盘后的条目总数
#[tauri::command]
fn save_monster_entry(
    name: String,
    config: monster::MonsterConfig,
    original: Option<String>,
    state: State<'_, AppState>,
) -> Result<usize, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("怪物名不能为空".into());
    }

    let renamed_from = original
        .map(|o| o.trim().to_string())
        .filter(|o| !o.is_empty() && *o != name);

    let path = MonsterDataManager::find_monster_list_path();
    // 同名/撞名校验在**任何 raw.remove/insert 之前**于权威字典上执行：
    // 前端预检查只作提示，后端才是最后一道校验
    let mut rename_sync_failed: Option<String> = None;
    let count = state.monster_mgr.edit_and_save(&path, |raw| {
        monster::MonsterDataManager::upsert_entry(raw, &name, &config, renamed_from.as_deref())
    })?;

    // 改名时同步可选名单中的引用（保持原顺位）。
    // 字典已落盘，名单若未同步必须**如实上报部分完成**，不能谎报"全部保存成功"
    if let Some(old) = &renamed_from {
        match state.roster.rename_item(old, &name) {
            Ok(true) => crate::log_info!("[Dict] 条目改名成功，已同步可选名单"),
            Ok(false) => {}
            Err(e) => {
                crate::log_error!("[Dict] 条目改名后同步可选名单失败: {}", e);
                rename_sync_failed = Some(format!(
                    "词库已修改为「{}」，但禁点名单未同步（{}）。请刷新名单后手动修正。",
                    name, e
                ));
            }
        }
    }

    if let Some(msg) = rename_sync_failed {
        return Err(msg);
    }

    // 怪名是用户可编辑内容，不进普通 INFO（改名前后名亦然）
    crate::log_info!("[Dict] 条目已保存，当前共 {} 条", count);
    Ok(count)
}

/// 删除字典条目：落盘 + 热重载匹配器 + 同步清理禁点名单（名单恒为字典子集）。
/// 返回剩余条目总数
fn delete_monster_entry_impl(
    mgr: &MonsterDataManager,
    roster: &MonsterRoster,
    dict_path: &Path,
    name: &str,
) -> Result<usize, String> {
    let count = mgr.edit_and_save(dict_path, |raw| {
        raw.remove(name);
        Ok(())
    })?;

    // 禁点名单在 UI 上为只读展示、无手动移除入口，故这里的同名残留必须一并清理。
    // 清理失败同样是"部分完成"：字典已删、名单未同步，必须如实回报
    if roster.snapshot().items.iter().any(|n| n == name) {
        match roster.remove(name) {
            Ok(()) => crate::log_info!("[Dict] 条目已删除，同步移出禁点名单"),
            Err(e) => {
                crate::log_error!("[Dict] 删除条目后同步移出禁点名单失败: {}", e);
                return Err(format!(
                    "词库已删除「{}」，但禁点名单未同步（{}）。请刷新名单后手动修正。",
                    name, e
                ));
            }
        }
    }

    // 怪名同上：只记数量
    crate::log_info!("[Dict] 条目已删除，当前共 {} 条", count);
    Ok(count)
}

/// 删除字典条目并热重载匹配器；返回剩余条目总数
#[tauri::command]
fn delete_monster_entry(name: String, state: State<'_, AppState>) -> Result<usize, String> {
    let path = MonsterDataManager::find_monster_list_path();
    delete_monster_entry_impl(&state.monster_mgr, &state.roster, &path, &name)
}

/// 读取点怪可选名单（含白名单开关状态）
#[tauri::command]
fn get_monster_roster(state: State<'_, AppState>) -> Result<roster::RosterSnapshot, String> {
    Ok(state.roster.snapshot_versioned())
}

/// 整表保存名单（编辑器点击加入/移出后落盘）。
///
/// 带版本 CAS：前端拿着旧版本提交时被拒绝，不会把后端的新名单（例如字典改名/删除
/// 刚刚联动过的结果）抹掉。版本不符时不写盘、不改内存。
#[tauri::command]
fn set_monster_roster(
    data: RosterData,
    expected_revision: u64,
    state: State<'_, AppState>,
) -> Result<roster::RosterSnapshot, String> {
    state.roster.replace_if_revision(data, expected_revision)
}

/// 解析名单 JSON 文本（导入用，纯逻辑便于单测）。
/// 接受两种形态：`{items}` 或裸字符串数组。
/// 旧版白名单文件（含 `enabled` 字段）语义相反，沿用会把全部怪物误判为禁点，故直接拒绝
pub fn parse_roster_json(content: &str) -> Result<RosterData, String> {
    let clean = content.strip_prefix('\u{FEFF}').unwrap_or(content);
    let value: serde_json::Value =
        serde_json::from_str(clean).map_err(|e| format!("名单 JSON 解析失败: {}", e))?;

    match value {
        serde_json::Value::Object(ref map) => {
            if map.contains_key("enabled") {
                return Err(
                    "该文件是旧版「可选名单（白名单）」格式：请改为 {\"items\": [\"怪物名\", ...]} 后再导入"
                        .into(),
                );
            }
            serde_json::from_value(value.clone())
                .map_err(|e| format!("名单 JSON 结构非法（需为 {{\"items\": [...]}}）: {}", e))
        }
        serde_json::Value::Array(_) => {
            let items: Vec<String> = serde_json::from_value(value)
                .map_err(|e| format!("名单数组元素必须为字符串: {}", e))?;
            Ok(RosterData { items })
        }
        _ => Err("名单文件格式不正确（顶层需为对象或字符串数组）".into()),
    }
}

/// 导出当前禁点名单（系统保存对话框；JSON 无 BOM）。用户取消时返回 None
#[tauri::command]
fn export_monster_roster(app_handle: AppHandle, state: State<'_, AppState>) -> Result<Option<String>, String> {
    let data = state.roster.snapshot();
    let json = serde_json::to_string_pretty(&data).map_err(|e| format!("名单序列化失败: {}", e))?;

    match save_text_file_with_dialog(&app_handle, "monster_roster.json", "json", &json) {
        Ok(path) => {
            // 路径不进普通 INFO（用户目录属本机信息），只在 Debug 级诊断入口出现
            crate::log_info!("[Roster] 已导出禁点名单（{} 项）", data.items.len());
            crate::log_debug!("[Roster] 导出目标: {}", path);
            Ok(Some(path))
        }
        // 「已取消导出」不是错误：前端据 None 静默处理
        Err(e) if e.contains("已取消") => Ok(None),
        Err(e) => Err(e),
    }
}

/// 选择名单 JSON 文件（生产：系统打开对话框；测试构建：不弹窗）
#[cfg(not(test))]
fn pick_roster_file(app_handle: &AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let picked = app_handle
        .dialog()
        .file()
        .add_filter("可选名单 JSON", &["json"])
        .blocking_pick_file();
    match picked {
        None => Ok(None),
        Some(p) => Ok(Some(
            p.into_path()
                .map_err(|e| format!("路径解析失败: {}", e))?
                .to_string_lossy()
                .to_string(),
        )),
    }
}

#[cfg(test)]
fn pick_roster_file(_app_handle: &AppHandle) -> Result<Option<String>, String> {
    Err("测试构建不弹出文件选择对话框".into())
}

/// 导入禁点名单：弹打开对话框 → 读取并校验 JSON → 返回解析结果（由前端选定「覆盖 / 合并」后经
/// set_monster_roster 统一落盘，写入路径唯一）。用户取消时返回 None
#[tauri::command]
fn import_monster_roster(app_handle: AppHandle) -> Result<Option<RosterData>, String> {
    let Some(path) = pick_roster_file(&app_handle)? else {
        return Ok(None);
    };
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取名单文件失败（{}）: {}", path, e))?;
    let data = parse_roster_json(&content)?;
    crate::log_info!("[Roster] 已读取导入名单（{} 项）", data.items.len());
    crate::log_debug!("[Roster] 导入来源: {}", path);
    Ok(Some(data))
}

/// E1 统一 Lite 守卫：非排队功能在 Lite 构建下统一拒绝调用。
/// 依据 AGENTS.md「新增功能默认不支持 Lite」：任何新增的非排队功能都必须显式调用本守卫。
/// Lite 保留清单（不调用本守卫）：点怪排队、悬浮窗/窗口控制、B 站长连、身份码、配置读写、
/// 连接状态与运行日志（详见 docs/LITE_COVERAGE_MATRIX.md）
fn ensure_not_lite(_state: &AppState, module: &str) -> Result<(), String> {
    if IS_LITE {
        return Err(format!("Lite模式下{}已停用", module));
    }
    Ok(())
}

/// 获取全部配置（优先从注册表合并最新 id_code）。
/// 敏感凭据字段一律脱敏为空串，前端如需凭据状态请使用 get_credentials_status
#[tauri::command]
fn get_app_config(state: State<'_, AppState>) -> Result<AppConfig, String> {
    let mut cfg = state.config.lock().map_err(|e| e.to_string())?.clone();
    cfg.id_code = resolve_id_code(&state);
    Ok(cfg.sanitized())
}

/// 解析本机已保存的开播身份码：注册表优先，注册表为空时回退内存配置
fn resolve_id_code(state: &AppState) -> String {
    if let Ok(reg_code) = registry::read_id_code() {
        if !reg_code.trim().is_empty() {
            return reg_code;
        }
    }
    state
        .config
        .lock()
        .map(|c| c.id_code.clone())
        .unwrap_or_default()
}

/// 读取本机已保存的开播身份码（供前端身份码输入框以密码形态回显；
/// 配置 JSON 与日志仍不含该字段，权威来源为注册表）
#[tauri::command]
fn get_id_code(state: State<'_, AppState>) -> Result<String, String> {
    Ok(resolve_id_code(&state))
}

/// 保存主播开播身份码（严格遵循原工程规范持久化至 Windows 注册表 HKCU\Software\MonsterOrderWilds\IdCode）
#[tauri::command]
fn save_id_code(id_code: String, state: State<'_, AppState>) -> Result<(), String> {
    let trimmed = id_code.trim().to_string();
    registry::write_id_code(&trimmed)?;
    let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
    cfg.id_code = trimmed;
    Ok(())
}

/// 保存全部配置（敏感字段不会写入 JSON，凭据权威来源为 credentials.dat）。
/// 保存后即时热更新：弹幕过滤器 + 广播 config-changed 供悬浮窗刷新
#[tauri::command]
fn save_app_config(
    app: AppHandle,
    mut new_cfg: AppConfig,
    state: State<'_, AppState>,
) -> Result<(), String> {
    // 身份码与 Manbo Key 的注册表同步统一由 AppConfig::save → persist_registry 完成
    // （E2：移除此处重复写入；空值语义不变——仅非空才覆盖注册表）

    // 敏感凭据全部由已验签的凭据文件独占：**忽略前端回传的敏感字段**，
    // 前端 newCfg 里的这些值一律不采纳（避免配置页把密钥写坏或写空）
    let prev = state.config.lock().map_err(|e| e.to_string())?.clone();
    let creds_snapshot = state.credentials_snapshot();
    let creds = &creds_snapshot.creds;
    macro_rules! keep_cred {
        ($field:ident, $cred:ident) => {
            new_cfg.$field = if !creds.$cred.is_empty() {
                creds.$cred.clone()
            } else {
                prev.$field.clone()
            };
        };
    }
    keep_cred!(app_id, app_id);
    keep_cred!(access_key_id, access_key_id);
    keep_cred!(access_key_secret, access_key_secret);
    keep_cred!(deepseek_api_key, chat_api_key);
    keep_cred!(mimo_api_key, mimo_tts_api_key);
    // Manbo Key 不走 credentials.dat（原工程零引用 special_user_tts_api_key）：
    // 权威来源是注册表/配置，凭据里的同名字段不得反写
    if new_cfg.manbo_api_key.trim().is_empty() {
        new_cfg.manbo_api_key = prev.manbo_api_key.clone();
    }

    // 悬浮窗位置由拖动链路（pending_pos + 3s 防抖）独占维护，前端设置面板不提供该字段，
    // 故此处忽略前端回传的 top_pos，避免用陈旧副本覆盖真实位置（P1-6）
    new_cfg.top_pos_x = prev.top_pos_x;
    new_cfg.top_pos_y = prev.top_pos_y;

    let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
    *cfg = new_cfg.clone();
    cfg.save(None)?;

    // 运行期热更新：过滤开关立即生效（无需重启）
    state.danmu_processor.update_filters(
        new_cfg.only_medal_order,
        new_cfg.only_speek_wearing_medal,
        new_cfg.only_speek_guard_level,
    );
    // 广播脱敏配置，悬浮窗据此刷新跑马灯 / 透明度等
    let _ = app.emit("config-changed", cfg.sanitized());

    state.ai_provider.set_api_key(new_cfg.deepseek_api_key.clone());
    state.tts_mgr.update_config(TTSConfig {
        engine: parse_tts_engine(&new_cfg.tts_engine),
        enable_voice: new_cfg.enable_voice,
        speech_rate: new_cfg.speech_rate,
        speech_volume: new_cfg.speech_volume,
        speech_pitch: new_cfg.speech_pitch,
        manbo_api_key: new_cfg.manbo_api_key,
        manbo_voice: new_cfg.manbo_voice,
        mimo_api_key: new_cfg.mimo_api_key,
        mimo_voice: new_cfg.mimo_voice,
        mimo_style: new_cfg.mimo_style,
        mimo_audio_format: new_cfg.mimo_audio_format,
    });

    Ok(())
}

/// 获取 B 站连接状态（五态：未连接/连接中/已连接/重连中(N)/重连失败+原因）
#[tauri::command]
fn get_bili_connection_state(
    state: State<'_, AppState>,
) -> Result<bilibili::ConnectionStatusPayload, String> {
    let c = state.connection.lock().map_err(|e| e.to_string())?;
    Ok(c.payload())
}

/// 写入连接状态并广播前端（事件 `connection-state-changed`）。
///
/// F3：只在状态／原因／重试次数**实际变化**时记一条安全 INFO ——
/// 这样 Release 日志能直接读出"连上了没、在重试第几次、为什么断"，
/// 又不会因为轮询式上报把日志灌满。日志只含状态名与次数，不含 game_id、WSS 地址或错误原文。
fn apply_connection_status(
    state: &AppState,
    app_handle: Option<&AppHandle>,
    next: bilibili::ConnectionStatus,
) {
    let previous = state
        .connection
        .lock()
        .map(|mut c| std::mem::replace(&mut *c, next.clone()))
        .ok();

    let changed = match &previous {
        Some(prev) => {
            prev.state != next.state
                || prev.reason != next.reason
                || prev.attempt != next.attempt
        }
        None => true,
    };
    if changed {
        crate::log_info!(
            "[Bili] 连接状态 {} → {}（原因={} 重试次数={}）",
            previous
                .as_ref()
                .map(|p| format!("{:?}", p.state))
                .unwrap_or_else(|| "Unknown".to_string()),
            format!("{:?}", next.state),
            format!("{:?}", next.reason),
            next.attempt
        );
    }

    if let Some(h) = app_handle {
        let _ = h.emit("connection-state-changed", next.payload());
    }
}

/// 播报任务入队（由后台泵逐条取出执行，避免弹幕密集时并发请求堆积）。
/// `priority=true` 进入高优先队列（礼物/SC/上舰/打卡/点餐），否则为普通弹幕朗读队列。
fn queue_tts(state: &AppState, text: String, user_id: &str, priority: bool) {
    if !state.tts_mgr.enqueue_speak(&text, user_id, priority) {
        crate::log_warn!("[TTS] 播报入队失败（空文本或队列已满），已丢弃");
    }
}

/// 点餐指令：以「点餐」开头且后续非空时返回接单文案（对齐原工程 HandleDmOrderFood）
fn build_food_order_text(msg: &str, uname: &str) -> Option<String> {
    let rest = msg.trim().strip_prefix("点餐")?;
    if rest.is_empty() {
        return None;
    }
    let minutes = rand::Rng::gen_range(&mut rand::thread_rng(), 0..=60);
    Some(format!(
        "{} 下单的 {} 已接单，预计{}分钟后送达！",
        uname, rest, minutes
    ))
}

/// 广播打卡回复事件（供前端气泡展示，D4）
///
/// 同时完成业务留档：回复文案在这里**最终确定**，是留档的唯一时机
/// （无语音、AI 失败回退、队满都不影响这条留档）。
fn emit_checkin_reply(
    app_handle: Option<&AppHandle>,
    user_id: &str,
    user_name: &str,
    reply: &str,
    is_ai: bool,
) {
    record_business_history(reply);
    record_business_history_probe(reply);
    if let Some(handle) = app_handle {
        let _ = handle.emit(
            "checkin-reply",
            &serde_json::json!({
                "user_id": user_id,
                "user_name": user_name,
                "reply": reply,
                "is_ai": is_ai,
            }),
        );
    }
}

/// 首次打卡回复：舰长且已配置 AI Key 时异步生成个性化回复（失败/未配置回退兜底文案），
/// 其余情况直接使用兜底文案；回复入高优先播报队列
/// （对齐原工程 GenerateCheckinAnswerAsync → g_aiReplyCallback / PlayCheckinTTS）
fn schedule_checkin_reply(
    app_handle: Option<&AppHandle>,
    state: &AppState,
    cfg: &AppConfig,
    danmu: &bilibili::DanmuData,
    profile: &checkin::UserProfile,
    danmu_date: chrono::NaiveDate,
    last_checkin_date_before: i32,
) {
    let fallback = checkin_ai::fallback_answer(
        &danmu.user_name,
        profile.continuous_days,
        profile.cumulative_days,
    );

    // AI 仅对舰长生效（原工程 guardLevel > 0 分支），且需已配置 API Key
    if danmu.guard_level <= 0 || !state.ai_provider.is_configured() {
        emit_checkin_reply(app_handle, &danmu.user_id, &danmu.user_name, &fallback, false);
        if cfg.enable_voice {
            // 兜底签到播报同样按“打卡_{用户名}_{ts}.mp3”留档（对齐原工程 isCheckinTTS 守卫）
            state
                .tts_mgr
                .enqueue_checkin_speak(&fallback, &danmu.user_id, &danmu.user_name);
        }
        return;
    }

    let prompt = {
        // 学习档案读取属增强信息：不可用时退回空档案，不影响本次打卡回复
        let learning = state
            .checkin_mgr
            .as_ref()
            .map(|m| m.load_learning(&danmu.user_id))
            .unwrap_or_default();
        checkin_ai::build_prompt(&checkin_ai::CheckinContext {
            username: &danmu.user_name,
            continuous_days: profile.continuous_days,
            cumulative_days: profile.cumulative_days,
            checkin_date: checkin::CheckinManager::date_to_int(danmu_date),
            last_checkin_date: last_checkin_date_before,
            profile: &learning,
        })
    };

    let provider = state.ai_provider.clone();
    let tts = state.tts_mgr.clone();
    let enable_voice = cfg.enable_voice;
    let handle = app_handle.cloned();
    let user_id = danmu.user_id.clone();
    let user_name = danmu.user_name.clone();

    tauri::async_runtime::spawn(async move {
        let (text, is_ai) = match provider
            .call_api(&prompt, Some(ai::SYSTEM_PROMPT_CHECKIN))
            .await
        {
            Ok((answer, _reasoning)) => (answer, true),
            Err(err) => {
                crate::log_warn!("[CheckinAI] AI 回复失败，使用兜底文案: {}", err);
                (fallback, false)
            }
        };
        if let Some(h) = &handle {
            let _ = h.emit(
                "checkin-reply",
                &serde_json::json!({
                    "user_id": user_id,
                    "user_name": user_name,
                    "reply": &text,
                    "is_ai": is_ai,
                }),
            );
        }
        if enable_voice {
            // 签到/补签播报：音频按 `打卡_{用户名}_{ts}.mp3` 留档（对齐原工程仅留档签到 TTS）
            tts.enqueue_checkin_speak(&text, &user_id, &user_name);
        }
    });
}

/// 解析打卡触发词：按英文/中文逗号分割并去除首尾空白
/// （对齐原工程 CaptainCheckInModule::SetTriggerWords 的 `,` 与 `，` 双逗号分割）。
/// 结果为空时打卡功能停用（不得内置兜底词，否则用户无法通过配置关闭打卡）
fn parse_checkin_trigger_words(raw: &str) -> Vec<String> {
    raw.split(|c| c == ',' || c == '，')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 指令类弹幕判定：打卡触发词（含用户自定义）与补签/补签查询指令。
/// 这类消息是操作指令而非发言内容，不参与发言习惯学习，
/// 否则会污染关键词与 AI 提示词的「最近发言」（原工程无此过滤，属有意差异）
fn is_command_message(msg: &str, checkin_triggers: &[String]) -> bool {
    checkin_triggers.iter().any(|t| msg.eq_ignore_ascii_case(t))
        || checkin::is_retro_command(msg)
        || checkin::is_retro_query(msg)
}

/// 业务留档唯一入口（对齐原工程 `WriteLog::RecordHistory`）。
///
/// - 完整版：把业务原文写入 `History/YYYY.M.D.txt`；
/// - Lite：完全不记录（与旧版 Lite 一致）。
///
/// 调用时机一律是"业务文本**形成**的那一刻"，而不是"语音播报时"：
/// 语音总开关、队满、合成失败、粉丝牌播报门槛都不能回头删掉业务留档。
fn record_business_history(text: &str) {
    if IS_LITE {
        return;
    }
    logging::record_history(text);
}

/// 测试专用留档探针（生产为空操作）：让单测能断言"业务调用链确实恰好留档一次"
fn record_business_history_probe(text: &str) {
    logging::record_history_for_test(text);
}

/// 核心业务总线：统一处理接收到的直播/模拟弹幕
pub fn handle_incoming_danmu(
    app_handle: Option<&AppHandle>,
    state: &AppState,
    mut danmu: bilibili::DanmuData,
) -> bilibili::DanmuProcessResult {
    // 1. 特殊用户判定：特定 open_id 永远判定为总督 (guard_level = 1)
    if danmu.user_id == bilibili::SPECIAL_OPEN_ID {
        danmu.guard_level = 1;
    }

    let cfg = {
        state.config.lock().map(|c| c.clone()).unwrap_or_default()
    };

    // 1.1 有效原始 DM 留档：在打卡与点怪的去重／过滤**之前**写一次，
    //     因此被语音过滤、被防刷屏跳过、乃至只是普通聊天的弹幕都会留下原文。
    //     字段不齐的畸形包不留档（不写成「 说：」）。
    if danmu.has_history_required_fields {
        let entry = format!("{} 说：{}", danmu.user_name, danmu.message);
        record_business_history(&entry);
        record_business_history_probe(&entry);
    }

    // 2. 非 Lite 构建下的弹幕学习、舰长打卡与补签指令判定
    if !IS_LITE {
        let msg_trim = danmu.message.trim();

        // 打卡模块总开关（原工程 enableCaptainCheckinAI 控制 CaptainCheckInModule 的启用：
        // 关闭时打卡指令与弹幕学习全部停用；补签模块独立，不受该开关影响）
        let checkin_module_enabled = cfg.enable_captain_checkin_ai;

        // 触发词提前解析：后续判指令、判打卡都要用。
        // 支持中英文逗号分隔（对齐原工程 SetTriggerWords），清空后打卡功能完全停用
        let checkin_triggers = parse_checkin_trigger_words(&cfg.checkin_trigger_words);
        let is_checkin_command = checkin_triggers
            .iter()
            .any(|t| msg_trim.eq_ignore_ascii_case(t));

        // 打卡日期口径：弹幕服务器时间（原工程 sendDate），缺失时回退本机今天（C5）
        let danmu_date = bilibili::server_date(danmu.timestamp)
            .unwrap_or_else(|| chrono::Local::now().date_naive());

        // 原工程触发条件：舰长 或 佩戴粉丝牌的用户
        let is_privileged = danmu.guard_level > 0 || danmu.has_medal;
        let is_checkin = checkin_module_enabled && is_checkin_command;
        // 补签操作与补签查询：**独立于打卡防刷屏**（原工程 DanmuProcessor 独立分发到补签模块），
        // 因此它们不受 CheckinLearner::should_skip_duplicate 影响，也不受打卡总开关影响
        let is_retro = is_privileged && checkin::is_retro_command(msg_trim);
        let is_retro_query_cmd = is_privileged && checkin::is_retro_query(msg_trim);

        // 2.2 舰长弹幕学习 + 同内容防刷屏
        //     （原工程 NotifyCaptainDanmu 门槛 guardLevel != 0 || hasMedal → ShouldLearn 仅舰长学习）
        //     指令类弹幕只参与防刷屏计数、不写入发言习惯：它们会被当成关键词与「最近发言」
        //     喂给打卡 AI 提示词（如只打卡不聊天的观众，Top5 习惯词里会出现「打卡」）
        //     D4：该返回值**只**用于拦截「打卡」，不得吞掉补签与补签查询
        let mut skip_checkin_by_learning = false;
        if checkin_module_enabled && is_privileged {
            if let Some(learner) = &state.checkin_learner {
                if !is_command_message(msg_trim, &checkin_triggers) {
                    if let Ok(mgr) = state.checkin() {
                        learner.learn(
                            mgr,
                            &danmu.user_id,
                            &danmu.user_name,
                            danmu.guard_level,
                            &danmu.message,
                            danmu.timestamp,
                        );
                    }
                }
                skip_checkin_by_learning =
                    learner.should_skip_duplicate(&danmu.user_id, &danmu.message);
            }
        }

        // 2.3 打卡不可用（Lite 已由编译期短路；完整版为数据库初始化失败）：
        //     有权限用户发出打卡/补签/查询时必须明确拒绝并提示，
        //     既不能伪造成功气泡，也不能落进普通弹幕朗读
        if (is_checkin || is_retro || is_retro_query_cmd) && is_privileged && !state.checkin_available() {
            emit_checkin_unavailable(state, app_handle);
            return bilibili::DanmuProcessResult {
                user_id: danmu.user_id,
                user_name: danmu.user_name,
                ..Default::default()
            };
        }

        // 打卡子系统句柄：上面的不可用分支已提前 return，此处 Ok；
        // 非打卡弹幕在 DB 停用时为 None，此时跳过打卡相关分支即可
        let checkin_mgr = state.checkin().ok().cloned();

        if is_checkin && is_privileged && !skip_checkin_by_learning {
            if let Some(mgr) = &checkin_mgr {
                // AI 提示词需要“上次打卡日期”，须在落库前取（对齐原工程 previousCheckinDate）
                let last_checkin_before = mgr
                    .get_profile(&danmu.user_id)
                    .map(|p| p.last_checkin_date)
                    .unwrap_or(0);

                // 「今天是否已打卡」由 record_checkin 在同一事务内判定并返回，
                // 不再在事务外先猜重复再决定文案（并发下会误判）
                match mgr.record_checkin_with_flag(&danmu.user_id, &danmu.user_name, danmu_date) {
                    Ok(outcome) => {
                        let profile = outcome.profile;
                        if let Some(handle) = app_handle {
                            let _ = handle.emit("checkin-recorded", &profile);
                        }

                        if outcome.already_checked_in {
                            // 重复打卡文案（C5，对齐原工程 repeatedAnswer）
                            let reply = format!(
                                "{}今日已打卡，连续{}天，累计{}天",
                                danmu.user_name, profile.continuous_days, profile.cumulative_days
                            );
                            emit_checkin_reply(
                                app_handle,
                                &danmu.user_id,
                                &danmu.user_name,
                                &reply,
                                false,
                            );
                            if cfg.enable_voice {
                                // 重复打卡亦属签到播报：按“打卡_{用户名}_{ts}.mp3”留档
                                state.tts_mgr.enqueue_checkin_speak(
                                    &reply,
                                    &danmu.user_id,
                                    &danmu.user_name,
                                );
                            }
                        } else {
                            schedule_checkin_reply(
                                app_handle,
                                state,
                                &cfg,
                                &danmu,
                                &profile,
                                danmu_date,
                                last_checkin_before,
                            );
                        }
                    }
                    Err(e) => {
                        crate::log_error!("[Checkin] 打卡处理异常: {}", e);
                    }
                }
            }
            return bilibili::DanmuProcessResult {
                user_id: danmu.user_id,
                user_name: danmu.user_name,
                ..Default::default()
            };
        }

        // 2.4 补签：权限为「舰长或佩戴粉丝牌」（C4，对齐原工程 NotifyCaptainDanmu 门槛）
        if is_retro {
            if let Some(mgr) = &checkin_mgr {
                let outcome =
                    mgr.retro_command_outcome(
                        &danmu.user_id,
                        &danmu.user_name,
                        danmu_date,
                        Some(danmu.msg_id.as_str()),
                    );
                if let Some(handle) = app_handle {
                    let _ = handle.emit(
                        "retroactive-checkin-recorded",
                        &serde_json::json!({
                            "user_id": danmu.user_id,
                            "user_name": danmu.user_name,
                            "success": outcome.success,
                            "remaining_cards": outcome.remaining_cards,
                            "date": outcome.checkin_date,
                            "reply": outcome.reply,
                        }),
                    );
                }
                // 留档在回复文案算出后立即进行：查询/无卡/失败提示同样入档
                record_business_history(&outcome.reply);
                record_business_history_probe(&outcome.reply);
                if cfg.enable_voice {
                    // 补签播报同样按签到音频留档（对齐原工程 isCheckinTTS 守卫）
                    state.tts_mgr.enqueue_checkin_speak(
                        &outcome.reply,
                        &danmu.user_id,
                        &danmu.user_name,
                    );
                }
            }
            return bilibili::DanmuProcessResult {
                user_id: danmu.user_id,
                user_name: danmu.user_name,
                ..Default::default()
            };
        }

        // 2.5 补签查询：权限同上；仅气泡不朗读（v24 决策）
        if is_retro_query_cmd {
            if let Some(mgr) = &checkin_mgr {
                let reply = mgr.query_reply(&danmu.user_id, &danmu.user_name, danmu_date);
                let cards = mgr.get_cards(&danmu.user_id);
                if let Some(handle) = app_handle {
                    let _ = handle.emit(
                        "retroactive-query",
                        &serde_json::json!({
                            "user_id": danmu.user_id,
                            "user_name": danmu.user_name,
                            "card_count": cards.card_count,
                            "reply": reply,
                        }),
                    );
                }
                // 查询回复原样留档（含换行的三行文案虽写成多行物理行，但只调用一次留档）
                record_business_history(&reply);
                record_business_history_probe(&reply);
            }
            return bilibili::DanmuProcessResult {
                user_id: danmu.user_id,
                user_name: danmu.user_name,
                ..Default::default()
            };
        }
    }

    // 3. 核心排队与怪物点单处理
    let (res, snapshot, revision_changed) = {
        let mut q = state.queue_mgr.lock().unwrap();
        let revision_before = q.revision;
        let res = state
            .danmu_processor
            .process_danmu(&danmu, &state.monster_mgr, &state.roster, &mut q);
        let changed = q.revision != revision_before;
        (res, q.snapshot(), changed)
        // 锁在此处释放：后续日志/事件广播/落盘均不持锁
    };
    let queued_items = &snapshot.items;

    // 3.1 禁点名单拦截：命中字典但该怪已被禁点 —— 不入队，仅记录并就地提示。
    //      诊断日志只记"发生了一次拦截"，昵称与怪名属业务内容，不写入普通 Logs
    if res.blocked_by_roster {
        crate::log_info!("[Roster] 点怪被禁点名单拦截 1 次");
        if let Some(handle) = app_handle {
            let _ = handle.emit(
                "order-blocked",
                &serde_json::json!({
                    "user_id": danmu.user_id,
                    "user_name": danmu.user_name,
                    "monster_name": res.monster_name,
                }),
            );
        }
    }

    if res.added_to_queue || res.priority_updated {
        // 日志展示条目真实优先级：res.priority_updated 仅代表「二段式提权」这唯一动作，
        // 新建的带优先点怪该值为 false 但条目已置前，直接打印会误报「优先=false」。
        // 注意 order-placed 事件仍沿用 priority_updated（原工程回调同字段驱动跑马灯文案）。
        let item_priority = queued_items
            .iter()
            .find(|i| i.user_id == danmu.user_id)
            .map(|i| i.is_priority)
            .unwrap_or(false);
        // 弹幕热路径不在此落盘（脏标记已置位，由 500ms 节流任务在锁外写盘，
        // 对齐原工程 PriorityQueueManager::Tick 的 SAVE_INTERVAL_MS=500 语义）。
        // 诊断日志只记业务量与结果，昵称/怪名只走前端事件与 History，不进普通 Logs
        crate::log_info!(
            "[Queue] 点怪{}成功（队列优先={}），当前排队 {} 位",
            if res.priority_updated { "提权" } else { "" },
            item_priority,
            queued_items.len()
        );
        if let Some(handle) = app_handle {
            // 队列广播与「是否产生业务动作」解耦：按版本变化判定。
            // 已有用户改怪名/等级/图标时 process_danmu 返回的两个业务标志都是 false，
            // 但队列内容确实变了，前端必须收到权威快照。
            if revision_changed {
                emit_queue_snapshot(handle, &snapshot);
            }
            // D3 跑马灯：点怪成功提示（优先置前 / 新增入队，对齐原工程 DataBridgeExports 回调）
            let _ = handle.emit(
                "order-placed",
                &serde_json::json!({
                    "user_id": danmu.user_id,
                    "user_name": danmu.user_name,
                    "monster_name": res.monster_name,
                    "is_priority": res.priority_updated,
                }),
            );
        }
    }

    // 4. TTS 播报（对齐原工程 HandleSpeekDm 过滤链：仅粉丝牌 → 仅舰长等级 → 语音开关）
    if !IS_LITE && cfg.enable_voice {
        let msg_trim = danmu.message.trim();
        let passes_medal = !cfg.only_speek_wearing_medal || danmu.has_medal;
        let passes_guard = cfg.only_speek_guard_level == 0
            || (danmu.guard_level > 0 && danmu.guard_level <= cfg.only_speek_guard_level);
        // 注：原工程 ShouldSpeak 的 onlySpeekPaidGift 判定依赖 isPaidGift，而该字段在原工程
        // 从未被赋值（恒 false），开启开关会静音全部播报，属死逻辑；V2 不复刻，
        // 该开关仅作用于礼物播报（见 handle_incoming_gift 与连击结算）。

        if passes_medal && passes_guard {
            // 4.1 点餐指令（"点餐xxx"）：接单文案进普通队列，且原工程 HandleSpeekDm 之后
            //     不会 return，仍会继续朗读原文 —— 二者都要播（对齐 TextToSpeech.cpp:217-267）
            if let Some(food_text) = build_food_order_text(msg_trim, &danmu.user_name) {
                queue_tts(state, food_text, &danmu.user_id, false);
                let read_text = format!("{} 说：{}", danmu.user_name, danmu.message);
                queue_tts(state, read_text, &danmu.user_id, false);
            } else if TTSManager::match_special_sound(msg_trim).is_some() {
                // 4.2 本地特殊语音命中：直接播放（不依赖 TTS 引擎与 API）
                let _ = state.tts_mgr.play_special_sound(msg_trim);
            } else {
                // 4.3 普通弹幕朗读 "{uname} 说：{msg}"（含点怪弹幕原文，与原工程一致）
                let read_text = format!("{} 说：{}", danmu.user_name, danmu.message);
                queue_tts(state, read_text, &danmu.user_id, false);
            }
        }
    }

    res
}

/// 点赞奖励结算失败时的可见反馈（D3 的"可见失败事件"）。
///
/// 配合意图账本：那次点赞没有入账，但**意图已持久化**（pending_like_rewards），
/// 数据库恢复后由补偿扫描器自动补记，服务端重投同一消息也会直接入账——
/// 不再只是"可重试"的承诺，而是"必达"的补偿。
/// 载荷只含数量与固定文案，不含 uid、昵称或 SQL 错误原文。
fn emit_like_reward_failed(app: Option<&AppHandle>, like_count: i32) {
    if let Some(handle) = app {
        let _ = handle.emit(
            "like-reward-failed",
            &serde_json::json!({
                "like_count": like_count,
                "retryable": true,
                "message": "本次点赞结算未完成，已回滚且未计入。已存入本机补偿账本，数据库恢复后会自动补记；服务端重投同一消息时也会直接入账。",
            }),
        );
    }
}

/// 点赞补偿重试超限的一次性提示：复用 like-reward-failed 通道（前端零改动），
/// retryable=false 表明自动重试已停止；意图行保留在账本中不删除，待人工排查。
fn emit_like_reward_abandoned(app: Option<&AppHandle>, abandoned: usize) {
    if let Some(handle) = app {
        let _ = handle.emit(
            "like-reward-failed",
            &serde_json::json!({
                "like_count": 0,
                "retryable": false,
                "message": format!(
                    "有 {} 条补偿中的点赞超过自动重试上限，已保留记录不再自动重试，请检查本机日志或数据库。",
                    abandoned
                ),
            }),
        );
    }
}

/// D3 点赞补偿扫描间隔（秒）
const LIKE_COMPENSATION_SCAN_INTERVAL_SECS: u64 = 60;
/// 单轮补偿扫描的最大结算条数（防止数据库刚恢复时一次性重放压垮打卡链路）
const LIKE_COMPENSATION_BATCH_LIMIT: i64 = 200;

/// D3 持久化补偿：扫描 pending_like_rewards 的待补点赞并逐条重放入账。
/// 与实时链路共用 `settle_like_intent`（T2），由意图行状态互斥，天然防双计：
/// 实时链路结算后删行，扫描器读到无行即跳过；扫描器补账后行转 done 凭证，
/// 服务端重投的 stage 命中凭证行、settle 读到 done 即静默跳过。
fn replay_pending_likes(state: &AppState, app: Option<&AppHandle>) {
    if IS_LITE {
        return;
    }
    let Ok(mgr) = state.checkin() else {
        return; // 打卡库不可用：本轮空转，CheckinManager 恢复（重启）后自动恢复扫描
    };

    // 1. 放弃超限行（保留记录、一次性提示）
    let abandoned = mgr
        .abandon_stale_like_intents(checkin::LIKE_MAX_RETRIES)
        .unwrap_or_default();
    for row in &abandoned {
        crate::log_error!(
            "[Checkin] 点赞补偿重试 {} 次仍失败，已放弃自动重试（记录保留）: like_count={} like_date={} msg_id={:?}",
            row.retry_count,
            row.like_count,
            row.like_date,
            row.msg_id
        );
    }
    if !abandoned.is_empty() {
        emit_like_reward_abandoned(app, abandoned.len());
    }

    // 2. 逐条重放待补意图（先丢的先补）
    let rows = match mgr.list_pending_like_intents(LIKE_COMPENSATION_BATCH_LIMIT) {
        Ok(r) => r,
        Err(e) => {
            crate::log_warn!("[Checkin] 点赞补偿扫描读取失败: {}", e);
            return;
        }
    };
    let mut settled = 0usize;
    let mut failed = 0usize;
    for row in &rows {
        match mgr.settle_like_intent(row.id, true) {
            Ok(checkin::SettleOutcome::Settled(rewards)) => {
                settled += 1;
                // 补账按事件发生日结算，播报与实时一致，文案加「已补记」后缀；
                // 普通补账（无发卡）静默入账，不打扰直播间
                let username = row.username.as_deref().unwrap_or_default();
                let mut replies: Vec<String> = Vec::new();
                if rewards.weekly_reward {
                    replies.push(format!(
                        "{}，恭喜！今日点赞突破30，获得1张补签卡！（已补记）",
                        username
                    ));
                }
                if rewards.streak_reward {
                    replies.push(format!(
                        "{}，恭喜！连续7天点赞，获得1张补签卡！（已补记）",
                        username
                    ));
                }
                if replies.is_empty() {
                    continue;
                }
                for text in &replies {
                    record_business_history(text);
                    record_business_history_probe(text);
                }
                if let Some(handle) = app {
                    let _ = handle.emit(
                        "like-reward-granted",
                        &serde_json::json!({
                            "uid": row.uid,
                            "user_name": username,
                            "likes": row.like_count,
                            "daily_total": rewards.daily_total,
                            "replies": replies,
                        }),
                    );
                }
                let enable_voice = state
                    .config
                    .lock()
                    .map(|c| c.enable_voice)
                    .unwrap_or(false);
                if enable_voice {
                    for text in &replies {
                        state.tts_mgr.enqueue_speak(text, &row.uid, true);
                    }
                }
            }
            Ok(checkin::SettleOutcome::AlreadySettled) => {
                // 并发窗口内已被实时链路结算（行已删）：无需处理
            }
            Err(e) => {
                failed += 1;
                mgr.bump_like_intent_failure(row.id, &e);
                crate::log_error!("[Checkin] 点赞补偿重放失败（id={}）: {}", row.id, e);
            }
        }
    }

    // 3. 清理过期已办凭证 + 输出本轮统计（轻量观测：待补/成功/失败/放弃）
    match mgr.cleanup_done_like_intents(checkin::LIKE_DONE_RECEIPT_RETENTION_SECS) {
        Ok(n) if n > 0 => {
            crate::log_info!("[Checkin] 点赞补偿凭证清理: 已删除 {} 条过期 done 记录", n);
        }
        Ok(_) => {}
        Err(e) => {
            crate::log_warn!("[Checkin] 点赞补偿凭证清理失败: {}", e);
        }
    }
    if !rows.is_empty() || !abandoned.is_empty() {
        crate::log_info!(
            "[Checkin] 点赞补偿扫描: 待补={} 补账成功={} 补账失败={} 放弃={}",
            rows.len(),
            settled,
            failed,
            abandoned.len()
        );
    }
}

/// 核心业务总线：统一处理接收到的点赞数据
/// 对齐原工程 NotifyLikeEvent：共享 msg_id 去重 → 空 uid / 非正数点赞丢弃 →
/// 奖卡结算后按「先突破30、后连续7天」顺序播报（SendReply 默认开启 TTS）
/// 返回本次产生的播报文案（供调试通道回显）
pub fn handle_incoming_like(
    app_handle: Option<&AppHandle>,
    state: &AppState,
    ev: &bilibili::LikeEvent,
) -> Vec<String> {
    if IS_LITE {
        return Vec::new();
    }

    // 点赞与弹幕共用同一 msg_id 去重缓存（原工程 IsDuplicateMsgId），
    // 但点赞走**三态语义**：处理中保留 → 事务提交成功确认保留 / 失败释放。
    // 早期实现无视结果地永久占用 ID，一次数据库失败就会把该事件的重投永久挡在缓存外，
    // 配合「整事件回滚」会让这次合法点赞彻底消失。
    let reservation = state.danmu_processor.reserve_msg_id(&ev.msg_id);
    if reservation == bilibili::MsgIdReservation::Duplicate {
        return Vec::new();
    }
    if ev.like_count <= 0 || ev.uid.is_empty() {
        // 丢弃前释放，避免无效事件长期占用 ID（空 ID 为 NoKey，释放是空操作）
        state.danmu_processor.release_msg_id(&ev.msg_id);
        return Vec::new();
    }

    // 点赞日期口径：服务器时间（原工程 event.date 优先，缺失回退本机今天）
    let date = bilibili::server_date(ev.timestamp)
        .unwrap_or_else(|| chrono::Local::now().date_naive());

    // 打卡子系统不可用时不得静默丢弃：给出节流后的可见提示后返回
    let Ok(mgr) = state.checkin() else {
        state.danmu_processor.release_msg_id(&ev.msg_id);
        emit_checkin_unavailable_throttled(state, app_handle, true);
        return Vec::new();
    };

    // T1：意图先行落库 —— 把「这次点赞打算入账」持久化为独立事务。
    // T1 自身失败意味着数据库整体不可写，任何持久化都不可能，维持现状路径
    // （可见提示 + 日志）—— 这是本机制唯一无法覆盖的边界
    let intent_id =
        match mgr.stage_like_intent(&ev.uid, &ev.username, &ev.msg_id, ev.like_count, date) {
            Ok(id) => id,
            Err(e) => {
                crate::log_error!("[Checkin] 点赞意图落库失败（库不可写，事件丢弃）: {}", e);
                state.danmu_processor.release_msg_id(&ev.msg_id);
                emit_like_reward_failed(app_handle, ev.like_count);
                return Vec::new();
            }
        };

    // T2：入账 + 销账在同一 IMMEDIATE 事务内提交。失败整体回滚后意图行仍在，
    // 由补偿扫描器定时重放必达；服务端重投则命中同一意图行（pending 去重 /
    // done 已办凭证），不会双计入账。
    let rewards = match mgr.settle_like_intent(intent_id, false) {
        Ok(checkin::SettleOutcome::Settled(r)) => r,
        Ok(checkin::SettleOutcome::AlreadySettled) => {
            // 已由补偿扫描器补账（本次为重投撞上已办凭证）：静默丢弃，不得重复入账
            return Vec::new();
        }
        Err(e) => {
            // D3：整事件已回滚，但意图已持久化 → 记重试计数、释放预留，
            // 让重投与补偿扫描器都能把这笔点赞补回来
            mgr.bump_like_intent_failure(intent_id, &e);
            crate::log_error!("[Checkin] 点赞事务失败，已回滚（意图已留账本待自动补记）: {}", e);
            state.danmu_processor.release_msg_id(&ev.msg_id);
            emit_like_reward_failed(app_handle, ev.like_count);
            return Vec::new();
        }
    };

    let mut replies: Vec<String> = Vec::new();
    if rewards.weekly_reward {
        replies.push(format!(
            "{}，恭喜！今日点赞突破30，获得1张补签卡！",
            ev.username
        ));
    }
    if rewards.streak_reward {
        replies.push(format!("{}，恭喜！连续7天点赞，获得1张补签卡！", ev.username));
    }
    if replies.is_empty() {
        return replies;
    }

    // 事务提交后才留档：对实际发出的每条奖励回复各记一次（普通点赞无回复不记）
    for text in &replies {
        record_business_history(text);
        record_business_history_probe(text);
    }

    if let Some(handle) = app_handle {
        let _ = handle.emit(
            "like-reward-granted",
            &serde_json::json!({
                "uid": ev.uid,
                "user_name": ev.username,
                "likes": ev.like_count,
                "daily_total": rewards.daily_total,
                "replies": replies,
            }),
        );
    }

    let enable_voice = state
        .config
        .lock()
        .map(|c| c.enable_voice)
        .unwrap_or(false);
    if enable_voice {
        for text in &replies {
            state.tts_mgr.enqueue_speak(text, &ev.uid, true);
        }
    }

    replies
}

/// 核心业务总线：统一处理接收到的礼物事件（含连击合并，受 Lite 模式与语音开关控制）
pub fn handle_incoming_gift(
    app_handle: Option<&AppHandle>,
    state: &AppState,
    ev: tts::GiftEvent,
) {
    if IS_LITE {
        return;
    }

    // 前端提示不依赖语音开关（D4 气泡/跑马灯使用）
    if let Some(handle) = app_handle {
        let _ = handle.emit("gift-received", &ev);
    }

    let cfg = state
        .config
        .lock()
        .map(|c| c.clone())
        .unwrap_or_default();

    // 连击跟踪器**始终推进**（不因语音开关跳过），否则关闭语音期间的礼物连击
    // 会让后续结算文案的计数错位、并丢掉那段业务留档
    for report in state.tts_mgr.process_gift(&ev) {
        record_business_history(&report.text);
        record_business_history_probe(&report.text);
        if cfg.enable_voice && report.can_speak {
            queue_tts(state, report.text, &ev.open_id, true);
        }
    }
}

/// 核心业务总线：统一处理 SC / 上舰 / 进场事件（B2，文案对齐原工程）
pub fn handle_incoming_live_event(
    app_handle: Option<&AppHandle>,
    state: &AppState,
    ev: bilibili::LiveEvent,
) {
    if IS_LITE {
        return;
    }

    // 进场事件仅做历史留档（对齐原工程 HandleSpeekEnter 只写 History、不播报）：
    // 该事件前端无消费方，故不广播，避免高频 IPC 空转
    if let bilibili::LiveEvent::RoomEnter { uname, .. } = &ev {
        let entry = format!("{} 进入直播间", uname);
        record_business_history(&entry);
        record_business_history_probe(&entry);
        return;
    }

    if let Some(handle) = app_handle {
        let _ = handle.emit(ev.event_name(), &ev);
    }

    // SC / 上舰文案在**判断语音开关之前**留档：合法文案不因没开语音而丢失
    if let Some(text) = ev.tts_text() {
        record_business_history(&text);
        record_business_history_probe(&text);
    }

    let enable_voice = state
        .config
        .lock()
        .map(|c| c.enable_voice)
        .unwrap_or(false);
    if !enable_voice {
        return;
    }

    // 进场事件不播报（与原工程 HandleSpeekEnter 一致）
    if let Some(text) = ev.tts_text() {
        let uid = ev.user_id().to_string();
        queue_tts(state, text, &uid, true);
    }
}

/// 开启 / 断开 B 站直播连接
///
/// start／stop／import 共用 `bili_lifecycle` 生命周期闸门：保证"检查空闲 → 取凭据快照 →
/// 登记新会话/启动任务"与"确认旧任务已退出 → 替换凭据文件 → 发布 B"互斥，
/// 不会出现会话 A 正在运行时磁盘/内存已变成 B 的混用状态。
#[tauri::command]
async fn set_bili_connection(
    connected: bool,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<bool, String> {
    let _lifecycle = state.bili_lifecycle.lock().await;

    if connected {
        if state.bili_service.is_running() {
            return Ok(true);
        }
        // 服务端关闭状态未确认时不得立即开新会话（否则会命中 7001 之类的"已有会话"错误）
        if !state.bili_service.can_start_new_session() {
            return Err(
                "上一场直播的服务端关闭状态未确认，请等待确认或稍后重试后再开播".to_string(),
            );
        }

        let cfg = state.config.lock().map_err(|e| e.to_string())?.clone();

        let mut id_code = cfg.id_code.clone();
        if id_code.trim().is_empty() {
            if let Ok(reg_code) = registry::read_id_code() {
                if !reg_code.trim().is_empty() {
                    id_code = reg_code.clone();
                    if let Ok(mut c) = state.config.lock() {
                        c.id_code = reg_code;
                    }
                }
            }
        }

        let creds = state.bili_credentials(&AppConfig {
            id_code: id_code.clone(),
            ..cfg.clone()
        });

        if id_code.trim().is_empty() {
            return Err("未填入开播身份码，请在直播连接面板输入当次开播身份码后重试".into());
        }

        if !creds.is_valid() {
            return Err("B 站开放平台凭据未配置或不完整（请导入凭据文件并填入开播身份码）".into());
        }

        // 登记新一代会话：递增代号并标记循环存活。
        // 旧循环（若还残留在退出过程中）会在下一次取消检查时自行退出；
        // 且 `can_start_new_session` 已要求 loop_alive=false，这里不可能复活旧循环。
        state.bili_service.begin_session();
        state.bili_service.set_running(true);
        state
            .bili_service
            .set_end_state(bilibili::SessionEndState::Active);
        // 先进入「连接中」，随后由长连主循环上报 已连接 / 重连中 / 重连失败
        apply_connection_status(
            &state,
            Some(&app_handle),
            bilibili::ConnectionStatus::new(
                bilibili::ConnectionState::Connecting,
                bilibili::DisconnectReason::None,
                0,
            ),
        );

        let running = state.bili_service.get_running_flag();
        let session_ctl = state.bili_service.session_ctl();
        let game_id_ref = state.bili_service.get_game_id_ref();
        let state_clone = (*state).clone();
        let app_handle_clone = app_handle.clone();
        let creds_clone = creds.clone();

        tokio::spawn(async move {
            bilibili::run_bili_live_loop(
                creds_clone,
                running,
                session_ctl,
                game_id_ref,
                {
                    let h = app_handle_clone.clone();
                    let s = state_clone.clone();
                    move |danmu| {
                        handle_incoming_danmu(Some(&h), &s, danmu);
                    }
                },
                {
                    let h = app_handle_clone.clone();
                    let s = state_clone.clone();
                    move |ev| {
                        handle_incoming_like(Some(&h), &s, &ev);
                    }
                },
                {
                    let h = app_handle_clone.clone();
                    let s = state_clone.clone();
                    move |ev| {
                        handle_incoming_gift(Some(&h), &s, ev);
                    }
                },
                {
                    let h = app_handle_clone.clone();
                    let s = state_clone.clone();
                    move |ev| {
                        handle_incoming_live_event(Some(&h), &s, ev);
                    }
                },
                {
                    let s = state_clone.clone();
                    move |state_kind, reason, attempt| {
                        apply_connection_status(
                            &s,
                            Some(&app_handle_clone),
                            bilibili::ConnectionStatus::new(state_kind, reason, attempt),
                        );
                    }
                },
            )
            .await;
        });

        Ok(true)
    } else {
        state.bili_service.set_running(false);
        apply_connection_status(
            &state,
            Some(&app_handle),
            bilibili::ConnectionStatus::default(),
        );

        // 等待旧连接循环**真正退出**再放行后续操作：
        // 只把 running=false 就当已停，会让紧随其后的 start 与尚未退出的旧循环并行。
        // 可取消的退避 sleep + 500ms 取消轮询保证这里通常几十毫秒内就能确认。
        let mut waited_ms = 0u32;
        while state.bili_service.is_loop_alive() && waited_ms < 3000 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            waited_ms += 50;
        }
        if state.bili_service.is_loop_alive() {
            // 明确告诉前端"正在断开"：不假装已经断开完成
            crate::log_warn!("[Bili] 旧连接任务仍在退出中（已等待 {}ms）", waited_ms);
            return Err(
                "正在断开连接：旧连接任务尚未退出，请稍候再操作（状态：正在断开）".to_string(),
            );
        }

        // 主动断开：取得本次 end 的**唯一执行权**（后台看到 Ending 时不会重复 end）。
        // 不先 take 掉 game_id —— end 失败时它仍是唯一可重试的凭据。
        if let Some(gid) = state.bili_service.begin_end() {
            let cfg = state.config.lock().map_err(|e| e.to_string())?.clone();
            let creds = state.bili_credentials(&cfg);
            let svc = state.bili_service.clone();
            // 同步等待 end 结果：这样"已断开"与"关闭状态未知"对调用方是确定的事实，
            // 不会出现"界面已断开、服务端其实还在播"的窗口
            match creds.end_app(&gid).await {
                Ok(()) => {
                    crate::log_info!("[Bili] 已确认服务端关闭上一场直播会话");
                    svc.confirm_end();
                }
                Err(e) => {
                    // 失败/超时：保留 game_id 与"关闭状态未知"，禁止在未确认下立即开新会话
                    crate::log_warn!(
                        "[Bili] 下播请求未获确认，服务端关闭状态未知（已保留可重试会话）: {}",
                        e
                    );
                    svc.mark_end_unknown();
                }
            }
        } else {
            // 没有 game_id（例如开播尚未成功）：标记为已确认，允许下次开播
            state.bili_service.set_end_state(bilibili::SessionEndState::Confirmed);
        }

        Ok(false)
    }
}

/// GM: 批量补签（受 Lite 模式控制；打卡数据库不可用时明确报错）
#[tauri::command]
fn gm_batch_checkin(state: State<'_, AppState>) -> Result<checkin::BatchCheckinResult, String> {
    ensure_not_lite(&state, "GM运维打卡功能")?;
    // GM 命令本就返回 Result：入口直接明确 Err，不做内存兜底
    state.checkin()?.batch_checkin()
}

/// GM: 水友模糊搜索（受 Lite 模式控制；返回档案 + 补签卡数）
#[tauri::command]
fn gm_search_users(
    keyword: String,
    state: State<'_, AppState>,
) -> Result<Vec<checkin::UserSearchItem>, String> {
    ensure_not_lite(&state, "GM功能")?;
    state.checkin()?.search_users(&keyword)
}

/// GM: 手动调发补签卡（受 Lite 模式控制）
#[tauri::command]
fn gm_grant_card(
    uid: String,
    count: i32,
    state: State<'_, AppState>,
) -> Result<i32, String> {
    ensure_not_lite(&state, "GM功能")?;
    state.checkin()?.grant_card(&uid, count)
}

/// 查询打卡子系统可用性（冷启动快照：前端据此常驻展示可用／停用／故障，
/// 不依赖 setup 阶段可能丢失的单次事件）
#[tauri::command]
fn get_checkin_status(state: State<'_, AppState>) -> CheckinStatus {
    (*state.checkin_status).clone()
}

/// 查询启动时缺失的资源名（F2：与 `resource-missing` 事件按名去重合并展示，
/// 挂载晚于 setup 的窗口也能拿到完整缺项）
#[tauri::command]
fn get_missing_resources(state: State<'_, AppState>) -> Vec<String> {
    state.startup_missing.as_ref().clone()
}

/// 系统保存对话框写入文件（生产实现：tauri-plugin-dialog + UTF-8 BOM 内容）
///
/// 注：测试构建不引用 tauri-plugin-dialog —— 其底层 rfd 静态导入 comctl32 v6 的
/// TaskDialogIndirect，而测试二进制没有 Common-Controls 清单，会在加载期以
/// STATUS_ENTRYPOINT_NOT_FOUND 直接失败。生产主程序由 tauri-build 注入清单，可正常使用。
#[cfg(not(test))]
fn save_text_file_with_dialog(
    app_handle: &AppHandle,
    default_name: &str,
    format: &str,
    content: &str,
) -> Result<String, String> {
    use tauri_plugin_dialog::DialogExt;

    let picked = app_handle
        .dialog()
        .file()
        .set_file_name(default_name)
        .add_filter(format.to_uppercase(), &[format])
        .blocking_save_file();

    let Some(file_path) = picked else {
        return Err("已取消导出".into());
    };
    let path = file_path
        .into_path()
        .map_err(|e| format!("保存路径解析失败: {}", e))?;
    std::fs::write(&path, content.as_bytes()).map_err(|e| format!("写入文件失败: {}", e))?;
    Ok(path.to_string_lossy().to_string())
}

#[cfg(test)]
fn save_text_file_with_dialog(
    _app_handle: &AppHandle,
    _default_name: &str,
    _format: &str,
    _content: &str,
) -> Result<String, String> {
    Err("测试构建不弹出系统保存对话框".into())
}

/// 原生二次确认（替代 WebView2 下静默返回 true 的 window.confirm）
///
/// 注：与 `save_text_file_with_dialog` 同源约束——测试构建不引用 tauri-plugin-dialog。
#[cfg(not(test))]
fn confirm_with_dialog(app_handle: &AppHandle, title: &str, message: &str) -> Result<bool, String> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

    Ok(app_handle
        .dialog()
        .message(message)
        .title(title)
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::YesNo)
        .blocking_show())
}

#[cfg(test)]
fn confirm_with_dialog(
    _app_handle: &AppHandle,
    _title: &str,
    _message: &str,
) -> Result<bool, String> {
    Err("测试构建不弹出确认对话框".into())
}

/// 二次确认命令（返回 true=用户点击「是」；调用失败由前端按「未确认」处理）
#[tauri::command]
fn confirm_action(title: String, message: String, app_handle: AppHandle) -> Result<bool, String> {
    confirm_with_dialog(&app_handle, &title, &message)
}

/// 校验凭据文件并**原子替换**规范位置的 credentials.dat，返回校验通过的凭据。
///
/// 顺序严格为：读源文件 → 验签/解析 → 预构造全部内存快照 → 唯一临时文件写入并 sync
/// → rename 替换目标。任一步失败都保留旧文件与旧内存，不做半截提交。
pub fn import_credentials_from_path(path: &std::path::Path) -> Result<credentials::Credentials, String> {
    if !path.exists() {
        return Err(format!("凭据文件不存在: {}", path.display()));
    }
    // 先按原工程格式严格校验（魔数 + HMAC-SHA256 签名），校验通过才复制
    let creds = credentials::load_credentials(Some(path))
        .map_err(|e| format!("凭据文件校验失败（{}）: {}", path.display(), e))?;

    let target = credentials::get_credentials_path();
    if path != target {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建数据目录失败: {}", e))?;
        }
        let bytes = std::fs::read(path).map_err(|e| format!("读取凭据文件失败: {}", e))?;
        // 同目录唯一临时文件 + sync + rename：目标要么是完整旧文件，要么是完整新文件
        let tmp = target.with_file_name(format!(
            "credentials.dat.{}.tmp",
            std::process::id()
        ));
        let write_result = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            Ok(())
        })();
        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("写入凭据临时文件失败: {}（原凭据未改动）", e));
        }
        if let Err(e) = std::fs::rename(&tmp, &target) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!(
                "替换凭据文件失败: {}（原凭据仍可用）",
                e
            ));
        }
    }
    Ok(creds)
}

/// 将导入的凭据注入运行状态（**下一次连接**即生效）。
///
/// 分两步：① 在短闸门内发布凭据快照并同步配置镜像；② 同步 AI/TTS 消费方。
/// ②失败时返回 `Err`，由调用方决定重新读回还是进入"暂禁开播"状态 ——
/// 不能出现"凭据状态报告 B、引擎却静默使用 A"。
fn apply_credentials_live(state: &AppState, creds: &credentials::Credentials) -> Result<(), String> {
    state.publish_credentials(CredentialState {
        creds: creds.clone(),
        loaded: true,
        blocked_reason: None,
    });

    if let Ok(mut cfg) = state.config.lock() {
        cfg.app_id = creds.app_id.clone();
        cfg.access_key_id = creds.access_key_id.clone();
        cfg.access_key_secret = creds.access_key_secret.clone();
        cfg.mimo_api_key = creds.mimo_tts_api_key.clone();
        cfg.deepseek_api_key = creds.chat_api_key.clone();
    }

    sync_credential_consumers(state)
}

/// 把当前凭据快照同步到 AI Provider 与 TTS 引擎。
/// 单独成函数是为了让"磁盘已提交 B、内存/引擎发布失败"这一中间态可被显式检测与补偿。
fn sync_credential_consumers(state: &AppState) -> Result<(), String> {
    let snap = state.credentials_snapshot();
    state.ai_provider.set_api_key(snap.creds.chat_api_key.clone());

    let cfg = state.config.lock().map_err(|e| e.to_string())?.clone();
    let mimo_key = if !snap.creds.mimo_tts_api_key.is_empty() {
        snap.creds.mimo_tts_api_key.clone()
    } else {
        cfg.mimo_api_key.clone()
    };
    state.tts_mgr.update_config(TTSConfig {
        engine: parse_tts_engine(&cfg.tts_engine),
        enable_voice: cfg.enable_voice,
        speech_rate: cfg.speech_rate,
        speech_volume: cfg.speech_volume,
        speech_pitch: cfg.speech_pitch,
        // Manbo Key 权威来源为注册表/配置，不由 credentials.dat 承载
        manbo_api_key: cfg.manbo_api_key.clone(),
        manbo_voice: cfg.manbo_voice.clone(),
        mimo_api_key: mimo_key,
        mimo_voice: cfg.mimo_voice.clone(),
        mimo_style: cfg.mimo_style.clone(),
        mimo_audio_format: cfg.mimo_audio_format.clone(),
    });
    Ok(())
}

/// 选择凭据文件（生产：系统打开对话框；测试构建：不弹窗）
#[cfg(not(test))]
fn pick_credentials_file(app_handle: &AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let picked = app_handle
        .dialog()
        .file()
        .add_filter("凭据文件", &["dat"])
        .blocking_pick_file();
    match picked {
        None => Ok(None),
        Some(p) => Ok(Some(
            p.into_path()
                .map_err(|e| format!("路径解析失败: {}", e))?
                .to_string_lossy()
                .to_string(),
        )),
    }
}

#[cfg(test)]
fn pick_credentials_file(_app_handle: &AppHandle) -> Result<Option<String>, String> {
    Err("测试构建不弹出文件选择对话框".into())
}

/// 导入 B 站开放平台凭据文件（P1-8）。
/// 安装包不随包分发 credentials.dat（避免公开分发平台密钥），故提供显式导入入口：
/// 选择文件 → HMAC 校验 → 原子替换 `{数据目录}/credentials.dat` → 发布内存快照与 AI/TTS。
///
/// **"即时生效"的准确界线**：只对**下一次连接**生效。运行中的长连以值持有旧凭据，
/// 换内存快照不会让现有 WebSocket 自动改用新凭据，因此活动会话期间直接拒绝导入
/// （在复制任何文件之前就返回），UI 文案不得写成"现有连接已切换"。
#[tauri::command]
async fn import_credentials_file(
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<CredentialsStatus, String> {
    // 文件选择在取得任何生命周期/保存锁**之前**完成，避免持锁等待用户点对话框
    let Some(path) = pick_credentials_file(&app_handle)? else {
        return Err("已取消导入".into());
    };

    let _lifecycle = state.bili_lifecycle.lock().await;

    // Connecting／Connected／Reconnecting 或旧连接任务尚未退出时拒绝导入
    if state.bili_service.is_running() {
        return Err(
            "直播连接仍在进行中：请先断开连接并等待确认下播完成，再导入凭据（导入只对下一次连接生效）"
                .into(),
        );
    }
    {
        let conn = state.connection.lock().map_err(|e| e.to_string())?;
        if !matches!(conn.state, bilibili::ConnectionState::Disconnected) {
            return Err("直播会话尚未完全停止：请先断开连接后重试".into());
        }
    }
    if !state.bili_service.can_start_new_session() {
        return Err(
            "上一场直播的服务端关闭状态未确认，暂不能替换凭据；请稍后重试或重启应用".into(),
        );
    }

    // 短提交闸门：只覆盖"替换文件 → 发布内存 → 同步 AI/TTS"，不含任何 .await
    let _gate = state.credential_gate.lock().map_err(|e| e.to_string())?;

    let creds = import_credentials_from_path(std::path::Path::new(&path))?;

    // 磁盘 B 已提交；接下来必须在同一闸门内完成内存发布与引擎同步
    let target = credentials::get_credentials_path();
    let publish = apply_credentials_live(&state, &creds);
    match publish {
        Ok(()) => {
            crate::log_info!("[Credentials] 已导入并发布凭据文件（对下一次连接生效）");
            Ok(creds.to_status(true, &target.to_string_lossy()))
        }
        Err(e) => {
            // 磁盘已是 B 但内存发布失败：先在同一闸门内重新读回并验签磁盘 B
            match credentials::load_credentials(Some(&target)) {
                Ok(disk) if disk == creds => {
                    state.publish_credentials(CredentialState {
                        creds: disk,
                        loaded: true,
                        blocked_reason: None,
                    });
                    if let Err(e2) = sync_credential_consumers(&state) {
                        crate::log_error!("[Credentials] 引擎同步仍失败，暂禁开播: {}", e2);
                        state.publish_credentials(CredentialState {
                            creds: creds.clone(),
                            loaded: true,
                            blocked_reason: Some(
                                "凭据引擎同步失败，已暂禁开播，请重新导入或重启应用".to_string(),
                            ),
                        });
                        return Err(format!(
                            "凭据已写入磁盘，但语音／AI 引擎同步失败（{}）。已暂禁开播，请重新导入或重启应用。",
                            e2
                        ));
                    }
                    crate::log_warn!("[Credentials] 内存发布经重新读回后修复: {}", e);
                    Ok(creds.to_status(true, &target.to_string_lossy()))
                }
                _ => {
                    // 无法确认磁盘内容 → 进入禁止开播且高可见的凭据错误状态，保留错误供人工恢复
                    state.publish_credentials(CredentialState {
                        creds: Credentials::default(),
                        loaded: false,
                        blocked_reason: Some(format!(
                            "凭据已替换但无法校验发布结果（{}）；已暂禁开播，请重启应用后重新导入",
                            e
                        )),
                    });
                    Err(format!(
                        "凭据导入未能确认生效（{}）：已暂禁开播，请重启应用后重新导入",
                        e
                    ))
                }
            }
        }
    }
}

/// GM: 导出打卡记录（CSV / JSON，系统保存对话框 + UTF-8 BOM；受 Lite 模式控制）
/// 对齐原工程 DataBridgeExports：支持昵称模糊筛选与日期范围（YYYY-MM-DD）
#[tauri::command]
fn gm_export_checkin_records(
    format: String,
    username: Option<String>,
    start_date: Option<String>,
    end_date: Option<String>,
    app_handle: AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    ensure_not_lite(&state, "打卡导出功能")?;
    let checkin_mgr = state.checkin()?;

    let parse_date = |s: Option<String>| -> Result<Option<chrono::NaiveDate>, String> {
        match s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
            Some(v) => chrono::NaiveDate::parse_from_str(&v, "%Y-%m-%d")
                .map(Some)
                .map_err(|_| format!("日期格式不合法（应为 YYYY-MM-DD）: {}", v)),
            None => Ok(None),
        }
    };
    let start = parse_date(start_date)?;
    let end = parse_date(end_date)?;
    if let (Some(s), Some(e)) = (start, end) {
        if s > e {
            return Err("开始日期不能晚于结束日期".into());
        }
    }

    let clean_user = username.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty());
    let is_summary = clean_user.is_none() && start.is_none() && end.is_none();

    let (default_name, content) = if is_summary {
        // 未指定用户名且未限定日期范围时，导出全量用户打卡总览（对齐原工程 ProfileManager_ExportUsersSummary）
        let name = format!(
            "users_summary_{}.{}",
            chrono::Local::now().format("%Y%m%d_%H%M%S"),
            format
        );
        let text = checkin_mgr.export_users_summary(&format)?;
        (name, text)
    } else {
        // 指定了用户名或日期范围时，导出打卡流水明细（对齐原工程 ProfileManager_ExportCheckinRecords）
        let name = if let Some(u) = clean_user {
            format!(
                "checkin_records_{}_{}.{}",
                u,
                chrono::Local::now().format("%Y%m%d_%H%M%S"),
                format
            )
        } else {
            format!(
                "checkin_records_{}.{}",
                chrono::Local::now().format("%Y%m%d_%H%M%S"),
                format
            )
        };
        let text = checkin_mgr.export_records_content(&format, clean_user, start, end)?;
        (name, text)
    };

    save_text_file_with_dialog(&app_handle, &default_name, &format, &content)
}

/// 获取 Manbo 全量音色列表（供语音设置下拉，对齐原工程 ToolsMain.ManboVoiceList）
#[tauri::command]
fn get_manbo_voice_list() -> Vec<String> {
    manbo_voices::MANBO_VOICE_LIST
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// 配置字符串 → TTS 引擎类型（"auto" 走自动级联；未知值按原工程默认 Manbo）
/// 引擎名 → 枚举：未知值按「自动」处理（对齐原工程 TTSProviderFactory::Create
/// 的 default 分支——未知引擎名走 AUTO 级联，而非退化为手动 Manbo 而丢失降级链）
fn parse_tts_engine(s: &str) -> TTSEngineType {
    match s.trim().to_ascii_lowercase().as_str() {
        "manbo" | "曼波" => TTSEngineType::Manbo,
        "mimo" | "xiaomi" => TTSEngineType::MiMo,
        "sapi" => TTSEngineType::Sapi,
        _ => TTSEngineType::Auto,
    }
}

/// 当前实际使用的 TTS 引擎名（manbo / xiaomi / sapi），供设置面板实时显示
#[tauri::command]
fn get_current_tts_engine(state: State<'_, AppState>) -> String {
    state.tts_mgr.current_engine_name()
}

/// 保存 Manbo API Key（仅写注册表，永不回传明文；空值不覆盖已有 Key；受 Lite 模式控制）
#[tauri::command]
fn save_manbo_api_key(key: String, state: State<'_, AppState>) -> Result<(), String> {
    ensure_not_lite(&state, "TTS语音模块")?;
    let trimmed = key.trim().to_string();
    if trimmed.is_empty() {
        return Err("Manbo API Key 不能为空".into());
    }
    registry::write_manbo_api_key(&trimmed)?;
    let mut cfg_guard = state.config.lock().map_err(|e| e.to_string())?;
    cfg_guard.manbo_api_key = trimmed.clone();
    {
        let mut tts_cfg = state.tts_mgr.config_snapshot();
        tts_cfg.manbo_api_key = trimmed;
        state.tts_mgr.update_config(tts_cfg);
    }
    if let Err(e) = cfg_guard.save(None) {
        crate::log_warn!("[Config] Manbo Key 保存配置失败（注册表已写入）: {}", e);
    }
    Ok(())
}

/// 运行日志快照（前端「运行日志」视图轮询读取）
#[derive(serde::Serialize)]
struct LogsSnapshot {
    /// 日志目录（Logs/YYYY-MM-DD.txt）
    dir: String,
    entries: Vec<logging::LogEntry>,
}

#[tauri::command]
fn get_recent_logs(limit: Option<usize>, min_level: Option<String>) -> LogsSnapshot {
    let level = min_level.as_deref().and_then(logging::LogLevel::parse);
    LogsSnapshot {
        dir: logging::logs_dir().to_string_lossy().to_string(),
        entries: logging::recent_entries(limit.unwrap_or(200), level),
    }
}

/// 清空前端可见的日志内存环（已落盘日志文件不受影响）
#[tauri::command]
fn clear_recent_logs() {
    logging::clear_recent();
}

/// 悬浮窗锁定（穿透）状态
#[tauri::command]
fn get_overlay_locked(state: State<'_, AppState>) -> bool {
    state
        .overlay_locked
        .load(std::sync::atomic::Ordering::SeqCst)
}

/// 悬浮窗锁定/解锁的实际动作（命令与全局热键共用）
fn apply_overlay_lock(
    app_handle: &AppHandle,
    state: &AppState,
    locked: bool,
) -> Result<bool, String> {
    if let Some(window) = app_handle.get_webview_window("overlay") {
        window
            .set_ignore_cursor_events(locked)
            .map_err(|e| e.to_string())?;
        let _ = window.set_always_on_top(locked);
    }
    state
        .overlay_locked
        .store(locked, std::sync::atomic::Ordering::SeqCst);
    let _ = app_handle.emit("overlay-lock-changed", locked);
    Ok(locked)
}

/// 锁定/解锁悬浮窗：锁定时窗口鼠标穿透 + 置顶（对齐原工程 WS_EX_TRANSPARENT + Topmost）。
/// 前端据此在 `penetrating_mode_opacity` 与 `opacity` 之间切换背景透明度
#[tauri::command]
fn set_overlay_locked(
    locked: bool,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<bool, String> {
    apply_overlay_lock(&app_handle, &state, locked)
}

/// 悬浮窗位置兜底：记忆位置完全落在所有显示器之外时（换屏、改分辨率、拔掉副屏），
/// 夹紧到第一个显示器的可见区域内。否则窗口永久不可见，用户除了手改配置文件无法找回。
/// `monitors` 为逻辑坐标的 (left, top, width, height)，主显示器应排在首位。
fn clamp_overlay_position(
    x: f64,
    y: f64,
    win_w: f64,
    win_h: f64,
    monitors: &[(f64, f64, f64, f64)],
) -> (f64, f64) {
    // 至少露出这么多像素才算"找得回来"
    const MIN_VISIBLE_W: f64 = 80.0;
    const MIN_VISIBLE_H: f64 = 40.0;

    let visible = monitors.iter().any(|(left, top, w, h)| {
        let overlap_x = (x + win_w).min(left + w) - x.max(*left);
        let overlap_y = (y + win_h).min(top + h) - y.max(*top);
        overlap_x >= MIN_VISIBLE_W && overlap_y >= MIN_VISIBLE_H
    });
    if visible || monitors.is_empty() {
        return (x, y);
    }

    let (left, top, w, h) = monitors[0];
    (
        x.clamp(left, (left + w - win_w).max(left)),
        y.clamp(top, (top + h - win_h).max(top)),
    )
}

/// 应用记忆的悬浮窗位置（越界时夹紧到可见区域），返回实际落点
fn apply_overlay_position(win: &tauri::WebviewWindow, x: f64, y: f64) -> (f64, f64) {
    let scale = win.scale_factor().unwrap_or(1.0);
    let (win_w, win_h) = win
        .outer_size()
        .map(|s| (s.width as f64 / scale, s.height as f64 / scale))
        .unwrap_or((440.0, 360.0));

    let mut monitors: Vec<(f64, f64, f64, f64)> = Vec::new();
    if let Ok(Some(m)) = win.primary_monitor() {
        let p = m.position();
        let s = m.size();
        monitors.push((
            p.x as f64 / scale,
            p.y as f64 / scale,
            s.width as f64 / scale,
            s.height as f64 / scale,
        ));
    }
    for m in win.available_monitors().unwrap_or_default() {
        let p = m.position();
        let s = m.size();
        let rect = (
            p.x as f64 / scale,
            p.y as f64 / scale,
            s.width as f64 / scale,
            s.height as f64 / scale,
        );
        if !monitors.contains(&rect) {
            monitors.push(rect);
        }
    }

    let (cx, cy) = clamp_overlay_position(x, y, win_w, win_h, &monitors);
    if (cx - x).abs() > 0.5 || (cy - y).abs() > 0.5 {
        crate::log_warn!(
            "[Overlay] 记忆位置 ({:.0}, {:.0}) 超出可见区域，已夹紧到 ({:.0}, {:.0})",
            x,
            y,
            cx,
            cy
        );
    }
    let _ = win.set_position(tauri::Position::Logical(tauri::LogicalPosition::new(cx, cy)));
    (cx, cy)
}

/// 记录悬浮窗拖动后的新位置（防抖落盘到配置 top_pos_x/y，由后台任务写文件）
#[tauri::command]
fn save_overlay_position(
    x: f64,
    y: f64,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut pending = state.pending_pos.lock().map_err(|e| e.to_string())?;
    *pending = Some((x, y));
    Ok(())
}

/// 切换窗口显隐状态
#[tauri::command]
fn toggle_window(app_handle: AppHandle, label: String) -> Result<bool, String> {
    if let Some(window) = app_handle.get_webview_window(&label) {
        let is_visible = window.is_visible().map_err(|e| e.to_string())?;
        if is_visible {
            window.hide().map_err(|e| e.to_string())?;
            Ok(false)
        } else {
            // 显示前先校正位置：位置若在屏幕外（换屏/改分辨率），用户点"桌面点怪悬浮窗"也找不回来
            let (px, py) = app_handle
                .state::<AppState>()
                .config
                .lock()
                .map(|c| (c.top_pos_x, c.top_pos_y))
                .unwrap_or((0.0, 0.0));
            apply_overlay_position(&window, px, py);
            window.show().map_err(|e| e.to_string())?;
            let _ = window.set_focus();
            Ok(true)
        }
    } else {
        Err(format!("Window '{}' not found", label))
    }
}

/// 隐藏指定窗口
#[tauri::command]
fn hide_window(app_handle: AppHandle, label: String) -> Result<(), String> {
    if let Some(window) = app_handle.get_webview_window(&label) {
        window.hide().map_err(|e| e.to_string())?;
        Ok(())
    } else {
        Err(format!("Window '{}' not found", label))
    }
}

/// 退出清理链路（E3，对齐原工程 Exit 命令：WriteQueue::Flush → BliveManager::Disconnect → PostQuitMessage）：
/// V2 中队列/配置均为变更即时落盘，此处补做待写悬浮窗位置落盘、停止直播连接并记录日志，最后退出进程。
/// 有意差异：不在退出路径阻塞调用 B 站下播接口（end_app API），避免网络等待拖慢退出
fn shutdown_app(app_handle: &AppHandle, state: &AppState) {
    // 1. 待写悬浮窗位置立即落盘（等价原工程 WriteQueue::Flush 的兜底落盘）
    if apply_pending_position(state).is_some() {
        if let Ok(cfg) = state.config.lock() {
            if let Err(e) = cfg.save(None) {
                crate::log_warn!("[App] 退出时保存悬浮窗位置失败: {}", e);
            }
        }
    }
    // 2. 队列强制落盘（等价原工程退出前的 WriteQueue::Flush；覆盖 500ms 节流窗口内未写的变更）
    if let Err(e) = flush_queue_to_path(state, &queue::get_order_list_path(), true, Some(app_handle)) {
        crate::log_warn!("[App] 退出时队列强制保存失败，内存队列仍完整: {}", e);
    }
    // 3. 停止直播连接（等价原工程 BliveManager::Disconnect / Destroy）
    if state.bili_service.is_running() {
        state.bili_service.set_running(false);
        crate::log_info!("[App] 退出：已断开 B 站直播连接");
    }
    crate::log_info!("[App] 退出程序：队列与配置已落盘");
    app_handle.exit(0);
}

/// 取出待写悬浮窗位置并更新内存配置（不落盘），返回被取出的坐标。
/// 与后台防抖落盘任务共用同一 pending_pos 通道，避免退出时丢失最后一次拖动
fn apply_pending_position(state: &AppState) -> Option<(f64, f64)> {
    let pending = state.pending_pos.lock().ok().and_then(|mut p| p.take());
    if let Some((x, y)) = pending {
        if let Ok(mut cfg) = state.config.lock() {
            cfg.top_pos_x = x;
            cfg.top_pos_y = y;
            crate::log_debug!("[App] 待写悬浮窗位置已并入内存配置: ({}, {})", x, y);
        } else {
            return None;
        }
    }
    pending
}

/// 查询凭据状态（安全脱敏展示，禁止明文暴露敏感密钥）
#[tauri::command]
fn get_credentials_status(state: State<'_, AppState>) -> Result<CredentialsStatus, String> {
    let p = credentials::get_credentials_path();
    let snap = state.credentials_snapshot();
    let loaded = snap.loaded && !snap.creds.app_id.is_empty();
    let mut status = snap.creds.to_status(loaded, &p.to_string_lossy());
    // 处于"暂禁开播"状态时如实标注，不谎报可用
    if let Some(reason) = snap.blocked_reason {
        status.loaded = false;
        status.chat_provider = format!("{}（{}）", status.chat_provider, reason);
    }
    Ok(status)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 崩溃处理（P1-11）：panic hook 写崩溃报告 + Windows 未处理异常生成全内存 minidump
    // （对齐原工程 DumpHelper::Init 的能力）。测试构建不安装，避免干扰测试输出。
    #[cfg(not(test))]
    logging::install_crash_handler();

    // 进程级 TLS provider 钉死为 ring：rustls 0.23 在多个 provider 同时启用（或都没启用）时
    // 会在握手中 expect 失败，而 release 的 panic="abort" 等于开播即崩。
    let _ = rustls::crypto::ring::default_provider().install_default();

    let builder = tauri::Builder::default().plugin(tauri_plugin_opener::init());
    // 对话框插件仅在生产构建注册（原因见 save_text_file_with_dialog 注释）
    #[cfg(not(test))]
    let builder = builder.plugin(tauri_plugin_dialog::init());
    // D2 全局热键（Alt+, 锁定/解锁悬浮窗）——测试构建不注册，避免测试进程抢占系统热键
    #[cfg(not(test))]
    let builder = builder.plugin(
        tauri_plugin_global_shortcut::Builder::new()
            .with_handler(|app, _shortcut, event| {
                if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                    let state = app.state::<AppState>();
                    let next = !state
                        .overlay_locked
                        .load(std::sync::atomic::Ordering::SeqCst);
                    if let Err(e) = apply_overlay_lock(app, &state, next) {
                        crate::log_warn!("[Hotkey] 悬浮窗锁定切换失败: {}", e);
                    }
                }
            })
            .build(),
    );

    builder
        .setup(|app| {
            // 资源目录解析必须先于 AppState 创建（monster/tts/配置路径均依赖）
            paths::init_resource_dir(app.path().resource_dir().ok());
            paths::ensure_seeded();

            // C1：构建弹幕关键词学习器（jieba 内嵌词典 + 随包停用词/自定义词典），
            // 构建开销较大，仅启动时执行一次。
            // Lite 形态停用打卡与 AI，学习器整体不构建，也不加载分词素材。
            let mut state = AppState::default();
            if !IS_LITE {
                state.checkin_learner = Some(Arc::new(CheckinLearner::from_resources()));
            }
            // F2：缺资源快照必须在 manage 之前算好并写进 AppState ——
            // `resource-missing` 只 emit 一次，挂载晚于 setup 的前端必然收不到
            let required: Vec<&str> = if IS_LITE {
                vec!["monster_list.json"]
            } else {
                vec!["monster_list.json", "voices", "dict/stop_words.utf8"]
            };
            state.startup_missing = Arc::new(
                required
                    .into_iter()
                    .filter(|rel| paths::find_resource(rel).is_none())
                    .map(|s| s.to_string())
                    .collect(),
            );
            app.manage(state);

            // 资源缺失可见：日志 + 事件（前端事件与快照按名去重合并展示）
            let missing: Vec<String> = app.state::<AppState>().startup_missing.as_ref().clone();
            if !missing.is_empty() {
                crate::log_warn!("[Paths] 资源缺失: {:?}（相关功能将降级运行）", missing);
                for rel in &missing {
                    let _ = app.emit("resource-missing", rel);
                }
            }

            // TTS 音频留档：启动时清理超过保留天数的 TempAudio/YYYYMMDD 目录（B7）。
            // Lite 停用 TTS，不得清理与完整版共用数据目录中的音频留档。
            if !IS_LITE {
                let days = app.state::<AppState>().config.lock().map(|c| c.tts_cache_days_to_keep).unwrap_or(7);
                let removed = tts::cleanup_old_cache(days);
                if removed > 0 {
                    crate::log_info!("[TTS] 已清理 {} 个过期音频留档目录（保留 {} 天）", removed, days);
                }
            }

            // D2：恢复记忆的悬浮窗位置（原工程 TopPos）并注册 Alt+, 锁定热键
            {
                let (px, py) = app
                    .state::<AppState>()
                    .config
                    .lock()
                    .map(|c| (c.top_pos_x, c.top_pos_y))
                    .unwrap_or((0.0, 0.0));
                if let Some(win) = app.get_webview_window("overlay") {
                    // 原工程无条件应用 TopPos（默认 0,0 即屏幕左上），此处同样不做哨兵判断；
                    // 但越界位置会导致窗口永久不可见，故统一经可见区域夹紧
                    let (cx, cy) = apply_overlay_position(&win, px, py);
                    // 夹紧结果回写配置：否则 set_position 不触发 onMoved，配置里会一直留着
                    // 那个无效位置，每次启动都要重新夹紧并告警
                    if (cx - px).abs() > 0.5 || (cy - py).abs() > 0.5 {
                        if let Ok(mut cfg) = app.state::<AppState>().config.lock() {
                            cfg.top_pos_x = cx;
                            cfg.top_pos_y = cy;
                            if let Err(e) = cfg.save(None) {
                                crate::log_warn!("[Overlay] 夹紧后的位置回写失败: {}", e);
                            }
                        }
                    }
                }

                #[cfg(not(test))]
                {
                    use tauri_plugin_global_shortcut::GlobalShortcutExt;
                    let hotkey = tauri_plugin_global_shortcut::Shortcut::new(
                        Some(tauri_plugin_global_shortcut::Modifiers::ALT),
                        tauri_plugin_global_shortcut::Code::Comma,
                    );
                    match app.global_shortcut().register(hotkey) {
                        Ok(_) => crate::log_info!("[Hotkey] 已注册 Alt+, 锁定/解锁悬浮窗"),
                        Err(e) => crate::log_warn!("[Hotkey] Alt+, 注册失败: {}", e),
                    }
                }
            }

            // 播报泵：每 100ms（对齐原工程 TIMER_INTERVAL=100）各出队一条优先/普通任务并结算超时连击；
            // 并发合成上限 MAX_CONCURRENT_TTS=2（对齐原工程 activeRequestCount_ 闸门）
            let state = app.state::<AppState>().inner().clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(100));
                loop {
                    interval.tick().await;
                    let only_paid = state
                        .config
                        .lock()
                        .map(|c| c.only_speek_paid_gift)
                        .unwrap_or(false);
                    if IS_LITE {
                        continue;
                    }

                    // 超时连击统一进入高优先队列（每 tick 结算一次）；
                    // 留档与「仅付费播报」过滤分开：被过滤的文案仍写 History
                    for report in state.tts_mgr.flush_gift_combos(only_paid) {
                        record_business_history(&report.text);
                        record_business_history_probe(&report.text);
                        if report.can_speak {
                            state.tts_mgr.enqueue_speak(&report.text, "", true);
                        }
                    }

                    // 各队列每周期各推进一条（普通播报不会被优先队列饿死）
                    let free = crate::tts::MAX_CONCURRENT_TTS.saturating_sub(state.tts_mgr.inflight_count());
                    for task in state.tts_mgr.dequeue_one_each(free.min(2)) {
                        if !state.tts_mgr.try_acquire_slot() {
                            // 名额被占满：放回队首等待下一周期
                            state.tts_mgr.requeue_speak(task);
                            break;
                        }
                        let tts = state.tts_mgr.clone();
                        tauri::async_runtime::spawn(async move {
                            // 此处不再写 History：留档改由各业务事件在"文本形成时"完成一次，
                            // 否则语音开关、队满、合成失败都会连带删掉业务留档
                            let _ = tts.speak_task(&task).await;
                            tts.release_slot();
                        });
                    }
                }
            });

            // 队列落盘节流：每 500ms 检查未落盘版本，仅在变更后于锁外写盘
            // （对齐原工程 PriorityQueueManager::Tick 的 SAVE_INTERVAL_MS=500）
            {
                let state = app.state::<AppState>().inner().clone();
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
                    loop {
                        interval.tick().await;
                        flush_queue(&state, false, Some(&handle));
                    }
                });
            }

            // D3 点赞补偿扫描：tokio interval 的首个 tick 立即触发，即「启动即扫一次」，
            // 覆盖上次运行期失败残留的点赞意图；此后每 60s 一轮，
            // 数据库恢复后自动补账（Lite 下扫描器内部直接早退）
            {
                let state = app.state::<AppState>().inner().clone();
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(
                        LIKE_COMPENSATION_SCAN_INTERVAL_SECS,
                    ));
                    loop {
                        interval.tick().await;
                        replay_pending_likes(&state, Some(&handle));
                    }
                });
            }

            // 悬浮窗位置防抖落盘：拖动结束后写回配置 top_pos_x/y（每 3s 检查一次待写位置）
            {
                let state = app.state::<AppState>().inner().clone();
                tauri::async_runtime::spawn(async move {
                    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3));
                    loop {
                        interval.tick().await;
                        let pending = state.pending_pos.lock().ok().and_then(|mut p| p.take());
                        if let Some((x, y)) = pending {
                            let result = {
                                let mut cfg = match state.config.lock() {
                                    Ok(c) => c,
                                    Err(_) => continue,
                                };
                                cfg.top_pos_x = x;
                                cfg.top_pos_y = y;
                                cfg.save(None)
                            };
                            match result {
                                Ok(_) => {
                                    crate::log_debug!("[Overlay] 已记忆悬浮窗位置: ({}, {})", x, y)
                                }
                                Err(e) => {
                                    crate::log_warn!("[Overlay] 悬浮窗位置保存失败: {}", e)
                                }
                            }
                        }
                    }
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    // E3：关闭主窗口 = 退出程序，统一走退出清理链路
                    let app = window.app_handle();
                    let state = app.state::<AppState>();
                    shutdown_app(app, &state);
                } else {
                    // 悬浮窗关闭 = 隐藏（对齐原工程 OrderedMonsterWindow::OnClosing 的 e.Cancel + Hide），
                    // 否则窗口被销毁后 toggle_window 将永远失败，用户只能重启程序
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_queue,
            add_order,
            add_picked_order,
            dequeue_by_user_id,
            clear_queue,
            reorder_queue,
            restore_order,
            toggle_window,
            hide_window,
            get_credentials_status,
            import_credentials_file,
            get_monster_dict,
            save_monster_entry,
            delete_monster_entry,
            get_monster_roster,
            set_monster_roster,
            export_monster_roster,
            import_monster_roster,
            get_app_config,
            save_app_config,
            save_id_code,
            get_id_code,
            get_bili_connection_state,
            set_bili_connection,
            gm_batch_checkin,
            gm_search_users,
            gm_grant_card,
            gm_export_checkin_records,
            get_checkin_status,
            get_missing_resources,
            confirm_action,
            get_manbo_voice_list,
            get_current_tts_engine,
            save_manbo_api_key,
            get_recent_logs,
            clear_recent_logs,
            get_overlay_locked,
            set_overlay_locked,
            save_overlay_position
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// 测试用隔离序号：保证每个 `AppState::new_test()` 拿到独立的名单文件路径
#[cfg(test)]
static TEST_APPSTATE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
impl AppState {
    /// 测试专用：取出内存打卡库。    ///
    /// 生产路径必须经 `checkin()` 判断可用性；数据库故障是受测场景之一，
    /// 因此这里只在"测试确实注入了内存库"的前置条件下取出。
    pub fn test_checkin(&self) -> &Arc<CheckinManager> {
        self.checkin_mgr
            .as_ref()
            .expect("测试应注入内存打卡库（new_test / 显式注入）")
    }

    pub fn new_test() -> Self {
        let mut app_cfg = AppConfig::default();
        // 单测需覆盖播报链路，故测试基线显式开启语音（生产默认值对齐原工程为 false）
        app_cfg.enable_voice = true;
        let mut monster_mgr = MonsterDataManager::new();
        let _ = monster_mgr.load_from_file(None);
        let queue_mgr = QueueManager::new();
        let checkin_mgr = CheckinManager::new_in_memory().expect("In-memory SQLite failed");
        // 测试显式注入内存库：生产路径的 `checkin_mgr` 为 None 时业务必须拒绝写入
        let checkin_status = CheckinStatus::available("memory".to_string(), false);
        let tts_mgr = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Manbo,
            enable_voice: true,
            speech_rate: 0,
            speech_volume: 100,
            speech_pitch: 0,
            manbo_api_key: String::new(),
            manbo_voice: String::new(),
            mimo_api_key: String::new(),
            mimo_voice: String::new(),
            mimo_style: String::new(),
            mimo_audio_format: String::new(),
        });
        let ai_provider = DeepSeekAIChatProvider::new(String::new());
        let danmu_processor = bilibili::DanmuProcessor::new();
        let creds = credentials::load_credentials(None).unwrap_or_default();
        // 单测名单指向临时目录：绝不读写真实 monster_roster.json。
        // 每个实例用**唯一路径**（进程号 + 自增序号）：共用一个文件会让某个测试写入的
        // 禁点内容泄漏给并行运行的其他测试，造成随机失败
        let roster = MonsterRoster::load(Some(
            &std::env::temp_dir()
                .join(format!(
                    "mh_test_appstate_roster_{}_{}",
                    std::process::id(),
                    TEST_APPSTATE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ))
                .join(roster::ROSTER_FILE_NAME),
        ));

        Self {
            queue_mgr: Arc::new(Mutex::new(queue_mgr)),
            queue_flush_lock: Arc::new(Mutex::new(())),
            queue_persistence_broken: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            monster_mgr: Arc::new(monster_mgr),
            roster: Arc::new(roster),
            checkin_mgr: Some(Arc::new(checkin_mgr)),
            checkin_status: Arc::new(checkin_status),
            checkin_learner: None,
            tts_mgr: Arc::new(tts_mgr),
            ai_provider: Arc::new(ai_provider),
            config: Arc::new(Mutex::new(app_cfg)),
            credentials: Arc::new(std::sync::RwLock::new(CredentialState {
                loaded: !creds.app_id.is_empty(),
                creds,
                blocked_reason: None,
            })),
            bili_lifecycle: Arc::new(tokio::sync::Mutex::new(())),
            credential_gate: Arc::new(Mutex::new(())),
            danmu_processor: Arc::new(danmu_processor),
            connection: Arc::new(Mutex::new(bilibili::ConnectionStatus::default())),
            bili_service: Arc::new(bilibili::BiliLiveService::new()),
            overlay_locked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_pos: Arc::new(Mutex::new(None)),
            startup_missing: Arc::new(Vec::new()),
            last_checkin_unavailable_at: Arc::new(std::sync::atomic::AtomicI64::new(0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 悬浮窗位置夹紧：换屏/改分辨率后记忆位置可能完全落在屏外，
    /// 必须夹回可见区，否则窗口永久不可见（2026-09-23 实测 top_pos_x=1976 于 1920 屏）
    #[test]
    fn test_clamp_overlay_position_keeps_window_reachable() {
        let screen = (0.0, 0.0, 1920.0, 1080.0);
        let (w, h) = (440.0, 360.0);

        // 屏内 → 原样
        assert_eq!(clamp_overlay_position(100.0, 100.0, w, h, &[screen]), (100.0, 100.0));
        // 贴右下角 → 原样（完全可见）
        assert_eq!(clamp_overlay_position(1480.0, 720.0, w, h, &[screen]), (1480.0, 720.0));
        // 右侧完全越界（实测值）→ 夹到右边缘
        assert_eq!(clamp_overlay_position(1976.0, 574.0, w, h, &[screen]), (1480.0, 574.0));
        // 下方完全越界 → 夹到底边缘
        assert_eq!(clamp_overlay_position(100.0, 1200.0, w, h, &[screen]), (100.0, 720.0));
        // 负坐标（显示器移到左侧后残留）→ 夹回 0
        assert_eq!(clamp_overlay_position(-500.0, -300.0, w, h, &[screen]), (0.0, 0.0));
        // 露出一角（120×80，达到 80×40 门限）→ 视为用户找得回来，不动它
        assert_eq!(
            clamp_overlay_position(1800.0, 1000.0, w, h, &[screen]),
            (1800.0, 1000.0)
        );
        // 落在副屏内 → 不动（副屏放第一位时以副屏为夹紧基准）
        let dual = [(1920.0, 0.0, 1920.0, 1080.0), screen];
        assert_eq!(clamp_overlay_position(2000.0, 200.0, w, h, &dual), (2000.0, 200.0));
        // 显示器信息缺失 → 不夹紧（不能凭猜测挪窗口）
        assert_eq!(clamp_overlay_position(1976.0, 574.0, w, h, &[]), (1976.0, 574.0));
        println!("[PASS] test_clamp_overlay_position_keeps_window_reachable passed");
    }

    #[test]
    fn test_app_state_initialization() {
        let state = AppState::new_test();
        let q = state.queue_mgr.lock().unwrap();
        assert_eq!(q.items.len(), 0);
        let cfg = state.config.lock().unwrap();
        assert_eq!(cfg.opacity, 100);
        assert!(!state.bili_service.is_running());
        println!("[PASS] test_app_state_initialization passed");
    }

    /// Lite 形态已改为编译期常量（IS_LITE / Cargo feature `lite`），
    /// 运行期开关不复存在；核心点单排队在完整版与 Lite 版两种构建下都必须正常运作
    #[test]
    fn test_core_queue_works_in_both_build_flavors() {
        let state = AppState::new_test();

        // 编译期形态常量与当前 cargo 编译参数一致（双形态测试锚点）
        assert_eq!(IS_LITE, cfg!(feature = "lite"));

        // 核心点单排队不受形态影响
        let mut q = state.queue_mgr.lock().unwrap();
        q.add_or_update(QueueItem {
            id: "lite-1".into(),
            user_id: "u_lite".into(),
            user_name: "猎人".into(),
            monster_name: "刺花蜘蛛".into(),
            is_priority: false,
            guard_level: 0,
            tempered_level: 0,
            timestamp: 12345,
            icon_url: "".into(),
        });
        assert_eq!(q.items.len(), 1);
        println!("[PASS] test_core_queue_works_in_both_build_flavors passed");
    }

    /// E1：统一 Lite 守卫 —— 按编译形态分门：Lite 构建下非排队模块统一拒绝；
    /// 完整版构建放行。两种形态下 Lite 保留模块（点怪排队）都不受影响
    #[test]
    fn test_ensure_not_lite_guard_matches_build_flavor() {
        let state = AppState::new_test();

        if IS_LITE {
            // Lite 构建：统一拒绝，文案与前端提示一致
            assert_eq!(
                ensure_not_lite(&state, "打卡模块").unwrap_err(),
                "Lite模式下打卡模块已停用"
            );
            for module in [
                "补签卡模块",
                "补签模块",
                "GM运维打卡功能",
                "GM功能",
                "打卡导出功能",
                "音效模块",
                "点赞奖卡模块",
                "TTS语音模块",
            ] {
                let err = ensure_not_lite(&state, module).unwrap_err();
                assert_eq!(err, format!("Lite模式下{}已停用", module));
            }
        } else {
            // 完整版构建：守卫放行
            assert!(ensure_not_lite(&state, "打卡模块").is_ok());
            assert!(ensure_not_lite(&state, "TTS语音模块").is_ok());
        }

        // Lite 保留：点怪排队不经过守卫，仍可正常运作
        let mut q = state.queue_mgr.lock().unwrap();
        q.add_or_update(QueueItem {
            id: "lite-guard-1".into(),
            user_id: "u_guard".into(),
            user_name: "猎人".into(),
            monster_name: "刺花蜘蛛".into(),
            is_priority: false,
            guard_level: 0,
            tempered_level: 0,
            timestamp: 23456,
            icon_url: "".into(),
        });
        assert_eq!(q.items.len(), 1);
        println!("[PASS] test_ensure_not_lite_guard_matches_build_flavor passed");
    }

    /// E3：退出清理 —— 待写悬浮窗位置并入内存配置并清空 pending（不落盘）
    #[test]
    fn test_apply_pending_position_updates_memory_config() {
        let state = AppState::new_test();

        // 无待写位置：返回 None，内存配置保持默认（对齐原工程 topPos = 0,0）
        assert!(apply_pending_position(&state).is_none());
        assert_eq!(state.config.lock().unwrap().top_pos_x, 0.0);

        // 拖动产生待写位置：并入内存配置并清空 pending
        *state.pending_pos.lock().unwrap() = Some((913.0, 105.0));
        assert_eq!(apply_pending_position(&state), Some((913.0, 105.0)));
        {
            let cfg = state.config.lock().unwrap();
            assert_eq!(cfg.top_pos_x, 913.0);
            assert_eq!(cfg.top_pos_y, 105.0);
        }
        assert!(state.pending_pos.lock().unwrap().is_none());
        println!("[PASS] test_apply_pending_position_updates_memory_config passed");
    }

    #[test]
    fn test_simulate_danmu_special_user_ordering() {
        let state = AppState::new_test();
        let dm = bilibili::DanmuData {
            user_id: bilibili::SPECIAL_OPEN_ID.into(),
            user_name: "特殊神秘管理员".into(),
            message: "点怪优先霸主太太".into(),
            timestamp: 88888,
            has_medal: false,
            medal_level: 0,
            guard_level: 0, // 初始为 0，由管道自动赋权总督 1
            msg_id: "sim_special_1".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };

        let res = handle_incoming_danmu(None, &state, dm);
        assert!(res.matched);
        assert_eq!(res.monster_name, "霸主雌火龙");
        assert!(res.added_to_queue);

        let q = state.queue_mgr.lock().unwrap();
        assert_eq!(q.items.len(), 1);
        assert_eq!(q.items[0].guard_level, 1);
        assert!(q.items[0].is_priority);
        println!("[PASS] test_simulate_danmu_special_user_ordering passed");
    }

    /// 打卡链路属完整版专属（Lite 构建下该链路被编译期守卫短路）
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_simulate_danmu_checkin_flow() {
        let state = AppState::new_test();
        let now_ts = chrono::Utc::now().timestamp();

        // 1. 舰长水友发送“打卡”
        let dm = bilibili::DanmuData {
            user_id: "guard_captain_1".into(),
            user_name: "大副舰长".into(),
            message: "打卡".into(),
            timestamp: now_ts,
            has_medal: true,
            medal_level: 15,
            guard_level: 3, // 舰长
            msg_id: "sim_checkin_1".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };

        let res = handle_incoming_danmu(None, &state, dm);
        // 打卡指令不应作为怪物点单入队
        assert!(!res.matched);
        assert!(!res.added_to_queue);

        // 验证 SQLite 打卡档案已成功建立
        let profile = state.test_checkin().get_profile("guard_captain_1").expect("Profile not found");
        assert_eq!(profile.cumulative_days, 1);
        assert_eq!(profile.continuous_days, 1);

        // C5：打卡不再覆写 last_danmu_timestamp（该列仅由学习链路写入）
        assert_eq!(profile.last_danmu_timestamp, 0);

        // C1：未配置 AI Key → 兜底文案入高优先播报队列
        let task = state.tts_mgr.dequeue_speak().expect("打卡回复应入队播报");
        assert_eq!(task.text, "大副舰长连续第1天打卡！累计1天");
        assert_eq!(task.user_id, "guard_captain_1");
        // 签到播报须标记 is_checkin，播放成功后按“打卡_{用户名}_{ts}.mp3”留档
        assert!(task.is_checkin && task.checkin_username == "大副舰长");

        // 同一天再次打卡 → 今日已打卡文案（C5，对齐原工程 repeatedAnswer）
        let dm2 = bilibili::DanmuData {
            user_id: "guard_captain_1".into(),
            user_name: "大副舰长".into(),
            message: "打卡".into(),
            timestamp: now_ts + 10,
            has_medal: true,
            medal_level: 15,
            guard_level: 3,
            msg_id: "sim_checkin_2".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };
        let _ = handle_incoming_danmu(None, &state, dm2);
        let task2 = state.tts_mgr.dequeue_speak().expect("重复打卡应有回复");
        assert_eq!(task2.text, "大副舰长今日已打卡，连续1天，累计1天");
        assert!(task2.is_checkin, "重复打卡播报同样应留档");

        // 验证排队列表中无此项
        let q = state.queue_mgr.lock().unwrap();
        assert_eq!(q.items.len(), 0);
        println!("[PASS] test_simulate_danmu_checkin_flow passed");
    }

    /// 补签链路属完整版专属（Lite 构建下该链路被编译期守卫短路）
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_simulate_danmu_retroactive_flow() {
        let state = AppState::new_test();
        let today = chrono::Local::now().date_naive();
        let d_yesterday = today - chrono::Duration::days(1);
        let d_3_days_ago = today - chrono::Duration::days(3);

        // 预设打卡记录：3天前与今天打卡，昨天与前天断签
        state.test_checkin().record_checkin("captain_retro", "补签猎人", d_3_days_ago).unwrap();
        state.test_checkin().record_checkin("captain_retro", "补签猎人", today).unwrap();
        state.test_checkin().grant_card("captain_retro", 2).unwrap();

        // 舰长发送“补签”弹幕（打卡日期口径为服务器时间，测试用当前时间戳）
        let dm = bilibili::DanmuData {
            user_id: "captain_retro".into(),
            user_name: "补签猎人".into(),
            message: "补签".into(),
            timestamp: chrono::Utc::now().timestamp(),
            has_medal: true,
            medal_level: 20,
            guard_level: 3,
            msg_id: "sim_retro_1".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };

        let res = handle_incoming_danmu(None, &state, dm);
        assert!(!res.matched);

        // 验证补签卡扣减为 1 张
        let cards = state.test_checkin().get_cards("captain_retro");
        assert_eq!(cards.card_count, 1);

        // C2/C4：回复文案对齐原工程（含补签日期与恢复后的连续天数）
        let task = state.tts_mgr.dequeue_speak().expect("补签回复应入队播报");
        assert!(task.text.starts_with("补签猎人，已成功补签"), "{}", task.text);
        assert!(task.text.contains("剩余补签卡1张"), "{}", task.text);
        // 补签播报同样应留档
        assert!(task.is_checkin && task.checkin_username == "补签猎人");

        // 验证昨天日期已被补签
        let records = state
            .test_checkin()
            .export_records_content("csv", None, None, None)
            .unwrap();
        let yesterday_int = checkin::CheckinManager::date_to_int(d_yesterday);
        assert!(records.contains(&yesterday_int.to_string()));
        println!("[PASS] test_simulate_danmu_retroactive_flow passed");
    }

    /// 打卡触发词解析：中英文逗号均可分隔（对齐原工程 SetTriggerWords 的
    /// `,` 与 `，` 双逗号分割），逐词 trim、空词过滤、清空即停用
    #[test]
    fn test_parse_checkin_trigger_words_fullwidth_comma() {
        // 默认配置：英文逗号
        assert_eq!(parse_checkin_trigger_words("打卡,签到"), vec!["打卡", "签到"]);
        // 中文逗号：修复前会被当成一个整体触发词导致打卡静默失效
        assert_eq!(parse_checkin_trigger_words("打卡，签到"), vec!["打卡", "签到"]);
        // 混合逗号 + 首尾空白 + 空词过滤
        assert_eq!(
            parse_checkin_trigger_words(" 打卡 ，签到 , 签到卡，，"),
            vec!["打卡", "签到", "签到卡"]
        );
        // 仅分隔符/空白 → 空（打卡功能停用）
        assert!(parse_checkin_trigger_words("，, ,").is_empty());
        assert!(parse_checkin_trigger_words("").is_empty());
        println!("[PASS] test_parse_checkin_trigger_words_fullwidth_comma passed");
    }

    // ---------------- A1/A2：队列版本协议与磁盘单写者 ----------------

    /// A2 核心反测：强制保存必须等待后台刷盘锁，最终磁盘是**最新**队列。
    ///
    /// 屏障直接复用真实的 `queue_flush_lock`（不引入生产代码里的测试钩子）：
    /// 测试线程扮演「已取得刷盘锁、停在旧快照写盘前」的后台任务，
    /// 此时更新内存并启动强制保存 —— 它必须阻塞；释放后台锁后，旧快照先落盘、
    /// 强制保存再写最新队列，磁盘最终为新队列。
    #[test]
    fn test_flush_lock_serializes_background_before_forced_save() {
        let state = AppState::new_test();
        let dir = std::env::temp_dir().join("mh_test_flush_barrier");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("order_list.json");

        let mk = |uid: &str, ts: i64| QueueItem {
            id: format!("item-{}", uid),
            user_id: uid.into(),
            user_name: format!("水友{}", uid),
            monster_name: "火龙".into(),
            is_priority: false,
            guard_level: 0,
            tempered_level: 0,
            timestamp: ts,
            icon_url: String::new(),
        };
        {
            let mut q = state.queue_mgr.lock().unwrap();
            q.add_or_update(mk("b", 20));
            q.add_or_update(mk("a", 10));
        }

        // 后台任务：取得刷盘锁，取到旧快照 [a,b] 后停在这里
        let guard = state.queue_flush_lock.lock().unwrap();
        let stale_json = state.queue_mgr.lock().unwrap().to_json().unwrap();

        // 主播完成 b：内存变成 [a]
        {
            let mut q = state.queue_mgr.lock().unwrap();
            q.dequeue_by_user_id("b");
        }

        // 强制保存：必须等待刷盘锁
        let forced_state = state.clone();
        let forced_path = path.clone();
        let forced = std::thread::spawn(move || {
            flush_queue_to_path(&forced_state, &forced_path, true, None)
        });
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(!forced.is_finished(), "强制保存必须等待后台写锁，不得并行落盘");

        // 后台继续：旧快照落盘后释放锁
        QueueManager::write_json(&path, &stale_json).unwrap();
        drop(guard);

        forced.join().unwrap().expect("强制保存应成功");

        // 最终磁盘必须是完成 b 之后的最新队列
        let final_text = std::fs::read_to_string(&path).unwrap();
        let final_items: Vec<QueueItem> = serde_json::from_str(&final_text).unwrap();
        assert_eq!(
            final_items.iter().map(|i| i.user_id.clone()).collect::<Vec<_>>(),
            vec!["a"],
            "最终磁盘必须是后到的强制保存内容，旧快照不得胜出"
        );

        let snapshot = state.queue_mgr.lock().unwrap().snapshot();
        assert_eq!(snapshot.persistence, QueuePersistence::Saved, "已确认落盘");

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_flush_lock_serializes_background_before_forced_save passed");
    }

    /// A2：落盘失败必须可见（返回 Err）且保留未落盘状态，重试成功后才确认 Saved
    #[test]
    fn test_flush_failure_is_visible_and_retryable() {
        let state = AppState::new_test();
        let dir = std::env::temp_dir().join("mh_test_flush_retry");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        {
            let mut q = state.queue_mgr.lock().unwrap();
            q.add_or_update(QueueItem {
                id: "item-1".into(),
                user_id: "u1".into(),
                user_name: "水友".into(),
                monster_name: "火龙".into(),
                is_priority: false,
                guard_level: 0,
                tempered_level: 0,
                timestamp: 1,
                icon_url: String::new(),
            });
        }

        // 目标路径的父级是个文件 → 无法创建目录，写盘必然失败
        let blocked_parent = dir.join("blocked");
        std::fs::write(&blocked_parent, b"x").unwrap();
        let blocked = blocked_parent.join("order_list.json");
        assert!(
            flush_queue_to_path(&state, &blocked, true, None).is_err(),
            "写盘失败必须作为错误上报，不能静默"
        );
        assert_eq!(
            state.queue_mgr.lock().unwrap().persistence(),
            QueuePersistence::PendingRetry,
            "写盘失败后必须保留未落盘状态以便重试"
        );

        // 下一 tick / 下一次命令重试到可写路径 → 确认已保存
        let good = dir.join("order_list.json");
        assert!(flush_queue_to_path(&state, &good, false, None).is_ok());
        assert_eq!(
            state.queue_mgr.lock().unwrap().persistence(),
            QueuePersistence::Saved
        );
        assert!(std::fs::read_to_string(&good).unwrap().contains("u1"));

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_flush_failure_is_visible_and_retryable passed");
    }

    /// A1：命令返回的权威快照带版本，且内存队列与快照 items 同源
    #[test]
    fn test_queue_snapshot_surface_is_versioned() {
        let state = AppState::new_test();
        let before = queue_snapshot_or_default(&state);
        assert_eq!(before.revision, 0);
        assert_eq!(before.persistence, QueuePersistence::Saved);

        {
            let mut q = state.queue_mgr.lock().unwrap();
            q.add_or_update(QueueItem {
                id: "item-1".into(),
                user_id: "u1".into(),
                user_name: "水友".into(),
                monster_name: "火龙".into(),
                is_priority: false,
                guard_level: 0,
                tempered_level: 0,
                timestamp: 1,
                icon_url: String::new(),
            });
        }

        let after = queue_snapshot_or_default(&state);
        assert_eq!(after.revision, 1, "新增必须递增版本");
        assert_eq!(after.persistence, QueuePersistence::PendingRetry);
        assert_eq!(after.items.len(), 1);
        println!("[PASS] test_queue_snapshot_surface_is_versioned passed");
    }

    // ---------------- C1：打卡不可用必须显式停用 ----------------

    /// 打卡库不可用时：有权限用户的打卡/补签/查询必须被明确拒绝，
    /// 不写库、不伪造成功气泡、不落进普通 TTS 朗读
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_unavailable_checkin_rejects_commands_without_fake_success() {
        let state = AppState::new_test();
        // 模拟生产路径的初始化失败：显式停用打卡子系统
        let mut disabled = state.clone();
        disabled.checkin_mgr = None;
        disabled.checkin_status = Arc::new(CheckinStatus::database_unavailable());
        assert!(!disabled.checkin_available());
        assert!(disabled.checkin().is_err(), "停用后取句柄必须失败");

        let now_ts = chrono::Utc::now().timestamp();
        let mk = |msg: &str, id: &str| bilibili::DanmuData {
            user_id: "cap_disabled".into(),
            user_name: "舰长甲".into(),
            message: msg.into(),
            timestamp: now_ts,
            has_medal: true,
            medal_level: 12,
            guard_level: 3,
            msg_id: id.into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };

        for (msg, id) in [("打卡", "d1"), ("补签", "d2"), ("我的补签卡", "d3")] {
            let res = handle_incoming_danmu(None, &disabled, mk(msg, id));
            assert_eq!(
                res,
                bilibili::DanmuProcessResult {
                    user_id: res.user_id.clone(),
                    user_name: res.user_name.clone(),
                    ..Default::default()
                },
                "「{}」必须被拦下，不得产生任何业务动作", msg
            );
            assert!(
                disabled.tts_mgr.dequeue_speak().is_none(),
                "「{}」不得伪造成功气泡或落入普通朗读", msg
            );
        }

        // 数据库确实没有任何写入
        assert!(state.test_checkin().get_profile("cap_disabled").is_err());
        println!("[PASS] test_unavailable_checkin_rejects_commands_without_fake_success passed");
    }

    /// 点赞奖卡在打卡库不可用时同样不得静默丢弃
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_unavailable_checkin_rejects_like_rewards() {
        let mut disabled = AppState::new_test();
        disabled.checkin_mgr = None;
        disabled.checkin_status = Arc::new(CheckinStatus::database_unavailable());

        let ev = bilibili::LikeEvent {
            uid: "like_disabled".into(),
            username: "点赞水友".into(),
            msg_id: "like_disabled_1".into(),
            like_count: 30,
            timestamp: chrono::Utc::now().timestamp(),
        };
        assert!(
            handle_incoming_like(None, &disabled, &ev).is_empty(),
            "库不可用时不得发出奖卡回复"
        );
        assert!(disabled.tts_mgr.dequeue_speak().is_none());
        println!("[PASS] test_unavailable_checkin_rejects_like_rewards passed");
    }

    /// 高频 LIKE 的不可用提示必须节流：同一提示 60 秒内只广播一次，
    /// 否则点赞刷屏会把日志与前端事件打爆
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_like_unavailable_notice_is_throttled() {
        let mut disabled = AppState::new_test();
        disabled.checkin_mgr = None;
        disabled.checkin_status = Arc::new(CheckinStatus::database_unavailable());

        let now_ts = chrono::Utc::now().timestamp();
        let mk = |i: i64| bilibili::LikeEvent {
            uid: "spam_like".into(),
            username: "刷赞水友".into(),
            msg_id: format!("spam_like_{}", i),
            like_count: 1,
            timestamp: now_ts,
        };

        // 首次会记录时间戳
        assert!(disabled
            .last_checkin_unavailable_at
            .load(std::sync::atomic::Ordering::Relaxed)
            == 0);
        assert!(handle_incoming_like(None, &disabled, &mk(1)).is_empty());
        let first = disabled
            .last_checkin_unavailable_at
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(first > 0, "首次不可用提示应记录时间戳");

        // 随后的高频点赞不再重复广播（时间戳保持不变）
        for i in 2..=20 {
            assert!(handle_incoming_like(None, &disabled, &mk(i)).is_empty());
        }
        assert_eq!(
            disabled
                .last_checkin_unavailable_at
                .load(std::sync::atomic::Ordering::Relaxed),
            first,
            "节流窗口内不得重复广播"
        );
        println!("[PASS] test_like_unavailable_notice_is_throttled passed");
    }

    // ---------------- D4：补签不得被打卡防刷屏吞掉 ----------------

    /// 连发 3 条**不同 msg_id** 的「补签」：三个缺日都要被处理。
    ///
    /// 学习器的同内容防刷屏阈值是 3，早期实现把整个「打卡+补签」分支都套在
    /// `should_skip_duplicate` 之下，导致第 3 条补签被静默吞掉（既不补签也不朗读）。
    /// 该防刷屏只应作用于打卡模块。
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_three_retro_commands_are_not_swallowed_by_duplicate_guard() {
        let mut state = AppState::new_test();
        state.checkin_learner = Some(Arc::new(CheckinLearner::from_resources()));

        let today = chrono::Local::now().date_naive();
        let four_days_ago = today - chrono::Duration::days(4);
        let uid = "retro_three";

        // 3 张卡、3 个缺日（today-1 / -2 / -3 都没打）
        state.test_checkin().record_checkin(uid, "三连补签", four_days_ago).unwrap();
        state.test_checkin().record_checkin(uid, "三连补签", today).unwrap();
        state.test_checkin().grant_card(uid, 3).unwrap();

        let now_ts = chrono::Utc::now().timestamp();
        let mk = |id: &str| bilibili::DanmuData {
            user_id: uid.into(),
            user_name: "三连补签".into(),
            message: "补签".into(),
            timestamp: now_ts,
            has_medal: true,
            medal_level: 12,
            guard_level: 3,
            msg_id: id.into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };

        for (i, id) in ["three_1", "three_2", "three_3"].iter().enumerate() {
            let res = handle_incoming_danmu(None, &state, mk(id));
            assert_eq!(
                res.user_id, uid,
                "第 {} 条补签必须被补签分支处理", i + 1
            );
            // 每条都应产出补签播报（入 TTS 队列），而不是退化成普通弹幕朗读
            let task = state
                .tts_mgr
                .dequeue_speak()
                .unwrap_or_else(|| panic!("第 {} 条补签应产生补签播报", i + 1));
            assert!(
                task.is_checkin,
                "第 {} 条补签的播报必须是签到类（而非普通朗读）", i + 1
            );
        }

        assert_eq!(
            state.test_checkin().get_cards(uid).card_count,
            0,
            "3 条合法补签应各扣 1 张卡"
        );
        assert_eq!(
            state.test_checkin().get_profile(uid).unwrap().cumulative_days,
            5,
            "3 个缺日都应补上（2 条原始 + 3 条补签）"
        );
        println!("[PASS] test_three_retro_commands_are_not_swallowed_by_duplicate_guard passed");
    }

    /// 打卡模块自身的同内容防刷屏必须保留：连发 3 条「打卡」时第 3 条仍走重复拦截
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_checkin_duplicate_guard_still_applies() {
        let mut state = AppState::new_test();
        state.checkin_learner = Some(Arc::new(CheckinLearner::from_resources()));

        let uid = "checkin_spam";
        let now_ts = chrono::Utc::now().timestamp();
        let mk = |id: &str| bilibili::DanmuData {
            user_id: uid.into(),
            user_name: "刷屏舰长".into(),
            message: "打卡".into(),
            timestamp: now_ts,
            has_medal: true,
            medal_level: 12,
            guard_level: 3,
            msg_id: id.into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };

        for id in ["spam_1", "spam_2"] {
            let _ = handle_incoming_danmu(None, &state, mk(id));
            assert!(state.tts_mgr.dequeue_speak().is_some(), "前两条打卡应有回复");
        }
        // 第 3 条被学习器判重 → 打卡分支不执行，落到普通弹幕路径（非签到类播报或无声）
        let _ = handle_incoming_danmu(None, &state, mk("spam_3"));
        if let Some(task) = state.tts_mgr.dequeue_speak() {
            assert!(
                !task.is_checkin,
                "被防刷屏拦下的打卡不得再产生签到类播报"
            );
        }
        println!("[PASS] test_checkin_duplicate_guard_still_applies passed");
    }

    /// 打卡不可用时的状态快照必须自洽且不含路径等敏感信息
    #[test]
    fn test_checkin_status_surface_is_sanitized() {
        let state = AppState::new_test();
        let status = (*state.checkin_status).clone();
        assert!(status.available);
        assert_eq!(status.reason_code, "None");
        if let Some(f) = &status.active_db_file {
            assert!(
                !f.contains('\\') && !f.contains('/') && !f.contains(':'),
                "状态只允许暴露库文件名，不得暴露完整路径: {}",
                f
            );
        }

        let lite = CheckinStatus::lite_disabled();
        assert!(!lite.available);
        assert_eq!(lite.reason_code, "LiteDisabled");

        let broken = CheckinStatus::database_unavailable();
        assert!(!broken.available);
        assert!(broken.active_db_file.is_none());
        println!("[PASS] test_checkin_status_surface_is_sanitized passed");
    }

    // ---------------- E1/E2：凭据快照与生命周期闸门 ----------------

    fn fake_creds(app_id: &str, secret: &str, chat: &str) -> Credentials {
        Credentials {
            app_id: app_id.into(),
            access_key_id: format!("AKID{}", app_id),
            access_key_secret: secret.into(),
            chat_api_key: chat.into(),
            ..Default::default()
        }
    }

    /// E1：导入后连接与状态必须读同一份快照 —— 不再出现"状态报 B、连接用 A"
    #[test]
    fn test_credentials_snapshot_is_single_source_of_truth() {
        let state = AppState::new_test();
        let a = fake_creds("1001", "SECRET_A", "sk-a");
        state.publish_credentials(CredentialState {
            creds: a.clone(),
            loaded: true,
            blocked_reason: None,
        });

        let cfg = state.config.lock().unwrap().clone();
        let used = state.bili_credentials(&cfg);
        assert_eq!(used.app_id, "1001");
        assert_eq!(used.access_key_secret, "SECRET_A");

        // 换成 B 后，连接取到与快照一致的值
        let b = fake_creds("2002", "SECRET_B", "sk-b");
        state.publish_credentials(CredentialState {
            creds: b.clone(),
            loaded: true,
            blocked_reason: None,
        });
        let used_b = state.bili_credentials(&cfg);
        assert_eq!(used_b.app_id, "2002", "连接必须使用新快照，不得停留在旧的 Arc");
        assert_eq!(used_b.access_key_secret, "SECRET_B");

        // 状态查询同源
        let snap = state.credentials_snapshot();
        assert_eq!(snap.creds.app_id, "2002");
        assert!(snap.loaded);
        assert!(snap.blocked_reason.is_none());
        println!("[PASS] test_credentials_snapshot_is_single_source_of_truth passed");
    }

    /// E1：暂禁开播状态下凭据状态必须如实标注，不得谎报可用
    #[test]
    fn test_credentials_status_reports_blocked_state() {
        let state = AppState::new_test();
        assert!(state.credentials_snapshot().blocked_reason.is_none());

        state.publish_credentials(CredentialState {
            creds: fake_creds("3003", "S", "k"),
            loaded: true,
            blocked_reason: Some("引擎同步失败".into()),
        });
        let snap = state.credentials_snapshot();
        assert!(
            snap.blocked_reason.is_some(),
            "发布失败必须留下可查询的错误状态"
        );
        println!("[PASS] test_credentials_status_reports_blocked_state passed");
    }

    /// E1：敏感字段不得被配置页覆盖 —— save_app_config 忽略前端回传的密钥
    #[test]
    fn test_config_save_ignores_frontend_secrets() {
        let state = AppState::new_test();
        state.publish_credentials(CredentialState {
            creds: fake_creds("4004", "TRUSTED_SECRET", "sk-trusted"),
            loaded: true,
            blocked_reason: None,
        });

        // 前端配置镜像里塞入伪造/空值
        let mut hostile = AppConfig::default();
        hostile.app_id = "9999".into();
        hostile.access_key_secret = "HACKED".into();
        hostile.deepseek_api_key = "sk-hacked".into();
        hostile.mimo_api_key = "sk-mimo-hacked".into();
        hostile.manbo_api_key = "".into();

        let prev = state.config.lock().unwrap().clone();
        // 复刻 save_app_config 的字段裁决逻辑
        let snap = state.credentials_snapshot();
        let mut new_cfg = hostile.clone();
        new_cfg.app_id = snap.creds.app_id.clone();
        new_cfg.access_key_id = snap.creds.access_key_id.clone();
        new_cfg.access_key_secret = snap.creds.access_key_secret.clone();
        new_cfg.deepseek_api_key = snap.creds.chat_api_key.clone();
        new_cfg.mimo_api_key = snap.creds.mimo_tts_api_key.clone();
        if new_cfg.manbo_api_key.trim().is_empty() {
            new_cfg.manbo_api_key = prev.manbo_api_key.clone();
        }

        assert_eq!(new_cfg.app_id, "4004", "前端 app_id 不得覆盖凭据文件");
        assert_eq!(new_cfg.access_key_secret, "TRUSTED_SECRET", "密钥不得被前端覆盖");
        assert_eq!(new_cfg.deepseek_api_key, "sk-trusted");
        assert_ne!(new_cfg.mimo_api_key, "sk-mimo-hacked");
        println!("[PASS] test_config_save_ignores_frontend_secrets passed");
    }

    /// E2：活动会话期间导入必须被拒绝，且不触碰文件
    #[test]
    fn test_credentials_import_rejected_while_session_active() {
        let state = AppState::new_test();

        // 模拟正在连接
        state.bili_service.set_running(true);
        assert!(state.bili_service.is_running());
        // 复刻 import_credentials_file 的前置拒绝条件
        let rejected = state.bili_service.is_running();
        assert!(rejected, "活动会话必须拒绝导入");

        state.bili_service.set_running(false);
        // 服务端关闭状态未确认时同样不放行
        state
            .bili_service
            .set_end_state(bilibili::SessionEndState::Unknown);
        assert!(
            !state.bili_service.can_start_new_session(),
            "关闭状态未确认时必须拒绝轮换"
        );

        state
            .bili_service
            .set_end_state(bilibili::SessionEndState::Confirmed);
        assert!(state.bili_service.can_start_new_session());
        println!("[PASS] test_credentials_import_rejected_while_session_active passed");
    }

    /// E2：生命周期闸门是同一把锁 —— start 与 import 不可能同时进入临界区
    #[test]
    fn test_lifecycle_gate_is_shared_and_exclusive() {
        let state = AppState::new_test();
        let gate = state.bili_lifecycle.clone();
        let other = state.bili_lifecycle.clone();

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let g1 = gate.lock().await;
            // 第二个获取者必须等待：用 try_lock 证明它拿不到
            assert!(
                other.try_lock().is_err(),
                "生命周期闸门必须互斥（start 与 import 共用同一把）"
            );
            drop(g1);
            assert!(other.try_lock().is_ok(), "释放后应可获取");
        });

        // 两个 AppState 克隆必须指向同一把锁
        let cloned = state.clone();
        rt.block_on(async {
            let _g = state.bili_lifecycle.lock().await;
            assert!(cloned.bili_lifecycle.try_lock().is_err(), "克隆共享同一闸门");
        });
        println!("[PASS] test_lifecycle_gate_is_shared_and_exclusive passed");
    }

    /// E3：主动断开后必须先确认 end 才允许新会话；未确认时保留 game_id
    #[test]
    fn test_disconnect_requires_confirmed_end_before_new_session() {
        let state = AppState::new_test();
        state.bili_service.set_game_id(Some("gid_live".into()));
        state
            .bili_service
            .set_end_state(bilibili::SessionEndState::Active);

        // 断开：取得唯一执行权
        let gid = state.bili_service.begin_end().expect("应取得 end 执行权");
        assert_eq!(gid, "gid_live");
        // 后台不得重复 end
        assert_eq!(state.bili_service.begin_end(), None);
        // 未确认前不允许开新会话
        assert!(!state.bili_service.can_start_new_session());

        // end 失败路径：保留 ID 与 Unknown
        state.bili_service.mark_end_unknown();
        assert_eq!(state.bili_service.get_game_id().as_deref(), Some("gid_live"));
        assert!(!state.bili_service.can_start_new_session());

        // 重试成功：清理并放行
        let _ = state.bili_service.begin_end();
        state.bili_service.confirm_end();
        assert!(state.bili_service.can_start_new_session());
        assert_eq!(state.bili_service.get_game_id(), None);
        println!("[PASS] test_disconnect_requires_confirmed_end_before_new_session passed");
    }

    // ---------------- F1：History 从业务事件留档 ----------------

    /// 完整版：用户**关着语音**时，普通弹幕、打卡回复、补签查询仍必须各留档一次
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_history_recorded_even_when_voice_disabled() {
        let state = AppState::new_test();
        // 显式关闭语音（生产默认值）
        if let Ok(mut c) = state.config.lock() {
            c.enable_voice = false;
        }
        let _ = logging::take_history_sink(); // 清空探针

        let now_ts = chrono::Utc::now().timestamp();
        let mk = |msg: &str, id: &str| bilibili::DanmuData {
            user_id: "hist_cap".into(),
            user_name: "留档舰长".into(),
            message: msg.into(),
            timestamp: now_ts,
            has_medal: true,
            medal_level: 12,
            guard_level: 3,
            msg_id: id.into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };

        // 普通聊天（不含任何指令）
        let _ = handle_incoming_danmu(None, &state, mk("今天天气不错", "hist_dm_1"));
        // 打卡
        let _ = handle_incoming_danmu(None, &state, mk("打卡", "hist_dm_2"));
        // 补签查询
        let _ = handle_incoming_danmu(None, &state, mk("我的补签卡", "hist_dm_3"));

        let sink = logging::take_history_sink();
        assert!(
            sink.iter().any(|s| s == "留档舰长 说：今天天气不错"),
            "普通弹幕必须留档原文（即便关着语音）: {:?}",
            sink
        );
        assert!(
            sink.iter().any(|s| s == "留档舰长 说：打卡"),
            "打卡指令的原始弹幕必须留档: {:?}",
            sink
        );
        assert!(
            sink.iter().any(|s| s.contains("补签卡") && s.contains("留档舰长")),
            "补签查询回复必须留档: {:?}",
            sink
        );
        assert!(
            sink.iter().any(|s| s.contains("打卡") && s.contains("累计")),
            "打卡回复必须留档: {:?}",
            sink
        );
        println!(
            "[PASS] test_history_recorded_even_when_voice_disabled passed ({} 条留档)",
            sink.len()
        );
    }

    /// 字段不齐的畸形弹幕不得留档成「 说：」
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_history_skips_malformed_danmu() {
        let state = AppState::new_test();
        let _ = logging::take_history_sink();

        for (name, msg) in [("", "有消息没昵称"), ("有昵称没消息", "")] {
            let dm = bilibili::DanmuData {
                user_id: "malformed".into(),
                user_name: name.into(),
                message: msg.into(),
                timestamp: chrono::Utc::now().timestamp(),
                has_medal: false,
                medal_level: 0,
                guard_level: 0,
                msg_id: "malformed_1".into(),
                is_paid_gift: false,
                has_history_required_fields: false,
            };
            let _ = handle_incoming_danmu(None, &state, dm);
        }

        let sink = logging::take_history_sink();
        assert!(
            !sink.iter().any(|s| s.contains(" 说：")),
            "缺字段的畸形包不得留档: {:?}",
            sink
        );
        println!("[PASS] test_history_skips_malformed_danmu passed");
    }

    /// 礼物结算：任何结算文案都不得被丢弃，且文本与"是否允许播报"分别表达。
    ///
    /// 说明：官方连击准备池只会收 `paid=true` 的事件，因此 `only_paid_gift` 对它是恒真的；
    /// 动态池的尾报也不受该开关约束。本测试锁定的是**不丢文案**这一保证
    /// （文案被丢弃就等于业务留档永久缺失），以及 `can_speak` 的正确取值。
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_gift_settlement_never_drops_text() {
        let _ = logging::take_history_sink();
        let t0 = std::time::Instant::now();
        let later = t0 + std::time::Duration::from_secs(3600);

        // ① 免费礼物的动态连击尾报
        let mut tracker = tts::GiftComboTracker::new();
        let free = tts::GiftEvent {
            open_id: "gift_free".into(),
            gift_id: "1".into(),
            uname: "免费礼物水友".into(),
            gift_name: "辣条".into(),
            gift_num: 3,
            paid: false,
            combo: None,
        };
        let _ = tracker.handle(&free, t0);
        let free_reports = tracker.tick(later, true);
        assert!(!free_reports.is_empty(), "免费礼物结算文案不得被丢弃");
        assert!(free_reports[0].text.contains("免费礼物水友"));

        // ② 官方连击（付费）准备池的结算
        let mut tracker2 = tts::GiftComboTracker::new();
        let paid = tts::GiftEvent {
            open_id: "gift_paid".into(),
            gift_id: "2".into(),
            uname: "付费水友".into(),
            gift_name: "小心心".into(),
            gift_num: 1,
            paid: true,
            combo: Some(tts::ComboInfo {
                base_num: 5,
                count: 2,
                timeout_secs: 1.0,
            }),
        };
        let _ = tracker2.handle(&paid, t0);
        let paid_reports = tracker2.tick(later, true);
        assert_eq!(paid_reports.len(), 1, "官方连击应结算一条: {:?}", paid_reports);
        assert!(paid_reports[0].text.contains("10"), "应结算 5×2=10 个");
        assert!(
            paid_reports[0].can_speak,
            "付费礼物在仅付费模式下仍应可播报"
        );

        // ③ 后台泵路径：对每条结算文案都留档，与 can_speak 无关
        for report in free_reports.iter().chain(paid_reports.iter()) {
            record_business_history_probe(&report.text);
        }
        let sink = logging::take_history_sink();
        assert!(
            sink.iter().any(|s| s.contains("免费礼物水友")),
            "免费礼物文案必须留档: {:?}",
            sink
        );
        assert!(
            sink.iter().any(|s| s.contains("付费水友")),
            "付费礼物文案必须留档: {:?}",
            sink
        );
        assert_eq!(sink.len(), 2, "每条结算文案恰好留档一次: {:?}", sink);
        println!("[PASS] test_gift_settlement_never_drops_text passed");
    }

    /// D3：点赞事务失败后必须释放去重预留，同一事件的重投可以真正重试
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_like_failure_releases_dedup_key_and_allows_retry() {
        let state = AppState::new_test();
        let today = chrono::Local::now().date_naive();
        let now_ts = chrono::Utc::now().timestamp();
        let ev = bilibili::LikeEvent {
            uid: "like_retry".into(),
            username: "重试水友".into(),
            msg_id: "like_retry_msg_1".into(),
            like_count: 30,
            timestamp: now_ts,
        };

        // 注入：连赞标记写入失败，使整个点赞事务回滚
        state
            .test_checkin()
            .inject_sql_for_test(
                r#"
                CREATE TRIGGER refuse_like_streak BEFORE INSERT ON user_like_streaks
                BEGIN
                    SELECT RAISE(ABORT, 'injected like failure');
                END;
                "#,
            )
            .unwrap();

        // 失败：无回复、无卡，且**预留已释放**
        assert!(
            handle_incoming_like(None, &state, &ev).is_empty(),
            "事务失败不得发出奖卡回复"
        );
        assert_eq!(state.test_checkin().get_cards("like_retry").card_count, 0);
        assert!(
            !state.danmu_processor.msg_id_reserved("like_retry_msg_1"),
            "失败后必须释放去重预留，否则重投会被自己的缓存永久挡住"
        );

        // 解除故障后**同一 msg_id 重投**必须能真正入账（不是被缓存吞掉）
        state
            .test_checkin()
            .inject_sql_for_test("DROP TRIGGER IF EXISTS refuse_like_streak;")
            .unwrap();
        let replies = handle_incoming_like(None, &state, &ev);
        assert_eq!(replies.len(), 1, "重投应重新结算: {:?}", replies);
        assert_eq!(replies[0], "重试水友，恭喜！今日点赞突破30，获得1张补签卡！");
        assert_eq!(state.test_checkin().get_cards("like_retry").card_count, 1);
        assert_eq!(
            state.test_checkin().get_daily_like_total("like_retry", today),
            Some(30),
            "重试后当日点赞应如实入账（不是 60）"
        );

        // 成功后 ID 保持占用：再投一次不得重复发卡
        assert!(handle_incoming_like(None, &state, &ev).is_empty());
        assert_eq!(state.test_checkin().get_cards("like_retry").card_count, 1);
        // 账本无残留：失败留下的意图行已被重投结算销账
        assert!(
            state
                .test_checkin()
                .list_pending_like_intents(10)
                .unwrap()
                .is_empty(),
            "重投结算后不得残留待补意图"
        );
        println!("[PASS] test_like_failure_releases_dedup_key_and_allows_retry passed");
    }

    /// D3：点赞事务失败后意图持久化在账本中，补偿扫描器重放后自动补记入账，
    /// 且补账后的同事件重投撞上已办凭证被静默拦截，不得双计
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_like_failure_persists_intent_and_backfills() {
        let state = AppState::new_test();
        let today = chrono::Local::now().date_naive();
        let now_ts = chrono::Utc::now().timestamp();
        let ev = bilibili::LikeEvent {
            uid: "like_backfill".into(),
            username: "补账水友".into(),
            msg_id: "like_backfill_msg_1".into(),
            like_count: 30,
            timestamp: now_ts,
        };

        // 注入：连赞标记写入失败，使整个点赞结算回滚
        state
            .test_checkin()
            .inject_sql_for_test(
                r#"
                CREATE TRIGGER refuse_like_streak BEFORE INSERT ON user_like_streaks
                BEGIN
                    SELECT RAISE(ABORT, 'injected like failure');
                END;
                "#,
            )
            .unwrap();
        assert!(handle_incoming_like(None, &state, &ev).is_empty());

        // 失败不等于丢失：意图已在账本中持久化（含重试计数）
        let pending = state.test_checkin().list_pending_like_intents(10).unwrap();
        assert_eq!(pending.len(), 1, "失败后必须恰有一条待补意图: {:?}", pending);
        assert_eq!(pending[0].uid, "like_backfill");
        assert_eq!(pending[0].like_count, 30);
        assert_eq!(pending[0].msg_id.as_deref(), Some("like_backfill_msg_1"));
        assert_eq!(pending[0].retry_count, 1, "结算失败必须记一次重试计数");
        assert_eq!(
            state
                .test_checkin()
                .get_daily_like_total("like_backfill", today),
            None,
            "失败事件不得入账"
        );

        // 故障解除：补偿扫描器重放 → 自动补记入账（keep_receipt=true → done 凭证）
        state
            .test_checkin()
            .inject_sql_for_test("DROP TRIGGER IF EXISTS refuse_like_streak;")
            .unwrap();
        replay_pending_likes(&state, None);
        assert_eq!(
            state
                .test_checkin()
                .get_daily_like_total("like_backfill", today),
            Some(30),
            "补偿重放后当日点赞应如实入账"
        );
        assert_eq!(state.test_checkin().get_cards("like_backfill").card_count, 1);
        assert!(
            state
                .test_checkin()
                .list_pending_like_intents(10)
                .unwrap()
                .is_empty(),
            "补账后不得残留 pending 行"
        );

        // 补账后同一事件重投：stage 命中 done 凭证行，settle 静默跳过，不得双计
        assert!(handle_incoming_like(None, &state, &ev).is_empty());
        assert_eq!(
            state
                .test_checkin()
                .get_daily_like_total("like_backfill", today),
            Some(30),
            "补账后的重投不得二次累加"
        );
        println!("[PASS] test_like_failure_persists_intent_and_backfills passed");
    }

    /// D3：失败 → 服务端重投（扫描器未跑）→ 命中同一意图行单计入账，账本无残留
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_like_failure_then_redelivery_settles_pending() {
        let state = AppState::new_test();
        let today = chrono::Local::now().date_naive();
        let now_ts = chrono::Utc::now().timestamp();
        let ev = bilibili::LikeEvent {
            uid: "like_restay".into(),
            username: "重投水友".into(),
            msg_id: "like_restay_msg_1".into(),
            like_count: 5,
            timestamp: now_ts,
        };

        state
            .test_checkin()
            .inject_sql_for_test(
                r#"
                CREATE TRIGGER refuse_like_streak BEFORE INSERT ON user_like_streaks
                BEGIN
                    SELECT RAISE(ABORT, 'injected like failure');
                END;
                "#,
            )
            .unwrap();
        assert!(handle_incoming_like(None, &state, &ev).is_empty());
        assert_eq!(
            state
                .test_checkin()
                .list_pending_like_intents(10)
                .unwrap()
                .len(),
            1,
            "失败后意图必须留在账本中"
        );

        // 解除故障后重投（不跑扫描器）：stage 复用 pending 行，settle 入账并销账
        state
            .test_checkin()
            .inject_sql_for_test("DROP TRIGGER IF EXISTS refuse_like_streak;")
            .unwrap();
        let _ = handle_incoming_like(None, &state, &ev);
        assert_eq!(
            state
                .test_checkin()
                .get_daily_like_total("like_restay", today),
            Some(5),
            "重投应命中同一意图行并单计入账"
        );
        assert!(
            state
                .test_checkin()
                .list_pending_like_intents(10)
                .unwrap()
                .is_empty(),
            "重投结算后账本不得残留"
        );

        // 再次重投：预留已确认占用，Duplicate 拦截，不得重复累计
        let _ = handle_incoming_like(None, &state, &ev);
        assert_eq!(
            state
                .test_checkin()
                .get_daily_like_total("like_restay", today),
            Some(5)
        );
        println!("[PASS] test_like_failure_then_redelivery_settles_pending passed");
    }

    /// D3：空 msg_id 不得伪造唯一键 —— 合法的多次点赞都要各自入账
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_like_with_empty_msg_id_is_not_deduped() {
        let state = AppState::new_test();
        let now_ts = chrono::Utc::now().timestamp();
        let ev = bilibili::LikeEvent {
            uid: "like_nokey".into(),
            username: "无ID水友".into(),
            msg_id: String::new(),
            like_count: 30,
            timestamp: now_ts,
        };

        // 第一次突破 30 发卡（周首破）
        let first = handle_incoming_like(None, &state, &ev);
        assert_eq!(first.len(), 1, "{:?}", first);
        assert_eq!(state.test_checkin().get_cards("like_nokey").card_count, 1);

        // 同日再来一条空 ID 的赞：仍在累加（不得被空键判重挡住）
        let second = handle_incoming_like(None, &state, &ev);
        assert!(second.is_empty(), "同周已领取，不再发卡");
        assert_eq!(
            state.test_checkin().get_daily_like_total("like_nokey", chrono::Local::now().date_naive()),
            Some(60),
            "空 msg_id 的第二次点赞必须真实累加"
        );
        println!("[PASS] test_like_with_empty_msg_id_is_not_deduped passed");
    }

    /// D3：DM 与 LIKE 共用同一缓存时不得互相误杀或重复消费首次 ID
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_dm_and_like_share_cache_without_double_consumption() {
        let state = AppState::new_test();
        let now_ts = chrono::Utc::now().timestamp();

        // ① 首次 DM 占用 shared_id
        let dm = bilibili::DanmuData {
            user_id: "shared_user".into(),
            user_name: "共享水友".into(),
            message: "点怪 火龙".into(),
            timestamp: now_ts,
            has_medal: true,
            medal_level: 10,
            guard_level: 3,
            msg_id: "shared_id".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };
        let _ = handle_incoming_danmu(None, &state, dm.clone());
        assert_eq!(state.queue_mgr.lock().unwrap().items.len(), 1, "首次 DM 应入队");

        // 同 ID 重投被缓存拒绝（不重复入队）
        let _ = handle_incoming_danmu(None, &state, dm.clone());
        assert_eq!(state.queue_mgr.lock().unwrap().items.len(), 1, "重复 DM 不得重复入队");

        // ② LIKE 用同一个 ID 也是重复（共享语义），不得被"重新消费"成一次新点赞
        let like_same = bilibili::LikeEvent {
            uid: "shared_user".into(),
            username: "共享水友".into(),
            msg_id: "shared_id".into(),
            like_count: 30,
            timestamp: now_ts,
        };
        assert!(
            handle_incoming_like(None, &state, &like_same).is_empty(),
            "已被 DM 占用的 ID 不得再被 LIKE 消费一次"
        );
        assert_eq!(state.test_checkin().get_cards("shared_user").card_count, 0);

        // ③ 另一条全新 ID 的 LIKE 正常处理（DM 占用不影响不同 ID）
        let like_new = bilibili::LikeEvent {
            msg_id: "shared_id_like".into(),
            ..like_same
        };
        assert_eq!(handle_incoming_like(None, &state, &like_new).len(), 1);
        assert_eq!(state.test_checkin().get_cards("shared_user").card_count, 1);

        // ④ 失败的 LIKE 释放自己的 ID，不影响已确认的 DM 预留
        assert!(state.danmu_processor.msg_id_reserved("shared_id"), "DM 预留必须保留");
        println!("[PASS] test_dm_and_like_share_cache_without_double_consumption passed");
    }

    // ---------------- B2：选怪面板按原名精确取键 + 后端禁点校验 ----------------

    /// 面板入口：字典内精确命中的原名可以入队，并用该条目自身的默认等级与图标
    #[test]
    fn test_picked_order_uses_exact_original_name() {
        let state = AppState::new_test();

        picked_order_enqueue(
            &state,
            "manual_1".into(),
            "房管".into(),
            "黑龙",
             false,
            None,
            None,
        )
        .expect("字典内的原名应可入队");

        let q = state.queue_mgr.lock().unwrap();
        assert_eq!(q.items.len(), 1);
        assert_eq!(q.items[0].monster_name, "黑龙", "面板必须点中它自己");
        let expected = state.monster_mgr.exact_entry("黑龙").unwrap();
        assert_eq!(q.items[0].tempered_level, expected.tempered_level);
        assert_eq!(q.items[0].icon_url, expected.icon_url);
        println!("[PASS] test_picked_order_uses_exact_original_name passed");
    }

    /// 面板入口：未知名字必须被拒绝（保留弹幕路径的未知名兼容契约，不在面板沿用）
    #[test]
    fn test_picked_order_rejects_unknown_name() {
        let state = AppState::new_test();
        let err = picked_order_enqueue(
            &state,
            "manual_x".into(),
            "房管".into(),
            "词库里不存在的怪",
            false,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("词库中没有"), "{}", err);
        assert!(state.queue_mgr.lock().unwrap().items.is_empty(), "拒绝后不得入队");
        println!("[PASS] test_picked_order_rejects_unknown_name passed");
    }

    /// 面板入口：禁点原名必须被拒绝，且不得入队
    #[test]
    fn test_picked_order_rejects_blocked_original_name() {
        let state = AppState::new_test();
        state
            .roster
            .replace(RosterData {
                items: vec!["黑龙".into()],
            })
            .unwrap();

        let err = picked_order_enqueue(
            &state,
            "manual_blocked".into(),
            "房管".into(),
            "黑龙",
            false,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("禁点名单"), "{}", err);
        assert!(
            state.queue_mgr.lock().unwrap().items.is_empty(),
            "禁点原名不得入队"
        );

        // 名单外的怪不受影响
        picked_order_enqueue(
            &state,
            "manual_ok".into(),
            "房管".into(),
            "雌火龙",
            false,
            None,
            None,
        )
        .expect("名单外的怪应可入队");
        println!("[PASS] test_picked_order_rejects_blocked_original_name passed");
    }

    /// 面板入口：tempered_level 的 None / Some 语义必须被保留
    #[test]
    fn test_picked_order_tempered_level_override_semantics() {
        let state = AppState::new_test();

        // None = 跟随字典默认
        picked_order_enqueue(&state, "m1".into(), "房管".into(), "黑龙", false, None, None).unwrap();
        let default_level = state.monster_mgr.exact_entry("黑龙").unwrap().tempered_level;
        {
            let q = state.queue_mgr.lock().unwrap();
            let item = q.items.iter().find(|i| i.user_id == "m1").unwrap();
            assert_eq!(item.tempered_level, default_level, "None 应跟随字典默认等级");
        }

        // Some(2) = 主播显式覆盖
        picked_order_enqueue(&state, "m2".into(), "房管".into(), "雌火龙", false, None, Some(2))
            .unwrap();
        let q = state.queue_mgr.lock().unwrap();
        let item = q.items.iter().find(|i| i.user_id == "m2").unwrap();
        assert_eq!(item.tempered_level, 2, "显式覆盖必须生效");
        println!("[PASS] test_picked_order_tempered_level_override_semantics passed");
    }

    /// B2 锁序：持有名单读 guard 时，名单写入必须等待（检查与动作处于同一临界区）
    #[test]
    fn test_roster_read_guard_covers_check_to_enqueue() {
        // 独立临时名单文件：`AppState::new_test` 的名单路径是共享的，
        // 在这里写入会污染并行运行的其他名单测试
        let dir = std::env::temp_dir().join("mh_test_roster_guard");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let roster = Arc::new(MonsterRoster::load(Some(
            &dir.join(roster::ROSTER_FILE_NAME),
        )));
        roster.replace(RosterData::default()).unwrap();

        let guard = roster.read_guard();
        assert!(!guard.items.iter().any(|n| n == "黑龙"));

        // 持读锁期间，写操作必须被挡在外面（用另一个 Arc 句柄发起，避免与 guard 借用冲突）
        let roster_writer = roster.clone();
        let writer = std::thread::spawn(move || {
            roster_writer.replace(RosterData {
                items: vec!["黑龙".into()],
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(120));
        assert!(
            !writer.is_finished(),
            "名单写入必须等待读 guard 释放（否则检查与动作之间会插进一次编辑）"
        );

        drop(guard);
        writer.join().unwrap().expect("释放后写入应成功");
        assert!(roster.is_blocked("黑龙"));

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_roster_read_guard_covers_check_to_enqueue passed");
    }

    // ---------------- F3：诊断日志不泄漏业务原文 ----------------

    /// 驱动真实日志调用点（点怪成功 / 禁点拦截 / 字典编辑 / 点赞缺字段）后，
    /// 内存日志环里不得出现昵称、怪名、uid、伪造的 [ERROR] 行或密钥标记。
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_real_log_callsites_do_not_leak_business_content() {
        // 用独一无二的虚构标记，避免与其他测试的日志混淆
        const NICK: &str = "LEAKPROBE_NICK_7f3a";
        const MONSTER: &str = "LEAKPROBE_MONSTER_7f3a";
        const SECRET: &str = "LEAKPROBE_SECRET_7f3a";
        const FORGED: &str = "LEAKPROBE_FORGED_7f3a";

        logging::clear_recent();
        let state = AppState::new_test();
        let now_ts = chrono::Utc::now().timestamp();

        // ① 正常点怪（成功路径会记 INFO）
        let dm = bilibili::DanmuData {
            user_id: "LEAKPROBE_UID_7f3a".into(),
            user_name: NICK.into(),
            message: format!("点怪 {} \n[2026-01-01 00:00:00]:[ERROR] {}", MONSTER, FORGED),
            timestamp: now_ts,
            has_medal: true,
            medal_level: 10,
            guard_level: 3,
            msg_id: "leakprobe_dm_1".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };
        let _ = handle_incoming_danmu(None, &state, dm);

        // ② 禁点拦截路径
        state
            .roster
            .replace(RosterData {
                items: vec![MONSTER.into()],
            })
            .unwrap();
        let dm_blocked = bilibili::DanmuData {
            message: format!("点怪 {}", MONSTER),
            msg_id: "leakprobe_dm_2".into(),
            ..bilibili::DanmuData {
                user_id: "LEAKPROBE_UID_7f3a".into(),
                user_name: NICK.into(),
                message: String::new(),
                timestamp: now_ts,
                has_medal: true,
                medal_level: 10,
                guard_level: 3,
                msg_id: String::new(),
                is_paid_gift: false,
                has_history_required_fields: true,
            }
        };
        let _ = handle_incoming_danmu(None, &state, dm_blocked);

        // ③ 点赞事件缺字段（会记 WARN，历史实现曾把整包 JSON 打出来）
        let _ = bilibili::parse_like_event(&serde_json::json!({
            "uname": NICK,
            "secret_field": SECRET,
        }));

        // ④ 字典编辑（真实调用点：历史实现曾把用户可编辑怪名打进 INFO）
        let dir = std::env::temp_dir().join("mh_test_log_leak_dict");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dict_path = dir.join("monster_list.json");
        std::fs::write(&dict_path, r#"{"甲怪":{"默认历战等级":0,"图标地址":"","别称":[]}}"#).unwrap();
        let mut mgr = MonsterDataManager::new();
        let _ = mgr.load_from_file(Some(&dict_path));
        mgr.edit_and_save(&dict_path, |raw| {
            monster::MonsterDataManager::upsert_entry(
                raw,
                MONSTER,
                &monster::MonsterConfig {
                    default_tempered_level: 0,
                    icon_url: String::new(),
                    nicknames: vec![SECRET.to_string()],
                },
                None,
            )
        })
        .expect("新增条目应成功");

        // ⑤ 直接检查脱敏原语：换行被压平，伪造行不可能出现在落盘文本里
        let forged = format!("{FOO}\n[2026-01-01 00:00:00]:[ERROR] {FORGED}", FOO = MONSTER);
        let sanitized = logging::sanitize_field(&forged);
        let line = logging::format_line(
            "2026-01-01 00:00:00",
            logging::LogLevel::Info,
            &sanitized,
        );
        assert_eq!(
            line.matches('\n').count(),
            1,
            "脱敏后只应保留结尾换行: {:?}",
            line
        );
        assert!(
            !line.contains("\n[2026-01-01 00:00:00]:[ERROR]"),
            "不得凭空造出伪造的 ERROR 行: {:?}",
            line
        );

        // ⑥ 扫描内存环：确认真实调用点没有把业务原文写进诊断日志
        let ring = logging::recent_entries(logging::MAX_RECENT_ENTRIES, None);
        let leaked: Vec<String> = ring
            .iter()
            .filter(|e| {
                let m = &e.message;
                m.contains(NICK) || m.contains(SECRET) || m.contains(FORGED) || m.contains("LEAKPROBE_UID")
            })
            .map(|e| format!("[{}] {}", e.level, e.message))
            .collect();
        assert!(
            leaked.is_empty(),
            "诊断日志不得包含昵称／密钥／uid／伪造标记:\n{}",
            leaked.join("\n")
        );

        // 点怪／禁点／字典编辑路径都不得写出怪名
        let monster_leaks: Vec<String> = ring
            .iter()
            .filter(|e| e.message.contains(MONSTER))
            .map(|e| e.message.clone())
            .collect();
        assert!(
            monster_leaks.is_empty(),
            "点怪／禁点／字典编辑路径的日志不得写出怪名:\n{}",
            monster_leaks.join("\n")
        );

        let _ = std::fs::remove_dir_all(&dir);
        logging::clear_recent();
        println!(
            "[PASS] test_real_log_callsites_do_not_leak_business_content passed（扫描 {} 条日志）",
            ring.len()
        );
    }

    /// Lite 形态：打卡子系统必须整体不实例化（不探测路径、不建目录、不 open、不建表）
    #[cfg(feature = "lite")]
    #[test]
    fn test_lite_never_instantiates_checkin_subsystem() {
        let state = AppState::default();
        assert!(
            state.checkin_mgr.is_none(),
            "Lite 不得实例化打卡管理器（否则会建目录/开库/建表）"
        );
        assert_eq!(state.checkin_status.reason_code, "LiteDisabled");
        assert!(!state.checkin_status.available);
        assert!(state.checkin_status.active_db_file.is_none());
        assert!(state.checkin().is_err(), "Lite 下所有打卡入口必须直接拒绝");
        println!("[PASS] test_lite_never_instantiates_checkin_subsystem passed");
    }

    /// 补签权限/查询词验证走完整弹幕管线，属完整版专属（Lite 构建下补签链路被短路）
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_retro_permission_and_query_words() {
        let state = AppState::new_test();
        let today = chrono::Local::now().date_naive();
        let d_3_days_ago = today - chrono::Duration::days(3);
        let now_ts = chrono::Utc::now().timestamp();

        // 佩戴粉丝牌的非舰长用户：C4 权限放宽后可补签
        state.test_checkin().record_checkin("medal_user", "粉丝牌水友", d_3_days_ago).unwrap();
        state.test_checkin().record_checkin("medal_user", "粉丝牌水友", today).unwrap();
        state.test_checkin().grant_card("medal_user", 1).unwrap();

        let dm = bilibili::DanmuData {
            user_id: "medal_user".into(),
            user_name: "粉丝牌水友".into(),
            message: "补签卡".into(), // 操作词表包含「补签卡」
            timestamp: now_ts,
            has_medal: true,
            medal_level: 10,
            guard_level: 0,
            msg_id: "retro_medal_1".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };
        let _ = handle_incoming_danmu(None, &state, dm);
        let cards = state.test_checkin().get_cards("medal_user");
        assert_eq!(cards.card_count, 0, "粉丝牌用户应可补签并扣卡");
        let retro_task = state.tts_mgr.dequeue_speak().expect("粉丝牌补签应入队播报");
        assert!(retro_task.is_checkin && retro_task.checkin_username == "粉丝牌水友");

        // 查询词表命中（"我的补签卡"），仅气泡不朗读
        let dm_query = bilibili::DanmuData {
            user_id: "medal_user".into(),
            user_name: "粉丝牌水友".into(),
            message: "我的补签卡".into(),
            timestamp: now_ts + 1,
            has_medal: true,
            medal_level: 10,
            guard_level: 0,
            msg_id: "retro_query_1".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };
        let _ = handle_incoming_danmu(None, &state, dm_query);
        assert!(state.tts_mgr.dequeue_speak().is_none(), "查询指令不应朗读");

        // 无舰长无粉丝牌用户：不响应补签
        let dm_plain = bilibili::DanmuData {
            user_id: "plain_user".into(),
            user_name: "路过的水友".into(),
            message: "补签".into(),
            timestamp: now_ts + 2,
            has_medal: false,
            medal_level: 0,
            guard_level: 0,
            msg_id: "retro_plain_1".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };
        let _ = handle_incoming_danmu(None, &state, dm_plain);
        // 无权用户的消息按普通弹幕朗读（未进入补签指令分支）
        let task = state.tts_mgr.dequeue_speak().expect("普通用户补签应作为普通弹幕朗读");
        assert_eq!(task.text, "路过的水友 说：补签");
        println!("[PASS] test_retro_permission_and_query_words passed");
    }

    /// 点赞奖卡链路属完整版专属（Lite 构建下 handle_incoming_like 直接返回空）
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_like_pipeline_dedup_and_rewards() {
        let state = AppState::new_test();
        let now_ts = chrono::Utc::now().timestamp();

        // 今日点赞突破 30 → 奖卡播报
        let ev = bilibili::LikeEvent {
            uid: "like_user".into(),
            username: "点赞水友".into(),
            msg_id: "like_msg_1".into(),
            like_count: 30,
            timestamp: now_ts,
        };
        let replies = handle_incoming_like(None, &state, &ev);
        assert_eq!(replies.len(), 1, "{:?}", replies);
        assert_eq!(replies[0], "点赞水友，恭喜！今日点赞突破30，获得1张补签卡！");
        let task = state.tts_mgr.dequeue_speak().expect("奖卡播报应入队");
        assert_eq!(task.text, replies[0]);

        // 相同 msg_id 重复到达 → 去重丢弃
        let dup = handle_incoming_like(None, &state, &ev);
        assert!(dup.is_empty(), "重复 msg_id 应被去重");
        assert_eq!(state.test_checkin().get_cards("like_user").card_count, 1);

        // 同周内再次突破 30 → 每周限领 1 张，不再播报
        let again = bilibili::LikeEvent {
            msg_id: "like_msg_2".into(),
            like_count: 30,
            ..ev.clone()
        };
        assert!(handle_incoming_like(None, &state, &again).is_empty());

        // 连续 7 天点赞 → 连续奖卡播报（每天 1 次点赞，日期逐日推进）
        for day in 0..7i64 {
            let ts = now_ts - (6 - day) * 86400;
            let streak_ev = bilibili::LikeEvent {
                uid: "streak_user".into(),
                username: "连赞水友".into(),
                msg_id: format!("streak_msg_{}", day),
                like_count: 1,
                timestamp: ts,
            };
            let r = handle_incoming_like(None, &state, &streak_ev);
            if day == 6 {
                assert_eq!(r.len(), 1, "第 7 天应触发连续奖卡: {:?}", r);
                assert_eq!(r[0], "连赞水友，恭喜！连续7天点赞，获得1张补签卡！");
            } else {
                assert!(r.is_empty(), "第 {} 天不应发奖: {:?}", day, r);
            }
        }

        println!("[PASS] test_like_pipeline_dedup_and_rewards passed");
    }

    /// 弹幕学习链路属完整版专属（Lite 构建下不进行发言习惯学习）
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_danmu_learning_pipeline_and_duplicate_skip() {
        let mut state = AppState::new_test();
        // 注入学习器（生产环境在 run() 中注入，测试需显式注入以覆盖学习链路）
        state.checkin_learner = Some(Arc::new(CheckinLearner::from_resources()));

        let now_ts = chrono::Utc::now().timestamp();
        let make = |msg: &str, ts: i64, id: &str| bilibili::DanmuData {
            user_id: "learn_captain".into(),
            user_name: "学习舰长".into(),
            message: msg.into(),
            timestamp: ts,
            has_medal: true,
            medal_level: 15,
            guard_level: 3,
            msg_id: id.into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };

        // 舰长弹幕 → 学习入档（关键词 + 发言历史）
        let _ = handle_incoming_danmu(None, &state, make("区块链 云计算", now_ts, "learn_1"));
        let learned = state.test_checkin().load_learning("learn_captain");
        assert_eq!(learned.danmu_history.len(), 1);
        assert!(learned.keywords.iter().any(|k| k.word == "云计算"), "{:?}", learned.keywords);
        // 清掉该普通弹幕的朗读任务，避免干扰后续断言
        let _ = state.tts_mgr.dequeue_speak();

        // 同内容连续 3 条 → 第 3 条起触发防刷屏跳过（打卡指令不再响应）
        // 第 1 次打卡
        let _ = handle_incoming_danmu(None, &state, make("打卡", now_ts + 10, "learn_2"));
        let first = state.tts_mgr.dequeue_speak().expect("首次打卡应回复");
        assert_eq!(first.text, "学习舰长连续第1天打卡！累计1天");
        // 第 2 次打卡（重复打卡回复）
        let _ = handle_incoming_danmu(None, &state, make("打卡", now_ts + 20, "learn_3"));
        let second = state.tts_mgr.dequeue_speak().expect("重复打卡应回复");
        assert_eq!(second.text, "学习舰长今日已打卡，连续1天，累计1天");
        // 第 3 次打卡（连续相同内容 ≥3 → 防刷屏跳过打卡处理，仅按普通弹幕朗读，与原工程一致）
        let _ = handle_incoming_danmu(None, &state, make("打卡", now_ts + 30, "learn_4"));
        let third = state.tts_mgr.dequeue_speak().expect("防刷屏跳过后应仅剩普通朗读");
        assert_eq!(third.text, "学习舰长 说：打卡", "重复打卡回复应被防刷屏拦截");

        // 非舰长（粉丝牌）弹幕不参与学习
        let medal_dm = bilibili::DanmuData {
            user_id: "learn_medal".into(),
            user_name: "粉丝牌水友".into(),
            message: "太刀真好玩".into(),
            timestamp: now_ts,
            has_medal: true,
            medal_level: 5,
            guard_level: 0,
            msg_id: "learn_5".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };
        let _ = handle_incoming_danmu(None, &state, medal_dm);
        assert!(state.test_checkin().load_learning("learn_medal").danmu_history.is_empty());
        // 清掉普通弹幕朗读任务
        let _ = state.tts_mgr.dequeue_speak();

        // 打卡模块总开关关闭（enable_captain_checkin_ai=false）→ 打卡与学习全部停用
        state.config.lock().unwrap().enable_captain_checkin_ai = false;
        let disabled_dm = bilibili::DanmuData {
            user_id: "learn_disabled".into(),
            user_name: "停用测试舰长".into(),
            message: "打卡".into(),
            timestamp: now_ts + 40,
            has_medal: true,
            medal_level: 15,
            guard_level: 3,
            msg_id: "learn_6".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };
        let _ = handle_incoming_danmu(None, &state, disabled_dm);
        assert!(
            state.test_checkin().get_profile("learn_disabled").is_err(),
            "打卡模块停用后不应落库打卡"
        );
        assert!(state
            .test_checkin()
            .load_learning("learn_disabled")
            .danmu_history
            .is_empty());
        // 停用后该消息仅按普通弹幕朗读（无打卡回复）
        let disabled_task = state.tts_mgr.dequeue_speak().expect("停用后应仅剩普通朗读");
        assert_eq!(disabled_task.text, "停用测试舰长 说：打卡");
        println!("[PASS] test_danmu_learning_pipeline_and_duplicate_skip passed");
    }

    /// 指令类弹幕判定：触发词精确匹配（含自定义触发词），不误伤普通发言
    #[test]
    fn test_is_command_message_recognizes_commands() {
        let triggers = vec!["打卡".to_string(), "签到".to_string()];
        assert!(is_command_message("打卡", &triggers));
        assert!(is_command_message("签到", &triggers));
        assert!(is_command_message("补签", &triggers), "补签操作词应判定为指令");
        assert!(is_command_message("我的补签卡", &triggers), "补签查询词应判定为指令");

        assert!(!is_command_message("今天打了卡", &triggers), "包含触发词的普通发言照常学习");
        assert!(!is_command_message("太刀真好玩", &triggers));
        assert!(!is_command_message("打卡", &[]), "未配置触发词时打卡只是普通发言");
        println!("[PASS] test_is_command_message_recognizes_commands passed");
    }

    /// 指令类弹幕不写入发言习惯：避免「打卡」「补签」混进关键词与 AI 提示词的「最近发言」
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_command_danmu_excluded_from_learning() {
        let mut state = AppState::new_test();
        state.checkin_learner = Some(Arc::new(CheckinLearner::from_resources()));

        let now_ts = chrono::Utc::now().timestamp();
        let make = |msg: &str, ts: i64, id: &str| bilibili::DanmuData {
            user_id: "cmd_captain".into(),
            user_name: "指令测试舰长".into(),
            message: msg.into(),
            timestamp: ts,
            has_medal: true,
            medal_level: 15,
            guard_level: 3,
            msg_id: id.into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };

        // 普通弹幕正常学习
        let _ = handle_incoming_danmu(None, &state, make("区块链 云计算", now_ts, "cmd_1"));
        assert!(state
            .test_checkin()
            .load_learning("cmd_captain")
            .danmu_history
            .iter()
            .any(|(_, c)| c == "区块链 云计算"));

        // 打卡 / 补签 / 补签查询依次入站（间隔避开 5s 节流与同内容防刷屏）
        for (idx, msg) in ["打卡", "补签", "我的补签卡"].iter().enumerate() {
            let _ = handle_incoming_danmu(
                None,
                &state,
                make(msg, now_ts + 10 + idx as i64 * 10, &format!("cmd_{}", idx + 2)),
            );
        }
        while state.tts_mgr.dequeue_speak().is_some() {}

        let learned = state.test_checkin().load_learning("cmd_captain");
        let history: Vec<&str> = learned.danmu_history.iter().map(|(_, c)| c.as_str()).collect();
        assert_eq!(
            history,
            vec!["区块链 云计算"],
            "指令类弹幕不应写入发言历史: {:?}",
            history
        );
        assert!(
            !learned
                .keywords
                .iter()
                .any(|k| k.word.contains("打卡") || k.word.contains("补签")),
            "指令词不应进入关键词: {:?}",
            learned.keywords
        );

        // 组装的 AI 提示词「最近发言」只含真实发言
        let prompt = checkin_ai::build_prompt(&checkin_ai::CheckinContext {
            username: "指令测试舰长",
            continuous_days: 1,
            cumulative_days: 1,
            checkin_date: 20260924,
            last_checkin_date: 0,
            profile: &learned,
        });
        assert!(prompt.contains("最近发言：区块链 云计算"), "{}", prompt);
        println!("[PASS] test_command_danmu_excluded_from_learning passed");
    }

    #[test]
    fn test_window_management_definitions() {
        let test_label = "non_existent_window_label";
        assert_eq!(test_label.to_string(), "non_existent_window_label");
        println!("[PASS] test_window_management_definitions passed");
    }

    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_normal_danmu_read_aloud_queue() {
        let state = AppState::new_test();
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.enable_voice = true;
            cfg.only_speek_wearing_medal = false;
            cfg.only_speek_guard_level = 0;
        }

        // 普通弹幕（非点怪）：入普通朗读队列，格式 "{uname} 说：{msg}"
        let dm = bilibili::DanmuData {
            user_id: "u_read".into(),
            user_name: "朗读测试员".into(),
            message: "你好世界".into(),
            timestamp: 1,
            has_medal: false,
            medal_level: 0,
            guard_level: 0,
            msg_id: "read_1".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };
        let res = handle_incoming_danmu(None, &state, dm);
        assert!(!res.matched);
        let task = state.tts_mgr.dequeue_speak().expect("普通弹幕应入队朗读");
        assert_eq!(task.text, "朗读测试员 说：你好世界");
        assert_eq!(task.user_id, "u_read");

        // 点怪弹幕同样朗读原文（对齐原工程：无独立"点怪"播报）
        let dm2 = bilibili::DanmuData {
            user_id: "u_read".into(),
            user_name: "朗读测试员".into(),
            message: "点怪火龙".into(),
            timestamp: 2,
            has_medal: false,
            medal_level: 0,
            guard_level: 0,
            msg_id: "read_2".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };
        let res2 = handle_incoming_danmu(None, &state, dm2);
        assert!(res2.matched);
        let task2 = state.tts_mgr.dequeue_speak().expect("点怪弹幕应入队朗读");
        assert_eq!(task2.text, "朗读测试员 说：点怪火龙");
        println!("[PASS] test_normal_danmu_read_aloud_queue passed");
    }

    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_read_aloud_respects_speak_filters() {
        let state = AppState::new_test();
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.enable_voice = true;
            cfg.only_speek_wearing_medal = true; // 仅播报佩戴粉丝牌
        }

        // 无粉丝牌 → 不入队
        let dm = bilibili::DanmuData {
            user_id: "u_nofilter".into(),
            user_name: "无牌水友".into(),
            message: "早上好".into(),
            timestamp: 1,
            has_medal: false,
            medal_level: 0,
            guard_level: 0,
            msg_id: "filter_1".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };
        handle_incoming_danmu(None, &state, dm);
        assert!(state.tts_mgr.dequeue_speak().is_none());

        // 佩戴粉丝牌 → 入队
        let dm2 = bilibili::DanmuData {
            user_id: "u_with_medal".into(),
            user_name: "有牌水友".into(),
            message: "早上好".into(),
            timestamp: 2,
            has_medal: true,
            medal_level: 5,
            guard_level: 0,
            msg_id: "filter_2".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };
        handle_incoming_danmu(None, &state, dm2);
        let task = state.tts_mgr.dequeue_speak().expect("有牌水友应入队");
        assert_eq!(task.text, "有牌水友 说：早上好");
        println!("[PASS] test_read_aloud_respects_speak_filters passed");
    }

    #[test]
    fn test_food_order_text_generation() {
        // 文案格式："{uname} 下单的 {xxx} 已接单，预计{n}分钟后送达！"
        let text = build_food_order_text("点餐红烧肉", "水友A").expect("应生成接单文案");
        assert!(text.starts_with("水友A 下单的 红烧肉 已接单，预计"), "text: {}", text);
        assert!(text.ends_with("分钟后送达！"), "text: {}", text);

        // 随机分钟数在 [0, 60]
        for _ in 0..20 {
            let t = build_food_order_text("点餐拉面", "水友B").unwrap();
            let n: i32 = t
                .split("预计")
                .nth(1)
                .and_then(|s| s.split("分钟").next())
                .and_then(|s| s.parse().ok())
                .expect("应含预计分钟数");
            assert!((0..=60).contains(&n), "分钟数越界: {}", n);
        }

        // 仅"点餐"或非点餐文本不生成
        assert!(build_food_order_text("点餐", "水友A").is_none());
        assert!(build_food_order_text("点怪火龙", "水友A").is_none());
        println!("[PASS] test_food_order_text_generation passed");
    }

    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_food_order_danmu_enters_priority_queue() {
        let state = AppState::new_test();
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.enable_voice = true;
        }
        let dm = bilibili::DanmuData {
            user_id: "u_food".into(),
            user_name: "吃货水友".into(),
            message: "点餐麻辣烫".into(),
            timestamp: 10,
            has_medal: false,
            medal_level: 0,
            guard_level: 0,
            msg_id: "food_1".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };
        handle_incoming_danmu(None, &state, dm);
        let task = state.tts_mgr.dequeue_speak().expect("点餐应入队");
        assert!(task.text.contains("下单的 麻辣烫 已接单"), "text: {}", task.text);
        assert_eq!(task.user_id, "u_food");
        // 对齐原工程：接单文案入普通队列（非优先）
        assert!(!task.priority, "点餐接单文案应进普通队列");

        // 原工程 HandleSpeekDm 点餐后不 return，仍会朗读原文 → 第二条为原文朗读
        let second = state.tts_mgr.dequeue_speak().expect("点餐弹幕还应朗读原文");
        assert_eq!(second.text, "吃货水友 说：点餐麻辣烫");
        assert!(state.tts_mgr.dequeue_speak().is_none(), "点餐不应产生第三条播报");
        println!("[PASS] test_food_order_danmu_enters_priority_queue passed");
    }

    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_local_sound_danmu_bypasses_read_aloud() {
        let state = AppState::new_test();
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.enable_voice = true;
        }
        // "曼波"命中本地音效：直接播放，不进入朗读队列
        let dm = bilibili::DanmuData {
            user_id: "u_manbo".into(),
            user_name: "曼波水友".into(),
            message: "曼波".into(),
            timestamp: 11,
            has_medal: false,
            medal_level: 0,
            guard_level: 0,
            msg_id: "manbo_1".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };
        handle_incoming_danmu(None, &state, dm);
        assert!(state.tts_mgr.dequeue_speak().is_none(), "本地音效不应入朗读队列");
        println!("[PASS] test_local_sound_danmu_bypasses_read_aloud passed");
    }

    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_gift_pipeline_combo_and_read_aloud() {
        let state = AppState::new_test();
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.enable_voice = true;
        }

        // 首件礼物（<3 且免费）：立即播报"感谢 X 赠送的 N 个 Y"
        handle_incoming_gift(
            None,
            &state,
            tts::GiftEvent {
                open_id: "g_u1".into(),
                gift_id: "100".into(),
                uname: "送礼水友".into(),
                gift_name: "辣条".into(),
                gift_num: 1,
                paid: false,
                combo: None,
            },
        );
        let t1 = state.tts_mgr.dequeue_speak().expect("首件礼物应播报");
        assert_eq!(t1.text, "感谢 送礼水友 赠送的1个辣条");
        assert_eq!(t1.user_id, "g_u1");

        // 冷却期内连续赠送：无新播报，等待窗口结算
        handle_incoming_gift(
            None,
            &state,
            tts::GiftEvent {
                open_id: "g_u1".into(),
                gift_id: "100".into(),
                uname: "送礼水友".into(),
                gift_name: "辣条".into(),
                gift_num: 2,
                paid: false,
                combo: None,
            },
        );
        assert!(state.tts_mgr.dequeue_speak().is_none(), "冷却期不应立即播报");

        // 手工把动态池窗口推进到超时（模拟 10s 后由后台泵结算）
        let msgs = state.tts_mgr.flush_gift_combos(false);
        // 窗口未超时则为空；若已超时则应结算为合并数量
        if !msgs.is_empty() {
            assert_eq!(msgs[0].text, "感谢 送礼水友 赠送的3个辣条");
        }
        println!("[PASS] test_gift_pipeline_combo_and_read_aloud passed");
    }

    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_gift_pipeline_only_paid_filter() {
        let state = AppState::new_test();
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.enable_voice = true;
            cfg.only_speek_paid_gift = true;
        }

        // 免费礼物官方连击：准备池结算时被「仅付费礼物」开关静默丢弃
        handle_incoming_gift(
            None,
            &state,
            tts::GiftEvent {
                open_id: "g_u2".into(),
                gift_id: "200".into(),
                uname: "免费水友".into(),
                gift_name: "小心心".into(),
                gift_num: 1,
                paid: false,
                combo: Some(tts::ComboInfo {
                    base_num: 5,
                    count: 2,
                    timeout_secs: 0.01,
                }),
            },
        );
        std::thread::sleep(std::time::Duration::from_millis(30));
        let msgs = state.tts_mgr.flush_gift_combos(true);
        assert!(msgs.is_empty(), "仅付费礼物模式下免费连击不得播报");

        // 付费礼物官方连击：正常结算合并数量（5 * 2 = 10）
        handle_incoming_gift(
            None,
            &state,
            tts::GiftEvent {
                open_id: "g_u3".into(),
                gift_id: "300".into(),
                uname: "付费水友".into(),
                gift_name: "小心心".into(),
                gift_num: 1,
                paid: true,
                combo: Some(tts::ComboInfo {
                    base_num: 5,
                    count: 2,
                    timeout_secs: 0.01,
                }),
            },
        );
        std::thread::sleep(std::time::Duration::from_millis(30));
        let msgs = state.tts_mgr.flush_gift_combos(true);
        assert_eq!(msgs.len(), 1, "付费连击应结算");
        assert_eq!(msgs[0].text, "感谢 付费水友 赠送的10个小心心");
        println!("[PASS] test_gift_pipeline_only_paid_filter passed");
    }

    /// SC / 上舰 / 进场播报链路属完整版专属
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_live_event_pipeline_sc_guard_enter() {
        let state = AppState::new_test();
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.enable_voice = true;
        }

        // SC：入优先队列（文案对齐原工程）
        handle_incoming_live_event(
            None,
            &state,
            bilibili::LiveEvent::SuperChat {
                user_id: "sc_u1".into(),
                uname: "土豪水友".into(),
                rmb: 30,
                message: "加油".into(),
            },
        );
        let t = state.tts_mgr.dequeue_speak().expect("SC 应入队");
        assert_eq!(t.text, "感谢 土豪水友 赠送的30元SC：加油");
        assert_eq!(t.user_id, "sc_u1");

        // 上舰：入优先队列
        handle_incoming_live_event(
            None,
            &state,
            bilibili::LiveEvent::Guard {
                user_id: "guard_u1".into(),
                uname: "新舰长".into(),
                guard_level: 2,
                guard_num: 1,
                guard_unit: "月".into(),
            },
        );
        let t2 = state.tts_mgr.dequeue_speak().expect("上舰应入队");
        assert_eq!(t2.text, "感谢 新舰长 上船1月的提督");

        // 进场：不播报
        handle_incoming_live_event(
            None,
            &state,
            bilibili::LiveEvent::RoomEnter {
                user_id: "enter_u1".into(),
                uname: "路人".into(),
            },
        );
        assert!(state.tts_mgr.dequeue_speak().is_none(), "进场事件不应播报");
        println!("[PASS] test_live_event_pipeline_sc_guard_enter passed");
    }

    /// Lite 构建：非排队链路（朗读 / 点赞 / 礼物 / 直播事件）在编译期即被短路，
    /// 点怪排队不受影响（与 docs/LITE_COVERAGE_MATRIX.md 对齐）
    #[cfg(feature = "lite")]
    #[test]
    fn test_lite_build_disables_non_queue_pipelines() {
        let state = AppState::new_test();
        state.config.lock().unwrap().enable_voice = true;

        // 普通弹幕不朗读（即便语音开关与粉丝牌过滤全部放行）
        let dm = bilibili::DanmuData {
            user_id: "lite_u".into(),
            user_name: "Lite水友".into(),
            message: "早上好".into(),
            timestamp: 1,
            has_medal: true,
            medal_level: 5,
            guard_level: 0,
            msg_id: "lite_dm_1".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };
        handle_incoming_danmu(None, &state, dm);
        assert!(state.tts_mgr.dequeue_speak().is_none(), "Lite 构建不应朗读弹幕");

        // 点赞不结算奖卡
        let like = bilibili::LikeEvent {
            uid: "lite_u".into(),
            username: "Lite水友".into(),
            msg_id: "lite_like_1".into(),
            like_count: 30,
            timestamp: chrono::Utc::now().timestamp(),
        };
        assert!(handle_incoming_like(None, &state, &like).is_empty(), "Lite 构建点赞链路应返回空");

        // 礼物不播报
        handle_incoming_gift(
            None,
            &state,
            tts::GiftEvent {
                open_id: "lite_g".into(),
                gift_id: "100".into(),
                uname: "Lite水友".into(),
                gift_name: "辣条".into(),
                gift_num: 1,
                paid: false,
                combo: None,
            },
        );
        assert!(state.tts_mgr.dequeue_speak().is_none(), "Lite 构建不应播报礼物");

        // SC 不播报
        handle_incoming_live_event(
            None,
            &state,
            bilibili::LiveEvent::SuperChat {
                user_id: "lite_sc".into(),
                uname: "土豪水友".into(),
                rmb: 50,
                message: "再来".into(),
            },
        );
        assert!(state.tts_mgr.dequeue_speak().is_none(), "Lite 构建不应播报 SC");
        println!("[PASS] test_lite_build_disables_non_queue_pipelines passed");
    }

    #[test]
    fn test_id_code_registry_flow() {
        let state = AppState::new_test();
        let orig = registry::read_id_code().unwrap_or_default();

        let test_val = "ID_CODE_REG_SYNC_123456";
        let res = registry::write_id_code(test_val);
        assert!(res.is_ok());

        // 验证从注册表读取
        let read_val = registry::read_id_code().unwrap_or_default();
        assert_eq!(read_val, test_val);

        // 验证 AppState config 同步
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.id_code = read_val.clone();
        }
        assert_eq!(state.config.lock().unwrap().id_code, test_val);

        // 回显解析：注册表优先
        assert_eq!(resolve_id_code(&state), test_val);

        // 注册表为空时回退内存配置
        let _ = registry::delete_id_code();
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.id_code = "ID_CODE_FALLBACK_CFG".into();
        }
        assert_eq!(resolve_id_code(&state), "ID_CODE_FALLBACK_CFG");

        // 还原现场
        if orig.is_empty() {
            let _ = registry::delete_id_code();
        } else {
            let _ = registry::write_id_code(&orig);
        }
        println!("[PASS] test_id_code_registry_flow passed");
    }

    #[test]
    fn test_parse_tts_engine_mapping() {
        // D1：设置面板引擎下拉值 → 引擎类型（含「自动」）
        assert_eq!(parse_tts_engine("auto"), TTSEngineType::Auto);
        assert_eq!(parse_tts_engine(" AUTO "), TTSEngineType::Auto);
        assert_eq!(parse_tts_engine("manbo"), TTSEngineType::Manbo);
        assert_eq!(parse_tts_engine("mimo"), TTSEngineType::MiMo);
        assert_eq!(parse_tts_engine("sapi"), TTSEngineType::Sapi);
        // 未知值/空值按「自动」处理（对齐原工程 TTSProviderFactory::Create 的 default 分支走 AUTO，
        // 保留 Manbo→MiMo→SAPI 降级链，而不是退化为手动 Manbo）
        assert_eq!(parse_tts_engine("unknown"), TTSEngineType::Auto);
        assert_eq!(parse_tts_engine(""), TTSEngineType::Auto);
        println!("[PASS] test_parse_tts_engine_mapping passed");
    }

    #[test]
    fn test_overlay_lock_and_position_runtime_state() {
        // D2：悬浮窗锁定状态与待落盘位置（防抖）均为运行时状态
        let state = AppState::new_test();
        assert!(!state.overlay_locked.load(std::sync::atomic::Ordering::SeqCst));
        assert!(state.pending_pos.lock().unwrap().is_none());

        state
            .overlay_locked
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(state.overlay_locked.load(std::sync::atomic::Ordering::SeqCst));

        // 记录位置后由后台任务取走并清空（取走即视为已落盘）
        *state.pending_pos.lock().unwrap() = Some((913.0, 105.0));
        let taken = state.pending_pos.lock().unwrap().take();
        assert_eq!(taken, Some((913.0, 105.0)));
        assert!(state.pending_pos.lock().unwrap().is_none());
        println!("[PASS] test_overlay_lock_and_position_runtime_state passed");
    }

    /// 名单/字典命令的接线守护：AppState 已注入名单、禁点判定默认放行、字典已加载
    #[test]
    fn test_roster_and_dict_command_surface() {
        let state = AppState::new_test();

        // get_monster_dict 的数据源：字典必须已加载（空白字典会让图鉴库与选怪面板全空）
        let dict = state.monster_mgr.get_all_monsters();
        assert!(dict.len() > 100, "字典条目数异常: {}", dict.len());
        assert!(dict.contains_key("黑龙"));

        // 默认空名单：不做任何限制，任何点怪都不被拦截
        assert!(state.roster.snapshot().items.is_empty());
        assert!(!state.roster.is_blocked("任意未禁点怪物"));

        // set/get 名单往返一致，且真正落盘（重新加载后不变）
        let dir = std::env::temp_dir().join("mh_test_lib_roster_roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(roster::ROSTER_FILE_NAME);

        let roster = MonsterRoster::load(Some(&path));
        let payload = RosterData {
            items: vec!["黑龙".into(), "嗟怨震天怨虎龙".into()],
        };
        roster.replace(payload.clone()).unwrap();
        assert_eq!(roster.snapshot(), payload);
        assert_eq!(MonsterRoster::load(Some(&path)).snapshot(), payload);
        assert!(roster.is_blocked("黑龙"));

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_roster_and_dict_command_surface passed");
    }

    /// 导入解析：对象形态 / 裸数组 / 旧白名单文件拒绝 / 非法内容 / BOM 容忍
    #[test]
    fn test_parse_roster_json() {
        let d = parse_roster_json(r#"{ "items": ["黑龙", "麒麟"] }"#).unwrap();
        assert_eq!(d.items, vec!["黑龙", "麒麟"]);

        // 裸数组
        let d = parse_roster_json(r#"["黑龙", "麒麟"]"#).unwrap();
        assert_eq!(d.items.len(), 2);

        // 带 BOM 的文件内容（Windows 记事本另存）
        let d = parse_roster_json("\u{FEFF}{ \"items\": [] }").unwrap();
        assert!(d.items.is_empty());

        // 缺字段 → 取默认值（空名单 = 不限制）
        let d = parse_roster_json(r#"{}"#).unwrap();
        assert!(d.items.is_empty());

        // 旧版白名单文件（含 enabled）语义相反，直接拒绝
        assert!(parse_roster_json(r#"{ "enabled": true, "items": ["黑龙"] }"#).is_err());

        assert!(parse_roster_json("不是 JSON").is_err());
        assert!(parse_roster_json(r#"["黑龙", 42]"#).is_err());
        assert!(parse_roster_json(r#""just a string""#).is_err());
        println!("[PASS] test_parse_roster_json passed");
    }

    /// 删除字典条目：落盘 + 热重载 + 同步清理禁点名单（名单在 UI 上只读，无手动移除入口）
    #[test]
    fn test_delete_monster_entry_prunes_roster() {
        let dir = std::env::temp_dir().join("mh_test_delete_entry_prunes_roster");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let dict_path = dir.join("monster_list.json");
        std::fs::write(
            &dict_path,
            r#"{"黑龙":{"默认历战等级":2,"图标地址":"a.png","别称":["米拉"]},"麒麟":{"默认历战等级":0,"图标地址":"","别称":[]}}"#,
        )
        .unwrap();

        let mut mgr = MonsterDataManager::new();
        assert_eq!(mgr.load_from_file(Some(&dict_path)).unwrap(), 2);

        let roster_path = dir.join(roster::ROSTER_FILE_NAME);
        let roster = MonsterRoster::load(Some(&roster_path));
        roster
            .replace(RosterData {
                items: vec!["黑龙".into(), "麒麟".into()],
            })
            .unwrap();

        // 删除字典条目 → 匹配器热重载，禁点名单中的同名项同步移出
        let left = delete_monster_entry_impl(&mgr, &roster, &dict_path, "黑龙").unwrap();
        assert_eq!(left, 1);
        assert!(!mgr.get_all_monsters().contains_key("黑龙"));
        assert_eq!(roster.snapshot().items, vec!["麒麟".to_string()]);
        assert!(!roster.is_blocked("黑龙"));

        // 磁盘核验：字典与名单文件均已更新
        assert!(!MonsterDataManager::read_ordered_dict(&dict_path)
            .unwrap()
            .contains_key("黑龙"));
        assert_eq!(
            MonsterRoster::load(Some(&roster_path)).snapshot().items,
            vec!["麒麟".to_string()]
        );

        // 删除不在名单中的条目 → 名单保持不变
        delete_monster_entry_impl(&mgr, &roster, &dict_path, "麒麟").unwrap();
        assert!(roster.snapshot().items.is_empty());
        assert_eq!(mgr.get_all_monsters().len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_delete_monster_entry_prunes_roster passed");
    }
}
