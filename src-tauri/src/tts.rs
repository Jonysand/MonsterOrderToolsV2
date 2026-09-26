//! 多引擎 TTS 语音域。
//! 对照原工程实现：`TextToSpeech.cpp` / `ManboTTSProvider.cpp` /
//! `SpecialManboTTSProvider.cpp` / `TTSCacheManager.cpp` / `LocalVoiceManager.cpp`。

#[cfg(not(test))]
use rodio::{Decoder, OutputStream, Sink};
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
/// `Auto` 对齐原工程 `TTSProviderFactory` 的 AUTO 模式：按 Manbo → SAPI 顺序取首个可用引擎
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TTSEngineType {
    Auto,
    Manbo,
    Sapi,
}

impl TTSEngineType {
    /// 引擎名（对齐原工程 `TTSManager_GetCurrentProviderName` 的返回值：manbo / sapi）
    pub fn provider_name(self) -> &'static str {
        match self {
            TTSEngineType::Auto => "auto",
            TTSEngineType::Manbo => "manbo",
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
}

impl Default for TTSConfig {
    fn default() -> Self {
        Self {
            engine: TTSEngineType::Manbo,
            enable_voice: true,
            speech_rate: 0,
            // 新刻度 50 = 旧刻度 100（原工程默认）的等效响度
            speech_volume: 50,
            speech_pitch: 0,
            manbo_api_key: String::new(),
            // 原工程默认音色为「曼波」，走 /apis/mbAIscvip 专用端点
            manbo_voice: "曼波".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// 串行音频播放队列（B3 防叠音）
// ---------------------------------------------------------------------------

/// 播放任务载荷（音量随任务携带：对齐原工程 `AudioPlayer::SetVolume` 在起播前设置 MCI 音量）
#[derive(Debug)]
enum AudioJob {
    /// 本地音效：增益 = volume_gain（不做语音响度校准）
    Bytes { bytes: Vec<u8>, volume: i32 },
    /// 云端 TTS 语音（Manbo）：增益 = volume_gain × MANBO_LOUDNESS_GAIN（响度与 SAPI 对齐）
    TtsBytes { bytes: Vec<u8>, volume: i32 },
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

    /// 云端 TTS 语音入队：增益额外乘响度校准系数（与本地音效区分）
    pub fn play_tts_bytes(&self, bytes: Vec<u8>, volume: i32) -> Result<(), String> {
        self.tx
            .send(AudioJob::TtsBytes { bytes, volume })
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
                AudioJob::TtsBytes { bytes, volume } => bytes.len() + *volume as usize,
                AudioJob::File { path, volume } => path.as_os_str().len() + *volume as usize,
                AudioJob::Sapi { text, params } => text.len() + params.rate.unsigned_abs() as usize,
            };
        }
        #[cfg(not(test))]
        match job {
            AudioJob::Bytes { bytes, volume } => {
                if let Ok(decoder) = Decoder::new(Cursor::new(bytes.clone())) {
                    play_decoder_sync(decoder, volume_gain(*volume));
                }
            }
            AudioJob::TtsBytes { bytes, volume } => {
                if let Ok(decoder) = Decoder::new(Cursor::new(bytes.clone())) {
                    play_decoder_sync(decoder, tts_stream_gain(*volume));
                }
            }
            AudioJob::File { path, volume } => {
                if let Ok(file) = File::open(path) {
                    if let Ok(decoder) = Decoder::new(file) {
                        play_decoder_sync(decoder, volume_gain(*volume));
                    }
                }
            }
            AudioJob::Sapi { text, params } => {
                run_local_speech(text, params);
            }
        }
    }
}

/// 配置音量换算为 rodio 增益：新刻度 0~200（100 = 旧刻度满量 200 即 1.0 增益，
/// 200 = 2.0 增益，对应用户要求的「新 200 = 旧 400」），增益可大于 1.0（rodio 支持放大）
pub fn volume_gain(speech_volume: i32) -> f32 {
    (speech_volume.clamp(0, 200) as f32) / 100.0
}

/// Manbo 云端语音响度校准增益：实测「佩奇猪」音色原始输出 RMS 与
/// Windows SAPI 满档（$s.Volume=100，Huihui）对齐所得（三次测量 0.748/0.741/0.724，
/// 中位数取 0.74，即佩奇猪原始输出比 SAPI 满档约响 2.6 dB，需衰减对齐），
/// 测量工具见 `test_manbo_peiqi_loudness_calibration`（#[ignore]，可复跑）。
/// 仅应用于云端 TTS 流；本地音效保持各自原始响度，不受此系数影响。
pub const MANBO_LOUDNESS_GAIN: f32 = 0.74;

/// 云端 TTS 流的 rodio 增益 = 用户刻度增益 × 响度校准系数
pub fn tts_stream_gain(speech_volume: i32) -> f32 {
    volume_gain(speech_volume) * MANBO_LOUDNESS_GAIN
}

/// rodio 同步播放（带 60s 播放超时保护，防止异常流卡死播放线程）
#[cfg(not(test))]
fn play_decoder_sync<R>(decoder: Decoder<R>, gain: f32) -> bool
where
    R: std::io::Read + std::io::Seek + Send + 'static,
{
    if let Ok((_stream, handle)) = OutputStream::try_default() {
        if let Ok(sink) = Sink::try_new(&handle) {
            // 对齐原工程：起播前按配置设置音量（Manbo/本地音效同样生效）
            sink.set_volume(gain);
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
    // SAPI 档位 0~100：新刻度 100 即满档（等价旧刻度 200），200 起钳制到满档
    let volume = params.volume.clamp(0, 100);
    format!(
        "Add-Type -AssemblyName System.Speech; \
$s=New-Object System.Speech.Synthesis.SpeechSynthesizer; \
try{{$zh=@($s.GetInstalledVoices()|Where-Object{{$_.VoiceInfo.Culture.Name -like 'zh*'}}); if($zh.Count -gt 0){{$s.SelectVoice($zh[0].VoiceInfo.Name)}}}}catch{{}}; \
$s.Rate={rate}; $s.Volume={volume}; $s.SpeakSsml('{ssml}')"
    )
}

/// 本地离线语音兜底的实际执行入口。
/// Windows → PowerShell + System.Speech（SAPI）；macOS → 内置 `say` 命令。
/// 二者均同步等待播放结束，并在超时后强杀子进程，
/// 防止子进程异常挂起导致播放线程被永久占用。
#[cfg(not(test))]
fn run_local_speech(text: &str, params: &SapiParams) -> bool {
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

    #[cfg(target_os = "macos")]
    {
        run_say_macos(text, params)
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        // 其余平台暂无内置离线语音实现
        let _ = (text, params);
        false
    }
}

/// SAPI 档位参数 → `say` 参数映射（纯函数，便于单测）。
/// - `rate`：SAPI 为 -10~10 档位，`say -r` 为词/分钟（系统默认约 175），按 15 wpm/档折算
/// - `volume`：新刻度 0~200（100 = 原满量），映射为 `say` 内嵌音量命令的 0.0~1.0（上限 1.0）
pub fn say_params_from(rate: i32, volume: i32) -> (i32, f64) {
    (
        (175 + rate.clamp(-10, 10) * 15).clamp(80, 400),
        ((volume.clamp(0, 200) as f64) / 100.0).min(1.0),
    )
}

/// 构建 macOS `say` 的参数表（不含可执行文件自身）。
/// 音量以文本前缀 `[[volm 0~1]]` 内嵌语音命令承载 —— `say` 没有独立音量开关。
/// `pitch` 在 `say` 上无对应参数，此路径不生效（仅影响音调，不影响可听性）。
pub fn build_say_args(text: &str, params: &SapiParams, voice: Option<&str>) -> Vec<String> {
    let (rate, volume) = say_params_from(params.rate, params.volume);
    let mut args = vec!["-r".to_string(), rate.to_string()];
    if let Some(v) = voice {
        args.push("-v".to_string());
        args.push(v.to_string());
    }
    args.push(format!("[[volm {:.2}]]{}", volume, text));
    args
}

/// 从 `say -v ?` 的输出中挑选首个中文音色名。
///
/// 音色名可能自带空格与括号（新版本地化命名），故不能按首个空白截断，
/// 需以「`#` 注释之前的最后一个空白分隔段」为 locale，其左侧整体为音色名：
/// ```text
/// Tingting            zh_CN    # 您好，我叫Tingting。
/// Eddy (中文（中国大陆）)     zh_CN    # 你好！我叫Eddy。
/// ```
/// 注意：`say` 对不存在的音色名不报错（静默回退默认音色），
/// 故必须精确解析，否则中文文本会被英文音色读出。
pub fn parse_zh_voice_from_listing(listing: &str) -> Option<String> {
    for line in listing.lines() {
        let head = line.split('#').next().unwrap_or("").trim_end();
        let Some(sep) = head.rfind(char::is_whitespace) else {
            continue;
        };
        let (name, locale) = head.split_at(sep);
        let name = name.trim();
        let locale = locale.trim();
        if name.is_empty() {
            continue;
        }
        if locale.eq_ignore_ascii_case("zh_CN")
            || locale.eq_ignore_ascii_case("zh_TW")
            || locale.eq_ignore_ascii_case("zh_HK")
        {
            return Some(name.to_string());
        }
    }
    None
}

/// macOS 中文音色（首次调用探测并缓存；系统未装中文音色时为 `None`，退回系统默认音色）
#[cfg(all(target_os = "macos", not(test)))]
static MACOS_ZH_VOICE: OnceLock<Option<String>> = OnceLock::new();

/// 探测并缓存系统首个中文音色（探测失败或系统无中文音色时返回 `None`）
#[cfg(all(target_os = "macos", not(test)))]
fn pick_macos_zh_voice() -> Option<String> {
    MACOS_ZH_VOICE
        .get_or_init(|| {
            let out = std::process::Command::new("/usr/bin/say")
                .arg("-v")
                .arg("?")
                .output()
                .ok()?;
            parse_zh_voice_from_listing(&String::from_utf8_lossy(&out.stdout))
        })
        .clone()
}

/// macOS 离线兜底播报（与 Windows SAPI 路径同为同步等待 + 超时强杀）
#[cfg(all(target_os = "macos", not(test)))]
fn run_say_macos(text: &str, params: &SapiParams) -> bool {
    let voice = pick_macos_zh_voice();
    let mut cmd = std::process::Command::new("/usr/bin/say");
    cmd.args(build_say_args(text, params, voice.as_deref()));

    match cmd.spawn() {
        Ok(mut child) => {
            let deadline = Instant::now() + SAPI_PLAYBACK_TIMEOUT;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return status.success(),
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

/// 连击播报报告：把「文本」与「是否允许播报」分开。
///
/// 「仅付费礼物」开关只应影响 `can_speak`，不得连带删掉业务留档 ——
/// 若在 tracker 内部直接丢弃免费礼物文案，那段历史就永久缺了。
#[derive(Debug, Clone, PartialEq)]
pub struct ComboReport {
    pub text: String,
    pub can_speak: bool,
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
    pub fn handle(&mut self, ev: &GiftEvent, now: Instant) -> Vec<ComboReport> {
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

        let mut out: Vec<ComboReport> = Vec::new();
        let mut set_cooldown = false;
        match self.dynamic.get_mut(&key) {
            Some(d) => {
                d.gift_num += ev.gift_num;
                d.deadline = now + DYNAMIC_COMBO_WINDOW;
                if !d.first_reported && d.gift_num >= 3 {
                    out.push(ComboReport {
                        text: format!("感谢 {} 开始赠送{}", d.uname, d.gift_name),
                        can_speak: true,
                    });
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
                    out.push(ComboReport {
                        text: format!(
                            "感谢 {} 赠送的{}个{}",
                            ev.uname, ev.gift_num, ev.gift_name
                        ),
                        can_speak: true,
                    });
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

    /// 周期结算：超时的连击池出队。
    ///
    /// 「仅付费礼物」只决定 `can_speak`，结算文案本身**总是**产出，
    /// 因此关闭该开关期间的免费礼物同样会进入业务留档。
    pub fn tick(&mut self, now: Instant, only_paid_gift: bool) -> Vec<ComboReport> {
        let mut out: Vec<ComboReport> = Vec::new();

        self.prepare.retain(|_, p| {
            if now >= p.deadline {
                out.push(ComboReport {
                    text: format!("感谢 {} 赠送的{}个{}", p.uname, p.gift_num, p.gift_name),
                    can_speak: !only_paid_gift || p.paid,
                });
                false
            } else {
                true
            }
        });

        self.dynamic.retain(|_, d| {
            if now >= d.deadline {
                if !d.first_reported || d.gift_num > 0 {
                    out.push(ComboReport {
                        text: format!("感谢 {} 赠送的{}个{}", d.uname, d.gift_num, d.gift_name),
                        can_speak: true,
                    });
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
            // 队满丢弃不再静默：写日志便于排查刷屏导致的丢播报。
            // 只记队列长度：被丢弃的文案属业务原文，不进普通 Logs（History 已另行留档）
            crate::log_warn!("[TTS] 播报队列已满（上限 {} 条），本条已丢弃", MAX_SPEAK_QUEUE);
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
            // 本地兜底恒常可用：Windows 走 System.Speech（SAPI），macOS 走内置 `say`
            TTSEngineType::Sapi => true,
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
            TTSEngineType::Sapi => {}
        }
    }

    /// 选择当前最优可用引擎（首选引擎健康则用之；否则按 Manbo -> Sapi 降级）。
    /// 配置为 `Auto`（对齐原工程 TTSProviderFactory 的 AUTO 模式）时直接走该降级链路。
    pub fn select_active_engine(&self) -> TTSEngineType {
        let preferred = self.config.lock().unwrap().engine;
        if preferred != TTSEngineType::Auto && self.is_engine_available(preferred) {
            return preferred;
        }

        // 故障降级链路
        if self.is_engine_available(TTSEngineType::Manbo) {
            TTSEngineType::Manbo
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

    /// 当前实际使用的引擎名（对齐原工程 `TTSManager_GetCurrentProviderName`：manbo / sapi）。
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
    pub fn process_gift(&self, ev: &GiftEvent) -> Vec<ComboReport> {
        let mut tracker = self.gift_tracker.lock().unwrap();
        tracker.handle(ev, Instant::now())
    }

    /// 刷新连击池（超时结算）
    pub fn flush_gift_combos(&self, only_paid_gift: bool) -> Vec<ComboReport> {
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
        // 锁中毒兜底 50 = 旧口径 100（原工程默认）的等效响度
        self.config.lock().map(|c| c.speech_volume).unwrap_or(50)
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
        AudioQueue::global().play_tts_bytes(bytes.to_vec(), self.current_volume())
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

        // 3. 选择当前最优健康引擎并尝试（Manbo 失败即降级，落到本地 SAPI 兜底）
        if self.select_active_engine() == TTSEngineType::Manbo {
            let url = Self::build_manbo_url(&cfg, trimmed);
            match Self::request_audio_bytes(&client, &url, Some(&cfg.manbo_api_key)).await {
                Ok(bytes) => {
                    self.mark_active_engine(TTSEngineType::Manbo);
                    return self.play_audio_bytes(&bytes, checkin_username);
                }
                Err(_) => {
                    self.mark_engine_degraded(TTSEngineType::Manbo);
                }
            }
        }

        // 4. 本地离线兜底（Windows SAPI / macOS say），全参数按平台映射
        self.mark_active_engine(TTSEngineType::Sapi);
        let params = SapiParams {
            rate: cfg.speech_rate,
            volume: cfg.speech_volume,
            pitch: cfg.speech_pitch,
        };
        AudioQueue::global().speak_sapi(trimmed.to_string(), params)
    }

    /// 试听指定参数的测试句（设置面板「试听」按钮）。
    /// 引擎选择与实际播报一致（首选引擎健康则用之，Manbo 失败自动降级 SAPI）；
    /// 音量/语速/音调用传入即时值（绕开配置自动保存防抖），
    /// 不受语音总开关限制（试听语义 = 配置阶段听当前参数效果）
    pub async fn speak_test_sample(
        &self,
        text: &str,
        volume: i32,
        rate: i32,
        pitch: i32,
    ) -> Result<(), String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        let cfg = self.config.lock().unwrap().clone();
        let client = reqwest::Client::builder()
            .timeout(Self::REQUEST_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;

        let preferred = self.select_active_engine();
        crate::log_info!(
            "[TTS] 试听播报: 文本=「{}」, 首选引擎={}, 音量={}, 语速={}, 音调={}",
            trimmed,
            preferred.provider_name(),
            volume,
            rate,
            pitch
        );

        if preferred == TTSEngineType::Manbo {
            // 曼波默认音色的 speed 参数按试听即时语速覆盖（其余音色无此参数，
            // 音调/音量对 Manbo 云端无效，音量在本地播放端应用）
            let url = Self::build_manbo_url(
                &TTSConfig {
                    speech_rate: rate,
                    ..cfg.clone()
                },
                trimmed,
            );
            match Self::request_audio_bytes(&client, &url, Some(&cfg.manbo_api_key)).await {
                Ok(bytes) => {
                    self.mark_active_engine(TTSEngineType::Manbo);
                    crate::log_info!(
                        "[TTS] 试听走 Manbo 引擎（音量 {} 由本地播放端增益应用）",
                        volume
                    );
                    return AudioQueue::global().play_tts_bytes(bytes, volume);
                }
                Err(e) => {
                    self.mark_engine_degraded(TTSEngineType::Manbo);
                    crate::log_warn!("[TTS] 试听 Manbo 请求失败({}),降级本地 SAPI", e);
                }
            }
        }

        self.mark_active_engine(TTSEngineType::Sapi);
        crate::log_info!(
            "[TTS] 试听走本地 SAPI: volume={}, rate={}, pitch={}（SAPI 档位 {}）",
            volume,
            rate,
            pitch,
            volume.clamp(0, 100)
        );
        AudioQueue::global().speak_sapi(
            trimmed.to_string(),
            SapiParams { rate, volume, pitch },
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::Decoder;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 手动校准工具（cargo test --release test_manbo_peiqi_loudness_calibration -- --ignored --nocapture）：
    /// 实测 Manbo「佩奇猪」音色原始输出与 Windows SAPI 满档（$s.Volume=100）的 RMS 差，
    /// 据此回填 [`MANBO_LOUDNESS_GAIN`]，使相同音量设置下两个引擎听感响度一致。
    /// 需要网络（Manbo API）与 PowerShell 环境；日常 CI 不跑（#[ignore]）。
    /// 注意：同一文本多次合成的波动约 ±0.5 dB，回填系数时以多次结果的中位数为准。
    #[test]
    #[ignore = "手动校准工具：需要网络，日常 CI 不跑"]
    fn test_manbo_peiqi_loudness_calibration() {
        const TEXT: &str = "你好，这是一条响度校准测试语音，用来对齐云端与本地引擎的音量。";

        // 1) Manbo「佩奇猪」（AIvoice 端点，无 key 参数）下载并解码
        let url = format!(
            "https://api.milorapart.top/apis/AIvoice?speaker={}&text={}",
            url_encode("佩奇猪"),
            url_encode(TEXT)
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        let bytes = rt.block_on(async {
            let client = reqwest::Client::builder()
                .timeout(TTSManager::REQUEST_TIMEOUT)
                .build()
                .unwrap();
            let resp = client.get(&url).send().await.unwrap();
            let json: serde_json::Value = resp.json().await.unwrap();
            let audio_url = json
                .get("url")
                .and_then(|u| u.as_str())
                .expect("Manbo 响应应含 url 字段")
                .to_string();
            let audio = client.get(audio_url).send().await.unwrap();
            let bytes = audio.bytes().await.unwrap().to_vec();
            println!("[校准] Manbo 音频大小: {} 字节", bytes.len());
            bytes
        });

        // 解码为 f32 采样（symphonia 解 mp3），统计 RMS 与峰值
        let manbo_samples = decode_all_mp3(bytes).expect("Manbo mp3 应可解码");
        let (manbo_rms, manbo_peak) = rms_peak(&manbo_samples);
        println!(
            "[校准] Manbo 佩奇猪: 采样={} rms={:.5} peak={:.5}",
            manbo_samples.len(),
            manbo_rms,
            manbo_peak
        );

        // 2) SAPI 满档基准：与 run_local_speech 同构（Huihui，Volume=100）渲染 WAV
        let dir = std::env::temp_dir().join("mh_sapi_calibration");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let escaped = TEXT
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let ps = format!(
            "Add-Type -AssemblyName System.Speech; \
             $r = New-Object System.Speech.Synthesis.SpeechSynthesizer; \
             $r.Volume = 100; \
             $r.SetOutputToWaveFile('{}'); \
             $r.SpeakSsml('<speak version=\"1.0\" xml:lang=\"zh-CN\"><prosody pitch=\"+0st\">{}</prosody></speak>'); \
             $r.SetOutputToWaveFile($null); $r.Dispose()",
            dir.join("sapi_ref.wav").display(),
            escaped
        );
        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
            .status()
            .expect("应能启动 PowerShell");
        assert!(status.success(), "SAPI 渲染失败");
        let wav = std::fs::read(dir.join("sapi_ref.wav")).unwrap();
        let sapi_samples = read_wav_pcm16(&wav).expect("WAV 应为 16-bit PCM");
        let (sapi_rms, sapi_peak) = rms_peak(&sapi_samples);
        println!(
            "[校准] SAPI 满档: 采样={} rms={:.5} peak={:.5}",
            sapi_samples.len(),
            sapi_rms,
            sapi_peak
        );

        // 3) 校准系数：使 Manbo 在 gain=校准系数时与 SAPI 满档 RMS 对齐
        let gain = sapi_rms / manbo_rms;
        let gain_db = 20.0 * gain.log10();
        let clipped_peak = manbo_peak * gain;
        println!(
            "[校准] 建议系数 = {:.3} ({:+.1} dB)；应用后 Manbo 峰值 = {:.3}（>1.0 有削波风险）",
            gain, gain_db, clipped_peak
        );
        println!("[PASS] test_manbo_peiqi_loudness_calibration passed");
    }

    /// 解码 mp3 全部采样（rodio 0.19 symphonia 后端输出 i16），归一化为 f32（-1.0..1.0）
    fn decode_all_mp3(bytes: Vec<u8>) -> Option<Vec<f32>> {
        let decoder = Decoder::new_mp3(Cursor::new(bytes)).ok()?;
        let mut out = Vec::new();
        for s in decoder {
            out.push(s as f32 / 32768.0);
        }
        Some(out)
    }

    /// 采样序列的 RMS 与峰值
    fn rms_peak(samples: &[f32]) -> (f32, f32) {
        let sum_sq: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
        let rms = (sum_sq / samples.len().max(1) as f64).sqrt() as f32;
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        (rms, peak)
    }

    /// 解析 16-bit PCM WAV 为 f32 采样（按 RIFF 块结构定位 data 块）
    fn read_wav_pcm16(bytes: &[u8]) -> Option<Vec<f32>> {
        let mut i = 12usize;
        while i + 8 <= bytes.len() {
            let id = &bytes[i..i + 4];
            let size =
                u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]])
                    as usize;
            if id == b"data" {
                let data = &bytes[i + 8..(i + 8 + size).min(bytes.len())];
                return Some(
                    data.chunks_exact(2)
                        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
                        .collect(),
                );
            }
            i += 8 + size + (size % 2);
        }
        None
    }

    #[test]
    fn test_auto_engine_cascade_and_current_engine_name() {
        // D1：引擎「自动」对齐原工程 TTSProviderFactory 的 AUTO 模式（manbo -> sapi）
        let mgr = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Auto,
            ..Default::default()
        });
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Manbo);
        assert_eq!(mgr.current_engine_name(), "manbo");

        // Manbo 熔断后自动降级 SAPI
        mgr.mark_engine_degraded(TTSEngineType::Manbo);
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Sapi);
        assert_eq!(mgr.current_engine_name(), "sapi");

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
        assert_eq!(TTSEngineType::Sapi.provider_name(), "sapi");
        println!("[PASS] test_auto_engine_cascade_and_current_engine_name passed");
    }

    #[test]
    fn test_tts_circuit_breaker_and_cooldown_recovery() {
        let mgr = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Manbo,
            ..Default::default()
        });

        // 初始状态：Manbo 为健康
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Manbo);

        // 模拟 Manbo 请求超时触发故障降级
        mgr.mark_engine_degraded(TTSEngineType::Manbo);
        assert!(!mgr.is_engine_available(TTSEngineType::Manbo));

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
        // 新刻度 0~200：100 = 旧刻度满量（1.0 增益）、200 = 旧刻度 400（2.0 增益）、0 = 静音
        assert_eq!(volume_gain(100), 1.0);
        assert_eq!(volume_gain(200), 2.0);
        assert_eq!(volume_gain(50), 0.5);
        assert_eq!(volume_gain(0), 0.0);
        // 越界钳制（0~200 之外收口到 0.0~2.0）
        assert_eq!(volume_gain(-10), 0.0);
        assert_eq!(volume_gain(999), 2.0);
        println!("[PASS] test_volume_gain_mapping passed");
    }

    #[test]
    fn test_tts_stream_gain_applies_calibration() {
        // 云端 TTS 流增益 = 用户刻度增益 × 响度校准系数（0.74）
        assert!((tts_stream_gain(100) - 0.74).abs() < 1e-6);
        assert!((tts_stream_gain(50) - 0.37).abs() < 1e-6);
        // 本地音效路径不做校准：同刻度下相差恰为校准系数
        assert!((volume_gain(100) / tts_stream_gain(100) - 1.0 / MANBO_LOUDNESS_GAIN).abs() < 1e-6);
        println!("[PASS] test_tts_stream_gain_applies_calibration passed");
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
        assert_eq!(r1[0].text, "感谢 水友A 赠送的1个小电视");

        // 冷却期内累加不播报（对齐原工程 5s 冷却）
        let r2 = tracker.handle(&ev(2), now + Duration::from_millis(100));
        assert!(r2.is_empty());

        // 动态窗口未结束：不结算
        let r3 = tracker.tick(now + Duration::from_millis(200), false);
        assert!(r3.is_empty());

        // 超过 10s 动态窗口：尾报合并数量（1+2=3）
        let r4 = tracker.tick(now + Duration::from_secs(11), false);
        assert_eq!(r4.len(), 1);
        assert_eq!(r4[0].text, "感谢 水友A 赠送的3个小电视");
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
        assert_eq!(r2[0].text, "感谢 水友B 开始赠送辣条");

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
        assert_eq!(r3[0].text, "感谢 水友C 赠送的15个小心心");

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

        // 命令包含中文音色选择、rate 直传、volume 直传（新刻度 100 即 SAPI 满档）
        let cmd = build_sapi_command(
            "你好",
            &SapiParams {
                rate: 2,
                volume: 100,
                pitch: 0,
            },
        );
        assert!(cmd.contains("$s.Rate=2"), "cmd: {}", cmd);
        assert!(cmd.contains("$s.Volume=100"), "cmd: {}", cmd);
        assert!(cmd.contains("SelectVoice"), "cmd: {}", cmd);
        assert!(cmd.contains("SpeakSsml"), "cmd: {}", cmd);

        // rate 越界钳制到 SAPI 允许范围；volume 超满档钳到 100（新 200 = 旧 400，SAPI 无增益空间）
        let cmd2 = build_sapi_command(
            "x",
            &SapiParams {
                rate: 99,
                volume: 200,
                pitch: 0,
            },
        );
        assert!(cmd2.contains("$s.Rate=10"), "cmd2: {}", cmd2);
        assert!(cmd2.contains("$s.Volume=100"), "cmd2: {}", cmd2);

        // rodio 增益换算：新刻度 100 = 1.0（原满量）、200 = 2.0（2 倍放大）、越界钳制
        assert_eq!(volume_gain(100), 1.0);
        assert_eq!(volume_gain(200), 2.0);
        assert_eq!(volume_gain(0), 0.0);
        assert_eq!(volume_gain(50), 0.5);
        assert_eq!(volume_gain(300), 2.0);
        assert_eq!(volume_gain(-1), 0.0);
        println!("[PASS] test_sapi_ssml_and_command passed");
    }

    /// macOS `say` 兜底路径的参数映射（跨平台可测：不依赖 macOS 运行时）
    #[test]
    fn test_say_args_mapping() {
        // 默认档位 rate=0 → 系统基准 175 wpm；新刻度 100 = 原满量 → volm 1.0
        assert_eq!(say_params_from(0, 100), (175, 1.0));
        // 正负档位线性折算；新刻度 50 = 旧 100 半量 → 0.5
        assert_eq!(say_params_from(2, 50), (205, 0.5));
        assert_eq!(say_params_from(-3, 0), (130, 0.0));
        // 越界钳制：rate 档位先钳到 -10~10（对应 25~325 wpm），再过 80~400 安全区间；volume 钳到 0~1
        assert_eq!(say_params_from(99, 999), (325, 1.0));
        assert_eq!(say_params_from(-99, -5), (80, 0.0));

        // 参数表：无音色时不带 -v；有音色时插入 -v <voice>
        let params = SapiParams { rate: 0, volume: 100, pitch: 5 };
        let args = build_say_args("你好", &params, None);
        assert_eq!(args, vec!["-r", "175", "[[volm 1.00]]你好"]);

        let args_zh = build_say_args("你好", &params, Some("Tingting"));
        assert_eq!(
            args_zh,
            vec!["-r", "175", "-v", "Tingting", "[[volm 1.00]]你好"]
        );
        println!("[PASS] test_say_args_mapping passed");
    }

    /// `say -v ?` 中文音色解析：必须保住含空格/括号的音色名，
    /// 否则会退化成英文音色把中文读成静音（`say` 对无效音色名不报错）
    #[test]
    fn test_parse_zh_voice_from_listing() {
        // 经典格式（音色名无空格）
        assert_eq!(
            parse_zh_voice_from_listing("Tingting            zh_CN    # 您好，我叫Tingting。"),
            Some("Tingting".to_string())
        );

        // 新版本地化格式：音色名含空格与括号，不得被首个空白截断
        assert_eq!(
            parse_zh_voice_from_listing("Eddy (中文（中国大陆）)     zh_CN    # 你好！我叫Eddy。"),
            Some("Eddy (中文（中国大陆）)".to_string())
        );

        // 跳过非中文音色，取首个中文音色
        let listing = "Alex                en_US    # Most people recognize me by my voice.\n\
                       Eddy (中文（中国大陆）)     zh_CN    # 你好！我叫Eddy。\n\
                       Tingting            zh_CN    # 您好，我叫Tingting。";
        assert_eq!(
            parse_zh_voice_from_listing(listing),
            Some("Eddy (中文（中国大陆）)".to_string())
        );

        // 繁体中文同样命中
        assert_eq!(
            parse_zh_voice_from_listing("Meijia              zh_TW    # 你好，我叫Meijia。"),
            Some("Meijia".to_string())
        );

        // 无中文音色 / 空输出
        assert_eq!(
            parse_zh_voice_from_listing("Alex                en_US    # hi"),
            None
        );
        assert_eq!(parse_zh_voice_from_listing(""), None);
        println!("[PASS] test_parse_zh_voice_from_listing passed");
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
    fn test_url_encode() {
        let raw = "测试文本 123 !";
        let encoded = url_encode(raw);
        assert!(encoded.contains("%E6%B5%8B%E8%AF%95"));
        println!("[PASS] test_url_encode passed");
    }
}
