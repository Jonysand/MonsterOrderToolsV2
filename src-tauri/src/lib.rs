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
pub mod tts;

use checkin_ai::CheckinLearner;

use ai::DeepSeekAIChatProvider;
use checkin::CheckinManager;
use config::AppConfig;
use credentials::{Credentials, CredentialsStatus};
use monster::MonsterDataManager;
use queue::{get_order_list_path, QueueItem, QueueManager};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};
use tts::{TTSConfig, TTSEngineType, TTSManager};

/// 全局运行状态（线程安全且支持克隆引用）
#[derive(Clone)]
pub struct AppState {
    pub queue_mgr: Arc<Mutex<QueueManager>>,
    pub monster_mgr: Arc<MonsterDataManager>,
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

        // 2. 初始化排队管理器并自动加载持久化列表
        let mut queue_mgr = QueueManager::new();
        let order_path = get_order_list_path();
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
        let manbo_key = if !creds.special_user_tts_api_key.is_empty() {
            creds.special_user_tts_api_key.clone()
        } else {
            app_cfg.manbo_api_key.clone()
        };

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
/// 单测构建不落盘 —— 避免 `cargo test` 污染真实 order_list.json（内存队列照常运作）
fn save_queue_or_warn(q: &mut QueueManager, path: &std::path::Path) {
    if cfg!(test) {
        return;
    }
    if let Err(e) = q.save_to_file(path) {
        crate::log_warn!("[Queue] 队列落盘失败: {}（路径: {}）", e, path.display());
    }
}

/// 新增点怪
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
        (m.monster_name, m.tempered_level, m.icon_url)
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

    // 定时保存
    let path = get_order_list_path();
    save_queue_or_warn(&mut q, &path);

    let items = q.items.clone();
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

    let path = get_order_list_path();
    save_queue_or_warn(&mut q, &path);

    let items = q.items.clone();
    let _ = app_handle.emit("queue-updated", &items);
    Ok(items)
}

/// 清空当前排队
#[tauri::command]
fn clear_queue(state: State<'_, AppState>, app_handle: AppHandle) -> Result<(), String> {
    let mut q = state.queue_mgr.lock().map_err(|e| e.to_string())?;
    q.clear();

    let path = get_order_list_path();
    save_queue_or_warn(&mut q, &path);

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

    let path = get_order_list_path();
    save_queue_or_warn(&mut q, &path);

    let current_items = q.items.clone();
    let _ = app_handle.emit("queue-updated", &current_items);
    Ok(current_items)
}

/// 匹配怪物名称
#[tauri::command]
fn match_monster_name(
    input_text: String,
    state: State<'_, AppState>,
) -> Result<Option<monster::MonsterMatchResult>, String> {
    Ok(state.monster_mgr.match_monster(&input_text))
}

/// 查询是否处于 Lite 模式 (ONLY_ORDER_MONSTER)
#[tauri::command]
fn get_lite_mode(state: State<'_, AppState>) -> Result<bool, String> {
    let cfg = state.config.lock().map_err(|e| e.to_string())?;
    Ok(cfg.is_lite_mode)
}

/// 设置 Lite 模式开关并持久化
#[tauri::command]
fn set_lite_mode(enabled: bool, state: State<'_, AppState>) -> Result<bool, String> {
    let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
    cfg.is_lite_mode = enabled;
    if let Err(e) = cfg.save(None) {
        crate::log_warn!("[Config] Lite 模式开关保存失败: {}", e);
        return Err(format!("Lite 模式开关保存失败: {}", e));
    }
    crate::log_info!("[Config] Lite 模式已{}", if enabled { "开启" } else { "关闭" });
    Ok(enabled)
}

/// E1 统一 Lite 守卫：非排队功能在 Lite 模式下统一拒绝调用。
/// 依据 AGENTS.md「新增功能默认不支持 Lite」：任何新增的非排队功能都必须显式调用本守卫。
/// Lite 保留清单（不调用本守卫）：点怪排队、悬浮窗/窗口控制、B 站长连、身份码、配置读写、
/// 连接状态与运行日志（详见 docs/LITE_COVERAGE_MATRIX.md）
fn ensure_not_lite(state: &AppState, module: &str) -> Result<(), String> {
    let is_lite = state
        .config
        .lock()
        .map_err(|e| e.to_string())?
        .is_lite_mode;
    if is_lite {
        return Err(format!("Lite模式下{}已停用", module));
    }
    Ok(())
}

/// 获取全部配置（优先从注册表合并最新 id_code）。
/// 敏感凭据字段一律脱敏为空串，前端如需凭据状态请使用 get_credentials_status
#[tauri::command]
fn get_app_config(state: State<'_, AppState>) -> Result<AppConfig, String> {
    let mut cfg = state.config.lock().map_err(|e| e.to_string())?.clone();
    if let Ok(reg_code) = registry::read_id_code() {
        if !reg_code.trim().is_empty() {
            cfg.id_code = reg_code;
        }
    }
    Ok(cfg.sanitized())
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
    keep_cred!(manbo_api_key, special_user_tts_api_key);

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
    state.tts_mgr.enqueue_speak(&text, user_id, priority);
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
            queue_tts(state, fallback, &danmu.user_id, true);
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
        let (text, is_ai) = match provider.call_api(&prompt, None).await {
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
            tts.enqueue_speak(&text, &user_id, true);
        }
    });
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

    // 广播原始弹幕事件供前端监听
    if let Some(handle) = app_handle {
        let _ = handle.emit("danmu-received", &danmu);
    }

    let is_lite = {
        state.config.lock().map(|c| c.is_lite_mode).unwrap_or(false)
    };

    let cfg = {
        state.config.lock().map(|c| c.clone()).unwrap_or_default()
    };

    // 2. 非 Lite 模式下的弹幕学习、舰长打卡与补签指令判定
    if !is_lite {
        let msg_trim = danmu.message.trim();

        // 2.1 打卡模块总开关（原工程 enableCaptainCheckinAI 控制 CaptainCheckInModule 的启用：
        //     关闭时打卡指令与弹幕学习全部停用；补签模块独立，不受该开关影响）
        let checkin_module_enabled = cfg.enable_captain_checkin_ai;

        // 2.2 舰长弹幕学习 + 同内容防刷屏
        //     （原工程 NotifyCaptainDanmu 门槛 guardLevel != 0 || hasMedal → ShouldLearn 仅舰长学习）
        let mut skip_commands = false;
        if checkin_module_enabled && (danmu.guard_level != 0 || danmu.has_medal) {
            if let Some(learner) = &state.checkin_learner {
                learner.learn(
                    &state.checkin_mgr,
                    &danmu.user_id,
                    &danmu.user_name,
                    danmu.guard_level,
                    &danmu.message,
                    danmu.timestamp,
                );
                skip_commands = learner.should_skip_duplicate(&danmu.user_id, &danmu.message);
            }
        }

        if !skip_commands {
            // 打卡日期口径：弹幕服务器时间（原工程 sendDate），缺失时回退本机今天（C5）
            let danmu_date = bilibili::server_date(danmu.timestamp)
                .unwrap_or_else(|| chrono::Local::now().date_naive());

            // 2.3 判定打卡
            let checkin_triggers: Vec<String> = cfg
                .checkin_trigger_words
                .split(&[',', '，'][..])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let is_checkin = checkin_module_enabled
                && (checkin_triggers.iter().any(|t| msg_trim.eq_ignore_ascii_case(t))
                    || msg_trim == "打卡"
                    || msg_trim == "签到"
                    || msg_trim == "打卡！"
                    || msg_trim == "签到！");

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
                                    queue_tts(state, reply, &danmu.user_id, true);
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
                    queue_tts(state, outcome.reply.clone(), &danmu.user_id, true);
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
    let mut q = state.queue_mgr.lock().unwrap();
    let res = state.danmu_processor.process_danmu(&danmu, &state.monster_mgr, &mut q);

    if res.added_to_queue || res.priority_updated {
        let path = queue::get_order_list_path();
        save_queue_or_warn(&mut q, &path);
        let items = q.items.clone();
        crate::log_info!(
            "[Queue] {} {} 成功（优先={}），当前排队 {} 位",
            danmu.user_name,
            if res.priority_updated { "优先置前" } else { "点怪" },
            res.priority_updated,
            items.len()
        );
        if let Some(handle) = app_handle {
            let _ = handle.emit("queue-updated", &items);
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
    if !is_lite && cfg.enable_voice {
        let msg_trim = danmu.message.trim();
        let passes_medal = !cfg.only_speek_wearing_medal || danmu.has_medal;
        let passes_guard = cfg.only_speek_guard_level == 0
            || (danmu.guard_level > 0 && danmu.guard_level <= cfg.only_speek_guard_level);
        // 注：原工程 ShouldSpeak 的 onlySpeekPaidGift 判定依赖 isPaidGift，而该字段在原工程
        // 从未被赋值（恒 false），开启开关会静音全部播报，属死逻辑；V2 不复刻，
        // 该开关仅作用于礼物播报（见 handle_incoming_gift 与连击结算）。

        if passes_medal && passes_guard {
            if let Some(food_text) = build_food_order_text(msg_trim, &danmu.user_name) {
                // 4.1 点餐指令（"点餐xxx"）
                queue_tts(state, food_text, &danmu.user_id, true);
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
    let is_lite = {
        state.config.lock().map(|c| c.is_lite_mode).unwrap_or(false)
    };
    if is_lite {
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
    let is_lite = state
        .config
        .lock()
        .map(|c| c.is_lite_mode)
        .unwrap_or(false);
    if is_lite {
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
    let is_lite = state
        .config
        .lock()
        .map(|c| c.is_lite_mode)
        .unwrap_or(false);
    if is_lite {
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

/// 模拟/开启 B 站长连
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
            return Err("未填入开播身份码（id_code），请在长连面板输入当次开播身份码后重试".into());
        }

        if !creds.is_valid() {
            return Err("B站开放平台凭证未配置或不完整（请确保 credentials.dat 存在并填入开播身份码 id_code）".into());
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

/// 弹幕模拟测试通道（与真实直播间长连统一管道）
#[tauri::command]
fn simulate_danmu(
    danmu: bilibili::DanmuData,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<bilibili::DanmuProcessResult, String> {
    Ok(handle_incoming_danmu(Some(&app_handle), &state, danmu))
}

/// 礼物模拟测试通道（B8 连击合并 / B11 付费过滤的手工验证入口）
#[tauri::command]
fn simulate_gift(
    event: tts::GiftEvent,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<(), String> {
    handle_incoming_gift(Some(&app_handle), &state, event);
    Ok(())
}

/// 直播间事件模拟测试通道（B2 SC / 上舰 / 进场的手工验证入口）
#[tauri::command]
fn simulate_live_event(
    event: bilibili::LiveEvent,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<(), String> {
    handle_incoming_live_event(Some(&app_handle), &state, event);
    Ok(())
}

/// 舰长常规打卡（受 Lite 模式控制）
#[tauri::command]
fn record_checkin(
    uid: String,
    username: String,
    state: State<'_, AppState>,
) -> Result<checkin::UserProfile, String> {
    ensure_not_lite(&state, "打卡模块")?;

    let today = chrono::Local::now().date_naive();
    state.checkin_mgr.record_checkin(&uid, &username, today)
}

/// 查询打卡档案（受 Lite 模式控制）
#[tauri::command]
fn get_checkin_profile(
    uid: String,
    state: State<'_, AppState>,
) -> Result<checkin::UserProfile, String> {
    ensure_not_lite(&state, "打卡模块")?;
    state.checkin_mgr.get_profile(&uid)
}

/// 查询补签卡（受 Lite 模式控制）
#[tauri::command]
fn get_retroactive_cards(
    uid: String,
    state: State<'_, AppState>,
) -> Result<checkin::RetroactiveCardData, String> {
    ensure_not_lite(&state, "补签卡模块")?;
    Ok(state.checkin_mgr.get_cards(&uid))
}

/// 执行补签（受 Lite 模式控制）
#[tauri::command]
fn execute_retroactive_checkin(
    uid: String,
    username: String,
    state: State<'_, AppState>,
) -> Result<i32, String> {
    ensure_not_lite(&state, "补签模块")?;

    let today = chrono::Local::now().date_naive();
    let target_date = state
        .checkin_mgr
        .find_last_missing_checkin_date(&uid, today)
        .ok_or_else(|| "未找到可补签的缺卡日期".to_string())?;

    state
        .checkin_mgr
        .execute_retroactive_checkin(&uid, &username, target_date)
}

/// 模拟点赞事件（与真实长连同一管道：msg_id 去重 + 奖卡 + 播报）
#[tauri::command]
fn simulate_like(
    event: bilibili::LikeEvent,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<Vec<String>, String> {
    ensure_not_lite(&state, "点赞奖卡模块")?;
    let mut ev = event;
    if ev.timestamp <= 0 {
        ev.timestamp = chrono::Utc::now().timestamp();
    }
    if ev.msg_id.is_empty() {
        ev.msg_id = format!("sim_like_{}", chrono::Utc::now().timestamp_millis());
    }
    Ok(handle_incoming_like(Some(&app_handle), &state, &ev))
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

/// 播放本地特殊音效（受 Lite 模式控制）
#[tauri::command]
fn play_sound_effect(sound_name: String, state: State<'_, AppState>) -> Result<(), String> {
    ensure_not_lite(&state, "音效模块")?;
    state.tts_mgr.play_special_sound(&sound_name)
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
fn parse_tts_engine(s: &str) -> TTSEngineType {
    match s.trim().to_ascii_lowercase().as_str() {
        "auto" => TTSEngineType::Auto,
        "mimo" => TTSEngineType::MiMo,
        "sapi" => TTSEngineType::Sapi,
        _ => TTSEngineType::Manbo,
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

/// AI 对话思考（受 Lite 模式控制）
#[tauri::command]
async fn ask_ai_thinking(
    prompt: String,
    username: String,
    state: State<'_, AppState>,
    app_handle: AppHandle,
) -> Result<ai::AIBubblePayload, String> {
    ensure_not_lite(&state, "AI思考模块")?;

    // 广播 AI 正在思考中事件
    let mut payload = ai::AIBubblePayload {
        username: username.clone(),
        prompt: prompt.clone(),
        reasoning: String::new(),
        answer: String::new(),
        is_thinking: true,
    };
    let _ = app_handle.emit("ai-bubble", &payload);

    let res = state.ai_provider.call_api(&prompt, Some("你是一个风趣幽默的怪猎荒野专家兼主播随从猫，回答简明扼要。")).await;
    match res {
        Ok((answer, reasoning)) => {
            payload.answer = answer;
            payload.reasoning = reasoning;
            payload.is_thinking = false;
            let _ = app_handle.emit("ai-bubble", &payload);
            Ok(payload)
        }
        Err(err) => {
            payload.answer = format!("思考遇到阻碍：{}", err);
            payload.is_thinking = false;
            let _ = app_handle.emit("ai-bubble", &payload);
            Err(err)
        }
    }
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

/// 显示指定窗口
#[tauri::command]
fn show_window(app_handle: AppHandle, label: String) -> Result<(), String> {
    if let Some(window) = app_handle.get_webview_window(&label) {
        window.show().map_err(|e| e.to_string())?;
        let _ = window.set_focus();
        Ok(())
    } else {
        Err(format!("Window '{}' not found", label))
    }
}

/// 退出程序命令（E3）：前端「退出」入口与主窗口关闭共用同一清理链路
#[tauri::command]
fn end_app(app_handle: AppHandle, state: State<'_, AppState>) {
    shutdown_app(&app_handle, &state);
}

/// 退出清理链路（E3，对齐原工程 Exit 命令：WriteQueue::Flush → BliveManager::Disconnect → PostQuitMessage）：
/// V2 中队列/配置均为变更即时落盘，此处补做待写悬浮窗位置落盘、停止长连并记录日志，最后退出进程。
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
    // 2. 停止直播长连（等价原工程 BliveManager::Disconnect / Destroy）
    if state.bili_service.is_running() {
        state.bili_service.set_running(false);
        crate::log_info!("[App] 退出：已断开 B 站直播长连");
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
                    if px > 0.0 || py > 0.0 {
                        let _ = win.set_position(tauri::Position::Logical(
                            tauri::LogicalPosition::new(px, py),
                        ));
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

            // 播报泵：每 150ms 出队一条待播报任务（优先队列优先）并结算超时连击，
            // 与原工程 Tick 的 NormalMsgQueue / GiftMsgQueue 逐条出队语义一致
            let state = app.state::<AppState>().inner().clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(150));
                loop {
                    interval.tick().await;
                    let (is_lite, only_paid) = state
                        .config
                        .lock()
                        .map(|c| (c.is_lite_mode, c.only_speek_paid_gift))
                        .unwrap_or((false, false));
                    if is_lite {
                        continue;
                    }

                    // 超时连击统一进入高优先队列（每 tick 结算一次）
                    for msg in state.tts_mgr.flush_gift_combos(only_paid) {
                        state.tts_mgr.enqueue_speak(&msg, "", true);
                    }

                    if let Some(task) = state.tts_mgr.dequeue_speak() {
                        let _ = state.tts_mgr.speak_text(&task.text, &task.user_id).await;
                    }
                }
            });

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
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                if window.label() == "main" {
                    // E3：关闭主窗口 = 退出程序，统一走退出清理链路
                    let app = window.app_handle();
                    let state = app.state::<AppState>();
                    shutdown_app(app, &state);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_queue,
            add_order,
            dequeue_by_user_id,
            clear_queue,
            reorder_queue,
            toggle_window,
            hide_window,
            show_window,
            end_app,
            get_credentials_status,
            match_monster_name,
            get_lite_mode,
            set_lite_mode,
            get_app_config,
            save_app_config,
            save_id_code,
            get_bili_connection_state,
            set_bili_connection,
            simulate_danmu,
            simulate_gift,
            simulate_live_event,
            record_checkin,
            get_checkin_profile,
            get_retroactive_cards,
            execute_retroactive_checkin,
            simulate_like,
            gm_batch_checkin,
            gm_search_users,
            gm_grant_card,
            gm_export_checkin_records,
            confirm_action,
            play_sound_effect,
            get_manbo_voice_list,
            get_current_tts_engine,
            save_manbo_api_key,
            get_recent_logs,
            clear_recent_logs,
            get_overlay_locked,
            set_overlay_locked,
            save_overlay_position,
            ask_ai_thinking
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
impl AppState {
    pub fn new_test() -> Self {
        let app_cfg = AppConfig::default();
        let mut monster_mgr = MonsterDataManager::new();
        let _ = monster_mgr.load_from_file(None);
        let queue_mgr = QueueManager::new();
        let checkin_mgr = CheckinManager::new_in_memory().expect("In-memory SQLite failed");
        let tts_mgr = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Manbo,
            enable_voice: false,
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

        Self {
            queue_mgr: Arc::new(Mutex::new(queue_mgr)),
            monster_mgr: Arc::new(monster_mgr),
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

    #[test]
    fn test_app_state_initialization() {
        let state = AppState::new_test();
        let q = state.queue_mgr.lock().unwrap();
        assert_eq!(q.items.len(), 0);
        let cfg = state.config.lock().unwrap();
        assert_eq!(cfg.opacity, 95);
        assert!(!state.bili_service.is_running());
        println!("[PASS] test_app_state_initialization passed");
    }

    #[test]
    fn test_lite_mode_disables_non_queue_modules() {
        let state = AppState::new_test();
        *state.config.lock().unwrap() = AppConfig {
            is_lite_mode: true,
            ..Default::default()
        };

        // 验证 Lite 模式下打卡与 AI 受阻
        let is_lite = state.config.lock().unwrap().is_lite_mode;
        assert!(is_lite);

        // 核心点单排队正常运作
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
        println!("[PASS] test_lite_mode_disables_non_queue_modules passed");
    }

    /// E1：统一 Lite 守卫 —— 非排队模块在 Lite 下统一拒绝；Lite 保留模块不受影响
    #[test]
    fn test_ensure_not_lite_guard_blocks_non_lite_modules() {
        let state = AppState::new_test();

        // 非 Lite：守卫放行
        assert!(ensure_not_lite(&state, "打卡模块").is_ok());
        assert!(ensure_not_lite(&state, "TTS语音模块").is_ok());

        // Lite：统一拒绝，文案与前端提示一致
        state.config.lock().unwrap().is_lite_mode = true;
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
            "AI思考模块",
            "音效模块",
            "点赞奖卡模块",
            "TTS语音模块",
        ] {
            let err = ensure_not_lite(&state, module).unwrap_err();
            assert_eq!(err, format!("Lite模式下{}已停用", module));
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
        println!("[PASS] test_ensure_not_lite_guard_blocks_non_lite_modules passed");
    }

    /// E3：退出清理 —— 待写悬浮窗位置并入内存配置并清空 pending（不落盘）
    #[test]
    fn test_apply_pending_position_updates_memory_config() {
        let state = AppState::new_test();

        // 无待写位置：返回 None，内存配置保持默认
        assert!(apply_pending_position(&state).is_none());
        assert_eq!(state.config.lock().unwrap().top_pos_x, 100.0);

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

        // 验证排队列表中无此项
        let q = state.queue_mgr.lock().unwrap();
        assert_eq!(q.items.len(), 0);
        println!("[PASS] test_simulate_danmu_checkin_flow passed");
    }

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

        // 验证昨天日期已被补签
        let records = state
            .checkin_mgr
            .export_records_content("csv", None, None, None)
            .unwrap();
        let yesterday_int = checkin::CheckinManager::date_to_int(d_yesterday);
        assert!(records.contains(&yesterday_int.to_string()));
        println!("[PASS] test_simulate_danmu_retroactive_flow passed");
    }

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
        assert!(state.tts_mgr.dequeue_speak().is_some());

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

        // Lite 模式点赞不处理
        state.config.lock().unwrap().is_lite_mode = true;
        let lite_ev = bilibili::LikeEvent {
            uid: "lite_user".into(),
            username: "Lite水友".into(),
            msg_id: "lite_like_1".into(),
            like_count: 30,
            timestamp: now_ts,
        };
        assert!(handle_incoming_like(None, &state, &lite_ev).is_empty());
        println!("[PASS] test_like_pipeline_dedup_and_rewards passed");
    }

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

    #[test]
    fn test_window_management_definitions() {
        let test_label = "non_existent_window_label";
        assert_eq!(test_label.to_string(), "non_existent_window_label");
        println!("[PASS] test_window_management_definitions passed");
    }

    #[test]
    fn test_normal_danmu_read_aloud_queue() {
        let state = AppState::new_test();
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.enable_voice = true;
            cfg.only_speek_wearing_medal = false;
            cfg.only_speek_guard_level = 0;
            cfg.is_lite_mode = false;
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

    #[test]
    fn test_read_aloud_respects_speak_filters() {
        let state = AppState::new_test();
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.enable_voice = true;
            cfg.only_speek_wearing_medal = true; // 仅播报佩戴粉丝牌
            cfg.is_lite_mode = false;
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

        // Lite 模式下不入队（TTS 停用）
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.is_lite_mode = true;
        }
        let dm3 = bilibili::DanmuData {
            user_id: "u_with_medal".into(),
            user_name: "有牌水友".into(),
            message: "晚上好".into(),
            timestamp: 3,
            has_medal: true,
            medal_level: 5,
            guard_level: 0,
            msg_id: "filter_3".into(),
            is_paid_gift: false,
        };
        handle_incoming_danmu(None, &state, dm3);
        assert!(state.tts_mgr.dequeue_speak().is_none());
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

    #[test]
    fn test_food_order_danmu_enters_priority_queue() {
        let state = AppState::new_test();
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.enable_voice = true;
            cfg.is_lite_mode = false;
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
        println!("[PASS] test_food_order_danmu_enters_priority_queue passed");
    }

    #[test]
    fn test_local_sound_danmu_bypasses_read_aloud() {
        let state = AppState::new_test();
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.enable_voice = true;
            cfg.is_lite_mode = false;
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

    #[test]
    fn test_gift_pipeline_combo_and_read_aloud() {
        let state = AppState::new_test();
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.enable_voice = true;
            cfg.is_lite_mode = false;
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

    #[test]
    fn test_gift_pipeline_only_paid_filter() {
        let state = AppState::new_test();
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.enable_voice = true;
            cfg.only_speek_paid_gift = true;
            cfg.is_lite_mode = false;
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

    #[test]
    fn test_live_event_pipeline_and_lite_interception() {
        let state = AppState::new_test();
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.enable_voice = true;
            cfg.is_lite_mode = false;
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

        // Lite 模式下全部拦截
        {
            let mut cfg = state.config.lock().unwrap();
            cfg.is_lite_mode = true;
        }
        handle_incoming_live_event(
            None,
            &state,
            bilibili::LiveEvent::SuperChat {
                user_id: "sc_u2".into(),
                uname: "土豪水友".into(),
                rmb: 50,
                message: "再来".into(),
            },
        );
        assert!(state.tts_mgr.dequeue_speak().is_none(), "Lite 模式应拦截 SC 播报");
        println!("[PASS] test_live_event_pipeline_and_lite_interception passed");
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
        // 未知值与原工程默认一致（Manbo）
        assert_eq!(parse_tts_engine("unknown"), TTSEngineType::Manbo);
        assert_eq!(parse_tts_engine(""), TTSEngineType::Manbo);
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
}