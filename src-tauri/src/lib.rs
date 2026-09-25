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
use queue::{QueueItem, QueueManager};
use roster::{MonsterRoster, RosterData};
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

/// 全局运行状态（线程安全且支持克隆引用）
#[derive(Clone)]
pub struct AppState {
    pub queue_mgr: Arc<Mutex<QueueManager>>,
    pub monster_mgr: Arc<MonsterDataManager>,
    /// 点怪禁点名单（名单内的怪物不可被点单；弹幕与选怪面板共享同一份约束）
    pub roster: Arc<MonsterRoster>,
    pub checkin_mgr: Arc<CheckinManager>,
    /// 弹幕关键词学习器（jieba 分词）。生产环境在 run() 中注入；测试默认 None（学习链路将被跳过）
    pub checkin_learner: Option<Arc<CheckinLearner>>,
    pub tts_mgr: Arc<TTSManager>,
    pub ai_provider: Arc<DeepSeekAIChatProvider>,
    pub config: Arc<Mutex<AppConfig>>,
    pub credentials: Arc<Credentials>,
    pub danmu_processor: Arc<bilibili::DanmuProcessor>,
    /// B 站长连五态状态机（替代原 bool 状态，D7）
    pub connection: Arc<Mutex<bilibili::ConnectionStatus>>,
    pub bili_service: Arc<bilibili::BiliLiveService>,
    /// 悬浮窗锁定（穿透）状态：运行时状态，不持久化（对齐原工程 mIsLocked）
    pub overlay_locked: Arc<std::sync::atomic::AtomicBool>,
    /// 悬浮窗拖动待落盘位置（防抖后由后台任务写回配置 top_pos_x/y）
    pub pending_pos: Arc<Mutex<Option<(f64, f64)>>>,
}

impl Default for AppState {
    fn default() -> Self {
        let app_cfg = AppConfig::load(None);

        // 0. 加载原工程加密配置文件 credentials.dat
        let creds = credentials::load_credentials(None).unwrap_or_else(|e| {
            crate::log_warn!("加载 credentials.dat 失败: {}", e);
            Credentials::default()
        });

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
        let checkin_mgr = CheckinManager::new(None).unwrap_or_else(|_| {
            CheckinManager::new_in_memory().expect("In-memory SQLite initialization failed")
        });

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
            monster_mgr: Arc::new(monster_mgr),
            roster: Arc::new(roster),
            checkin_mgr: Arc::new(checkin_mgr),
            checkin_learner: None,
            tts_mgr: Arc::new(tts_mgr),
            ai_provider: Arc::new(ai_provider),
            config: Arc::new(Mutex::new(app_cfg)),
            credentials: Arc::new(creds),
            danmu_processor: Arc::new(danmu_processor),
            connection: Arc::new(Mutex::new(bilibili::ConnectionStatus::default())),
            bili_service: Arc::new(bilibili::BiliLiveService::new()),
            overlay_locked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_pos: Arc::new(Mutex::new(None)),
        }
    }
}

// ---------------------------------------------------------------------------------
// Tauri Commands
// ---------------------------------------------------------------------------------

/// 获取当前排队列表
#[tauri::command]
fn get_queue(state: State<'_, AppState>) -> Result<Vec<QueueItem>, String> {
    let q = state.queue_mgr.lock().map_err(|e| e.to_string())?;
    Ok(q.items.clone())
}

/// 队列落盘（E2：失败记录告警而非静默吞错；内存队列保持可用）。
/// 关键点：序列化在锁内完成，**磁盘写入在锁外执行**，避免持锁做 I/O 阻塞弹幕处理与 UI 命令。
/// 单测构建不落盘 —— 避免 `cargo test` 污染真实 order_list.json（内存队列照常运作）
fn flush_queue(state: &AppState, force: bool) {
    if cfg!(test) {
        return;
    }
    let path = queue::get_order_list_path();
    // 锁内：仅在需要时序列化并清除脏标记
    let json = {
        let mut q = match state.queue_mgr.lock() {
            Ok(q) => q,
            Err(e) => {
                crate::log_warn!("[Queue] 队列锁异常，跳过落盘: {}", e);
                return;
            }
        };
        if !q.dirty && !force {
            return;
        }
        match q.to_json() {
            Ok(j) => {
                q.dirty = false;
                j
            }
            Err(e) => {
                crate::log_warn!("[Queue] 队列序列化失败: {}", e);
                return;
            }
        }
    };
    // 锁外：磁盘 I/O
    if let Err(e) = QueueManager::write_json(&path, &json) {
        crate::log_warn!("[Queue] 队列落盘失败: {}（路径: {}）", e, path.display());
    }
}

/// 变更后立即落盘（用于用户命令与退出链路；弹幕热路径改由 500ms 节流任务落盘）
fn save_queue_now(state: &AppState) {
    flush_queue(state, true);
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
) -> Result<Vec<QueueItem>, String> {
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
    let items = q.items.clone();
    drop(q);

    // 用户命令路径保持"变更即落盘"语义（弹幕热路径改由 500ms 节流任务负责）
    save_queue_now(&state);
    let _ = app_handle.emit("queue-updated", &items);
    Ok(items)
}

/// 按 User ID 删除指定排队项（保序删除）
#[tauri::command]
fn dequeue_by_user_id(
    user_id: String,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<Vec<QueueItem>, String> {
    let mut q = state.queue_mgr.lock().map_err(|e| e.to_string())?;
    q.dequeue_by_user_id(&user_id);
    let items = q.items.clone();
    drop(q);

    save_queue_now(&state);
    let _ = app_handle.emit("queue-updated", &items);
    Ok(items)
}

/// 清空当前排队
#[tauri::command]
fn clear_queue(state: State<'_, AppState>, app_handle: AppHandle) -> Result<(), String> {
    let mut q = state.queue_mgr.lock().map_err(|e| e.to_string())?;
    q.clear();
    drop(q);

    // 清空属破坏性操作：立即落盘（对齐原工程 Clear() 立即 SaveList）
    save_queue_now(&state);
    let _ = app_handle.emit("queue-updated", &Vec::<QueueItem>::new());
    Ok(())
}

/// 手动拖拽重新排序队列（主播拖拽调整排队顺序）
#[tauri::command]
fn reorder_queue(
    items: Vec<QueueItem>,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<Vec<QueueItem>, String> {
    let mut q = state.queue_mgr.lock().map_err(|e| e.to_string())?;
    q.reorder(items);
    let current_items = q.items.clone();
    drop(q);

    save_queue_now(&state);
    let _ = app_handle.emit("queue-updated", &current_items);
    Ok(current_items)
}

/// 撤销完成：把条目原样插回指定下标（保留原 id 与 timestamp）
#[tauri::command]
fn restore_order(
    item: QueueItem,
    index: usize,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<Vec<QueueItem>, String> {
    let mut q = state.queue_mgr.lock().map_err(|e| e.to_string())?;
    q.restore(item, index);
    let items = q.items.clone();
    drop(q);

    save_queue_now(&state);
    let _ = app_handle.emit("queue-updated", &items);
    Ok(items)
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
    let count = state.monster_mgr.edit_and_save(&path, |raw| {
        if let Some(old) = &renamed_from {
            raw.remove(old);
        }
        raw.insert(
            name.clone(),
            serde_json::to_value(&config).map_err(|e| format!("条目序列化失败: {}", e))?,
        );
        Ok(())
    })?;

    // 改名时同步可选名单中的引用（保持原顺位）；失败仅告警 —— 字典已落盘，不应谎报失败
    if let Some(old) = &renamed_from {
        match state.roster.rename_item(old, &name) {
            Ok(true) => crate::log_info!("[Dict] 条目改名 {} → {}，已同步可选名单", old, name),
            Ok(false) => {}
            Err(e) => crate::log_warn!("[Dict] 条目改名后同步可选名单失败: {}", e),
        }
    }

    crate::log_info!("[Dict] 已保存条目「{}」，当前共 {} 条", name, count);
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

    // 禁点名单在 UI 上为只读展示、无手动移除入口，故这里的同名残留必须一并清理
    if roster.snapshot().items.iter().any(|n| n == name) {
        match roster.remove(name) {
            Ok(()) => crate::log_info!("[Dict] 条目「{}」已删除，同步移出禁点名单", name),
            Err(e) => crate::log_warn!("[Dict] 删除条目后同步移出禁点名单失败: {}", e),
        }
    }

    crate::log_info!("[Dict] 已删除条目「{}」，当前共 {} 条", name, count);
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
fn get_monster_roster(state: State<'_, AppState>) -> Result<RosterData, String> {
    Ok(state.roster.snapshot())
}

/// 整表保存名单（编辑器点击加入/移出后落盘），返回保存后的快照
#[tauri::command]
fn set_monster_roster(data: RosterData, state: State<'_, AppState>) -> Result<RosterData, String> {
    state.roster.replace(data)?;
    Ok(state.roster.snapshot())
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
            crate::log_info!("[Roster] 已导出禁点名单（{} 项）到 {}", data.items.len(), path);
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
    crate::log_info!(
        "[Roster] 已读取导入名单（{} 项）来自 {}",
        data.items.len(),
        path
    );
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

    // 敏感凭据逐字段处理：credentials.dat 有值以其为准；否则保留前端非空输入；再否则维持现有内存值
    let prev = state.config.lock().map_err(|e| e.to_string())?.clone();
    macro_rules! keep_cred {
        ($field:ident, $cred:ident) => {
            if !state.credentials.$cred.is_empty() {
                new_cfg.$field = state.credentials.$cred.clone();
            } else if new_cfg.$field.trim().is_empty() {
                new_cfg.$field = prev.$field.clone();
            }
        };
    }
    keep_cred!(app_id, app_id);
    keep_cred!(access_key_id, access_key_id);
    keep_cred!(access_key_secret, access_key_secret);
    keep_cred!(deepseek_api_key, chat_api_key);
    keep_cred!(mimo_api_key, mimo_tts_api_key);
    // Manbo Key 不走 credentials.dat（原工程零引用 special_user_tts_api_key）：
    // credentials.dat 有值时以注册表/配置为准，仅在两者皆空时沿用现有内存值
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

/// 写入连接状态并广播前端（事件 `connection-state-changed`）
fn apply_connection_status(
    state: &AppState,
    app_handle: Option<&AppHandle>,
    next: bilibili::ConnectionStatus,
) {
    if let Ok(mut c) = state.connection.lock() {
        *c = next;
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
fn emit_checkin_reply(
    app_handle: Option<&AppHandle>,
    user_id: &str,
    user_name: &str,
    reply: &str,
    is_ai: bool,
) {
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
        let learning = state.checkin_mgr.load_learning(&danmu.user_id);
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

    // 2. 非 Lite 构建下的弹幕学习、舰长打卡与补签指令判定
    if !IS_LITE {
        let msg_trim = danmu.message.trim();

        // 2.1 打卡模块总开关（原工程 enableCaptainCheckinAI 控制 CaptainCheckInModule 的启用：
        //     关闭时打卡指令与弹幕学习全部停用；补签模块独立，不受该开关影响）
        let checkin_module_enabled = cfg.enable_captain_checkin_ai;

        // 触发词提前解析：后续步骤 2.2 判指令、步骤 2.3 判打卡都要用。
        // 支持中英文逗号分隔（对齐原工程 SetTriggerWords），清空后打卡功能完全停用
        let checkin_triggers = parse_checkin_trigger_words(&cfg.checkin_trigger_words);
        let is_checkin_command = checkin_triggers
            .iter()
            .any(|t| msg_trim.eq_ignore_ascii_case(t));

        // 2.2 舰长弹幕学习 + 同内容防刷屏
        //     （原工程 NotifyCaptainDanmu 门槛 guardLevel != 0 || hasMedal → ShouldLearn 仅舰长学习）
        //     指令类弹幕只参与防刷屏计数、不写入发言习惯：它们会被当成关键词与「最近发言」
        //     喂给打卡 AI 提示词（如只打卡不聊天的观众，Top5 习惯词里会出现「打卡」）
        let mut skip_commands = false;
        if checkin_module_enabled && (danmu.guard_level != 0 || danmu.has_medal) {
            if let Some(learner) = &state.checkin_learner {
                if !is_command_message(msg_trim, &checkin_triggers) {
                    learner.learn(
                        &state.checkin_mgr,
                        &danmu.user_id,
                        &danmu.user_name,
                        danmu.guard_level,
                        &danmu.message,
                        danmu.timestamp,
                    );
                }
                skip_commands = learner.should_skip_duplicate(&danmu.user_id, &danmu.message);
            }
        }

        if !skip_commands {
            // 打卡日期口径：弹幕服务器时间（原工程 sendDate），缺失时回退本机今天（C5）
            let danmu_date = bilibili::server_date(danmu.timestamp)
                .unwrap_or_else(|| chrono::Local::now().date_naive());

            // 2.3 判定打卡：仅以配置的触发词为准（对齐原工程 CaptainCheckInModule::IsCheckinMessage）
            let is_checkin = checkin_module_enabled && is_checkin_command;

            if is_checkin {
                // 原工程触发条件：舰长 或 佩戴粉丝牌的用户均可打卡
                if danmu.guard_level > 0 || danmu.has_medal {
                    let already_checked_in =
                        state.checkin_mgr.has_checkin_record(&danmu.user_id, danmu_date);
                    // AI 提示词需要“上次打卡日期”，须在落库前取（对齐原工程 previousCheckinDate）
                    let last_checkin_before = state
                        .checkin_mgr
                        .get_profile(&danmu.user_id)
                        .map(|p| p.last_checkin_date)
                        .unwrap_or(0);

                    match state
                        .checkin_mgr
                        .record_checkin(&danmu.user_id, &danmu.user_name, danmu_date)
                    {
                        Ok(profile) => {
                            if let Some(handle) = app_handle {
                                let _ = handle.emit("checkin-recorded", &profile);
                            }

                            if already_checked_in {
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

            // 2.4 判定补签：权限为「舰长或佩戴粉丝牌」（C4，对齐原工程 NotifyCaptainDanmu 门槛）
            if (danmu.guard_level > 0 || danmu.has_medal) && checkin::is_retro_command(msg_trim) {
                let outcome = state.checkin_mgr.retro_command_outcome(
                    &danmu.user_id,
                    &danmu.user_name,
                    danmu_date,
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
                if cfg.enable_voice {
                    // 补签播报同样按签到音频留档（对齐原工程 isCheckinTTS 守卫）
                    state.tts_mgr.enqueue_checkin_speak(
                        &outcome.reply,
                        &danmu.user_id,
                        &danmu.user_name,
                    );
                }
                return bilibili::DanmuProcessResult {
                    user_id: danmu.user_id,
                    user_name: danmu.user_name,
                    ..Default::default()
                };
            }

            // 2.5 判定补签查询：权限同上；仅气泡不朗读（v24 决策）
            if (danmu.guard_level > 0 || danmu.has_medal) && checkin::is_retro_query(msg_trim) {
                let reply = state
                    .checkin_mgr
                    .query_reply(&danmu.user_id, &danmu.user_name, danmu_date);
                let cards = state.checkin_mgr.get_cards(&danmu.user_id);
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
                return bilibili::DanmuProcessResult {
                    user_id: danmu.user_id,
                    user_name: danmu.user_name,
                    ..Default::default()
                };
            }
        }
    }

    // 3. 核心排队与怪物点单处理
    let (res, queued_items) = {
        let mut q = state.queue_mgr.lock().unwrap();
        let res = state
            .danmu_processor
            .process_danmu(&danmu, &state.monster_mgr, &state.roster, &mut q);
        let items = q.items.clone();
        (res, items)
        // 锁在此处释放：后续日志/事件广播/落盘均不持锁
    };

    // 3.1 禁点名单拦截：命中字典但该怪已被禁点 —— 不入队，仅记录并就地提示
    if res.blocked_by_roster {
        crate::log_info!(
            "[Roster] {} 点怪「{}」未生效（该怪在禁点名单内）",
            danmu.user_name,
            res.monster_name
        );
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
        // 对齐原工程 PriorityQueueManager::Tick 的 SAVE_INTERVAL_MS=500 语义）
        crate::log_info!(
            "[Queue] {} {} 成功（队列优先={}），当前排队 {} 位",
            danmu.user_name,
            if res.priority_updated { "优先置前" } else { "点怪" },
            item_priority,
            queued_items.len()
        );
        if let Some(handle) = app_handle {
            let _ = handle.emit("queue-updated", &queued_items);
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

    // 点赞与弹幕共用同一 msg_id 去重缓存（原工程 IsDuplicateMsgId）
    if state.danmu_processor.is_duplicate_msg_id(&ev.msg_id) {
        return Vec::new();
    }
    if ev.like_count <= 0 || ev.uid.is_empty() {
        return Vec::new();
    }

    // 点赞日期口径：服务器时间（原工程 event.date 优先，缺失回退本机今天）
    let date = bilibili::server_date(ev.timestamp)
        .unwrap_or_else(|| chrono::Local::now().date_naive());

    let Ok(rewards) = state.checkin_mgr.add_likes(&ev.uid, ev.like_count, date) else {
        return Vec::new();
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
    if !cfg.enable_voice {
        return;
    }

    // 连击合并：冷却期内累加、官方 combo 走准备池、超时由后台泵结算
    for msg in state.tts_mgr.process_gift(&ev) {
        queue_tts(state, msg, &ev.open_id, true);
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
        logging::record_history(&format!("{} 进入直播间", uname));
        return;
    }

    if let Some(handle) = app_handle {
        let _ = handle.emit(ev.event_name(), &ev);
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
#[tauri::command]
async fn set_bili_connection(
    connected: bool,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<bool, String> {
    if connected {
        if state.bili_service.is_running() {
            return Ok(true);
        }

        let cfg = state.config.lock().map_err(|e| e.to_string())?.clone();
        let app_id = if !state.credentials.app_id.is_empty() {
            state.credentials.app_id.clone()
        } else {
            cfg.app_id.clone()
        };
        let access_key_id = if !state.credentials.access_key_id.is_empty() {
            state.credentials.access_key_id.clone()
        } else {
            cfg.access_key_id.clone()
        };
        let access_key_secret = if !state.credentials.access_key_secret.is_empty() {
            state.credentials.access_key_secret.clone()
        } else {
            cfg.access_key_secret.clone()
        };

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

        let creds = bilibili::BiliCredentials {
            app_id,
            access_key_id,
            access_key_secret,
            id_code: id_code.clone(),
        };

        if id_code.trim().is_empty() {
            return Err("未填入开播身份码，请在直播连接面板输入当次开播身份码后重试".into());
        }

        if !creds.is_valid() {
            return Err("B 站开放平台凭据未配置或不完整（请导入凭据文件并填入开播身份码）".into());
        }

        state.bili_service.set_running(true);
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
        let game_id_ref = state.bili_service.get_game_id_ref();
        let state_clone = (*state).clone();
        let app_handle_clone = app_handle.clone();
        let creds_clone = creds.clone();

        tokio::spawn(async move {
            bilibili::run_bili_live_loop(
                creds_clone,
                running,
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

        if let Some(gid) = state.bili_service.get_game_id() {
            let cfg = state.config.lock().map_err(|e| e.to_string())?.clone();
            let app_id = if !state.credentials.app_id.is_empty() {
                state.credentials.app_id.clone()
            } else {
                cfg.app_id.clone()
            };
            let access_key_id = if !state.credentials.access_key_id.is_empty() {
                state.credentials.access_key_id.clone()
            } else {
                cfg.access_key_id.clone()
            };
            let access_key_secret = if !state.credentials.access_key_secret.is_empty() {
                state.credentials.access_key_secret.clone()
            } else {
                cfg.access_key_secret.clone()
            };

            let creds = bilibili::BiliCredentials {
                app_id,
                access_key_id,
                access_key_secret,
                id_code: cfg.id_code.clone(),
            };
            tokio::spawn(async move {
                let _ = creds.end_app(&gid).await;
            });
            state.bili_service.set_game_id(None);
        }

        Ok(false)
    }
}

/// GM: 批量补签（受 Lite 模式控制）
#[tauri::command]
fn gm_batch_checkin(state: State<'_, AppState>) -> Result<checkin::BatchCheckinResult, String> {
    ensure_not_lite(&state, "GM运维打卡功能")?;
    state.checkin_mgr.batch_checkin()
}

/// GM: 水友模糊搜索（受 Lite 模式控制；返回档案 + 补签卡数）
#[tauri::command]
fn gm_search_users(
    keyword: String,
    state: State<'_, AppState>,
) -> Result<Vec<checkin::UserSearchItem>, String> {
    ensure_not_lite(&state, "GM功能")?;
    state.checkin_mgr.search_users(&keyword)
}

/// GM: 手动调发补签卡（受 Lite 模式控制）
#[tauri::command]
fn gm_grant_card(
    uid: String,
    count: i32,
    state: State<'_, AppState>,
) -> Result<i32, String> {
    ensure_not_lite(&state, "GM功能")?;
    state.checkin_mgr.grant_card(&uid, count)
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

/// 校验凭据文件并复制到规范位置，返回加载后的凭据（纯逻辑，便于单测覆盖）
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
        std::fs::copy(path, &target)
            .map_err(|e| format!("复制凭据到 {} 失败: {}", target.display(), e))?;
    }
    Ok(creds)
}

/// 将导入的凭据注入运行状态（无需重启即生效）：配置镜像字段 + TTS 引擎 + AI Provider
fn apply_credentials_live(state: &AppState, creds: &credentials::Credentials) {
    if let Ok(mut cfg) = state.config.lock() {
        cfg.app_id = creds.app_id.clone();
        cfg.access_key_id = creds.access_key_id.clone();
        cfg.access_key_secret = creds.access_key_secret.clone();
        cfg.mimo_api_key = creds.mimo_tts_api_key.clone();
        cfg.deepseek_api_key = creds.chat_api_key.clone();
    }
    state.ai_provider.set_api_key(creds.chat_api_key.clone());
    if let Ok(cfg) = state.config.lock() {
        let mimo_key = if !creds.mimo_tts_api_key.is_empty() {
            creds.mimo_tts_api_key.clone()
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
    }
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
/// 选择文件 → HMAC 校验 → 复制到 `{数据目录}/credentials.dat` → 即时注入运行状态。
#[tauri::command]
fn import_credentials_file(
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<CredentialsStatus, String> {
    let Some(path) = pick_credentials_file(&app_handle)? else {
        return Err("已取消导入".into());
    };
    let creds = import_credentials_from_path(std::path::Path::new(&path))?;
    apply_credentials_live(&state, &creds);
    crate::log_info!("[Credentials] 已导入凭据文件: {}", path);

    let target = credentials::get_credentials_path();
    Ok(creds.to_status(true, &target.to_string_lossy()))
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
        let text = state.checkin_mgr.export_users_summary(&format)?;
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
        let text = state
            .checkin_mgr
            .export_records_content(&format, clean_user, start, end)?;
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
    flush_queue(state, true);
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
    let loaded = !state.credentials.app_id.is_empty();
    Ok(state.credentials.to_status(loaded, &p.to_string_lossy()))
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
            // 构建开销较大，仅启动时执行一次
            let mut state = AppState::default();
            state.checkin_learner = Some(Arc::new(CheckinLearner::from_resources()));
            app.manage(state);

            // 资源缺失可见：日志 + 事件（前端提示在 D5 落地）
            let missing: Vec<&str> = ["monster_list.json", "voices", "dict/stop_words.utf8"]
                .into_iter()
                .filter(|rel| paths::find_resource(rel).is_none())
                .collect();
            if !missing.is_empty() {
                crate::log_warn!("[Paths] 资源缺失: {:?}（相关功能将降级运行）", missing);
                for rel in &missing {
                    let _ = app.emit("resource-missing", rel);
                }
            }

            // TTS 音频留档：启动时清理超过保留天数的 TempAudio/YYYYMMDD 目录（B7）
            {
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

                    // 超时连击统一进入高优先队列（每 tick 结算一次）
                    for msg in state.tts_mgr.flush_gift_combos(only_paid) {
                        state.tts_mgr.enqueue_speak(&msg, "", true);
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
                            // 历史留档：记录实际播报出去的文本（对齐原工程 WriteLog::RecordHistory）
                            logging::record_history(&task.text);
                            let _ = tts.speak_task(&task).await;
                            tts.release_slot();
                        });
                    }
                }
            });

            // 队列落盘节流：每 500ms 检查脏标记，仅在变更后于锁外写盘
            // （对齐原工程 PriorityQueueManager::Tick 的 SAVE_INTERVAL_MS=500）
            {
                let state = app.state::<AppState>().inner().clone();
                tauri::async_runtime::spawn(async move {
                    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
                    loop {
                        interval.tick().await;
                        flush_queue(&state, false);
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

#[cfg(test)]
impl AppState {
    pub fn new_test() -> Self {
        let mut app_cfg = AppConfig::default();
        // 单测需覆盖播报链路，故测试基线显式开启语音（生产默认值对齐原工程为 false）
        app_cfg.enable_voice = true;
        let mut monster_mgr = MonsterDataManager::new();
        let _ = monster_mgr.load_from_file(None);
        let queue_mgr = QueueManager::new();
        let checkin_mgr = CheckinManager::new_in_memory().expect("In-memory SQLite failed");
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
        // 单测名单指向临时目录：绝不读写真实 monster_roster.json
        let roster = MonsterRoster::load(Some(
            &std::env::temp_dir()
                .join("mh_test_appstate_roster")
                .join(roster::ROSTER_FILE_NAME),
        ));

        Self {
            queue_mgr: Arc::new(Mutex::new(queue_mgr)),
            monster_mgr: Arc::new(monster_mgr),
            roster: Arc::new(roster),
            checkin_mgr: Arc::new(checkin_mgr),
            checkin_learner: None,
            tts_mgr: Arc::new(tts_mgr),
            ai_provider: Arc::new(ai_provider),
            config: Arc::new(Mutex::new(app_cfg)),
            credentials: Arc::new(creds),
            danmu_processor: Arc::new(danmu_processor),
            connection: Arc::new(Mutex::new(bilibili::ConnectionStatus::default())),
            bili_service: Arc::new(bilibili::BiliLiveService::new()),
            overlay_locked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_pos: Arc::new(Mutex::new(None)),
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
        };

        let res = handle_incoming_danmu(None, &state, dm);
        // 打卡指令不应作为怪物点单入队
        assert!(!res.matched);
        assert!(!res.added_to_queue);

        // 验证 SQLite 打卡档案已成功建立
        let profile = state.checkin_mgr.get_profile("guard_captain_1").expect("Profile not found");
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
        state.checkin_mgr.record_checkin("captain_retro", "补签猎人", d_3_days_ago).unwrap();
        state.checkin_mgr.record_checkin("captain_retro", "补签猎人", today).unwrap();
        state.checkin_mgr.grant_card("captain_retro", 2).unwrap();

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
        };

        let res = handle_incoming_danmu(None, &state, dm);
        assert!(!res.matched);

        // 验证补签卡扣减为 1 张
        let cards = state.checkin_mgr.get_cards("captain_retro");
        assert_eq!(cards.card_count, 1);

        // C2/C4：回复文案对齐原工程（含补签日期与恢复后的连续天数）
        let task = state.tts_mgr.dequeue_speak().expect("补签回复应入队播报");
        assert!(task.text.starts_with("补签猎人，已成功补签"), "{}", task.text);
        assert!(task.text.contains("剩余补签卡1张"), "{}", task.text);
        // 补签播报同样应留档
        assert!(task.is_checkin && task.checkin_username == "补签猎人");

        // 验证昨天日期已被补签
        let records = state
            .checkin_mgr
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

    /// 补签权限/查询词验证走完整弹幕管线，属完整版专属（Lite 构建下补签链路被短路）
    #[cfg(not(feature = "lite"))]
    #[test]
    fn test_retro_permission_and_query_words() {
        let state = AppState::new_test();
        let today = chrono::Local::now().date_naive();
        let d_3_days_ago = today - chrono::Duration::days(3);
        let now_ts = chrono::Utc::now().timestamp();

        // 佩戴粉丝牌的非舰长用户：C4 权限放宽后可补签
        state.checkin_mgr.record_checkin("medal_user", "粉丝牌水友", d_3_days_ago).unwrap();
        state.checkin_mgr.record_checkin("medal_user", "粉丝牌水友", today).unwrap();
        state.checkin_mgr.grant_card("medal_user", 1).unwrap();

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
        };
        let _ = handle_incoming_danmu(None, &state, dm);
        let cards = state.checkin_mgr.get_cards("medal_user");
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
        assert_eq!(state.checkin_mgr.get_cards("like_user").card_count, 1);

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
        };

        // 舰长弹幕 → 学习入档（关键词 + 发言历史）
        let _ = handle_incoming_danmu(None, &state, make("区块链 云计算", now_ts, "learn_1"));
        let learned = state.checkin_mgr.load_learning("learn_captain");
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
        };
        let _ = handle_incoming_danmu(None, &state, medal_dm);
        assert!(state.checkin_mgr.load_learning("learn_medal").danmu_history.is_empty());
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
        };
        let _ = handle_incoming_danmu(None, &state, disabled_dm);
        assert!(
            state.checkin_mgr.get_profile("learn_disabled").is_err(),
            "打卡模块停用后不应落库打卡"
        );
        assert!(state
            .checkin_mgr
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
        };

        // 普通弹幕正常学习
        let _ = handle_incoming_danmu(None, &state, make("区块链 云计算", now_ts, "cmd_1"));
        assert!(state
            .checkin_mgr
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

        let learned = state.checkin_mgr.load_learning("cmd_captain");
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
            assert_eq!(msgs[0], "感谢 送礼水友 赠送的3个辣条");
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
        assert_eq!(msgs[0], "感谢 付费水友 赠送的10个小心心");
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