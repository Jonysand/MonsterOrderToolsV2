//! 多引擎 TTS 语音域。
//! 对照原工程实现：`TextToSpeech.cpp` / `ManboTTSProvider.cpp` /
//! `SpecialManboTTSProvider.cpp` / `TTSCacheManager.cpp` / `LocalVoiceManager.cpp`。

#[cfg(not(test))]
use rodio::{Decoder, OutputStream, Sink};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::File;
#[cfg(not(test))]
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 语音引擎类型
/// `Auto` 对齐原工程 `TTSProviderFactory` 的 AUTO 模式：按 Manbo → MiMo → SAPI 顺序取首个可用引擎
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TTSEngineType {
    Auto,
    Manbo,
    MiMo,
    Sapi,
}

impl TTSEngineType {
    /// 引擎名（对齐原工程 `TTSManager_GetCurrentProviderName` 的返回值：manbo / xiaomi / sapi）
    pub fn provider_name(self) -> &'static str {
        match self {
            TTSEngineType::Auto => "auto",
            TTSEngineType::Manbo => "manbo",
            TTSEngineType::MiMo => "xiaomi",
            TTSEngineType::Sapi => "sapi",
        }
    }
}

impl Default for TTSEngineType {
    fn default() -> Self {
        Self::Manbo
    }
}

/// 引擎健康度与降级状态
#[derive(Debug, Clone)]
pub enum EngineHealth {
    Healthy,
    Degraded { failed_at: Instant, cooldown_secs: u64 },
}

/// TTS 智能语音配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TTSConfig {
    pub engine: TTSEngineType,
    pub enable_voice: bool,
    pub speech_rate: i32,
    pub speech_volume: i32,
    pub speech_pitch: i32,
    pub manbo_api_key: String,
    pub manbo_voice: String,
    pub mimo_api_key: String,
    pub mimo_voice: String,
    pub mimo_style: String,
    pub mimo_audio_format: String,
}

impl Default for TTSConfig {
    fn default() -> Self {
        Self {
            engine: TTSEngineType::Manbo,
            enable_voice: true,
            speech_rate: 0,
            speech_volume: 100,
            speech_pitch: 0,
            manbo_api_key: String::new(),
            // 原工程默认音色为「曼波」，走 /apis/mbAIscvip 专用端点
            manbo_voice: "曼波".into(),
            mimo_api_key: String::new(),
            mimo_voice: "mimo_default".into(),
            mimo_style: String::new(),
            mimo_audio_format: "mp3".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// 串行音频播放队列（B3 防叠音）
// ---------------------------------------------------------------------------

/// 播放任务载荷（音量随任务携带：对齐原工程 `AudioPlayer::SetVolume` 在起播前设置 MCI 音量）
#[derive(Debug)]
enum AudioJob {
    Bytes { bytes: Vec<u8>, volume: i32 },
    File { path: PathBuf, volume: i32 },
    Sapi { text: String, params: SapiParams },
}

/// SAPI 播报参数（对齐原工程 `SetupSapiVoiceParams`：rate 直传、音量减半、pitch 走 SSML）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SapiParams {
    pub rate: i32,
    pub volume: i32,
    pub pitch: i32,
}

pub const PLAYBACK_TIMEOUT: Duration = Duration::from_secs(60);
pub const SAPI_PLAYBACK_TIMEOUT: Duration = Duration::from_secs(30);

/// 全局串行音频播放队列：单播放线程消费 mpsc，所有播放入口统一入队。
/// 原工程 `AudioPlayer` 单实例串行播放，此处以队列 + 单线程等价实现。
pub struct AudioQueue {
    tx: mpsc::Sender<AudioJob>,
}

impl AudioQueue {
    /// 进程级全局队列（首次调用时创建播放线程）
    pub fn global() -> &'static AudioQueue {
        static QUEUE: OnceLock<AudioQueue> = OnceLock::new();
        QUEUE.get_or_init(|| Self::spawn(Self::play_job))
    }

    /// 使用自定义消费者构建队列（单测注入 mock 播放器）
    fn spawn<F>(player: F) -> AudioQueue
    where
        F: Fn(&AudioJob) + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<AudioJob>();
        std::thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                player(&job);
            }
        });
        AudioQueue { tx }
    }

    pub fn play_bytes(&self, bytes: Vec<u8>, volume: i32) -> Result<(), String> {
        self.tx
            .send(AudioJob::Bytes { bytes, volume })
            .map_err(|e| e.to_string())
    }

    pub fn play_file(&self, path: PathBuf, volume: i32) -> Result<(), String> {
        self.tx
            .send(AudioJob::File { path, volume })
            .map_err(|e| e.to_string())
    }

    pub fn speak_sapi(&self, text: String, params: SapiParams) -> Result<(), String> {
        self.tx
            .send(AudioJob::Sapi { text, params })
            .map_err(|e| e.to_string())
    }

    /// 真实播放实现（阻塞当前播放线程直至结束或超时）
    fn play_job(job: &AudioJob) {
        // 测试环境不产生真实音频输出（避免 cargo test 中途出声干扰）
        #[cfg(test)]
        {
            let _ = match job {
                AudioJob::Bytes { bytes, volume } => bytes.len() + *volume as usize,
                AudioJob::File { path, volume } => path.as_os_str().len() + *volume as usize,
                AudioJob::Sapi { text, params } => text.len() + params.rate.unsigned_abs() as usize,
            };
        }
        #[cfg(not(test))]
        match job {
            AudioJob::Bytes { bytes, volume } => {
                if let Ok(decoder) = Decoder::new(Cursor::new(bytes.clone())) {
                    play_decoder_sync(decoder, *volume);
                }
            }
            AudioJob::File { path, volume } => {
                if let Ok(file) = File::open(path) {
                    if let Ok(decoder) = Decoder::new(file) {
                        play_decoder_sync(decoder, *volume);
                    }
                }
            }
            AudioJob::Sapi { text, params } => {
                run_powershell_sapi(text, params);
            }
        }
    }
}

/// 配置音量（原工程 0~200，MCI 0~1000 即 `volume × 5`）换算为 rodio 的 0.0~1.0 增益
pub fn volume_gain(speech_volume: i32) -> f32 {
    (speech_volume.clamp(0, 200) as f32) / 200.0
}

/// rodio 同步播放（带 60s 播放超时保护，防止异常流卡死播放线程）
#[cfg(not(test))]
fn play_decoder_sync<R>(decoder: Decoder<R>, speech_volume: i32) -> bool
where
    R: std::io::Read + std::io::Seek + Send + 'static,
{
    if let Ok((_stream, handle)) = OutputStream::try_default() {
        if let Ok(sink) = Sink::try_new(&handle) {
            // 对齐原工程：起播前按配置设置音量（Manbo/MiMo/本地音效同样生效）
            sink.set_volume(volume_gain(speech_volume));
            sink.append(decoder);
            let deadline = Instant::now() + PLAYBACK_TIMEOUT;
            while !sink.empty() {
                if Instant::now() >= deadline {
                    sink.stop();
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            sink.sleep_until_end();
            return true;
        }
    }
    false
}

/// 构建 SAPI SSML（pitch 以 semitone 形式传递，文本做 XML 转义）
pub fn build_sapi_ssml(text: &str, pitch: i32) -> String {
    let pitch_str = if pitch >= 0 {
        format!("+{}st", pitch)
    } else {
        format!("{}st", pitch)
    };
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<speak version=\"1.0\" xml:lang=\"zh-CN\"><prosody pitch=\"{}\">{}</prosody></speak>",
        pitch_str, escaped
    )
}

/// 构建 PowerShell SAPI 播报命令（选择中文音色 + rate/volume/pitch 全参数）
pub fn build_sapi_command(text: &str, params: &SapiParams) -> String {
    let ssml = build_sapi_ssml(text, params.pitch).replace('\'', "''");
    let rate = params.rate.clamp(-10, 10);
    let volume = (params.volume / 2).clamp(0, 100);
    format!(
        "Add-Type -AssemblyName System.Speech; \
$s=New-Object System.Speech.Synthesis.SpeechSynthesizer; \
try{{$zh=@($s.GetInstalledVoices()|Where-Object{{$_.VoiceInfo.Culture.Name -like 'zh*'}}); if($zh.Count -gt 0){{$s.SelectVoice($zh[0].VoiceInfo.Name)}}}}catch{{}}; \
$s.Rate={rate}; $s.Volume={volume}; $s.SpeakSsml('{ssml}')"
    )
}

/// 执行 SAPI 播报（同步等待，30s 超时强杀，防止回调丢失导致播放线程卡死）
#[cfg(not(test))]
fn run_powershell_sapi(text: &str, params: &SapiParams) -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let cmd = build_sapi_command(text, params);
        match std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &cmd])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .spawn()
        {
            Ok(mut child) => {
                let deadline = Instant::now() + SAPI_PLAYBACK_TIMEOUT;
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => return true,
                        Ok(None) => {
                            if Instant::now() >= deadline {
                                let _ = child.kill();
                                return false;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        Err(_) => return false,
                    }
                }
            }
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (text, params);
        false
    }
}

// ---------------------------------------------------------------------------
// 音频留档与清理（B7，对齐 TTSCacheManager：TempAudio/YYYYMMDD + 启动清理）
// ---------------------------------------------------------------------------

/// 留档根目录：`{数据目录}/TempAudio`
pub fn cache_base_dir() -> PathBuf {
    crate::paths::config_dir().join("TempAudio")
}

/// 从播报文本提取留档前缀（对齐原工程 `GetContentPrefix`）：
/// 含「 说：」时取 `用户名_正文前5字`，否则取全文前 5 字
pub fn content_prefix(text: &str) -> String {
    const MARKER: &str = " 说：";
    if let Some(pos) = text.find(MARKER) {
        let username = &text[..pos];
        let after = &text[pos + MARKER.len()..];
        let first5: String = after.chars().take(5).collect();
        format!("{}_{}", username, first5)
    } else {
        text.chars().take(5).collect()
    }
}

/// 将播放成功的签到/补签音频留档（对齐原工程 `TTSCacheManager::SaveCheckinAudio`：`打卡_{用户名}_{时间戳}.mp3`）。
/// 原工程仅对签到 AI 回复音频留档，一般弹幕 TTS 播完即丢（v24 决策）。
pub fn save_checkin_audio(username: &str, bytes: &[u8]) -> Option<PathBuf> {
    let safe_name: String = username
        .chars()
        .map(|c| if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    let dir = cache_base_dir().join(chrono::Local::now().format("%Y%m%d").to_string());
    std::fs::create_dir_all(&dir).ok()?;
    let ts = chrono::Local::now().timestamp_millis();
    let path = dir.join(format!("打卡_{}_{}.mp3", safe_name, ts));
    std::fs::write(&path, bytes).ok()?;
    Some(path)
}

/// 清理超过保留天数的留档目录（按目录创建时间判定），返回清理数量
pub fn cleanup_old_cache(days_to_keep: i32) -> usize {
    let days = if days_to_keep > 0 { days_to_keep as u64 } else { 7 };
    let base = cache_base_dir();
    let Ok(entries) = std::fs::read_dir(&base) else {
        return 0;
    };
    let cutoff = std::time::SystemTime::now()
        .checked_sub(Duration::from_secs(days * 24 * 60 * 60))
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let created = std::fs::metadata(&path)
            .and_then(|m| m.created().or_else(|_| m.modified()))
            .ok();
        if let Some(t) = created {
            if t < cutoff && std::fs::remove_dir_all(&path).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

// ---------------------------------------------------------------------------
// 本地语音包（B10，local_voices.zip 内存解压 + 散装目录回退）
// ---------------------------------------------------------------------------

/// 读取本地语音文件字节：优先 zip 内读取，回退散装 voices 目录
pub fn load_local_voice(voice_file: &str) -> Option<Vec<u8>> {
    // 1. zip 内查找（原工程 LocalVoiceManager::LoadVoiceData）
    if let Some(zip_path) = crate::paths::find_resource("local_voices.zip") {
        if let Ok(file) = File::open(&zip_path) {
            if let Ok(mut archive) = zip::ZipArchive::new(file) {
                for candidate in zip_candidates(voice_file) {
                    if let Ok(mut entry) = archive.by_name(&candidate) {
                        let mut buf = Vec::new();
                        if std::io::Read::read_to_end(&mut entry, &mut buf).is_ok() && !buf.is_empty() {
                            return Some(buf);
                        }
                    }
                }
            }
        }
    }

    // 2. 散装目录回退（V2 兼容：voices/{manbo,mho}/...）
    for rel in [
        format!("voices/{}", voice_file),
        format!("voices/manbo/{}", voice_file),
        format!("voices/mho/{}", voice_file),
    ] {
        if let Some(p) = crate::paths::find_resource(&rel) {
            if let Ok(bytes) = std::fs::read(&p) {
                return Some(bytes);
            }
        }
    }
    None
}

/// zip 内候选路径：原样 + 无目录前缀时补 manbo/ 与 mho/
fn zip_candidates(voice_file: &str) -> Vec<String> {
    let mut v = vec![voice_file.to_string()];
    if !voice_file.contains('/') {
        v.push(format!("manbo/{}", voice_file));
        v.push(format!("mho/{}", voice_file));
    }
    v
}

// ---------------------------------------------------------------------------
// 礼物连击跟踪（B8，对齐 TextToSpeech.cpp HandleSpeekSendGift/Tick）
// ---------------------------------------------------------------------------

/// 官方连击信息（开放平台 `combo_info`）
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ComboInfo {
    pub base_num: i32,
    pub count: i32,
    pub timeout_secs: f32,
}

/// 礼物事件（统一载荷）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GiftEvent {
    pub open_id: String,
    pub gift_id: String,
    pub uname: String,
    pub gift_name: String,
    pub gift_num: i32,
    pub paid: bool,
    pub combo: Option<ComboInfo>,
}

pub const GIFT_COOLDOWN: Duration = Duration::from_secs(5);
pub const DYNAMIC_COMBO_WINDOW: Duration = Duration::from_secs(10);
/// 冷却表清理阈值：对齐原工程 `TextToSpeech.cpp:1418`（`cooldownMs * 2` = 5s × 2）
pub const COOLDOWN_CLEANUP: Duration = GIFT_COOLDOWN;

#[derive(Debug, Clone)]
struct DynamicCombo {
    uname: String,
    gift_name: String,
    gift_num: i32,
    first_reported: bool,
    deadline: Instant,
}

#[derive(Debug, Clone)]
struct PrepareCombo {
    uname: String,
    gift_name: String,
    gift_num: i32,
    paid: bool,
    deadline: Instant,
}

/// 礼物连击合并跟踪器
#[derive(Debug, Default)]
pub struct GiftComboTracker {
    /// 动态连击池（普通礼物：首报 >=3、尾报结算）
    dynamic: HashMap<String, DynamicCombo>,
    /// 官方连击准备池（paid + combo_info：超时后统一结算）
    prepare: HashMap<String, PrepareCombo>,
    /// 冷却表（键：open_id+gift_id）
    cooldowns: HashMap<String, Instant>,
}

impl GiftComboTracker {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(ev: &GiftEvent) -> String {
        format!("{}{}", ev.open_id, ev.gift_id)
    }

    fn in_cooldown(&self, key: &str, now: Instant) -> bool {
        self.cooldowns
            .get(key)
            .map(|t| now.duration_since(*t) < GIFT_COOLDOWN)
            .unwrap_or(false)
    }

    fn update_cooldown(&mut self, key: &str, now: Instant) {
        self.cooldowns.insert(key.to_string(), now);
    }

    /// 处理单个礼物事件，返回需要立即播报的文案
    pub fn handle(&mut self, ev: &GiftEvent, now: Instant) -> Vec<String> {
        let key = Self::key(ev);
        if ev.gift_num <= 0 {
            return Vec::new();
        }

        // 冷却期内仅累加动态池，不产生播报（对齐原工程 IsInCooldown 分支）
        if self.in_cooldown(&key, now) {
            if let Some(d) = self.dynamic.get_mut(&key) {
                d.gift_num += ev.gift_num;
                d.deadline = now + DYNAMIC_COMBO_WINDOW;
            }
            return Vec::new();
        }

        // 官方连击（paid + combo_info）：写入准备池，超时后结算
        if ev.paid {
            if let Some(ci) = ev.combo {
                if ci.base_num > 0 && ci.count > 0 && ci.timeout_secs > 0.0 {
                    let total = ci.base_num * ci.count;
                    let timeout = Duration::from_secs_f32(ci.timeout_secs);
                    if let Some(p) = self.prepare.get_mut(&key) {
                        p.gift_num = total;
                        p.deadline = now + timeout;
                    } else {
                        self.prepare.insert(
                            key,
                            PrepareCombo {
                                uname: ev.uname.clone(),
                                gift_name: ev.gift_name.clone(),
                                gift_num: total,
                                paid: ev.paid,
                                deadline: now + timeout,
                            },
                        );
                    }
                    return Vec::new();
                }
            }
        }

        let mut out = Vec::new();
        let mut set_cooldown = false;
        match self.dynamic.get_mut(&key) {
            Some(d) => {
                d.gift_num += ev.gift_num;
                d.deadline = now + DYNAMIC_COMBO_WINDOW;
                if !d.first_reported && d.gift_num >= 3 {
                    out.push(format!("感谢 {} 开始赠送{}", d.uname, d.gift_name));
                    d.first_reported = true;
                    set_cooldown = true;
                }
            }
            None => {
                self.dynamic.insert(
                    key.clone(),
                    DynamicCombo {
                        uname: ev.uname.clone(),
                        gift_name: ev.gift_name.clone(),
                        gift_num: ev.gift_num,
                        first_reported: false,
                        deadline: now + DYNAMIC_COMBO_WINDOW,
                    },
                );
                if ev.gift_num < 3 {
                    out.push(format!(
                        "感谢 {} 赠送的{}个{}",
                        ev.uname, ev.gift_num, ev.gift_name
                    ));
                    set_cooldown = true;
                    if let Some(d) = self.dynamic.get_mut(&key) {
                        d.first_reported = true;
                    }
                }
            }
        }
        if set_cooldown {
            self.update_cooldown(&key, now);
        }
        out
    }

    /// 周期结算：超时的连击池出队播报（prepare 池受「仅付费礼物」开关约束）
    pub fn tick(&mut self, now: Instant, only_paid_gift: bool) -> Vec<String> {
        let mut out = Vec::new();

        self.prepare.retain(|_, p| {
            if now >= p.deadline {
                if !only_paid_gift || p.paid {
                    out.push(format!(
                        "感谢 {} 赠送的{}个{}",
                        p.uname, p.gift_num, p.gift_name
                    ));
                }
                false
            } else {
                true
            }
        });

        self.dynamic.retain(|_, d| {
            if now >= d.deadline {
                if !d.first_reported || d.gift_num > 0 {
                    out.push(format!(
                        "感谢 {} 赠送的{}个{}",
                        d.uname, d.gift_num, d.gift_name
                    ));
                }
                false
            } else {
                true
            }
        });

        self.cooldowns
            .retain(|_, t| now.duration_since(*t) < COOLDOWN_CLEANUP * 2);
        out
    }
}

// ---------------------------------------------------------------------------
// 多引擎 TTS 状态机
// ---------------------------------------------------------------------------

/// 特殊用户专属引擎健康状态（对齐原工程 3 次失败 / 30s 熔断）
#[derive(Debug, Clone)]
struct SpecialEngineHealth {
    failures: u32,
    cooldown_until: Option<Instant>,
}

pub const SPECIAL_MAX_FAILURES: u32 = 3;
pub const SPECIAL_COOLDOWN_SECS: u64 = 30;

/// 待播报任务（对应原工程 NormalMsgQueue / GiftMsgQueue 的队列元素）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakTask {
    pub text: String,
    pub user_id: String,
    /// 是否为签到/补签播报（决定是否留档为 `打卡_{用户名}_{ts}.mp3`）
    pub is_checkin: bool,
    /// 签到用户名（仅 is_checkin 时有效）
    pub checkin_username: String,
    /// 是否来自高优先队列（回滚入队时还原到原队列）
    pub priority: bool,
}

/// 单队列容量上限（防止异常刷屏导致内存膨胀）
pub const MAX_SPEAK_QUEUE: usize = 200;

/// 并发合成上限（对齐原工程 `MAX_CONCURRENT_TTS = 2`）
pub const MAX_CONCURRENT_TTS: usize = 2;

/// 多引擎 TTS 状态机与音频管理器
pub struct TTSManager {
    config: Mutex<TTSConfig>,
    manbo_health: Mutex<EngineHealth>,
    mimo_health: Mutex<EngineHealth>,
    special_health: Mutex<SpecialEngineHealth>,
    gift_tracker: Mutex<GiftComboTracker>,
    /// 普通弹幕朗读队列（原工程 NormalMsgQueue）
    normal_queue: Mutex<VecDeque<SpeakTask>>,
    /// 高优先播报队列（礼物/SC/上舰/打卡，原工程 GiftMsgQueue）
    priority_queue: Mutex<VecDeque<SpeakTask>>,
    /// 在途合成任务数（对齐原工程 activeRequestCount_ 的并发闸门）
    inflight: std::sync::atomic::AtomicUsize,
    /// 最近一次实际使用的引擎（对齐原工程「当前引擎」显示，None = 尚未播报过）
    active_engine: Mutex<Option<TTSEngineType>>,
}

impl Default for TTSManager {
    fn default() -> Self {
        Self::new(TTSConfig::default())
    }
}

impl TTSManager {
    pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
    pub const COOLDOWN_SECS: u64 = 10;

    pub fn new(config: TTSConfig) -> Self {
        Self {
            config: Mutex::new(config),
            manbo_health: Mutex::new(EngineHealth::Healthy),
            mimo_health: Mutex::new(EngineHealth::Healthy),
            special_health: Mutex::new(SpecialEngineHealth {
                failures: 0,
                cooldown_until: None,
            }),
            gift_tracker: Mutex::new(GiftComboTracker::new()),
            normal_queue: Mutex::new(VecDeque::new()),
            priority_queue: Mutex::new(VecDeque::new()),
            inflight: std::sync::atomic::AtomicUsize::new(0),
            active_engine: Mutex::new(None),
        }
    }

    /// 入队待播报文本（`priority=true` 进入高优先队列；返回是否入队成功）
    pub fn enqueue_speak(&self, text: &str, user_id: &str, priority: bool) -> bool {
        self.enqueue_task(SpeakTask {
            text: text.to_string(),
            user_id: user_id.to_string(),
            is_checkin: false,
            checkin_username: String::new(),
            priority,
        }, priority)
    }

    /// 入队签到/补签播报（音频将按 `打卡_{用户名}_{ts}.mp3` 留档）
    pub fn enqueue_checkin_speak(&self, text: &str, user_id: &str, username: &str) -> bool {
        self.enqueue_task(SpeakTask {
            text: text.to_string(),
            user_id: user_id.to_string(),
            is_checkin: true,
            checkin_username: username.to_string(),
            priority: true,
        }, true)
    }

    /// 并发名额不足时把任务放回原队首，等待下一周期（不丢播报）
    pub fn requeue_speak(&self, task: SpeakTask) {
        if task.priority {
            self.priority_queue.lock().unwrap().push_front(task);
        } else {
            self.normal_queue.lock().unwrap().push_front(task);
        }
    }

    fn enqueue_task(&self, task: SpeakTask, priority: bool) -> bool {
        if task.text.trim().is_empty() {
            return false;
        }
        let mut q = if priority {
            self.priority_queue.lock().unwrap()
        } else {
            self.normal_queue.lock().unwrap()
        };
        if q.len() >= MAX_SPEAK_QUEUE {
            // 队满丢弃不再静默：写日志便于排查刷屏导致的丢播报
            crate::log_warn!(
                "[TTS] 播报队列已满（{} 条），丢弃：{}",
                MAX_SPEAK_QUEUE,
                task.text.chars().take(20).collect::<String>()
            );
            return false;
        }
        q.push_back(task);
        true
    }

    /// 取出下一条待播报任务（高优先队列优先，各队列严格 FIFO）
    pub fn dequeue_speak(&self) -> Option<SpeakTask> {
        if let Some(t) = self.priority_queue.lock().unwrap().pop_front() {
            return Some(t);
        }
        self.normal_queue.lock().unwrap().pop_front()
    }

    /// 按原工程 Tick 语义各取一条：礼物/优先队列与普通队列每周期各推进一条，避免普通播报被饿死
    pub fn dequeue_one_each(&self, max_total: usize) -> Vec<SpeakTask> {
        let mut out = Vec::new();
        if max_total == 0 {
            return out;
        }
        if let Some(t) = self.priority_queue.lock().unwrap().pop_front() {
            out.push(t);
        }
        if out.len() < max_total {
            if let Some(t) = self.normal_queue.lock().unwrap().pop_front() {
                out.push(t);
            }
        }
        out
    }

    /// 占用一个并发合成名额（对齐原工程 MAX_CONCURRENT_TTS）
    pub fn try_acquire_slot(&self) -> bool {
        use std::sync::atomic::Ordering;
        let mut cur = self.inflight.load(Ordering::SeqCst);
        while cur < MAX_CONCURRENT_TTS {
            match self.inflight.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
        false
    }

    pub fn release_slot(&self) {
        self.inflight
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn inflight_count(&self) -> usize {
        self.inflight.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 当前待播报任务总数（普通 + 优先）
    pub fn pending_speak_count(&self) -> usize {
        self.normal_queue.lock().unwrap().len() + self.priority_queue.lock().unwrap().len()
    }

    /// 更新 TTS 配置
    pub fn update_config(&self, new_cfg: TTSConfig) {
        let mut cfg = self.config.lock().unwrap();
        *cfg = new_cfg;
    }

    /// 当前 TTS 配置快照（用于局部字段更新，如保存 Manbo Key）
    pub fn config_snapshot(&self) -> TTSConfig {
        self.config.lock().unwrap().clone()
    }

    /// 判定指定引擎是否可用（包含冷却期结束自动探活逻辑）
    pub fn is_engine_available(&self, engine: TTSEngineType) -> bool {
        match engine {
            // 「自动」不参与降级判定（由 select_active_engine 级联解析为具体引擎）
            TTSEngineType::Auto => true,
            TTSEngineType::Manbo => {
                let mut h = self.manbo_health.lock().unwrap();
                match *h {
                    EngineHealth::Healthy => true,
                    EngineHealth::Degraded {
                        failed_at,
                        cooldown_secs,
                    } => {
                        if failed_at.elapsed() >= Duration::from_secs(cooldown_secs) {
                            *h = EngineHealth::Healthy; // 冷却结束，自动恢复重试
                            true
                        } else {
                            false
                        }
                    }
                }
            }
            TTSEngineType::MiMo => {
                // 未配置 API Key 时视为不可用（对齐原工程 TTSProvider.h IsAvailable：
                // `!apiKey_.empty() && available_`），避免发送空 Bearer 的无效请求
                if self
                    .config
                    .lock()
                    .map(|c| c.mimo_api_key.trim().is_empty())
                    .unwrap_or(true)
                {
                    return false;
                }
                let mut h = self.mimo_health.lock().unwrap();
                match *h {
                    EngineHealth::Healthy => true,
                    EngineHealth::Degraded {
                        failed_at,
                        cooldown_secs,
                    } => {
                        if failed_at.elapsed() >= Duration::from_secs(cooldown_secs) {
                            *h = EngineHealth::Healthy;
                            true
                        } else {
                            false
                        }
                    }
                }
            }
            TTSEngineType::Sapi => true, // SAPI 为本地兜底，恒常可用
        }
    }

    /// 标记引擎故障降级
    pub fn mark_engine_degraded(&self, engine: TTSEngineType) {
        let now = Instant::now();
        match engine {
            // 「自动」无独立健康度，实际降级由其解析出的具体引擎承担
            TTSEngineType::Auto => {}
            TTSEngineType::Manbo => {
                let mut h = self.manbo_health.lock().unwrap();
                *h = EngineHealth::Degraded {
                    failed_at: now,
                    cooldown_secs: Self::COOLDOWN_SECS,
                };
            }
            TTSEngineType::MiMo => {
                let mut h = self.mimo_health.lock().unwrap();
                *h = EngineHealth::Degraded {
                    failed_at: now,
                    cooldown_secs: Self::COOLDOWN_SECS,
                };
            }
            TTSEngineType::Sapi => {}
        }
    }

    /// 选择当前最优可用引擎（首选引擎健康则用之；否则按 Manbo -> MiMo -> Sapi 降级）。
    /// 配置为 `Auto`（对齐原工程 TTSProviderFactory 的 AUTO 模式）时直接走该降级链路。
    pub fn select_active_engine(&self) -> TTSEngineType {
        let preferred = self.config.lock().unwrap().engine;
        if preferred != TTSEngineType::Auto && self.is_engine_available(preferred) {
            return preferred;
        }

        // 故障降级链路
        if self.is_engine_available(TTSEngineType::Manbo) {
            TTSEngineType::Manbo
        } else if self.is_engine_available(TTSEngineType::MiMo) {
            TTSEngineType::MiMo
        } else {
            TTSEngineType::Sapi
        }
    }

    /// 记录本次实际使用的引擎（供「当前引擎」实时显示）
    fn mark_active_engine(&self, engine: TTSEngineType) {
        if let Ok(mut cur) = self.active_engine.lock() {
            *cur = Some(engine);
        }
    }

    /// 当前实际使用的引擎名（对齐原工程 `TTSManager_GetCurrentProviderName`：manbo / xiaomi / sapi）。
    /// 尚未播报过时返回按配置解析出的引擎名
    pub fn current_engine_name(&self) -> String {
        let tracked = self.active_engine.lock().ok().and_then(|c| *c);
        tracked
            .unwrap_or_else(|| self.select_active_engine())
            .provider_name()
            .to_string()
    }

    /// 特殊用户引擎是否可用（冷却期内不可用，到期自动恢复并清零失败计数）
    pub fn special_engine_available(&self) -> bool {
        let mut h = self.special_health.lock().unwrap();
        if let Some(until) = h.cooldown_until {
            if Instant::now() >= until {
                h.cooldown_until = None;
                h.failures = 0;
                return true;
            }
            return false;
        }
        true
    }

    fn special_mark_success(&self) {
        let mut h = self.special_health.lock().unwrap();
        h.failures = 0;
        h.cooldown_until = None;
    }

    fn special_mark_failure(&self) {
        let mut h = self.special_health.lock().unwrap();
        h.failures += 1;
        if h.failures >= SPECIAL_MAX_FAILURES {
            h.cooldown_until = Some(Instant::now() + Duration::from_secs(SPECIAL_COOLDOWN_SECS));
        }
    }

    /// 接收礼物连击事件，返回需立即播报的文案
    pub fn process_gift(&self, ev: &GiftEvent) -> Vec<String> {
        let mut tracker = self.gift_tracker.lock().unwrap();
        tracker.handle(ev, Instant::now())
    }

    /// 刷新连击池（超时结算）
    pub fn flush_gift_combos(&self, only_paid_gift: bool) -> Vec<String> {
        let mut tracker = self.gift_tracker.lock().unwrap();
        tracker.tick(Instant::now(), only_paid_gift)
    }

    /// 匹配本地特殊语音关键字（对齐原工程 LocalVoiceManager voiceMap，路径含 manbo/、mho/ 前缀）
    /// 本地音效映射（对齐原工程 `LocalVoiceManager` 的 9 个键，忽略大小写精确匹配）
    pub fn match_special_sound(name: &str) -> Option<&'static str> {
        let lower = name.trim().to_lowercase();
        match lower.as_str() {
            "曼波" => Some("manbo/manbo.mp3"),
            "曼波曼波" => Some("manbo/manbo_3x.mp3"),
            "duang" => Some("manbo/duang.mp3"),
            "噢耶" | "哦耶" | "欧耶" => Some("manbo/ohyeah.mp3"),
            "wow" => Some("manbo/wow.mp3"),
            "痛快！！" | "痛快!!" => Some("mho/tongkuai.mp3"),
            _ => None,
        }
    }

    /// 当前配置的播报音量（0~200），用于非 SAPI 播放路径
    fn current_volume(&self) -> i32 {
        self.config.lock().map(|c| c.speech_volume).unwrap_or(100)
    }

    /// 播放云端合成音频；`checkin_username` 非空时按签到音频留档（对齐原工程仅留档签到 TTS）
    pub fn play_audio_bytes(
        &self,
        bytes: &[u8],
        checkin_username: Option<&str>,
    ) -> Result<(), String> {
        if let Some(name) = checkin_username {
            if save_checkin_audio(name, bytes).is_none() {
                crate::log_warn!("[TTS] 签到音频留档失败（不影响播放）");
            }
        }
        AudioQueue::global().play_bytes(bytes.to_vec(), self.current_volume())
    }

    /// 查找并播放本地特殊音效（zip 优先、散装目录回退）
    pub fn play_special_sound(&self, name: &str) -> Result<(), String> {
        let file_name = if let Some(mapped) = Self::match_special_sound(name) {
            mapped.to_string()
        } else if name.ends_with(".mp3") || name.ends_with(".wav") {
            name.to_string()
        } else {
            format!("{}.mp3", name)
        };

        match load_local_voice(&file_name) {
            Some(bytes) => AudioQueue::global().play_bytes(bytes, self.current_volume()),
            None => Err(format!("Special sound effect not found: {}", file_name)),
        }
    }

    /// Manbo 通用引擎请求地址构建（对齐原工程 ManboTTSProvider::BuildRequestUrl）
    pub fn build_manbo_url(cfg: &TTSConfig, text: &str) -> String {
        if cfg.manbo_voice == "曼波" {
            format!(
                "https://api.milorapart.top/apis/mbAIscvip?text={}&format=mp3&speed={}&key={}",
                url_encode(text),
                cfg.speech_rate * 5,
                url_encode(&cfg.manbo_api_key)
            )
        } else {
            format!(
                "https://api.milorapart.top/apis/AIvoice?speaker={}&text={}",
                url_encode(&cfg.manbo_voice),
                url_encode(text)
            )
        }
    }

    /// 特殊用户专属引擎地址（对齐 SpecialManboTTSProvider：无 key、无 Authorization）
    pub fn build_special_manbo_url(text: &str) -> String {
        format!(
            "https://api.milorapart.top/apis/mbAIsc?text={}&format=mp3",
            url_encode(text)
        )
    }

    /// 请求 TTS 并下载音频字节；`auth` 为可选 Bearer 令牌
    async fn request_audio_bytes(
        client: &reqwest::Client,
        url: &str,
        auth: Option<&str>,
    ) -> Result<Vec<u8>, String> {
        let mut req = client.get(url);
        if let Some(token) = auth {
            if !token.is_empty() {
                req = req.header("Authorization", format!("Bearer {}", token));
            }
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let json_val: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        if json_val.get("code").and_then(|c| c.as_i64()) != Some(200) {
            return Err("API error".into());
        }
        let audio_url = json_val
            .get("url")
            .and_then(|u| u.as_str())
            .ok_or_else(|| "missing url".to_string())?;
        let audio_resp = client
            .get(audio_url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let bytes = audio_resp.bytes().await.map_err(|e| e.to_string())?;
        if bytes.is_empty() {
            return Err("empty audio".into());
        }
        Ok(bytes.to_vec())
    }

    /// 多引擎智能文本语音播报（特殊用户引擎、三级降级、3s 超时与故障熔断）。
    /// `user_id` 为空表示无用户上下文（如连击结算/模拟通道）；命中特殊用户时走专属引擎。
    /// 注意：本地特殊音效不在本入口拦截（对齐原工程仅在弹幕文本路径 `HandleSpeekDm` 匹配）。
    pub async fn speak_text(&self, text: &str, user_id: &str) -> Result<(), String> {
        self.speak_text_inner(text, user_id, None).await
    }

    /// 播报队列任务入口：按任务类型决定是否留档签到音频
    pub async fn speak_task(&self, task: &SpeakTask) -> Result<(), String> {
        let checkin = if task.is_checkin && !task.checkin_username.is_empty() {
            Some(task.checkin_username.as_str())
        } else {
            None
        };
        self.speak_text_inner(&task.text, &task.user_id, checkin).await
    }

    async fn speak_text_inner(
        &self,
        text: &str,
        user_id: &str,
        checkin_username: Option<&str>,
    ) -> Result<(), String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(());
        }

        let cfg = self.config.lock().unwrap().clone();
        if !cfg.enable_voice {
            return Ok(());
        }

        let client = reqwest::Client::builder()
            .timeout(Self::REQUEST_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;

        // 2. 特殊用户专属引擎（失败后回落通用链，不直接跳 SAPI）
        if !user_id.is_empty() && user_id == crate::bilibili::SPECIAL_OPEN_ID && self.special_engine_available()
        {
            let url = Self::build_special_manbo_url(trimmed);
            match Self::request_audio_bytes(&client, &url, None).await {
                Ok(bytes) => {
                    self.special_mark_success();
                    return self.play_audio_bytes(&bytes, checkin_username);
                }
                Err(_) => {
                    self.special_mark_failure();
                }
            }
        }

        // 3. 选择当前最优健康引擎并尝试
        let mut active = self.select_active_engine();

        if active == TTSEngineType::Manbo {
            let url = Self::build_manbo_url(&cfg, trimmed);
            match Self::request_audio_bytes(&client, &url, Some(&cfg.manbo_api_key)).await {
                Ok(bytes) => {
                    self.mark_active_engine(TTSEngineType::Manbo);
                    return self.play_audio_bytes(&bytes, checkin_username);
                }
                Err(_) => {
                    self.mark_engine_degraded(TTSEngineType::Manbo);
                    active = self.select_active_engine();
                }
            }
        }

        if active == TTSEngineType::MiMo {
            let full_text = build_mimo_text(&cfg.mimo_style, trimmed);
            let body = serde_json::json!({
                "model": "mimo-v2.5-tts",
                "messages": [
                    {"role": "user", "content": "Bright, bouncy, speak fast"},
                    {"role": "assistant", "content": full_text}
                ],
                "audio": {
                    "voice": if cfg.mimo_voice.is_empty() { "mimo_default" } else { &cfg.mimo_voice },
                    "format": if cfg.mimo_audio_format.is_empty() { "mp3" } else { &cfg.mimo_audio_format }
                }
            });

            match client
                .post("https://api.xiaomimimo.com/v1/chat/completions")
                .header("Authorization", format!("Bearer {}", cfg.mimo_api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(json_val) = resp.json::<serde_json::Value>().await {
                        if let Some(b64) = json_val
                            .pointer("/choices/0/message/audio/data")
                            .and_then(|v| v.as_str())
                        {
                            if let Ok(bytes) = base64_decode(b64) {
                                self.mark_active_engine(TTSEngineType::MiMo);
                                return self.play_audio_bytes(&bytes, checkin_username);
                            }
                        }
                    }
                    self.mark_engine_degraded(TTSEngineType::MiMo);
                }
                _ => {
                    self.mark_engine_degraded(TTSEngineType::MiMo);
                }
            }
        }

        // 4. Windows SAPI 本地离线兜底（rate/volume/pitch 全参数生效）
        self.mark_active_engine(TTSEngineType::Sapi);
        let params = SapiParams {
            rate: cfg.speech_rate,
            volume: cfg.speech_volume,
            pitch: cfg.speech_pitch,
        };
        AudioQueue::global().speak_sapi(trimmed.to_string(), params)
    }
}

/// 拼接 MiMo TTS 文本：全局风格标签前缀 + 行内 #标签# 转换。
/// 与原工程 XiaomiTTSProvider::HashtagToStyle / styleTag 语义一致。
pub fn build_mimo_text(style: &str, text: &str) -> String {
    let style_tag = if style.trim().is_empty() {
        String::new()
    } else {
        format!("<style>{}</style>", style)
    };
    format!("{}{}", style_tag, hashtag_to_style(text))
}

/// 将 `#标签#` 中的首个标签提升到串首（避免 API 忽略前缀），其余替换为 `<style>标签</style>`
fn hashtag_to_style(text: &str) -> String {
    let re = Regex::new(r"#([^#]+)#").unwrap();
    let first = re.captures(text).map(|c| c[1].to_string());

    let without_first = match &first {
        Some(f) => {
            let pat = format!("#{}#", f);
            text.replacen(&pat, "", 1)
        }
        None => text.to_string(),
    };

    let replaced = re
        .replace_all(&without_first, |caps: &regex::Captures| {
            format!("<style>{}</style>", &caps[1])
        })
        .to_string();

    match first {
        Some(f) => format!("<style>{}</style>{}", f, replaced),
        None => replaced,
    }
}

/// URL 百分比编码辅助函数 (RFC 3986)
pub fn url_encode(val: &str) -> String {
    let mut out = String::new();
    for b in val.bytes() {
        if (b >= b'a' && b <= b'z')
            || (b >= b'A' && b <= b'Z')
            || (b >= b'0' && b <= b'9')
            || b == b'-'
            || b == b'_'
            || b == b'.'
            || b == b'~'
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Base64 解码工具函数
pub fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut map = [255u8; 256];
    for (i, &b) in TABLE.iter().enumerate() {
        map[b as usize] = i as u8;
    }

    let clean: Vec<u8> = input
        .bytes()
        .filter(|&b| b != b'\r' && b != b'\n' && b != b' ')
        .collect();
    let mut out = Vec::with_capacity((clean.len() * 3) / 4);
    let mut buf = 0u32;
    let mut bits = 0;

    for &b in &clean {
        if b == b'=' {
            break;
        }
        let val = map[b as usize];
        if val == 255 {
            continue;
        }
        buf = (buf << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_auto_engine_cascade_and_current_engine_name() {
        // D1：引擎「自动」对齐原工程 TTSProviderFactory 的 AUTO 模式（manbo -> mimo -> sapi）
        let mgr = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Auto,
            mimo_api_key: "test-mimo-key".into(),
            ..Default::default()
        });
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Manbo);
        assert_eq!(mgr.current_engine_name(), "manbo");

        // Manbo 熔断后自动降级 MiMo，再熔断降级 SAPI
        mgr.mark_engine_degraded(TTSEngineType::Manbo);
        assert_eq!(mgr.select_active_engine(), TTSEngineType::MiMo);
        assert_eq!(mgr.current_engine_name(), "xiaomi");
        mgr.mark_engine_degraded(TTSEngineType::MiMo);
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Sapi);
        assert_eq!(mgr.current_engine_name(), "sapi");

        // 未配置 MiMo API Key 时跳过 MiMo 直接落到 SAPI（对齐原工程 IsAvailable 的 Key 非空判定）
        let no_key = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Auto,
            ..Default::default()
        });
        no_key.mark_engine_degraded(TTSEngineType::Manbo);
        assert!(
            !no_key.is_engine_available(TTSEngineType::MiMo),
            "无 MiMo Key 时该引擎不可用"
        );
        assert_eq!(no_key.select_active_engine(), TTSEngineType::Sapi);

        // 显式指定引擎时以其为准（故障时才降级），「自动」自身不参与降级判定
        let explicit = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Sapi,
            ..Default::default()
        });
        assert_eq!(explicit.select_active_engine(), TTSEngineType::Sapi);
        assert_eq!(explicit.current_engine_name(), "sapi");
        assert!(explicit.is_engine_available(TTSEngineType::Auto));
        explicit.mark_engine_degraded(TTSEngineType::Auto);
        assert_eq!(explicit.select_active_engine(), TTSEngineType::Sapi);

        // 引擎名映射（对齐原工程 TTSManager_GetCurrentProviderName 返回值）
        assert_eq!(TTSEngineType::Manbo.provider_name(), "manbo");
        assert_eq!(TTSEngineType::MiMo.provider_name(), "xiaomi");
        assert_eq!(TTSEngineType::Sapi.provider_name(), "sapi");
        println!("[PASS] test_auto_engine_cascade_and_current_engine_name passed");
    }

    #[test]
    fn test_tts_circuit_breaker_and_cooldown_recovery() {
        let mgr = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Manbo,
            mimo_api_key: "test-mimo-key".into(),
            ..Default::default()
        });

        // 初始状态：Manbo 为健康
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Manbo);

        // 模拟 Manbo 请求超时触发故障降级
        mgr.mark_engine_degraded(TTSEngineType::Manbo);
        assert!(!mgr.is_engine_available(TTSEngineType::Manbo));

        // 降级为 MiMo
        assert_eq!(mgr.select_active_engine(), TTSEngineType::MiMo);

        // 模拟 MiMo 亦故障降级
        mgr.mark_engine_degraded(TTSEngineType::MiMo);
        assert!(!mgr.is_engine_available(TTSEngineType::MiMo));

        // 最终降级至 SAPI 离线兜底
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Sapi);
        println!("[PASS] test_tts_circuit_breaker_and_cooldown_recovery passed");
    }

    #[test]
    fn test_special_engine_circuit_breaker() {
        let mgr = TTSManager::new(TTSConfig::default());
        assert!(mgr.special_engine_available());

        // 2 次失败仍可用，第 3 次触发 30s 熔断
        mgr.special_mark_failure();
        mgr.special_mark_failure();
        assert!(mgr.special_engine_available());
        mgr.special_mark_failure();
        assert!(!mgr.special_engine_available());

        // 成功一次后失败计数清零（人工重置冷却以验证计数逻辑）
        {
            let mut h = mgr.special_health.lock().unwrap();
            h.cooldown_until = None;
        }
        mgr.special_mark_failure();
        mgr.special_mark_success();
        mgr.special_mark_failure();
        assert!(mgr.special_engine_available());
        println!("[PASS] test_special_engine_circuit_breaker passed");
    }

    #[test]
    fn test_speak_queue_priority_and_fifo() {
        let mgr = TTSManager::new(TTSConfig::default());
        assert!(mgr.dequeue_speak().is_none());

        mgr.enqueue_speak("普通1", "u1", false);
        mgr.enqueue_speak("普通2", "u2", false);
        mgr.enqueue_speak("优先1", "u3", true);
        assert_eq!(mgr.pending_speak_count(), 3);

        // 优先队列先出队，其后普通队列严格 FIFO
        assert_eq!(mgr.dequeue_speak().unwrap().text, "优先1");
        assert_eq!(mgr.dequeue_speak().unwrap().text, "普通1");
        assert_eq!(mgr.dequeue_speak().unwrap().text, "普通2");
        assert!(mgr.dequeue_speak().is_none());

        // 空文本不入队
        assert!(!mgr.enqueue_speak("   ", "u", false));

        // 容量上限保护
        for i in 0..MAX_SPEAK_QUEUE {
            assert!(mgr.enqueue_speak(&format!("t{}", i), "u", false));
        }
        assert!(!mgr.enqueue_speak("overflow", "u", false));
        println!("[PASS] test_speak_queue_priority_and_fifo passed");
    }

    #[test]
    fn test_volume_gain_mapping() {
        // 原工程 MCI 音量 = speechVolume × 5（0~1000），故 200 → 满量程、100 → 半量程、0 → 静音
        assert_eq!(volume_gain(200), 1.0);
        assert_eq!(volume_gain(100), 0.5);
        assert_eq!(volume_gain(0), 0.0);
        assert_eq!(volume_gain(50), 0.25);
        // 越界钳制（对齐 AudioPlayer::SetVolume 的 0~200）
        assert_eq!(volume_gain(-10), 0.0);
        assert_eq!(volume_gain(999), 1.0);
        println!("[PASS] test_volume_gain_mapping passed");
    }

    #[test]
    fn test_special_sound_keys_match_legacy_voice_map() {
        // 对齐原工程 LocalVoiceManager::voiceMap_ 的 9 个键（忽略大小写精确匹配）
        for (k, v) in [
            ("曼波", "manbo/manbo.mp3"),
            ("曼波曼波", "manbo/manbo_3x.mp3"),
            ("duang", "manbo/duang.mp3"),
            ("噢耶", "manbo/ohyeah.mp3"),
            ("哦耶", "manbo/ohyeah.mp3"),
            ("欧耶", "manbo/ohyeah.mp3"),
            ("wow", "manbo/wow.mp3"),
            ("痛快！！", "mho/tongkuai.mp3"),
            ("痛快!!", "mho/tongkuai.mp3"),
        ] {
            assert_eq!(TTSManager::match_special_sound(k), Some(v), "键 {} 应映射 {}", k, v);
        }
        // 原工程未定义的键不得命中（避免吞掉正常播报文本）
        assert_eq!(TTSManager::match_special_sound("痛快"), None);
        assert_eq!(TTSManager::match_special_sound("ohyeah"), None);
        assert_eq!(TTSManager::match_special_sound("tongkuai"), None);
        println!("[PASS] test_special_sound_keys_match_legacy_voice_map passed");
    }

    #[test]
    fn test_speak_queue_capacity_logs_and_requeue() {
        let mgr = TTSManager::new(TTSConfig::default());
        assert!(mgr.enqueue_speak("普通一条", "u1", false));
        assert!(mgr.enqueue_checkin_speak("打卡回复", "u2", "舰长A"));

        // 各队列每周期各推进一条
        let batch = mgr.dequeue_one_each(2);
        assert_eq!(batch.len(), 2);
        assert!(batch[0].priority && batch[0].is_checkin && batch[0].checkin_username == "舰长A");
        assert!(!batch[1].priority);

        // 名额不足时回滚入队不丢播报
        mgr.requeue_speak(batch[1].clone());
        assert_eq!(mgr.dequeue_one_each(2).len(), 1);

        // 并发闸门上限为 MAX_CONCURRENT_TTS
        assert_eq!(MAX_CONCURRENT_TTS, 2);
        assert!(mgr.try_acquire_slot());
        assert!(mgr.try_acquire_slot());
        assert!(!mgr.try_acquire_slot(), "超出并发上限应拒绝");
        mgr.release_slot();
        assert!(mgr.try_acquire_slot());
        mgr.release_slot();
        mgr.release_slot();
        assert_eq!(mgr.inflight_count(), 0);
        println!("[PASS] test_speak_queue_capacity_logs_and_requeue passed");
    }

    #[test]
    fn test_audio_queue_serializes_jobs() {
        let inflight = Arc::new(AtomicUsize::new(0));
        let max_inflight = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));

        let q = {
            let inflight = inflight.clone();
            let max_inflight = max_inflight.clone();
            let done = done.clone();
            AudioQueue::spawn(move |_job| {
                let cur = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                max_inflight.fetch_max(cur, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(30));
                inflight.fetch_sub(1, Ordering::SeqCst);
                done.fetch_add(1, Ordering::SeqCst);
            })
        };

        for i in 0..3u8 {
            q.play_bytes(vec![i], 100).unwrap();
        }

        let start = Instant::now();
        while done.load(Ordering::SeqCst) < 3 && start.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(done.load(Ordering::SeqCst), 3, "全部任务应完成");
        assert_eq!(max_inflight.load(Ordering::SeqCst), 1, "播放必须串行，不得并发叠加");
        println!("[PASS] test_audio_queue_serializes_jobs passed");
    }

    #[test]
    fn test_gift_combo_merging() {
        let mut tracker = GiftComboTracker::new();
        let now = Instant::now();
        let ev = |num: i32| GiftEvent {
            open_id: "u1".into(),
            gift_id: "100".into(),
            uname: "水友A".into(),
            gift_name: "小电视".into(),
            gift_num: num,
            paid: true,
            combo: None,
        };

        // 首次 <3 个：立即播报
        let r1 = tracker.handle(&ev(1), now);
        assert_eq!(r1.len(), 1);
        assert_eq!(r1[0], "感谢 水友A 赠送的1个小电视");

        // 冷却期内累加不播报（对齐原工程 5s 冷却）
        let r2 = tracker.handle(&ev(2), now + Duration::from_millis(100));
        assert!(r2.is_empty());

        // 动态窗口未结束：不结算
        let r3 = tracker.tick(now + Duration::from_millis(200), false);
        assert!(r3.is_empty());

        // 超过 10s 动态窗口：尾报合并数量（1+2=3）
        let r4 = tracker.tick(now + Duration::from_secs(11), false);
        assert_eq!(r4.len(), 1);
        assert_eq!(r4[0], "感谢 水友A 赠送的3个小电视");
        println!("[PASS] test_gift_combo_merging passed");
    }

    #[test]
    fn test_gift_combo_first_report_and_cooldown() {
        let mut tracker = GiftComboTracker::new();
        let now = Instant::now();
        let ev = |num: i32, gift_id: &str| GiftEvent {
            open_id: "u2".into(),
            gift_id: gift_id.into(),
            uname: "水友B".into(),
            gift_name: "辣条".into(),
            gift_num: num,
            paid: false,
            combo: None,
        };

        // 首条 3 个（>=3）：进入动态池，不立即播报
        let r1 = tracker.handle(&ev(3, "1"), now);
        assert!(r1.is_empty());

        // 冷却期外的新事件累加：仍未首报且 >=3 → 首报「开始赠送」
        let r2 = tracker.handle(&ev(1, "1"), now + Duration::from_secs(6));
        assert_eq!(r2.len(), 1);
        assert_eq!(r2[0], "感谢 水友B 开始赠送辣条");

        // 冷却期内再次累加：无播报
        let r3 = tracker.handle(&ev(1, "1"), now + Duration::from_secs(7));
        assert!(r3.is_empty());
        println!("[PASS] test_gift_combo_first_report_and_cooldown passed");
    }

    #[test]
    fn test_gift_official_combo_and_paid_filter() {
        let mut tracker = GiftComboTracker::new();
        let now = Instant::now();
        let ev = |paid: bool| GiftEvent {
            open_id: "u3".into(),
            gift_id: "200".into(),
            uname: "水友C".into(),
            gift_name: "小心心".into(),
            gift_num: 1,
            paid,
            combo: Some(ComboInfo {
                base_num: 5,
                count: 3,
                timeout_secs: 2.0,
            }),
        };

        // 官方连击：base*count=15，进入准备池，不立即播报
        let r1 = tracker.handle(&ev(true), now);
        assert!(r1.is_empty());

        // 重复事件刷新窗口
        let r2 = tracker.handle(&ev(true), now + Duration::from_secs(1));
        assert!(r2.is_empty());

        // 未超时无结算
        assert!(tracker.tick(now + Duration::from_secs(2), false).is_empty());

        // 超时结算为合并数量 15
        let r3 = tracker.tick(now + Duration::from_secs(4), false);
        assert_eq!(r3.len(), 1);
        assert_eq!(r3[0], "感谢 水友C 赠送的15个小心心");

        // 仅付费礼物开关：非付费的官方连击被静默丢弃
        let _ = tracker.handle(&ev(false), now + Duration::from_secs(20));
        let r4 = tracker.tick(now + Duration::from_secs(23), true);
        assert!(r4.is_empty());
        println!("[PASS] test_gift_official_combo_and_paid_filter passed");
    }

    #[test]
    fn test_sapi_ssml_and_command() {
        // pitch 正负号与文本转义
        let ssml = build_sapi_ssml("你<好>&吗", 5);
        assert!(ssml.contains("pitch=\"+5st\""), "ssml: {}", ssml);
        assert!(ssml.contains("你&lt;好&gt;&amp;吗"), "ssml: {}", ssml);

        let ssml_neg = build_sapi_ssml("测试", -3);
        assert!(ssml_neg.contains("pitch=\"-3st\""), "ssml: {}", ssml_neg);

        // 命令包含中文音色选择、rate 直传、volume 减半
        let cmd = build_sapi_command(
            "你好",
            &SapiParams {
                rate: 2,
                volume: 100,
                pitch: 0,
            },
        );
        assert!(cmd.contains("$s.Rate=2"), "cmd: {}", cmd);
        assert!(cmd.contains("$s.Volume=50"), "cmd: {}", cmd);
        assert!(cmd.contains("SelectVoice"), "cmd: {}", cmd);
        assert!(cmd.contains("SpeakSsml"), "cmd: {}", cmd);

        // rate 越界钳制到 SAPI 允许范围
        let cmd2 = build_sapi_command(
            "x",
            &SapiParams {
                rate: 99,
                volume: 0,
                pitch: 0,
            },
        );
        assert!(cmd2.contains("$s.Rate=10"), "cmd2: {}", cmd2);
        assert!(cmd2.contains("$s.Volume=0"), "cmd2: {}", cmd2);
        println!("[PASS] test_sapi_ssml_and_command passed");
    }

    #[test]
    fn test_manbo_url_building() {
        let mut cfg = TTSConfig {
            manbo_voice: "曼波".into(),
            manbo_api_key: "k1".into(),
            speech_rate: 2,
            ..Default::default()
        };
        let url = TTSManager::build_manbo_url(&cfg, "你好");
        assert!(url.contains("/apis/mbAIscvip"), "url: {}", url);
        assert!(url.contains("speed=10"), "url: {}", url); // speech_rate * 5
        assert!(url.contains("key=k1"), "url: {}", url);

        cfg.manbo_voice = "撒娇学妹".into();
        let url2 = TTSManager::build_manbo_url(&cfg, "你好");
        assert!(url2.contains("/apis/AIvoice"), "url2: {}", url2);
        assert!(url2.contains("speaker="), "url2: {}", url2);

        let special = TTSManager::build_special_manbo_url("你好");
        assert!(special.contains("/apis/mbAIsc?"), "special: {}", special);
        assert!(!special.contains("key="), "special 端点不得携带 key: {}", special);
        println!("[PASS] test_manbo_url_building passed");
    }

    #[test]
    fn test_content_prefix_and_cache_cleanup() {
        // 与「 说：」标记对齐
        assert_eq!(content_prefix("水友A 说：你好世界一二三"), "水友A_你好世界一");
        assert_eq!(content_prefix("短文本"), "短文本");

        // 签到音频留档写入（命名对齐原工程 SaveCheckinAudio：打卡_{用户名}_{时间戳}.mp3）与超期清理
        let saved = save_checkin_audio("水友A", b"fake-mp3-bytes");
        assert!(saved.is_some(), "签到音频应成功留档");
        let path = saved.unwrap();
        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"fake-mp3-bytes");
        assert!(
            path.file_name().unwrap().to_string_lossy().starts_with("打卡_水友A_"),
            "留档文件名应为 打卡_{{用户名}}_{{时间戳}}.mp3，实际: {:?}",
            path.file_name()
        );

        // 保留天数内的今日目录不得被清理
        let today_dir = path.parent().unwrap().to_path_buf();
        let _ = cleanup_old_cache(7);
        assert!(today_dir.exists(), "今日留档目录不应被清理");

        // 清理测试产物（文件 + 空目录），避免污染仓库数据目录
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&today_dir);
        let _ = std::fs::remove_dir(cache_base_dir());
        println!("[PASS] test_content_prefix_and_cache_cleanup passed");
    }

    #[test]
    fn test_local_voice_zip_loading() {
        // zip 内路径（原工程 voiceMap 使用 manbo/ 前缀）
        let manbo = load_local_voice("manbo/manbo.mp3");
        assert!(manbo.is_some(), "local_voices.zip 内应存在 manbo/manbo.mp3");
        assert!(!manbo.unwrap().is_empty());

        // 仅文件名时自动补 manbo/ 前缀
        let auto = load_local_voice("duang.mp3");
        assert!(auto.is_some(), "duang.mp3 应可从 zip 的 manbo/ 或散装目录加载");

        // 不存在的文件返回 None
        assert!(load_local_voice("not_exists_voice.mp3").is_none());
        println!("[PASS] test_local_voice_zip_loading passed");
    }

    #[test]
    fn test_manbo_voice_list_data() {
        assert_eq!(
            crate::manbo_voices::MANBO_VOICE_LIST.len(),
            185,
            "原工程音色列表共 185 项"
        );
        assert_eq!(crate::manbo_voices::MANBO_VOICE_LIST[0], "曼波");
        let mut sorted = crate::manbo_voices::MANBO_VOICE_LIST.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            crate::manbo_voices::MANBO_VOICE_LIST.len(),
            "音色列表不得有重复项"
        );
        println!("[PASS] test_manbo_voice_list_data passed");
    }

    #[test]
    fn test_special_sound_detection() {
        // 映射返回 zip 内路径（含 manbo/、mho/ 前缀）
        assert_eq!(
            TTSManager::match_special_sound("曼波"),
            Some("manbo/manbo.mp3")
        );
        assert_eq!(
            TTSManager::match_special_sound("曼波曼波"),
            Some("manbo/manbo_3x.mp3")
        );
        assert_eq!(
            TTSManager::match_special_sound("痛快！！"),
            Some("mho/tongkuai.mp3")
        );
        assert_eq!(
            TTSManager::match_special_sound("噢耶"),
            Some("manbo/ohyeah.mp3")
        );
        assert_eq!(TTSManager::match_special_sound("wow"), Some("manbo/wow.mp3"));
        assert_eq!(TTSManager::match_special_sound("未知语音"), None);

        // 音效资源经 paths::find_resource 可定位
        assert!(
            crate::paths::find_resource("voices/duang.mp3").is_some(),
            "duang.mp3 should exist in sound paths"
        );
        println!("[PASS] test_special_sound_detection passed");
    }

    #[test]
    fn test_url_encode_and_base64_decode() {
        let raw = "测试文本 123 !";
        let encoded = url_encode(raw);
        assert!(encoded.contains("%E6%B5%8B%E8%AF%95"));

        let sample = b"Hello, Monster Hunter Wilds!";
        // 手动 Base64 编码对应 SGVsbG8sIE1vbnN0ZXIgSHVudGVyIFdpbGRzIQ==
        let decoded = base64_decode("SGVsbG8sIE1vbnN0ZXIgSHVudGVyIFdpbGRzIQ==").unwrap();
        assert_eq!(decoded, sample);
        println!("[PASS] test_url_encode_and_base64_decode passed");
    }

    #[test]
    fn test_mimo_style_and_hashtag() {
        // 无风格、无标签：原样输出
        assert_eq!(build_mimo_text("", "你好猎人"), "你好猎人");
        // 全局风格：前置 <style>
        assert_eq!(build_mimo_text("温柔", "你好猎人"), "<style>温柔</style>你好猎人");
        // 行内 #标签#：提升到串首
        assert_eq!(build_mimo_text("", "你好#开心#"), "<style>开心</style>你好");
        // 全局 + 行内组合
        assert_eq!(
            build_mimo_text("激昂", "快看#出击#"),
            "<style>激昂</style><style>出击</style>快看"
        );
        println!("[PASS] test_mimo_style_and_hashtag passed");
    }
}
