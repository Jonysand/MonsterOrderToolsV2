//! 多引擎 TTS 语音域。
//! 合成音源为**局域网曼波引擎**（GPT-SoVITS 图形客户端自带曼波模型，部署于局域网另一台
//! 机器，部署包与接口契约见 `F:\ManboTTS\package\MamboTTS-Engine\README.md`
//! 与 `对接契约变更-20261107.md`，zip 定稿 SHA256 见后者）；**大肥鱼 TTS** 为云端备选
//! （DeepSeek 官方语音 API，2026-11-13 开放：`POST https://api.deepseek.com/v1/tools/tts`，
//! 与打卡 AI 共用 credentials.dat 中的同一把 DeepSeek Key，按字符计费、官方接口无语速参数）；
//! 本地离线兜底为 Windows SAPI / macOS say。
//! 对照原工程实现：`TextToSpeech.cpp` / `TTSCacheManager.cpp` / `LocalVoiceManager.cpp`。
//! 迁移史：原云端 Manbo（api.milorapart.top）已于 2026-11-07 移除 —— 免 Key 试用端点
//! `/apis/mbAIsc` 被服务方下架（API 404），通用音色端点 `/apis/AIvoice` 与付费端点
//! `/apis/mbAIscvip` 一并退场，由局域网引擎整体替代（原工程 `SpecialManboTTSProvider.cpp`
//! / `ManboTTSProvider.cpp` 对应能力由本地 GPT-SoVITS 部署包承接）。
//! 引擎包定稿后参考音频已引擎侧固化（启动参数 `-dr/-dt/-dl` 预设），请求体只发
//! `text / text_language / speed`，refer 三字段整段省略（2026-11-07 契约变更）。

#[cfg(not(test))]
use rodio::{Decoder, OutputStream, Sink};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::File;
#[cfg(not(test))]
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 语音引擎类型
/// `Auto` 对齐原工程 `TTSProviderFactory` 的 AUTO 模式：按 曼波 → 大肥鱼 → SAPI 顺序取首个可用引擎
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TTSEngineType {
    Auto,
    Manbo,
    /// 大肥鱼（DeepSeek 云端 TTS，与 AI 对话共用 Key）
    Feiyu,
    Sapi,
}

impl TTSEngineType {
    /// 引擎名（对齐原工程 `TTSManager_GetCurrentProviderName` 的返回值：manbo / sapi；
    /// 新增的大肥鱼沿用同一命名口径，返回 feiyu）
    pub fn provider_name(self) -> &'static str {
        match self {
            TTSEngineType::Auto => "auto",
            TTSEngineType::Manbo => "manbo",
            TTSEngineType::Feiyu => "feiyu",
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

/// serde 缺省：自动模式引擎开关默认启用（旧配置/旧快照缺键时保持原有级联行为）
fn default_true() -> bool {
    true
}

/// TTS 智能语音配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TTSConfig {
    pub engine: TTSEngineType,
    /// 自动模式下是否启用曼波（局域网）引擎：关闭后自动链路跳过曼波。
    /// 仅约束「自动」；显式选中曼波不受影响；本地语音为最终兜底恒常启用
    #[serde(default = "default_true")]
    pub auto_enable_manbo_tts: bool,
    /// 自动模式下是否启用大肥鱼（DeepSeek 云端）引擎：关闭后自动链路跳过该引擎
    /// （用于避免曼波故障时静默产生云端计费）
    #[serde(default = "default_true")]
    pub auto_enable_feiyu_tts: bool,
    pub enable_voice: bool,
    pub speech_rate: i32,
    pub speech_volume: i32,
    pub speech_pitch: i32,
    /// 局域网曼波引擎（GPT-SoVITS）基础地址，如 `http://192.168.1.50:9880`。
    /// 空 = 未配置：曼波播报直接降级本地 SAPI（不熔断，属配置问题而非服务故障）。
    /// 参考音频已引擎侧固化（2026-11-07 契约变更），工具侧无对应配置
    #[serde(default)]
    pub lan_engine_base_url: String,
    /// 大肥鱼（DeepSeek TTS）音色 ID：`stella`（女声，官方默认）/ `tide`（男声）。
    /// Key 与 AI 对话共用 credentials.dat 的 DeepSeek Key，不随配置持久化
    #[serde(default)]
    pub feiyu_voice_id: String,
}

impl Default for TTSConfig {
    fn default() -> Self {
        Self {
            engine: TTSEngineType::Manbo,
            auto_enable_manbo_tts: true,
            auto_enable_feiyu_tts: true,
            enable_voice: true,
            speech_rate: 0,
            // 50 = 原工程默认 100（旧满量 200 的一半）的等效响度
            speech_volume: 50,
            speech_pitch: 0,
            lan_engine_base_url: String::new(),
            feiyu_voice_id: Self::FEIYU_DEFAULT_VOICE.to_string(),
        }
    }
}

impl TTSConfig {
    /// 大肥鱼默认音色：DeepSeek 官方默认女声 stella（男声为 tide）
    pub const FEIYU_DEFAULT_VOICE: &'static str = "stella";
}

// ---------------------------------------------------------------------------
// 播放开始气泡通知（对齐原工程 CheckinTTSPlay 回调：气泡与出声同帧出现）
// ---------------------------------------------------------------------------

/// 随播报任务携带的前端气泡事件：由音频线程在**真正开始播出**的瞬间投递。
///
/// 为什么不在业务定稿时发：TTS 播报队列与串行音频队列都可能积压，
/// 定稿时发会让气泡先于声音出现、甚至 15s 生命期结束后声音才来
/// （见 docs/CORE_FLOW_REMEDIATION_PLAN_2026-09-25.md 第 8 节第 3 条的产品决策）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BubbleNotice {
    /// Tauri 事件名（当前仅 `checkin-reply`）
    pub event: &'static str,
    /// 事件载荷（与业务层「立即显示」回退路径的载荷完全一致）
    pub payload: serde_json::Value,
}

impl BubbleNotice {
    pub fn checkin_reply(payload: serde_json::Value) -> Self {
        Self {
            event: "checkin-reply",
            payload,
        }
    }
}

/// 播放开始通知器：App 装配期注入（持有 AppHandle 发事件），单测可替换观测
type PlayNoticeFn = Arc<dyn Fn(&BubbleNotice) + Send + Sync>;
static PLAY_NOTICE_NOTIFIER: Mutex<Option<PlayNoticeFn>> = Mutex::new(None);

/// 装配播放开始通知器（App setup 时调用一次；重复调用以最后一次为准）
pub fn set_play_notice_notifier(f: PlayNoticeFn) {
    *PLAY_NOTICE_NOTIFIER.lock().unwrap() = Some(f);
}

/// 音频线程真正开始播出某个 job 的瞬间调用；未装配通知器（单测/Lite）时为 no-op
pub(crate) fn fire_play_notice(notice: &BubbleNotice) {
    // 先取出再调用：避免持锁执行回调（回调内可能再入本模块）
    let f = PLAY_NOTICE_NOTIFIER.lock().unwrap().clone();
    if let Some(f) = f {
        f(notice);
    }
}

// ---------------------------------------------------------------------------
// 引擎降级提示（界面 toast）
// ---------------------------------------------------------------------------

/// 引擎降级提示：说明"为什么这次没用上首选引擎"，由主窗口以 toast 展示。
///
/// 背景：缺 Key / 未填地址 / 熔断冷却等降级过去只写运行日志，界面无任何提示，
/// 主播点了试听只听到另一把声音，会误判为"功能坏了"（2026-11-25 实测反馈）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineNotice {
    /// 发生降级的引擎
    pub engine: TTSEngineType,
    /// 面向主播的中文原因说明
    pub message: String,
}

/// 引擎降级通知器：App 装配期注入（持有 AppHandle 发事件），单测可替换观测
type EngineNoticeFn = Arc<dyn Fn(&EngineNotice) + Send + Sync>;
static ENGINE_NOTICE_NOTIFIER: Mutex<Option<EngineNoticeFn>> = Mutex::new(None);

/// 装配引擎降级通知器（App setup 时调用一次；重复调用以最后一次为准）
pub fn set_engine_notice_notifier(f: EngineNoticeFn) {
    *ENGINE_NOTICE_NOTIFIER.lock().unwrap() = Some(f);
}

/// 投递引擎降级提示；未装配通知器（单测/Lite）时为 no-op
pub(crate) fn fire_engine_notice(notice: &EngineNotice) {
    let f = ENGINE_NOTICE_NOTIFIER.lock().unwrap().clone();
    if let Some(f) = f {
        f(notice);
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
    /// 云端引擎语音（曼波局域网 GPT-SoVITS / 大肥鱼 DeepSeek WAV）：增益 = volume_gain × MANBO_LOUDNESS_GAIN（响度与 SAPI 对齐）
    TtsBytes {
        bytes: Vec<u8>,
        volume: i32,
        /// 随行气泡事件：真正出声的瞬间投递（对齐原工程 CheckinTTSPlay 回调时机）
        notice: Option<BubbleNotice>,
    },
    File { path: PathBuf, volume: i32 },
    Sapi {
        text: String,
        params: SapiParams,
        notice: Option<BubbleNotice>,
    },
}

/// SAPI 播报参数（rate 直传、pitch 走 SSML；volume 为用户线性刻度 0~100，
/// 构建命令时经 [`sapi_volume_from`] 反补偿——与原工程 `SetVolume(speechVolume/2)` 的减半口径不同）
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
        self.play_tts_bytes_with_notice(bytes, volume, None)
    }

    /// 云端 TTS 语音入队，可随行气泡事件（真正出声时投递）
    pub fn play_tts_bytes_with_notice(
        &self,
        bytes: Vec<u8>,
        volume: i32,
        notice: Option<BubbleNotice>,
    ) -> Result<(), String> {
        self.tx
            .send(AudioJob::TtsBytes { bytes, volume, notice })
            .map_err(|e| e.to_string())
    }

    pub fn play_file(&self, path: PathBuf, volume: i32) -> Result<(), String> {
        self.tx
            .send(AudioJob::File { path, volume })
            .map_err(|e| e.to_string())
    }

    pub fn speak_sapi(&self, text: String, params: SapiParams) -> Result<(), String> {
        self.speak_sapi_with_notice(text, params, None)
    }

    /// 本地 SAPI 播报入队，可随行气泡事件（真正出声时投递）
    pub fn speak_sapi_with_notice(
        &self,
        text: String,
        params: SapiParams,
        notice: Option<BubbleNotice>,
    ) -> Result<(), String> {
        self.tx
            .send(AudioJob::Sapi { text, params, notice })
            .map_err(|e| e.to_string())
    }

    /// 真实播放实现（阻塞当前播放线程直至结束或超时）
    fn play_job(job: &AudioJob) {
        // 气泡与出声同帧：播放线程取出该 job 即将播出的一刻投递随行事件。
        // 此前若还有其他音频 job 在串行等待，气泡会等它们播完才出现——这正是
        // 「气泡与 TTS 播放时机相同」的关键（在 speak_task 里 emit 会早于真正的出声）
        match job {
            AudioJob::TtsBytes {
                notice: Some(notice),
                ..
            }
            | AudioJob::Sapi {
                notice: Some(notice),
                ..
            } => fire_play_notice(notice),
            _ => {}
        }
        // 测试环境不产生真实音频输出（避免 cargo test 中途出声干扰）
        #[cfg(test)]
        {
            let _ = match job {
                AudioJob::Bytes { bytes, volume } => bytes.len() + *volume as usize,
                AudioJob::TtsBytes { bytes, volume, .. } => {
                    bytes.len() + *volume as usize
                }
                AudioJob::File { path, volume } => path.as_os_str().len() + *volume as usize,
                AudioJob::Sapi { text, params, .. } => {
                    text.len() + params.rate.unsigned_abs() as usize
                }
            };
        }
        #[cfg(not(test))]
        match job {
            AudioJob::Bytes { bytes, volume } => {
                if let Ok(decoder) = Decoder::new(Cursor::new(bytes.clone())) {
                    play_decoder_sync(decoder, volume_gain(*volume));
                }
            }
            AudioJob::TtsBytes { bytes, volume, .. } => {
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
            AudioJob::Sapi { text, params, .. } => {
                // 本地兜底失败过去完全静默（返回 bool 被忽略），出问题时日志里毫无线索；
                // 这里显式留痕：PowerShell/System.Speech 无法启动、超时或被系统策略拦截都会走到
                if !run_local_speech(text, params) {
                    crate::log_warn!(
                        "[TTS] 本地语音播报失败（SAPI/系统语音组件未正常完成：启动失败、超时或系统策略拦截）"
                    );
                }
            }
        }
    }
}

/// 配置音量换算为 rodio 增益：刻度 0~100 直通（100 = 1.0 满量增益），
/// 与 SAPI 满档（Volume=100）共用同一刻度端点，超界防御性收口
pub fn volume_gain(speech_volume: i32) -> f32 {
    (speech_volume.clamp(0, 100) as f32) / 100.0
}

/// 云端引擎语音响度校准增益：按原云端 Manbo「佩奇猪」实测校准（中位数 0.74），
/// 局域网 GPT-SoVITS 引擎输出响度未单独标定，沿用同一系数避免切换后音量突跳；
/// 大肥鱼（DeepSeek）输出同样未经实测标定，一并沿用（如需精调可对引擎输出重新标定后回填）。
/// 仅应用于云端 TTS 流；本地音效保持各自原始响度，不受此系数影响。
pub const MANBO_LOUDNESS_GAIN: f32 = 0.74;

/// 云端 TTS 流（曼波/大肥鱼）的 rodio 增益 = 用户刻度增益 × 响度校准系数
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

/// 用户音量刻度（线性振幅 0~100）→ SAPI Volume 档位反补偿。
/// System.Speech 的 Volume 曲线为**两段式**（实测：同文本逐档渲染 WAV 测 RMS，2026-09-27）：
/// V=1..9 为线性段（amp ≈ 0.0075·V），V≥10 为对数段（amp = 10^((V-100)/80)，每 4 档约 1 dB、
/// 对数段 V=10~100 实际跨度约 22.5 dB），两段恰在 V=10 相交。分段反解使 SAPI 实际输出与 rodio 数字增益同刻度线性
/// （与 Manbo 校准对齐在任意档位成立，而非仅满档）：
/// 线性段 V = v/0.0075、对数段 V = 100 + 80·log10(v/100)（50 → 76、25 → 52、10 → 20）；
/// 满档 100 与静音 0 不变；v≥1 但反解值 <1 时饱和到 1（避免低段跳变为完全静音）。
pub fn sapi_volume_from(volume: i32) -> i32 {
    if volume <= 0 {
        return 0;
    }
    let v = volume.clamp(0, 100) as f64 / 100.0;
    let v_linear = v / 0.0075;
    let raw = if v_linear < 10.0 {
        v_linear
    } else {
        100.0 + 80.0 * v.log10()
    };
    raw.round().clamp(1.0, 100.0) as i32
}

/// 构建 PowerShell SAPI 播报命令（选择中文音色 + rate/volume/pitch 全参数）
pub fn build_sapi_command(text: &str, params: &SapiParams) -> String {
    let ssml = build_sapi_ssml(text, params.pitch).replace('\'', "''");
    let rate = params.rate.clamp(-10, 10);
    // 音量先经两段式曲线反补偿（详见 sapi_volume_from），使 SAPI 实际输出为线性刻度
    let volume = sapi_volume_from(params.volume);
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
/// - `volume`：刻度 0~100 直通映射为 `say` 内嵌音量命令的 0.0~1.0（100 = 满量 1.0）
pub fn say_params_from(rate: i32, volume: i32) -> (i32, f64) {
    (
        (175 + rate.clamp(-10, 10) * 15).clamp(80, 400),
        (volume.clamp(0, 100) as f64) / 100.0,
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

/// 将播放成功的签到/补签音频留档（对齐原工程 `TTSCacheManager::SaveCheckinAudio`）。
/// 原工程仅对签到 AI 回复音频留档，一般弹幕 TTS 播完即丢（v24 决策）。
/// 扩展名按内容探测：曼波引擎返回 WAV（RIFF 头）存 `.wav`，其余存 `.mp3`。
pub fn save_checkin_audio(username: &str, bytes: &[u8]) -> Option<PathBuf> {
    let safe_name: String = username
        .chars()
        .map(|c| if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    let dir = cache_base_dir().join(chrono::Local::now().format("%Y%m%d").to_string());
    std::fs::create_dir_all(&dir).ok()?;
    let ts = chrono::Local::now().timestamp_millis();
    let ext = if bytes.starts_with(b"RIFF") { "wav" } else { "mp3" };
    let path = dir.join(format!("打卡_{}_{}.{}", safe_name, ts, ext));
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
    /// 随行气泡事件：随任务走完队列与合成，由音频线程在真正出声时投递；
    /// 播报失败/语音关闭等「本次必不出声」的路径由消费方回退为立即投递
    pub bubble: Option<BubbleNotice>,
}

/// 单队列容量上限（防止异常刷屏导致内存膨胀）
pub const MAX_SPEAK_QUEUE: usize = 200;

/// 并发合成上限（对齐原工程 `MAX_CONCURRENT_TTS = 2`）
pub const MAX_CONCURRENT_TTS: usize = 2;

/// 单引擎熔断器（曼波/大肥鱼各一份）。
///
/// 计数与熔断状态**始终在同一把锁内更新**：两个并发播报一成一败时不会留下
/// 「健康但计数未清」或「计数为 0 却仍在熔断」的错位。
struct EngineBreaker {
    health: Mutex<EngineHealth>,
    /// 连续失败次数（成功即清零）：用于指数退避。
    /// 冷却结束后仍保留，若探活再次失败则继续升档；成功时清零并恢复该引擎
    failures: AtomicU32,
}

impl EngineBreaker {
    const fn new() -> Self {
        Self {
            health: Mutex::new(EngineHealth::Healthy),
            failures: AtomicU32::new(0),
        }
    }

    /// 是否可用（冷却期结束视为可探活，允许下一次播报重试）
    fn is_available(&self) -> bool {
        let mut h = self.health.lock().unwrap_or_else(|e| e.into_inner());
        match *h {
            EngineHealth::Healthy => true,
            EngineHealth::Degraded {
                failed_at,
                cooldown_secs,
            } => {
                if failed_at.elapsed() >= Duration::from_secs(cooldown_secs) {
                    // 冷却结束，允许下一次播报探活。失败计数**不**清零：
                    // 若探活仍失败，退避档位继续上升；成功时由 mark_success 清零并立即恢复
                    *h = EngineHealth::Healthy;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// 标记失败：按退避函数升档并返回累计连续失败次数
    fn mark_degraded(&self, backoff_secs: fn(u32) -> u64) -> u32 {
        let mut h = self.health.lock().unwrap_or_else(|e| e.into_inner());
        let failures = self.failures.fetch_add(1, Ordering::SeqCst) + 1;
        *h = EngineHealth::Degraded {
            failed_at: Instant::now(),
            cooldown_secs: backoff_secs(failures),
        };
        failures
    }

    /// 标记成功：清零连续失败计数、立即恢复健康，返回此前的连续失败次数
    fn mark_success(&self) -> u32 {
        let mut h = self.health.lock().unwrap_or_else(|e| e.into_inner());
        let previous_failures = self.failures.swap(0, Ordering::SeqCst);
        *h = EngineHealth::Healthy;
        previous_failures
    }

    /// 显式重置（切换引擎/改配置/换 Key 等主播的修复动作）
    fn reset(&self) {
        let mut h = self.health.lock().unwrap_or_else(|e| e.into_inner());
        self.failures.store(0, Ordering::SeqCst);
        *h = EngineHealth::Healthy;
    }
}

/// 多引擎 TTS 状态机与音频管理器
pub struct TTSManager {
    config: Mutex<TTSConfig>,
    /// 曼波（局域网 GPT-SoVITS）熔断器
    manbo_breaker: EngineBreaker,
    /// 大肥鱼（DeepSeek 云端）熔断器
    feiyu_breaker: EngineBreaker,
    /// 大肥鱼 API Key（与 AI 对话共用 credentials.dat 中的同一把 DeepSeek Key）。
    /// 不进入 TTSConfig / JSON 配置，只在内存持有，凭据加载与配置保存时同步
    feiyu_api_key: Mutex<String>,
    /// 引擎降级提示节流表（键：引擎名）：播报链路同一引擎最多每 5 分钟提示一次
    engine_notice_at: Mutex<HashMap<&'static str, Instant>>,
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
    /// 单跳合成请求超时。
    /// 局域网 GPT-SoVITS FP32 实测短句 0.6s / 中句 1.2s / 长句 2.0s（1080 Ti），
    /// 10s 足够覆盖偶发慢合成；就绪探测在 [`Self::LAN_PROBE_TIMEOUT`] 内单独收紧到 2s。
    pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
    /// 云端引擎（曼波/大肥鱼）退避首档：瞬断后 5s 就重试，避免长时间卡在本地语音
    pub const ENGINE_BACKOFF_BASE_SECS: u64 = 5;
    /// 退避上限：持续故障时最多 30s 探活一次；网络恢复后下一次播报即回到云端引擎
    pub const ENGINE_BACKOFF_MAX_SECS: u64 = 30;

    /// 连续失败 n 次后的冷却秒数：5 → 10 → 20 → 30（封顶）。
    /// 网络抖动通常是短时的，首档要快恢复；持续断网时拉长间隔，避免每条播报都
    /// 白等请求超时。
    pub fn engine_backoff_secs(failures: u32) -> u64 {
        match failures {
            0 | 1 => Self::ENGINE_BACKOFF_BASE_SECS,
            2 => 10,
            3 => 20,
            _ => Self::ENGINE_BACKOFF_MAX_SECS,
        }
    }

    pub fn new(config: TTSConfig) -> Self {
        Self {
            config: Mutex::new(config),
            manbo_breaker: EngineBreaker::new(),
            feiyu_breaker: EngineBreaker::new(),
            feiyu_api_key: Mutex::new(String::new()),
            engine_notice_at: Mutex::new(HashMap::new()),
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
            bubble: None,
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
            bubble: None,
        }, true)
    }

    /// 入队签到播报并随行气泡：气泡不随定稿立即出现，而是等音频线程真正出声
    /// （对齐原工程 CheckinTTSPlay 回调）。返回 false（空文本或队满）表示没有随行成功，
    /// 调用方必须回退为立即广播气泡，不得让回复静默消失。
    pub fn enqueue_checkin_speak_with_notice(
        &self,
        text: &str,
        user_id: &str,
        username: &str,
        notice: BubbleNotice,
    ) -> bool {
        self.enqueue_task(SpeakTask {
            text: text.to_string(),
            user_id: user_id.to_string(),
            is_checkin: true,
            checkin_username: username.to_string(),
            priority: true,
            bubble: Some(notice),
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

    /// 更新 TTS 配置。
    ///
    /// 引擎切换、局域网引擎地址、大肥鱼音色或自动模式引擎开关变化时**重置对应引擎的熔断与退避**：
    /// 这些都是主播的显式修复动作，不该被上一次故障留下的冷却期挡住 ——
    /// 否则改完配置还得等退避窗口才生效。切换引擎会同时清空两条云端链路。
    pub fn update_config(&self, new_cfg: TTSConfig) {
        let (manbo_changed, feiyu_changed) = {
            let mut cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
            let engine_changed = cfg.engine != new_cfg.engine;
            let manbo_changed = engine_changed
                || cfg.lan_engine_base_url != new_cfg.lan_engine_base_url
                || cfg.auto_enable_manbo_tts != new_cfg.auto_enable_manbo_tts;
            let feiyu_changed = engine_changed
                || cfg.feiyu_voice_id != new_cfg.feiyu_voice_id
                || cfg.auto_enable_feiyu_tts != new_cfg.auto_enable_feiyu_tts;
            *cfg = new_cfg;
            (manbo_changed, feiyu_changed)
        };
        if manbo_changed {
            self.reset_manbo_health();
        }
        if feiyu_changed {
            self.reset_feiyu_health();
        }
    }

    /// 清除曼波熔断与退避计数（切换引擎 / 改局域网地址 / 网络恢复后立即重试）
    pub fn reset_manbo_health(&self) {
        self.manbo_breaker.reset();
    }

    /// 清除大肥鱼熔断与退避计数（切换引擎 / 改音色 / 换 Key 后立即重试）
    pub fn reset_feiyu_health(&self) {
        self.feiyu_breaker.reset();
    }

    /// 同步大肥鱼 API Key（与 AI 对话共用 credentials.dat 中的 DeepSeek Key）。
    /// Key 变化视为显式修复动作：立即清空熔断与退避，下一条播报马上重试
    pub fn set_feiyu_api_key(&self, key: String) {
        let changed = {
            let mut k = self
                .feiyu_api_key
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let changed = *k != key;
            *k = key;
            changed
        };
        if changed {
            self.reset_feiyu_health();
        }
    }

    /// 是否已配置大肥鱼 Key（未配置时自动链路跳过该引擎，不视为服务故障）
    pub fn has_feiyu_key(&self) -> bool {
        !self
            .feiyu_api_key
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .trim()
            .is_empty()
    }

    /// 大肥鱼 Key 快照（仅合成请求瞬时使用，不得进入日志）
    fn feiyu_api_key_snapshot(&self) -> String {
        self.feiyu_api_key
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 当前 TTS 配置快照（用于局部字段更新，如保存 Manbo Key）
    pub fn config_snapshot(&self) -> TTSConfig {
        self.config.lock().unwrap().clone()
    }

    /// 判定指定引擎是否可用（包含冷却期结束自动探活逻辑）。
    /// 大肥鱼额外要求已配置 Key：缺 Key 是配置问题而非服务故障，自动链路直接跳过。
    pub fn is_engine_available(&self, engine: TTSEngineType) -> bool {
        match engine {
            // 「自动」不参与降级判定（由 select_active_engine 级联解析为具体引擎）
            TTSEngineType::Auto => true,
            TTSEngineType::Manbo => self.manbo_breaker.is_available(),
            TTSEngineType::Feiyu => self.has_feiyu_key() && self.feiyu_breaker.is_available(),
            // 本地兜底恒常可用：Windows 走 System.Speech（SAPI），macOS 走内置 `say`
            TTSEngineType::Sapi => true,
        }
    }

    /// 标记引擎故障降级（连续失败按指数退避，封顶 30s）
    pub fn mark_engine_degraded(&self, engine: TTSEngineType) {
        match engine {
            // 「自动」无独立健康度，实际降级由其解析出的具体引擎承担
            TTSEngineType::Auto | TTSEngineType::Sapi => {}
            TTSEngineType::Manbo => {
                // 状态与计数在同一把锁内更新：并发失败按到达顺序累计，后者写入更高档位
                let failures = self
                    .manbo_breaker
                    .mark_degraded(Self::engine_backoff_secs);
                crate::log_warn!(
                    "[TTS] Manbo 熔断：连续失败 {} 次，{}s 后自动重试（期间降级下一引擎）",
                    failures,
                    Self::engine_backoff_secs(failures)
                );
            }
            TTSEngineType::Feiyu => {
                let failures = self
                    .feiyu_breaker
                    .mark_degraded(Self::engine_backoff_secs);
                crate::log_warn!(
                    "[TTS] 大肥鱼（DeepSeek）熔断：连续失败 {} 次，{}s 后自动重试（期间降级下一引擎）",
                    failures,
                    Self::engine_backoff_secs(failures)
                );
            }
        }
    }

    /// 标记引擎请求成功：清零连续失败计数并立即恢复健康。
    /// 引擎恢复后的第一次成功播报会走这里，后续播报马上回到该引擎。
    pub fn mark_engine_success(&self, engine: TTSEngineType) {
        match engine {
            TTSEngineType::Auto => {}
            TTSEngineType::Manbo => {
                let previous_failures = self.manbo_breaker.mark_success();
                self.mark_active_engine(TTSEngineType::Manbo);
                if previous_failures > 0 {
                    crate::log_info!(
                        "[TTS] 曼波引擎已恢复（此前连续失败 {} 次），后续播报回到曼波语音",
                        previous_failures
                    );
                }
            }
            TTSEngineType::Feiyu => {
                let previous_failures = self.feiyu_breaker.mark_success();
                self.mark_active_engine(TTSEngineType::Feiyu);
                if previous_failures > 0 {
                    crate::log_info!(
                        "[TTS] 大肥鱼语音已恢复（此前连续失败 {} 次），后续播报回到大肥鱼语音",
                        previous_failures
                    );
                }
            }
            TTSEngineType::Sapi => self.mark_active_engine(TTSEngineType::Sapi),
        }
    }

    /// 引擎级联顺序（首选在前，末位恒为本地 SAPI 兜底）。
    ///
    /// - `Auto`（对齐原工程 TTSProviderFactory 的 AUTO 模式）：曼波（局域网）→ 大肥鱼（云端）→ 本地。
    ///   大肥鱼按字符计费，故排在免费的局域网引擎之后；曼波未配置/不可用时才走付费云端。
    ///   自动模式的引擎开关（`auto_enable_*`）只在这条链路里生效：关掉的引擎整段跳过，
    ///   本地兜底不受开关影响（关光了也只是全走本地语音）。
    /// - 显式引擎：只承诺该引擎，故障时直接落本地兜底，不擅自切换到另一个云端引擎。
    pub fn engine_chain(&self) -> Vec<TTSEngineType> {
        let cfg = self.config.lock().unwrap_or_else(|e| e.into_inner());
        match cfg.engine {
            TTSEngineType::Auto => {
                let mut chain = Vec::new();
                if cfg.auto_enable_manbo_tts {
                    chain.push(TTSEngineType::Manbo);
                }
                if cfg.auto_enable_feiyu_tts {
                    chain.push(TTSEngineType::Feiyu);
                }
                chain.push(TTSEngineType::Sapi);
                chain
            }
            TTSEngineType::Manbo => vec![TTSEngineType::Manbo, TTSEngineType::Sapi],
            TTSEngineType::Feiyu => vec![TTSEngineType::Feiyu, TTSEngineType::Sapi],
            TTSEngineType::Sapi => vec![TTSEngineType::Sapi],
        }
    }

    /// 选择当前最优可用引擎（按 [`Self::engine_chain`] 取首个可用项）
    pub fn select_active_engine(&self) -> TTSEngineType {
        for engine in self.engine_chain() {
            if self.is_engine_available(engine) {
                return engine;
            }
        }
        TTSEngineType::Sapi
    }

    /// 记录本次实际使用的引擎（供「当前引擎」实时显示）
    fn mark_active_engine(&self, engine: TTSEngineType) {
        if let Ok(mut cur) = self.active_engine.lock() {
            *cur = Some(engine);
        }
    }

    /// 当前实际使用的引擎名（对齐原工程 `TTSManager_GetCurrentProviderName`：manbo / sapi，
    /// 大肥鱼为 feiyu）。尚未播报过时返回按配置解析出的引擎名
    pub fn current_engine_name(&self) -> String {
        let tracked = self.active_engine.lock().ok().and_then(|c| *c);
        tracked
            .unwrap_or_else(|| self.select_active_engine())
            .provider_name()
            .to_string()
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

    /// 当前配置的播报音量（0~100），用于非 SAPI 播放路径
    fn current_volume(&self) -> i32 {
        // 锁中毒兜底 50 = 原工程默认 100（旧满量 200 的一半）的等效响度
        self.config.lock().map(|c| c.speech_volume).unwrap_or(50)
    }

    /// 播放云端合成音频；`checkin_username` 非空时按签到音频留档（对齐原工程仅留档签到 TTS）
    pub fn play_audio_bytes(
        &self,
        bytes: &[u8],
        checkin_username: Option<&str>,
        notice: Option<BubbleNotice>,
    ) -> Result<(), String> {
        if let Some(name) = checkin_username {
            if save_checkin_audio(name, bytes).is_none() {
                crate::log_warn!("[TTS] 签到音频留档失败（不影响播放）");
            }
        }
        AudioQueue::global().play_tts_bytes_with_notice(bytes.to_vec(), self.current_volume(), notice)
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

    /// 就绪探测超时：引擎冷启动约 20s~1 分钟，未就绪必须快速失败交给熔断退避，
    /// 不能拖住正常播报（合成跳另有 [`Self::REQUEST_TIMEOUT`] 兜底）。
    pub const LAN_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

    /// 规范化引擎基础地址：去首尾空白与结尾 `/`（拼接探测/合成路径用）
    pub fn normalize_lan_base_url(base: &str) -> String {
        base.trim().trim_end_matches('/').to_string()
    }

    /// 就绪探测地址（GPT-SoVITS API：GET /control，200 = 模型加载完毕）
    pub fn build_lan_control_url(base: &str) -> String {
        format!("{}/control", Self::normalize_lan_base_url(base))
    }

    /// 合成请求地址（POST /，UTF-8 JSON，返回 WAV 二进制流）
    pub fn build_lan_synthesis_url(base: &str) -> String {
        format!("{}/", Self::normalize_lan_base_url(base))
    }

    /// 语速滑杆（-10..10）→ GPT-SoVITS `speed` 因子（1.0 = 常速）。
    /// 每档 ±5%，全档 0.5~1.5 并钳制，避免极端变速导致发音破损。
    pub fn lan_speed_factor(speech_rate: i32) -> f64 {
        (1.0 + f64::from(speech_rate) * 0.05).clamp(0.5, 1.5)
    }

    /// 构建合成请求 JSON 体（2026-11-07 定稿最小契约：仅 `text` + `text_language` 必填，
    /// 工具侧保留 `speed`；参考音频由引擎启动参数 `-dr/-dt/-dl` 固化预设，refer 三字段
    /// 整段省略——裸跑 api.py 不带预设时最小请求会被引擎以 400 拒绝，属预期约束）
    fn build_lan_payload(text: &str, speech_rate: i32) -> serde_json::Value {
        serde_json::json!({
            "text": text,
            "text_language": "zh",
            "speed": Self::lan_speed_factor(speech_rate),
        })
    }

    /// 局域网曼波引擎合成：先探测就绪（2s 快速失败），再 POST 合成返回 WAV 字节。
    /// 任何失败都由调用方统一降级下一引擎并计入熔断退避。
    async fn synthesize_lan(
        client: &reqwest::Client,
        base_url: &str,
        text: &str,
        speech_rate: i32,
    ) -> Result<Vec<u8>, String> {
        let base = Self::normalize_lan_base_url(base_url);
        if base.is_empty() {
            return Err("未配置局域网引擎地址".into());
        }

        // 1. 就绪探测：连接失败/非 200 视为引擎未就绪（停机或冷启动中）
        let control = client
            .get(Self::build_lan_control_url(&base))
            .timeout(Self::LAN_PROBE_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("引擎未就绪（{}）", Self::describe_http_error(&e)))?;
        if !control.status().is_success() {
            return Err(format!("引擎未就绪（HTTP {}）", control.status()));
        }

        // 2. 合成：POST UTF-8 JSON（最小契约，无 refer 字段），响应为 audio/wav 二进制流
        //    （Transfer-Encoding: chunked 由 reqwest 透明解包；响应体即完整 WAV 字节流，
        //    无「先返回 URL 再取文件」二跳）
        let resp = client
            .post(Self::build_lan_synthesis_url(&base))
            .json(&Self::build_lan_payload(text, speech_rate))
            .timeout(Self::REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| Self::describe_http_error(&e))?;
        let status = resp.status();
        if !status.is_success() {
            // 带上响应体前段：引擎侧报错（路径不存在/模型异常）一眼可辨
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "HTTP {}: {}",
                status,
                body.chars().take(200).collect::<String>()
            ));
        }
        // Content-Type 校验（实测契约为 audio/wav）：引擎误返回 JSON/HTML 时给出明确错误；
        // 宽松放行 audio/* 变体（audio/x-wav 等），头缺失时交由下方 RIFF 头校验兜底
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        Self::validate_audio_content_type(content_type)?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("音频读取失败（{}）", Self::describe_http_error(&e)))?;
        // RIFF 头校验：拦截"头声明合规但内容异常"的响应（200 + 空 HTML 片段等）
        if bytes.len() < 12 || &bytes[..4] != b"RIFF" {
            return Err("返回内容不是 WAV 音频（缺少 RIFF 头）".into());
        }
        Ok(bytes.to_vec())
    }

    /// 校验合成响应的 Content-Type（曼波实测契约为 `audio/wav`，GPT-SoVITS api.py 固定
    /// `StreamingResponse(media_type="audio/"+media_type)`；大肥鱼同为 `audio/wav`）。
    /// 只要求 `audio/*` 前缀（兼容 x-wav/wave 等变体）；头缺失视为不确定，交由
    /// 响应体 RIFF 头校验兜底，不在此拒判。
    fn validate_audio_content_type(content_type: Option<&str>) -> Result<(), String> {
        let Some(ct) = content_type else {
            return Ok(());
        };
        let mime = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        if mime.starts_with("audio/") {
            Ok(())
        } else {
            Err(format!("引擎返回的不是音频（Content-Type: {ct}）"))
        }
    }

    // -----------------------------------------------------------------------
    // 大肥鱼（DeepSeek TTS）——2026-11-13 官方开放语音 API
    // -----------------------------------------------------------------------

    /// 合成端点（HTTP，非流式）：`POST https://api.deepseek.com/v1/tools/tts`
    pub const FEIYU_ENDPOINT: &'static str = "https://api.deepseek.com/v1/tools/tts";
    /// 合成超时：官方短句通常 1~3s；30s 覆盖慢响应，超时即降级下一引擎。
    /// 注意官方限制单请求最长 30 分钟音频、请求体 64 KiB、账单字符 10,000。
    pub const FEIYU_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

    /// 规范化音色 ID：官方仅 `stella`（女声，默认）/ `tide`（男声），
    /// 未知值回落默认女声（配置脏数据不阻断播报）
    pub fn normalize_feiyu_voice(voice: &str) -> &'static str {
        match voice.trim().to_ascii_lowercase().as_str() {
            "tide" => "tide",
            _ => TTSConfig::FEIYU_DEFAULT_VOICE,
        }
    }

    /// 构建大肥鱼请求体（官方契约字段）：
    /// - `text` 必填；`format` 固定 `wav`，便于 rodio 解码与签到音频按 RIFF 头留档；
    /// - `voice_id` 仅 stella/tide；`filter_markdown` 用官方默认 true（播报文本无 Markdown）；
    /// - 官方**无语速/音调参数**，语速滑杆与音调对该引擎不生效（仅本地音量增益有效）。
    pub fn build_feiyu_payload(text: &str, voice_id: &str) -> serde_json::Value {
        serde_json::json!({
            "text": text,
            "voice_id": Self::normalize_feiyu_voice(voice_id),
            "format": "wav",
        })
    }

    /// 大肥鱼合成：`POST /v1/tools/tts`（Bearer Key，与 AI 对话共用）→ `audio/wav` 字节流。
    /// 任何失败由调用方统一降级下一引擎并计入熔断退避。
    async fn synthesize_feiyu(
        client: &reqwest::Client,
        api_key: &str,
        text: &str,
        voice_id: &str,
    ) -> Result<Vec<u8>, String> {
        let key = api_key.trim();
        if key.is_empty() {
            return Err("DeepSeek API key is empty".into());
        }
        let resp = client
            .post(Self::FEIYU_ENDPOINT)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", key))
            .json(&Self::build_feiyu_payload(text, voice_id))
            .timeout(Self::FEIYU_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| Self::describe_http_error(&e))?;
        let status = resp.status();
        if !status.is_success() {
            // 官方错误码：400 文本不可读/内容审核未过、429 并发超限、503 服务繁忙、504 合成超时；
            // 带上响应体前段便于区分（不含 Key，可安全入日志）
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "HTTP {}: {}",
                status,
                body.chars().take(200).collect::<String>()
            ));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        Self::validate_audio_content_type(content_type)?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("音频读取失败（{}）", Self::describe_http_error(&e)))?;
        // RIFF 头校验：官方文档明确「合成中途失败会返回截断响应」，必须按失败处理；
        // 同时拦截 Content-Type 合规但内容是 JSON/HTML 的异常响应
        if bytes.len() < 12 || &bytes[..4] != b"RIFF" {
            return Err("返回内容不是 WAV 音频（缺少 RIFF 头）".into());
        }
        Ok(bytes.to_vec())
    }

    /// 进程级共享 HTTP 客户端（对齐原工程 `Network.cpp` 的 `GetSharedSession` 共享
    /// WinHTTP 会话：会话级连接缓存使后续请求复用已建立的 TCP/TLS 连接）。
    /// reqwest Client 内置连接池，逐条播报重建会随消息丢弃池，导致每跳都重新握手。
    /// 客户端默认超时 [`Self::REQUEST_TIMEOUT`]（合成跳）；就绪探测在
    /// [`Self::synthesize_lan`] 内单独收紧到 2s。
    /// 构建失败（仅 TLS 后端初始化异常，几乎不会发生）返回 `None`，调用方降级本地 SAPI。
    fn shared_http_client() -> Option<&'static reqwest::Client> {
        static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
        if let Some(c) = CLIENT.get() {
            return Some(c);
        }
        let built = reqwest::Client::builder()
            .timeout(Self::REQUEST_TIMEOUT)
            .build()
            .ok()?;
        Some(CLIENT.get_or_init(|| built))
    }

    /// 生成可安全写入日志的 HTTP 错误描述。
    ///
    /// **不要用 `reqwest::Error::to_string()`**：发送失败时它会把完整 URL 拼进消息，
    /// 而 Manbo URL 的 `key=` 查询参数就是密钥 —— 日志会直接泄露 Key。
    /// 这里只保留错误类别与底层错误链（底层错误不含 URL）。
    fn describe_http_error(e: &reqwest::Error) -> String {
        let mut parts: Vec<String> = Vec::new();
        if e.is_timeout() {
            parts.push("超时".into());
        }
        if e.is_connect() {
            parts.push("连接失败".into());
        }
        if e.is_body() {
            parts.push("响应体读取失败".into());
        }
        if e.is_decode() {
            parts.push("响应解码失败".into());
        }
        if parts.is_empty() {
            parts.push("网络请求失败".into());
        }
        let mut src = std::error::Error::source(e);
        while let Some(c) = src {
            parts.push(c.to_string());
            src = std::error::Error::source(c);
        }
        parts.join("：")
    }

    /// 播报队列任务入口：按任务类型决定是否留档签到音频。
    /// 随行气泡交由本函数托管：真正出声时由音频线程投递；本次必不出声
    /// （语音开关关闭/空文本）时在此立即回退投递，合成入队失败则由调用方兜底。
    /// 注意：本地特殊音效不在本入口拦截（对齐原工程仅在弹幕文本路径 `HandleSpeekDm` 匹配）。
    pub async fn speak_task(&self, task: &SpeakTask) -> Result<(), String> {
        let checkin = if task.is_checkin && !task.checkin_username.is_empty() {
            Some(task.checkin_username.as_str())
        } else {
            None
        };
        self.speak_text_inner(&task.text, checkin, task.bubble.as_ref())
            .await
    }

    /// 多引擎智能文本语音播报：按 [`Self::engine_chain`] 逐级尝试
    /// （曼波/大肥鱼为云端引擎、各有独立熔断与退避，末位本地 SAPI 兜底）。
    /// 原特殊用户专属引擎已于 2026-11-07 随服务方下架试用端点 `/apis/mbAIsc` 一并移除，
    /// 特殊用户与普通用户走同一引擎链。
    async fn speak_text_inner(
        &self,
        text: &str,
        checkin_username: Option<&str>,
        notice: Option<&BubbleNotice>,
    ) -> Result<(), String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            // 空文本必不出声：随行气泡退回立即投递（正常入队已拒绝空文本，防御性兜底）
            if let Some(n) = notice {
                fire_play_notice(n);
            }
            return Ok(());
        }

        let cfg = self.config.lock().unwrap().clone();
        if !cfg.enable_voice {
            // 语音开关已关闭：本次必不出声，随行气泡退回立即投递（关语音仍出气泡）
            if let Some(n) = notice {
                fire_play_notice(n);
            }
            return Ok(());
        }

        // 共享 HTTP 客户端（构建失败几乎不会发生；此时跳过云端引擎，直接落本地兜底）
        if let Some(client) = Self::shared_http_client() {
            for engine in self.engine_chain() {
                if engine == TTSEngineType::Sapi {
                    break;
                }
                if !self.is_engine_available(engine) {
                    // 自动链路里「未配置」的可选引擎静默跳过（只写日志）；
                    // 已配置却处于故障冷却、或主播显式选择了该引擎时才提示 ——
                    // 否则主播只听到另一把声音却不知道原因（2026-11-25 实测反馈）
                    let configured = match engine {
                        TTSEngineType::Manbo => !cfg.lan_engine_base_url.trim().is_empty(),
                        TTSEngineType::Feiyu => self.has_feiyu_key(),
                        _ => false,
                    };
                    if configured || cfg.engine == engine {
                        let reason = if configured {
                            "故障冷却中"
                        } else if engine == TTSEngineType::Feiyu {
                            "未配置 DeepSeek Key"
                        } else {
                            "未配置引擎地址"
                        };
                        self.notify_engine_fallback(
                            engine,
                            format!(
                                "{}引擎暂不可用（{}），本次播报已降级下一引擎",
                                Self::engine_label(engine),
                                reason
                            ),
                            false,
                        );
                    }
                    continue;
                }
                let attempt = match engine {
                    TTSEngineType::Manbo => {
                        if cfg.lan_engine_base_url.trim().is_empty() {
                            // 缺引擎地址是配置问题，不是服务故障：不去请求，也不熔断。
                            // 仅显式选中曼波时提示界面；自动链路里属「可选引擎未配置」，只写日志
                            crate::log_warn!(
                                "[TTS] 未配置局域网曼波引擎地址：跳过曼波（可在设置中填写引擎地址）"
                            );
                            if cfg.engine == TTSEngineType::Manbo {
                                self.notify_engine_fallback(
                                    TTSEngineType::Manbo,
                                    "未配置局域网曼波引擎地址，本次播报已降级本地语音".to_string(),
                                    false,
                                );
                            }
                            continue;
                        }
                        Self::synthesize_lan(
                            client,
                            &cfg.lan_engine_base_url,
                            trimmed,
                            cfg.speech_rate,
                        )
                        .await
                    }
                    TTSEngineType::Feiyu => {
                        // is_engine_available 已要求 Key 非空，此处仅防御 Key 被并发清空
                        let key = self.feiyu_api_key_snapshot();
                        if key.trim().is_empty() {
                            continue;
                        }
                        Self::synthesize_feiyu(client, &key, trimmed, &cfg.feiyu_voice_id).await
                    }
                    TTSEngineType::Auto | TTSEngineType::Sapi => continue,
                };
                match attempt {
                    Ok(bytes) => {
                        self.mark_engine_success(engine);
                        return self.play_audio_bytes(&bytes, checkin_username, notice.cloned());
                    }
                    Err(e) => {
                        crate::log_warn!(
                            "[TTS] {} 引擎请求失败：{}；降级下一引擎",
                            Self::engine_label(engine),
                            e
                        );
                        self.notify_engine_fallback(
                            engine,
                            format!(
                                "{}引擎请求失败（{}），本次播报已降级下一引擎",
                                Self::engine_label(engine),
                                Self::truncate_notice(&e, 120)
                            ),
                            false,
                        );
                        self.mark_engine_degraded(engine);
                    }
                }
            }
        }

        // 本地离线兜底（Windows SAPI / macOS say），全参数按平台映射
        self.mark_active_engine(TTSEngineType::Sapi);
        let params = SapiParams {
            rate: cfg.speech_rate,
            volume: cfg.speech_volume,
            pitch: cfg.speech_pitch,
        };
        let queued = AudioQueue::global().speak_sapi_with_notice(trimmed.to_string(), params, notice.cloned());
        if queued.is_err() {
            // 播放队列送入失败（接收端已不存在）：本次必不出声，气泡退回立即投递
            if let Some(n) = notice {
                fire_play_notice(n);
            }
        }
        queued
    }

    /// 日志/提示用中文引擎名（UI 展示文案由前端维护，不在此处定义）
    fn engine_label(engine: TTSEngineType) -> &'static str {
        match engine {
            TTSEngineType::Auto => "自动",
            TTSEngineType::Manbo => "曼波",
            TTSEngineType::Feiyu => "大肥鱼",
            TTSEngineType::Sapi => "本地语音",
        }
    }

    /// 播报链路降级提示的最小间隔（同一引擎）：持续故障时不必每条弹幕都弹 toast
    pub const ENGINE_NOTICE_INTERVAL: Duration = Duration::from_secs(300);

    /// 提示文案截断（按字符截断，避免多字节 UTF-8 边界 panic）
    fn truncate_notice(text: &str, max_chars: usize) -> String {
        let mut out: String = text.chars().take(max_chars).collect();
        if text.chars().count() > max_chars {
            out.push('…');
        }
        out
    }

    /// 投递引擎降级提示：`force=true`（试听等显式动作）绕过节流；
    /// 播报链路按引擎节流，避免持续故障时刷屏。
    fn notify_engine_fallback(&self, engine: TTSEngineType, message: String, force: bool) {
        if !force {
            let mut last = self
                .engine_notice_at
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            if last
                .get(engine.provider_name())
                .is_some_and(|t| now.duration_since(*t) < Self::ENGINE_NOTICE_INTERVAL)
            {
                return;
            }
            last.insert(engine.provider_name(), now);
        }
        fire_engine_notice(&EngineNotice { engine, message });
    }

    /// 试听指定参数的测试句（设置面板「试听」按钮）。
    /// 引擎选择与实际播报的首选一致（自动模式取链路中第一个已启用的云端引擎；
    /// 大肥鱼失败同样降级本地 SAPI）；
    /// **试听是显式探活动作**：即使首选引擎正处于退避窗口，也绕过冷却直接试一次 ——
    /// 成功会立即恢复该引擎，失败则按正常规则继续退避。
    /// 音量/语速/音调用传入即时值（绕开配置自动保存防抖），
    /// 不受语音总开关限制（试听语义 = 配置阶段听当前参数效果）。
    /// 注意：大肥鱼（DeepSeek）官方接口无语速/音调参数，这两项对该引擎不生效。
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

        // 试听绕过熔断冷却：按配置偏好直接探活首选引擎。
        // 「自动」取自动链路里第一个**已启用**的云端引擎（曼波→大肥鱼，开关全关时直接本地），
        // 与真实播报的链路一致；网络修好后主播点一次试听即可确认恢复，不必等退避窗口结束。
        let preferred = match cfg.engine {
            TTSEngineType::Auto => self
                .engine_chain()
                .into_iter()
                .find(|e| *e != TTSEngineType::Sapi)
                .unwrap_or(TTSEngineType::Sapi),
            explicit => explicit,
        };
        crate::log_info!(
            "[TTS] 试听播报: 文本=「{}」, 首选引擎={}, 音量={}, 语速={}, 音调={}",
            trimmed,
            preferred.provider_name(),
            volume,
            rate,
            pitch
        );

        if preferred == TTSEngineType::Manbo {
            if cfg.lan_engine_base_url.trim().is_empty() {
                crate::log_warn!("[TTS] 试听：未配置局域网曼波引擎地址，降级本地 SAPI");
                self.notify_engine_fallback(
                    TTSEngineType::Manbo,
                    "未配置局域网曼波引擎地址，试听已降级本地语音".to_string(),
                    true,
                );
            } else if let Some(client) = Self::shared_http_client() {
                // 试听语速按即时滑杆值生效；音量在本地播放端应用，音调对局域网引擎无效
                match Self::synthesize_lan(
                    client,
                    &cfg.lan_engine_base_url,
                    trimmed,
                    rate,
                )
                .await
                {
                    Ok(bytes) => {
                        self.mark_engine_success(TTSEngineType::Manbo);
                        crate::log_info!(
                            "[TTS] 试听走曼波引擎（音量 {} 由本地播放端增益应用）",
                            volume
                        );
                        return AudioQueue::global().play_tts_bytes(bytes, volume);
                    }
                    Err(e) => {
                        self.mark_engine_degraded(TTSEngineType::Manbo);
                        crate::log_warn!("[TTS] 试听曼波引擎请求失败({}),降级本地 SAPI", e);
                        self.notify_engine_fallback(
                            TTSEngineType::Manbo,
                            format!(
                                "曼波引擎请求失败（{}），试听已降级本地语音",
                                Self::truncate_notice(&e, 120)
                            ),
                            true,
                        );
                    }
                }
            } else {
                crate::log_warn!("[TTS] 共享 HTTP 客户端不可用，试听降级本地 SAPI");
                self.notify_engine_fallback(
                    TTSEngineType::Manbo,
                    "网络客户端初始化失败，试听已降级本地语音".to_string(),
                    true,
                );
            }
        }

        if preferred == TTSEngineType::Feiyu {
            let key = self.feiyu_api_key_snapshot();
            if key.trim().is_empty() {
                crate::log_warn!(
                    "[TTS] 试听：未配置 DeepSeek Key（大肥鱼与 AI 对话共用凭据），降级本地 SAPI"
                );
                self.notify_engine_fallback(
                    TTSEngineType::Feiyu,
                    "未配置 DeepSeek Key（大肥鱼与打卡 AI 共用凭据），试听已降级本地语音"
                        .to_string(),
                    true,
                );
            } else if let Some(client) = Self::shared_http_client() {
                // 官方接口无语速/音调参数；音量在本地播放端应用
                match Self::synthesize_feiyu(client, &key, trimmed, &cfg.feiyu_voice_id).await {
                    Ok(bytes) => {
                        self.mark_engine_success(TTSEngineType::Feiyu);
                        crate::log_info!(
                            "[TTS] 试听走大肥鱼引擎（音色 {}，音量 {} 由本地播放端增益应用；官方接口无语速参数）",
                            Self::normalize_feiyu_voice(&cfg.feiyu_voice_id),
                            volume
                        );
                        return AudioQueue::global().play_tts_bytes(bytes, volume);
                    }
                    Err(e) => {
                        self.mark_engine_degraded(TTSEngineType::Feiyu);
                        crate::log_warn!("[TTS] 试听大肥鱼引擎请求失败({}),降级本地 SAPI", e);
                        self.notify_engine_fallback(
                            TTSEngineType::Feiyu,
                            format!(
                                "大肥鱼引擎请求失败（{}），试听已降级本地语音",
                                Self::truncate_notice(&e, 120)
                            ),
                            true,
                        );
                    }
                }
            } else {
                crate::log_warn!("[TTS] 共享 HTTP 客户端不可用，试听降级本地 SAPI");
                self.notify_engine_fallback(
                    TTSEngineType::Feiyu,
                    "网络客户端初始化失败，试听已降级本地语音".to_string(),
                    true,
                );
            }
        }

        self.mark_active_engine(TTSEngineType::Sapi);
        crate::log_info!(
            "[TTS] 试听走本地 SAPI: volume={}, rate={}, pitch={}（SAPI 档位 {}，经音量曲线反补偿）",
            volume,
            rate,
            pitch,
            sapi_volume_from(volume)
        );
        AudioQueue::global().speak_sapi(
            trimmed.to_string(),
            SapiParams { rate, volume, pitch },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_auto_engine_cascade_and_current_engine_name() {
        // D1：引擎「自动」对齐原工程 TTSProviderFactory 的 AUTO 模式，
        // 链路为 曼波（局域网）→ 大肥鱼（云端）→ 本地 SAPI：
        // 大肥鱼按字符计费，故排在免费局域网引擎之后，仅作云端备选
        let mgr = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Auto,
            ..Default::default()
        });
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Manbo);
        assert_eq!(mgr.current_engine_name(), "manbo");

        // 未配置大肥鱼 Key：曼波熔断后直接落本地 SAPI（缺 Key 是配置问题，不算服务故障）
        mgr.mark_engine_degraded(TTSEngineType::Manbo);
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Sapi);
        assert_eq!(mgr.current_engine_name(), "sapi");

        // 配好 Key 后：曼波熔断 → 大肥鱼接棒（set_feiyu_api_key 只清大肥鱼熔断）
        mgr.set_feiyu_api_key("sk-test".into());
        assert!(mgr.is_engine_available(TTSEngineType::Feiyu));
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Feiyu);
        mgr.mark_engine_success(TTSEngineType::Feiyu);
        assert_eq!(mgr.current_engine_name(), "feiyu");

        // 两条云端链路都熔断 → 本地兜底
        mgr.mark_engine_degraded(TTSEngineType::Feiyu);
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Sapi);

        // 显式指定引擎时以其为准；故障时只落本地，不擅自切到另一个云端引擎
        let explicit = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Feiyu,
            ..Default::default()
        });
        assert_eq!(
            explicit.engine_chain(),
            vec![TTSEngineType::Feiyu, TTSEngineType::Sapi]
        );
        // 未配置 Key：显式选择也不可用，直接本地
        assert!(!explicit.is_engine_available(TTSEngineType::Feiyu));
        assert_eq!(explicit.select_active_engine(), TTSEngineType::Sapi);

        let explicit_sapi = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Sapi,
            ..Default::default()
        });
        assert_eq!(
            explicit_sapi.engine_chain(),
            vec![TTSEngineType::Sapi],
            "显式选本地语音时无需任何云端尝试"
        );
        assert!(explicit_sapi.is_engine_available(TTSEngineType::Auto));
        explicit_sapi.mark_engine_degraded(TTSEngineType::Auto);

        // 引擎名映射（对齐原工程 TTSManager_GetCurrentProviderName 返回值）
        assert_eq!(TTSEngineType::Manbo.provider_name(), "manbo");
        assert_eq!(TTSEngineType::Feiyu.provider_name(), "feiyu");
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
        // 刻度 0~100 直通：100 = 满量（1.0 增益）、0 = 静音
        assert_eq!(volume_gain(100), 1.0);
        assert_eq!(volume_gain(50), 0.5);
        assert_eq!(volume_gain(0), 0.0);
        // 越界防御性收口到 0.0~1.0
        assert_eq!(volume_gain(-10), 0.0);
        assert_eq!(volume_gain(150), 1.0);
        assert_eq!(volume_gain(999), 1.0);
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
    fn test_shared_http_client_is_process_wide_singleton() {
        // 对齐原工程 GetSharedSession 共享 WinHTTP 会话：进程级单例，
        // 跨播报复用同一连接池（TCP/TLS 握手只发生一次，不再随消息丢弃）
        let a = TTSManager::shared_http_client().expect("共享 HTTP 客户端应构建成功");
        let b = TTSManager::shared_http_client().expect("共享 HTTP 客户端应构建成功");
        assert!(std::ptr::eq(a, b), "两次获取应命中同一实例");
        println!("[PASS] test_shared_http_client_is_process_wide_singleton passed");
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

    /// 通知器是进程级全局：涉及播放通知的测试共用一把锁串行执行，避免互相覆盖
    static PLAY_NOTICE_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 引擎降级通知器同样是进程级全局：单独一把锁串行执行
    static ENGINE_NOTICE_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 引擎降级提示：缺 Key 时必须给出面向主播的原因（试听强制提示），
    /// 播报链路按引擎节流（同一引擎 5 分钟内只提示一次）。
    #[test]
    fn test_engine_fallback_notice_and_throttle() {
        let _guard = ENGINE_NOTICE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let fired = Arc::new(Mutex::new(Vec::<EngineNotice>::new()));
        {
            let fired = fired.clone();
            set_engine_notice_notifier(Arc::new(move |n: &EngineNotice| {
                fired.lock().unwrap().push(n.clone());
            }));
        }

        let rt = tokio::runtime::Runtime::new().expect("创建 tokio 运行时失败");

        // 1) 显式选大肥鱼但缺 Key：试听必须提示原因（force，绕过节流）
        let feiyu = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Feiyu,
            ..Default::default()
        });
        rt.block_on(async {
            feiyu.speak_test_sample("试听", 50, 0, 0).await.unwrap();
            feiyu.speak_test_sample("试听", 50, 0, 0).await.unwrap();
        });
        {
            let notices = fired.lock().unwrap();
            assert_eq!(notices.len(), 2, "试听为显式动作，每次都应提示: {:?}", *notices);
            assert_eq!(notices[0].engine, TTSEngineType::Feiyu);
            assert!(
                notices[0].message.contains("DeepSeek Key"),
                "提示应说明缺 Key: {}",
                notices[0].message
            );
        }

        // 2) 播报链路（显式曼波、地址为空）：首次提示、第二次被节流
        let manbo = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Manbo,
            ..Default::default()
        });
        let task = SpeakTask {
            text: "测试播报".into(),
            user_id: "u1".into(),
            is_checkin: false,
            checkin_username: String::new(),
            priority: false,
            bubble: None,
        };
        rt.block_on(async {
            manbo.speak_task(&task).await.unwrap();
            manbo.speak_task(&task).await.unwrap();
        });
        {
            let notices = fired.lock().unwrap();
            let manbo_notices: Vec<_> = notices
                .iter()
                .filter(|n| n.engine == TTSEngineType::Manbo)
                .collect();
            assert_eq!(
                manbo_notices.len(),
                1,
                "同一引擎播报提示必须节流: {:?}",
                *notices
            );
            assert!(
                manbo_notices[0].message.contains("曼波"),
                "提示应说明曼波地址缺失: {}",
                manbo_notices[0].message
            );
        }

        // 3) 自动链路里「未配置的可选引擎」只写日志，不打扰界面
        let auto = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Auto,
            ..Default::default()
        });
        let before = fired.lock().unwrap().len();
        rt.block_on(async {
            auto.speak_task(&task).await.unwrap();
        });
        assert_eq!(
            fired.lock().unwrap().len(),
            before,
            "自动链路缺配置不应弹提示"
        );

        // 4) 提示文案按字符截断（多字节安全）
        let long = "异".repeat(200);
        let cut = TTSManager::truncate_notice(&long, 120);
        assert_eq!(cut.chars().count(), 121, "120 字符 + 省略号");
        assert!(cut.ends_with('…'));
        assert_eq!(TTSManager::truncate_notice("短文本", 120), "短文本");

        println!("[PASS] test_engine_fallback_notice_and_throttle passed");
    }


    #[test]
    fn test_checkin_speak_with_notice_attaches_bubble() {
        let mgr = TTSManager::new(TTSConfig::default());

        // 随行气泡：入队成功即随任务走完队列，真正出声时才由音频线程投递
        let notice = BubbleNotice::checkin_reply(serde_json::json!({
            "user_id": "u1",
            "user_name": "舰长A",
            "reply": "回复文本",
            "is_ai": true,
        }));
        assert!(mgr.enqueue_checkin_speak_with_notice("打卡回复", "u1", "舰长A", notice));

        let task = mgr.dequeue_speak().expect("随行气泡任务应入队");
        assert!(task.is_checkin && task.priority);
        let bubble = task.bubble.as_ref().expect("随行气泡必须随任务保留");
        assert_eq!(bubble.event, "checkin-reply");
        assert_eq!(bubble.payload["user_name"], "舰长A");
        assert_eq!(bubble.payload["is_ai"], true);

        // 回滚入队（并发名额不足重排）不得丢气泡
        mgr.requeue_speak(task);
        assert!(mgr.dequeue_speak().unwrap().bubble.is_some());

        // 普通签到播报（无气泡）：补签等旧路径行为不变
        assert!(mgr.enqueue_checkin_speak("补签回复", "u2", "舰长B"));
        assert!(mgr.dequeue_speak().unwrap().bubble.is_none());

        // 队满：入队失败返回 false，由业务层回退立即广播气泡
        //（随行气泡走高优先队列，须填满的是优先队列）
        for i in 0..MAX_SPEAK_QUEUE {
            assert!(mgr.enqueue_speak(&format!("t{}", i), "u", true));
        }
        assert!(!mgr.enqueue_checkin_speak_with_notice(
            "溢出",
            "u",
            "舰长",
            BubbleNotice::checkin_reply(serde_json::json!({})),
        ));
        println!("[PASS] test_checkin_speak_with_notice_attaches_bubble passed");
    }

    #[test]
    fn test_play_job_fires_notice_at_play_start() {
        let _guard = PLAY_NOTICE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let fired = Arc::new(Mutex::new(Vec::<String>::new()));
        {
            let fired = fired.clone();
            set_play_notice_notifier(Arc::new(move |n: &BubbleNotice| {
                fired.lock().unwrap().push(n.event.to_string());
            }));
        }

        // 云端语音 job：播放线程取出并开始播出的一刻投递（测试构建不产生真实声音）
        AudioQueue::play_job(&AudioJob::TtsBytes {
            bytes: vec![0u8; 8],
            volume: 50,
            notice: Some(BubbleNotice::checkin_reply(serde_json::json!({"k": 1}))),
        });
        // 本地 SAPI job：同样在出声前投递
        AudioQueue::play_job(&AudioJob::Sapi {
            text: "测试".into(),
            params: SapiParams { rate: 0, volume: 50, pitch: 0 },
            notice: Some(BubbleNotice::checkin_reply(serde_json::json!({"k": 2}))),
        });
        // 无随行气泡的 job（本地音效）不得误触
        AudioQueue::play_job(&AudioJob::Bytes { bytes: vec![1u8], volume: 50 });

        let events = fired.lock().unwrap().clone();
        assert_eq!(
            events,
            vec!["checkin-reply", "checkin-reply"],
            "有随行气泡的 job 各投递一次、无气泡不触发: {:?}",
            events
        );
        println!("[PASS] test_play_job_fires_notice_at_play_start passed");
    }

    #[test]
    fn test_speak_task_fires_notice_when_voice_disabled() {
        let _guard = PLAY_NOTICE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let fired = Arc::new(Mutex::new(Vec::<BubbleNotice>::new()));
        {
            let fired = fired.clone();
            set_play_notice_notifier(Arc::new(move |n: &BubbleNotice| {
                fired.lock().unwrap().push(n.clone());
            }));
        }

        // 语音关闭：本次必不出声，随行气泡在播报入口立即回退投递（关语音仍出气泡）
        let mut cfg = TTSConfig::default();
        cfg.enable_voice = false;
        let mgr = TTSManager::new(cfg);
        assert!(mgr.enqueue_checkin_speak_with_notice(
            "打卡回复",
            "u1",
            "舰长A",
            BubbleNotice::checkin_reply(serde_json::json!({"reply": "打卡回复"})),
        ));
        let task = mgr.dequeue_speak().expect("任务应入队");

        let rt = tokio::runtime::Runtime::new().expect("创建 tokio 运行时失败");
        rt.block_on(async {
            assert!(mgr.speak_task(&task).await.is_ok(), "关语音按无事发生处理");
        });

        let events = fired.lock().unwrap();
        assert_eq!(events.len(), 1, "关语音时随行气泡必须回退立即投递: {:?}", *events);
        assert_eq!(events[0].payload["reply"], "打卡回复");
        println!("[PASS] test_speak_task_fires_notice_when_voice_disabled passed");
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

        // 命令包含中文音色选择、rate 直传、volume 经反补偿（100 恒等满档）
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

        // rate 越界钳制到 SAPI 允许范围；volume 越界钳到满档 100（反补偿下 100 恒等满档）
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

        // 音量经对数曲线反补偿：用户 50 → SAPI 档位 76（实际输出回到线性 0.5，与 Manbo 对齐）
        let cmd_mid = build_sapi_command(
            "x",
            &SapiParams {
                rate: 0,
                volume: 50,
                pitch: 0,
            },
        );
        assert!(cmd_mid.contains("$s.Volume=76"), "cmd_mid: {}", cmd_mid);

        // rodio 增益换算：100 = 1.0 满量、0 = 静音、越界防御性收口
        assert_eq!(volume_gain(100), 1.0);
        assert_eq!(volume_gain(200), 1.0);
        assert_eq!(volume_gain(0), 0.0);
        assert_eq!(volume_gain(50), 0.5);
        assert_eq!(volume_gain(300), 1.0);
        assert_eq!(volume_gain(-1), 0.0);
        println!("[PASS] test_sapi_ssml_and_command passed");
    }

    /// macOS `say` 兜底路径的参数映射（跨平台可测：不依赖 macOS 运行时）
    #[test]
    fn test_say_args_mapping() {
        // 默认档位 rate=0 → 系统基准 175 wpm；刻度 100 = 满量 → volm 1.0
        assert_eq!(say_params_from(0, 100), (175, 1.0));
        // 正负档位线性折算；刻度 50 = 半量 → 0.5
        assert_eq!(say_params_from(2, 50), (205, 0.5));
        assert_eq!(say_params_from(-3, 0), (130, 0.0));
        // 越界钳制：rate 档位先钳到 -10~10（对应 25~325 wpm），再过 80~400 安全区间；volume 钳到 0~100 → 0~1
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

    /// SAPI Volume 档位反补偿：实测 System.Speech 曲线两段式——
    /// V=1..9 线性段 amp≈0.0075·V、V≥10 对数段 10^((V-100)/80)（每 4 档约 1 dB），
    /// 两段在 V=10 相交。分段反解使实际输出线性化，与 rodio 数字增益（Manbo/音效）同刻度，
    /// 任意档位保持响度对齐
    #[test]
    fn test_sapi_volume_compensation() {
        // 对数段锚点：满档不变、50 → 76、25 → 52、10 → 20
        assert_eq!(sapi_volume_from(100), 100);
        assert_eq!(sapi_volume_from(75), 90);
        assert_eq!(sapi_volume_from(50), 76);
        assert_eq!(sapi_volume_from(25), 52);
        assert_eq!(sapi_volume_from(10), 20);
        // 线性段锚点（V=1..9 反解 V=v/0.0075）；v=8 起线性反解越过 V=10 交点，回落对数段
        assert_eq!(sapi_volume_from(1), 1);
        assert_eq!(sapi_volume_from(2), 3);
        assert_eq!(sapi_volume_from(3), 4);
        assert_eq!(sapi_volume_from(4), 5);
        assert_eq!(sapi_volume_from(5), 7);
        assert_eq!(sapi_volume_from(6), 8);
        assert_eq!(sapi_volume_from(7), 9);
        assert_eq!(sapi_volume_from(8), 12);
        assert_eq!(sapi_volume_from(9), 16);
        // 静音与越界防御
        assert_eq!(sapi_volume_from(0), 0);
        assert_eq!(sapi_volume_from(-5), 0);
        assert_eq!(sapi_volume_from(150), 100);
        // 补偿闭环：实测分段前向模型（与探针一致）下，输出对齐线性目标。
        // 对数段（v≥10）误差 ≤ 半档（0.125 dB）；线性段（v=2..9）受档位量化台阶限制
        // 误差 ≤ 1.1 dB；v=1 的最近档即档位 1，为 2.5 dB 量化极限（仅保证非静音）
        let amp_at = |v_sapi: f64| -> f64 {
            if v_sapi < 10.0 {
                0.0075 * v_sapi
            } else {
                10f64.powf((v_sapi - 100.0) / 80.0)
            }
        };
        for (v, max_err_db) in [(1, 2.6), (2, 1.1), (3, 1.1), (4, 1.1), (5, 1.1), (6, 1.1), (7, 1.1), (8, 1.1), (9, 1.1)] {
            let v_sapi = sapi_volume_from(v) as f64;
            let err_db = 20.0 * (amp_at(v_sapi) / (v as f64 / 100.0)).abs().log10();
            assert!(
                err_db <= max_err_db,
                "v={} → 档位 {} 误差 {:.2} dB（阈值 {}）",
                v,
                v_sapi,
                err_db,
                max_err_db
            );
        }
        for v in [10, 20, 33, 50, 66, 80, 95, 100] {
            let v_sapi = sapi_volume_from(v) as f64;
            let actual = 10f64.powf((v_sapi - 100.0) / 80.0);
            let target = v as f64 / 100.0;
            let err_db = 20.0 * (actual / target).abs().log10();
            assert!(
                err_db <= 0.125,
                "v={} → 档位 {} 实际输出 {} 目标 {} 误差 {:.3} dB",
                v,
                v_sapi,
                actual,
                target,
                err_db
            );
        }
        println!("[PASS] test_sapi_volume_compensation passed");
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

    /// 局域网引擎地址构建与请求体：地址规范化、探测/合成 URL、payload 字段与语速因子
    #[test]
    fn test_lan_engine_urls_and_payload() {
        assert_eq!(
            TTSManager::build_lan_control_url(" http://192.168.1.50:9880/ "),
            "http://192.168.1.50:9880/control"
        );
        assert_eq!(
            TTSManager::build_lan_synthesis_url("http://192.168.1.50:9880"),
            "http://192.168.1.50:9880/"
        );
        assert_eq!(
            TTSManager::build_lan_synthesis_url("http://192.168.1.50:9880/"),
            "http://192.168.1.50:9880/"
        );

        let payload = TTSManager::build_lan_payload("你好", 0);
        assert_eq!(payload["text"], "你好");
        assert_eq!(payload["text_language"], "zh");
        assert_eq!(payload["speed"], 1.0);
        // 2026-11-07 定稿最小契约：参考音频引擎侧固化，refer 三字段不得出现在请求体
        assert!(
            payload.get("refer_wav_path").is_none()
                && payload.get("prompt_text").is_none()
                && payload.get("prompt_language").is_none(),
            "请求体不得携带 refer 字段: {}",
            payload
        );

        // 语速滑杆 -10..10 → 0.5..1.5（每档 ±5%，越界钳制）
        assert_eq!(TTSManager::lan_speed_factor(0), 1.0);
        assert_eq!(TTSManager::lan_speed_factor(10), 1.5);
        assert_eq!(TTSManager::lan_speed_factor(-10), 0.5);
        assert_eq!(TTSManager::lan_speed_factor(99), 1.5, "越界钳制");
        println!("[PASS] test_lan_engine_urls_and_payload passed");
    }

    /// 合成响应 Content-Type 校验：实测契约为 audio/wav（api.py 固定 media_type）。
    /// audio/* 变体放行、非音频给出明确错误、头缺失不拒判（交 RIFF 头兜底）。
    #[test]
    fn test_audio_content_type_validation() {
        assert!(TTSManager::validate_audio_content_type(Some("audio/wav")).is_ok());
        assert!(TTSManager::validate_audio_content_type(Some("audio/x-wav")).is_ok());
        assert!(TTSManager::validate_audio_content_type(Some("Audio/WAV; charset=binary")).is_ok());
        assert!(TTSManager::validate_audio_content_type(None).is_ok(), "头缺失交 RIFF 兜底");

        let err = TTSManager::validate_audio_content_type(Some("application/json")).unwrap_err();
        assert!(err.contains("application/json"), "错误需带实际 Content-Type: {}", err);
        assert!(TTSManager::validate_audio_content_type(Some("text/html; charset=utf-8")).is_err());
        println!("[PASS] test_audio_content_type_validation passed");
    }

    /// 大肥鱼（DeepSeek TTS）请求契约：端点、请求体字段、音色规范化与超时预算。
    /// 官方契约（2026-11-13 开放语音 API）：`POST /v1/tools/tts`，Bearer Key 与 AI 对话共用，
    /// 请求体仅 text/voice_id/format，响应为 audio/wav 字节流；**无语速/音调参数**。
    #[test]
    fn test_feiyu_endpoint_payload_and_voice() {
        assert_eq!(
            TTSManager::FEIYU_ENDPOINT,
            "https://api.deepseek.com/v1/tools/tts"
        );

        let payload = TTSManager::build_feiyu_payload("你好", "tide");
        assert_eq!(payload["text"], "你好");
        assert_eq!(payload["voice_id"], "tide");
        assert_eq!(payload["format"], "wav", "固定 WAV 便于 rodio 解码与签到留档");
        // 官方接口无语速/音调参数：请求体不得出现（避免依赖未定义字段）
        assert!(payload.get("speed").is_none(), "payload: {}", payload);

        // 音色仅两种（大小写/空白宽松），脏值回落默认女声 stella
        assert_eq!(TTSManager::normalize_feiyu_voice("stella"), "stella");
        assert_eq!(TTSManager::normalize_feiyu_voice(" TIDE "), "tide");
        assert_eq!(TTSManager::normalize_feiyu_voice(""), "stella");
        assert_eq!(TTSManager::normalize_feiyu_voice("unknown"), "stella");
        assert_eq!(TTSConfig::default().feiyu_voice_id, "stella");
        assert_eq!(TTSManager::build_feiyu_payload("x", "")["voice_id"], "stella");

        // 超时预算：云端短句合成不得低于 10s、也不应无限等待
        assert!(TTSManager::FEIYU_REQUEST_TIMEOUT >= Duration::from_secs(10));
        assert!(TTSManager::FEIYU_REQUEST_TIMEOUT <= Duration::from_secs(60));
        println!("[PASS] test_feiyu_endpoint_payload_and_voice passed");
    }

    /// 自动模式引擎开关：关闭后自动链路跳过对应引擎（本地兜底恒在）；
    /// 显式选择引擎时开关不生效。
    #[test]
    fn test_auto_chain_respects_engine_switches() {
        let mgr = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Auto,
            ..Default::default()
        });
        assert_eq!(
            mgr.engine_chain(),
            vec![
                TTSEngineType::Manbo,
                TTSEngineType::Feiyu,
                TTSEngineType::Sapi
            ],
            "默认两个开关都开"
        );

        // 关闭曼波 + 大肥鱼有 Key → 自动链路直接走大肥鱼
        mgr.set_feiyu_api_key("sk-test".into());
        mgr.mark_engine_degraded(TTSEngineType::Manbo);
        assert!(!mgr.is_engine_available(TTSEngineType::Manbo));
        let mut cfg = mgr.config_snapshot();
        cfg.auto_enable_manbo_tts = false;
        mgr.update_config(cfg);
        assert!(
            mgr.is_engine_available(TTSEngineType::Manbo),
            "拨动自动开关属显式修复动作，应清对应引擎的熔断"
        );
        assert_eq!(
            mgr.engine_chain(),
            vec![TTSEngineType::Feiyu, TTSEngineType::Sapi]
        );
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Feiyu);

        // 恢复曼波、关闭大肥鱼 → 曼波 → 本地（不再产生云端计费）
        let mut cfg = mgr.config_snapshot();
        cfg.auto_enable_manbo_tts = true;
        cfg.auto_enable_feiyu_tts = false;
        mgr.update_config(cfg);
        assert_eq!(
            mgr.engine_chain(),
            vec![TTSEngineType::Manbo, TTSEngineType::Sapi]
        );

        // 两个都关 → 只剩本地兜底
        let mut cfg = mgr.config_snapshot();
        cfg.auto_enable_manbo_tts = false;
        mgr.update_config(cfg);
        assert_eq!(mgr.engine_chain(), vec![TTSEngineType::Sapi]);
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Sapi);

        // 显式选择引擎不受自动开关约束（开关只描述「自动」链路）
        let explicit = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Manbo,
            auto_enable_manbo_tts: false,
            auto_enable_feiyu_tts: false,
            ..Default::default()
        });
        assert_eq!(
            explicit.engine_chain(),
            vec![TTSEngineType::Manbo, TTSEngineType::Sapi]
        );
        assert_eq!(explicit.select_active_engine(), TTSEngineType::Manbo);
        println!("[PASS] test_auto_chain_respects_engine_switches passed");
    }

    /// 大肥鱼 Key 同步与熔断：缺 Key 时该引擎退出链路（不算故障），
    /// 换 Key 立即清熔断，成功清零退避。
    #[test]
    fn test_feiyu_key_sync_breaker_and_backoff() {
        let mgr = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Feiyu,
            ..Default::default()
        });
        assert!(!mgr.has_feiyu_key());
        assert!(!mgr.is_engine_available(TTSEngineType::Feiyu), "缺 Key 视为不可用");
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Sapi);

        mgr.set_feiyu_api_key(" sk-test ".into());
        assert!(mgr.has_feiyu_key());
        assert!(mgr.is_engine_available(TTSEngineType::Feiyu));
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Feiyu);

        // 连续失败计数与统一退避档位（5 → 10 → 20 → 30 封顶）
        mgr.mark_engine_degraded(TTSEngineType::Feiyu);
        assert_eq!(mgr.feiyu_breaker.failures.load(Ordering::SeqCst), 1);
        assert!(!mgr.is_engine_available(TTSEngineType::Feiyu));
        mgr.mark_engine_degraded(TTSEngineType::Feiyu);
        assert_eq!(mgr.feiyu_breaker.failures.load(Ordering::SeqCst), 2);
        {
            let h = mgr.feiyu_breaker.health.lock().unwrap();
            match *h {
                EngineHealth::Degraded { cooldown_secs, .. } => {
                    assert_eq!(cooldown_secs, 10, "第二次失败应进入 10s 档")
                }
                _ => panic!("连续失败后应处于熔断状态"),
            }
        }

        // 换 Key（凭据重新导入）立即清熔断与退避
        mgr.set_feiyu_api_key("sk-new".into());
        assert_eq!(mgr.feiyu_breaker.failures.load(Ordering::SeqCst), 0);
        assert!(mgr.is_engine_available(TTSEngineType::Feiyu));

        // 一次成功清零退避
        mgr.mark_engine_degraded(TTSEngineType::Feiyu);
        mgr.mark_engine_success(TTSEngineType::Feiyu);
        assert_eq!(mgr.feiyu_breaker.failures.load(Ordering::SeqCst), 0);
        assert_eq!(mgr.current_engine_name(), "feiyu");

        // Key 被撤回（凭据清空）：显式选择同样退回本地，不留失效云端请求
        mgr.set_feiyu_api_key(String::new());
        assert!(!mgr.is_engine_available(TTSEngineType::Feiyu));
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Sapi);
        println!("[PASS] test_feiyu_key_sync_breaker_and_backoff passed");
    }

    /// 切换引擎 / 大肥鱼音色必须重置大肥鱼熔断与退避；无关配置变更不得重置
    #[test]
    fn test_update_config_resets_feiyu_health_on_engine_or_voice_change() {
        let mgr = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Feiyu,
            ..Default::default()
        });
        mgr.set_feiyu_api_key("sk-test".into());
        mgr.mark_engine_degraded(TTSEngineType::Feiyu);
        assert!(!mgr.is_engine_available(TTSEngineType::Feiyu));

        // 音量变化与大肥鱼链路无关：熔断保持
        let mut cfg = mgr.config_snapshot();
        cfg.speech_volume = 55;
        mgr.update_config(cfg);
        assert!(
            !mgr.is_engine_available(TTSEngineType::Feiyu),
            "无关配置变更不应清熔断"
        );

        // 改音色 → 立即恢复重试
        let mut cfg = mgr.config_snapshot();
        cfg.feiyu_voice_id = "tide".into();
        mgr.update_config(cfg);
        assert!(mgr.is_engine_available(TTSEngineType::Feiyu), "改音色应清熔断");

        // 再熔断后切换播报引擎 → 同样立即恢复，并清零退避计数
        mgr.mark_engine_degraded(TTSEngineType::Feiyu);
        assert_eq!(mgr.feiyu_breaker.failures.load(Ordering::SeqCst), 1);
        let mut cfg = mgr.config_snapshot();
        cfg.engine = TTSEngineType::Auto;
        mgr.update_config(cfg);
        assert!(
            mgr.is_engine_available(TTSEngineType::Feiyu),
            "切换播报引擎应清熔断"
        );
        assert_eq!(mgr.feiyu_breaker.failures.load(Ordering::SeqCst), 0);
        println!("[PASS] test_update_config_resets_feiyu_health_on_engine_or_voice_change passed");
    }

    /// 切换引擎 / 局域网引擎地址必须重置曼波熔断与退避；无关配置变更不得重置
    #[test]
    fn test_update_config_resets_manbo_health_on_engine_or_lan_change() {
        let mgr = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Manbo,
            lan_engine_base_url: "http://192.168.1.50:9880".into(),
            ..Default::default()
        });
        mgr.mark_engine_degraded(TTSEngineType::Manbo);
        assert!(!mgr.is_engine_available(TTSEngineType::Manbo));

        // 音量变化与引擎/引擎地址无关：熔断保持
        let mut cfg = mgr.config_snapshot();
        cfg.speech_volume = 55;
        mgr.update_config(cfg);
        assert!(
            !mgr.is_engine_available(TTSEngineType::Manbo),
            "无关配置变更不应清熔断"
        );

        // 改引擎地址 → 立即恢复重试
        let mut cfg = mgr.config_snapshot();
        cfg.lan_engine_base_url = "http://192.168.1.60:9880".into();
        mgr.update_config(cfg);
        assert!(mgr.is_engine_available(TTSEngineType::Manbo), "改引擎地址应清熔断");

        // 再熔断后切换播报引擎 → 同样立即恢复，并清零退避计数
        mgr.mark_engine_degraded(TTSEngineType::Manbo);
        assert_eq!(mgr.manbo_breaker.failures.load(Ordering::SeqCst), 1);
        let mut cfg = mgr.config_snapshot();
        cfg.engine = TTSEngineType::Auto;
        mgr.update_config(cfg);
        assert!(
            mgr.is_engine_available(TTSEngineType::Manbo),
            "切换播报引擎应清熔断"
        );
        assert_eq!(mgr.manbo_breaker.failures.load(Ordering::SeqCst), 0);

        println!("[PASS] test_update_config_resets_manbo_health_on_engine_or_lan_change passed");
    }

    /// 合成跳超时必须覆盖长句合成（1080 Ti FP32 实测长句约 2s，留足余量）；
    /// 就绪探测必须快速失败（引擎冷启动约 20s~1 分钟，不能拖住播报）。
    #[test]
    fn test_lan_engine_timeout_budget() {
        assert!(
            TTSManager::REQUEST_TIMEOUT >= Duration::from_secs(10),
            "合成跳超时不得低于 10s"
        );
        assert!(
            TTSManager::LAN_PROBE_TIMEOUT <= Duration::from_secs(2),
            "就绪探测必须快速失败"
        );
        assert!(
            TTSManager::LAN_PROBE_TIMEOUT < TTSManager::REQUEST_TIMEOUT,
            "探测不应比合成更宽松"
        );
        println!("[PASS] test_lan_engine_timeout_budget passed");
    }

    /// 网络波动恢复策略：连续失败按 5/10/20/30s 指数退避；
    /// 任意一次成功立即清零并恢复曼波引擎，改引擎地址/引擎也立即清零。
    #[test]
    fn test_manbo_backoff_and_success_recovery() {
        assert_eq!(TTSManager::engine_backoff_secs(1), 5);
        assert_eq!(TTSManager::engine_backoff_secs(2), 10);
        assert_eq!(TTSManager::engine_backoff_secs(3), 20);
        assert_eq!(TTSManager::engine_backoff_secs(4), 30);
        assert_eq!(TTSManager::engine_backoff_secs(99), 30, "退避必须封顶");

        let mgr = TTSManager::new(TTSConfig {
            engine: TTSEngineType::Manbo,
            ..Default::default()
        });
        mgr.mark_engine_degraded(TTSEngineType::Manbo);
        assert_eq!(mgr.manbo_breaker.failures.load(Ordering::SeqCst), 1);
        assert!(!mgr.is_engine_available(TTSEngineType::Manbo));
        {
            let h = mgr.manbo_breaker.health.lock().unwrap();
            match *h {
                EngineHealth::Degraded { cooldown_secs, .. } => {
                    assert_eq!(cooldown_secs, 5, "首次失败应只冷却 5s 以便快速恢复")
                }
                _ => panic!("首次失败后应处于熔断状态"),
            }
        }

        // 连续失败：退避升档
        mgr.mark_engine_degraded(TTSEngineType::Manbo);
        assert_eq!(mgr.manbo_breaker.failures.load(Ordering::SeqCst), 2);
        assert!(!mgr.is_engine_available(TTSEngineType::Manbo));

        // 网络恢复：一次成功立即清零退避并回到 Manbo（不等冷却走完）
        mgr.mark_engine_success(TTSEngineType::Manbo);
        assert_eq!(mgr.manbo_breaker.failures.load(Ordering::SeqCst), 0);
        assert!(mgr.is_engine_available(TTSEngineType::Manbo));
        assert_eq!(mgr.select_active_engine(), TTSEngineType::Manbo);

        // 显式重置（换 Key/音色/引擎）同样清零
        mgr.mark_engine_degraded(TTSEngineType::Manbo);
        mgr.mark_engine_degraded(TTSEngineType::Manbo);
        assert_eq!(mgr.manbo_breaker.failures.load(Ordering::SeqCst), 2);
        mgr.reset_manbo_health();
        assert_eq!(mgr.manbo_breaker.failures.load(Ordering::SeqCst), 0);
        assert!(mgr.is_engine_available(TTSEngineType::Manbo));

        println!("[PASS] test_manbo_backoff_and_success_recovery passed");
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
}
