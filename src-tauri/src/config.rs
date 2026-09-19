use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

/// 完整应用配置（覆盖原 C++ 与 C# 全部 20+ 个配置字段）
/// 容器级 `serde(default)`：缺任一键时该字段取 `Default`，不会整份配置重置
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppConfig {
    // 1. 直播连接配置（敏感字段禁止序列化：身份码仅注册表，凭据仅 credentials.dat）
    #[serde(skip_serializing)]
    pub id_code: String,
    #[serde(skip_serializing)]
    pub app_id: String,
    #[serde(skip_serializing)]
    pub access_key_id: String,
    #[serde(skip_serializing)]
    pub access_key_secret: String,

    // 2. TTS 语音引擎配置
    pub tts_engine: String,
    pub enable_voice: bool,
    pub speech_rate: i32,
    pub speech_volume: i32,
    pub speech_pitch: i32,
    #[serde(skip_serializing)]
    pub manbo_api_key: String,
    pub manbo_voice: String,
    #[serde(skip_serializing)]
    pub mimo_api_key: String,
    pub mimo_voice: String,
    pub mimo_style: String,
    pub mimo_audio_format: String,
    pub tts_cache_days_to_keep: i32,

    // 3. 弹幕与播报过滤配置
    pub only_medal_order: bool,
    pub only_speek_wearing_medal: bool,
    pub only_speek_paid_gift: bool,
    pub only_speek_guard_level: i32,

    // 4. 悬浮窗与 OBS 视图配置
    pub opacity: i32,
    pub penetrating_mode_opacity: i32,
    pub top_pos_x: f64,
    pub top_pos_y: f64,
    pub default_marquee_text: String,

    // 5. 舰长打卡与 AI 配置
    pub enable_captain_checkin_ai: bool,
    pub checkin_trigger_words: String,
    #[serde(skip_serializing)]
    pub deepseek_api_key: String,

    // 6. 核心架构 Lite 模式
    pub is_lite_mode: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            id_code: String::new(),
            app_id: String::new(),
            access_key_id: String::new(),
            access_key_secret: String::new(),

            tts_engine: "manbo".into(),
            enable_voice: true,
            speech_rate: 0,
            speech_volume: 100,
            speech_pitch: 0,
            manbo_api_key: String::new(),
            // 原工程默认音色为「曼波」（专用端点 /apis/mbAIscvip，speed = speech_rate × 5）
            manbo_voice: "曼波".into(),
            mimo_api_key: String::new(),
            mimo_voice: "mimo_default".into(),
            // 原工程 mimoStyle 默认为空串
            mimo_style: String::new(),
            mimo_audio_format: "mp3".into(),
            tts_cache_days_to_keep: 7,

            only_medal_order: false,
            only_speek_wearing_medal: false,
            only_speek_paid_gift: false,
            only_speek_guard_level: 0,

            opacity: 95,
            penetrating_mode_opacity: 50,
            top_pos_x: 100.0,
            top_pos_y: 100.0,
            default_marquee_text: "欢迎来到直播间！发送“点怪+怪物名”即可加入排队。".into(),

            enable_captain_checkin_ai: true,
            checkin_trigger_words: "打卡,签到".into(),
            deepseek_api_key: String::new(),

            is_lite_mode: false,
        }
    }
}

impl AppConfig {
    /// V2 配置文件路径（统一经 paths::config_dir）
    pub fn get_config_path() -> PathBuf {
        crate::paths::config_dir().join("configs.json")
    }

    /// 原工程（旧版）配置文件路径：<config_dir>/MainConfig.cfg
    /// 用于把老工程的历史配置无损迁移进 V2
    pub fn get_legacy_config_path() -> PathBuf {
        crate::paths::config_dir().join("MainConfig.cfg")
    }

    /// 依据 JSON 内容自动判定格式并解析。
    /// 兼容原工程 SCREAMING_SNAKE 旧格式（MainConfig.cfg）。
    /// 解析失败时保留原文件内容到 `<file>.invalid` 供诊断，并回退默认值
    fn parse_content(content: &str, src_path: Option<&Path>) -> Self {
        let clean = content.strip_prefix('\u{FEFF}').unwrap_or(content);
        let val: serde_json::Value = match serde_json::from_str(clean) {
            Ok(v) => v,
            Err(e) => {
                crate::log_error!("[Config] JSON 解析失败，按字段默认值加载: {}", e);
                Self::dump_invalid(src_path, content);
                return Self::default();
            }
        };

        // 旧格式特征：存在 SCREAMING_SNAKE 键或 TopPos 嵌套对象
        if val.get("TTS_ENGINE").is_some()
            || val.get("ONLY_MEDAL_ORDER").is_some()
            || val.get("TopPos").is_some()
        {
            return Self::from_legacy(&val);
        }

        match serde_json::from_value(val) {
            Ok(cfg) => cfg,
            Err(e) => {
                crate::log_warn!("[Config] 配置字段解析失败，按字段默认值加载: {}", e);
                Self::dump_invalid(src_path, content);
                Self::default()
            }
        }
    }

    /// 解析失败时把原始内容另存为 `<file>.invalid`（不覆盖已存在的同名副本）
    fn dump_invalid(src_path: Option<&Path>, content: &str) {
        let Some(p) = src_path else { return };
        let mut name = p.as_os_str().to_os_string();
        name.push(".invalid");
        let out = PathBuf::from(name);
        if out.exists() {
            return;
        }
        if let Err(e) = fs::write(&out, content) {
            crate::log_warn!("[Config] 写出诊断副本失败 {:?}: {}", out, e);
        }
    }

    /// 把原工程 MainConfig.cfg 的旧格式映射到 V2 配置。
    /// 旧文件键为 SCREAMING_SNAKE；idCode / manboApiKey 不落 JSON（走注册表）。
    fn from_legacy(v: &serde_json::Value) -> Self {
        let mut cfg = Self::default();

        macro_rules! set_bool {
            ($key:literal, $field:ident) => {
                if let Some(b) = v.get($key).and_then(|x| x.as_bool()) {
                    cfg.$field = b;
                }
            };
        }
        macro_rules! set_int {
            ($key:literal, $field:ident) => {
                if let Some(n) = v.get($key).and_then(|x| x.as_i64()) {
                    cfg.$field = n as i32;
                }
            };
        }
        macro_rules! set_str {
            ($key:literal, $field:ident) => {
                if let Some(s) = v.get($key).and_then(|x| x.as_str()) {
                    cfg.$field = s.to_string();
                }
            };
        }

        set_bool!("ONLY_MEDAL_ORDER", only_medal_order);
        set_bool!("ENABLE_VOICE", enable_voice);
        set_int!("SPEECH_RATE", speech_rate);
        set_int!("SPEECH_PITCH", speech_pitch);
        set_int!("SPEECH_VOLUME", speech_volume);
        set_bool!("ONLY_SPEEK_WEARING_MEDAL", only_speek_wearing_medal);
        set_int!("ONLY_SPEEK_GUARD_LEVEL", only_speek_guard_level);
        set_bool!("ONLY_SPEEK_PAID_GIFT", only_speek_paid_gift);
        set_int!("OPACITY", opacity);
        set_int!("PENETRATING_MODE_OPACITY", penetrating_mode_opacity);

        if let Some(pos) = v.get("TopPos") {
            if let Some(x) = pos.get("X").and_then(|x| x.as_f64()) {
                cfg.top_pos_x = x;
            }
            if let Some(y) = pos.get("Y").and_then(|x| x.as_f64()) {
                cfg.top_pos_y = y;
            }
        }

        set_str!("DEFAULT_MARQUEE_TEXT", default_marquee_text);
        set_str!("TTS_ENGINE", tts_engine);
        set_str!("MIMO_API_KEY", mimo_api_key);
        set_str!("MIMO_VOICE", mimo_voice);
        set_str!("MIMO_STYLE", mimo_style);
        set_str!("MIMO_AUDIO_FORMAT", mimo_audio_format);
        set_str!("MANBO_VOICE", manbo_voice);
        set_int!("TTS_CACHE_DAYS_TO_KEEP", tts_cache_days_to_keep);
        set_bool!("ENABLE_CAPTAIN_CHECKIN_AI", enable_captain_checkin_ai);
        set_str!("CHECKIN_TRIGGER_WORDS", checkin_trigger_words);

        cfg
    }

    /// 从文件加载配置。
    /// 优先级：显式路径 > V2 的 configs.json > 原工程 MainConfig.cfg（自动迁移）> 默认值。
    /// 同时优先从 Windows 注册表加载 IdCode 与 ManboApiKey。
    pub fn load(path: Option<&Path>) -> Self {
        let mut cfg: Self = match path {
            Some(custom) => Self::load_from_file(custom),
            None => {
                let new_p = Self::get_config_path();
                if new_p.exists() {
                    Self::load_from_file(&new_p)
                } else {
                    // V2 配置尚不存在时，回退读取原工程历史配置（实现数据迁移）
                    let legacy_p = Self::get_legacy_config_path();
                    if legacy_p.exists() {
                        Self::load_from_file(&legacy_p)
                    } else {
                        Self::default()
                    }
                }
            }
        };

        // 遵循原工程设计规格：优先从注册表读取 idCode
        let reg_id_code = crate::registry::read_id_code().unwrap_or_default();
        if !reg_id_code.trim().is_empty() {
            cfg.id_code = reg_id_code;
        } else if !cfg.id_code.trim().is_empty() {
            // 数据迁移：若注册表中暂无但配置中存在，则自动持久化至注册表
            if let Err(e) = crate::registry::write_id_code(&cfg.id_code) {
                crate::log_warn!("[Config] IdCode 迁移写入注册表失败: {}", e);
            }
        }

        // Manbo API Key 同样以注册表为权威来源（与 idCode 同级）
        let reg_manbo_key = crate::registry::read_manbo_api_key().unwrap_or_default();
        if !reg_manbo_key.trim().is_empty() {
            cfg.manbo_api_key = reg_manbo_key;
        } else if !cfg.manbo_api_key.trim().is_empty() {
            if let Err(e) = crate::registry::write_manbo_api_key(&cfg.manbo_api_key) {
                crate::log_warn!("[Config] ManboApiKey 迁移写入注册表失败: {}", e);
            }
        }

        cfg
    }

    /// 脱敏副本：清空全部凭据字段，供返回 WebView / 事件广播使用（不落盘）
    /// 保留 id_code —— 前端身份码输入框依赖该值
    pub fn sanitized(&self) -> Self {
        let mut c = self.clone();
        c.app_id.clear();
        c.access_key_id.clear();
        c.access_key_secret.clear();
        c.manbo_api_key.clear();
        c.mimo_api_key.clear();
        c.deepseek_api_key.clear();
        c
    }

    /// 读取单个配置文件内容并解析（自动识别新旧格式）
    fn load_from_file(p: &Path) -> Self {
        match fs::read_to_string(p) {
            Ok(content) => Self::parse_content(&content, Some(p)),
            Err(_) => Self::default(),
        }
    }

    /// 原子保存配置到文件，并同步将 IdCode / ManboApiKey 持久化到 Windows 注册表。
    /// 敏感字段（`skip_serializing`）不会写入 JSON；注册表仅在有值时写入，避免空值清空。
    /// 注册表写入失败只记录警告、不阻断文件保存（E2：凭据仍可由 credentials.dat 承载）
    pub fn save(&self, path: Option<&Path>) -> Result<(), String> {
        self.persist_registry();

        let p = match path {
            Some(custom) => custom.to_path_buf(),
            None => Self::get_config_path(),
        };

        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let tmp = p.with_extension("tmp");
        let json_text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;

        let mut file = File::create(&tmp).map_err(|e| e.to_string())?;
        file.write_all(json_text.as_bytes()).map_err(|e| e.to_string())?;
        file.flush().map_err(|e| e.to_string())?;
        drop(file);

        fs::rename(&tmp, &p).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 同步 IdCode / ManboApiKey 至 Windows 注册表（注册表为权威来源，仅非空写入）。
    /// 失败仅告警 —— 注册表不可用时不应导致整份配置无法保存
    fn persist_registry(&self) {
        if !self.id_code.trim().is_empty() {
            if let Err(e) = crate::registry::write_id_code(&self.id_code) {
                crate::log_warn!("[Config] IdCode 写入注册表失败: {}", e);
            }
        }
        if !self.manbo_api_key.trim().is_empty() {
            if let Err(e) = crate::registry::write_manbo_api_key(&self.manbo_api_key) {
                crate::log_warn!("[Config] ManboApiKey 写入注册表失败: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_config_persistence_and_defaults() {
        let temp_dir = std::env::temp_dir().join("mh_test_config");
        let _ = fs::create_dir_all(&temp_dir);
        let path = temp_dir.join("test_configs.json");

        let mut cfg = AppConfig::default();
        assert_eq!(cfg.opacity, 95);
        assert_eq!(cfg.is_lite_mode, false);

        // id_code 不再经 JSON 往返（仅注册表持久化），此处只验证常规字段
        cfg.default_marquee_text = "自定义跑马灯通告".into();
        cfg.opacity = 80;
        cfg.is_lite_mode = true;

        assert!(cfg.save(Some(&path)).is_ok());

        let loaded = AppConfig::load(Some(&path));
        assert_eq!(loaded.default_marquee_text, "自定义跑马灯通告");
        assert_eq!(loaded.opacity, 80);
        assert_eq!(loaded.is_lite_mode, true);

        // sanitized()：凭据清空、常规字段保持
        let s = cfg.sanitized();
        assert_eq!(s.default_marquee_text, "自定义跑马灯通告");
        assert_eq!(s.opacity, 80);
        assert!(s.app_id.is_empty() && s.access_key_secret.is_empty());

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&temp_dir);
        println!("[PASS] test_app_config_persistence_and_defaults passed");
    }

    /// E2：空凭据保存不得改写注册表（仅非空才写），且文件保存不受注册表状态影响
    #[test]
    fn test_save_with_empty_credentials_keeps_registry_untouched() {
        let before_id = crate::registry::read_id_code().unwrap_or_default();
        let before_key = crate::registry::read_manbo_api_key().unwrap_or_default();

        let temp_dir = std::env::temp_dir().join("mh_test_config_registry");
        let _ = fs::create_dir_all(&temp_dir);
        let path = temp_dir.join("registry_configs.json");

        let mut cfg = AppConfig::default();
        cfg.id_code.clear();
        cfg.manbo_api_key.clear();
        cfg.save(Some(&path)).expect("空凭据时文件保存应成功");

        assert_eq!(
            crate::registry::read_id_code().unwrap_or_default(),
            before_id,
            "空 id_code 不得改写注册表"
        );
        assert_eq!(
            crate::registry::read_manbo_api_key().unwrap_or_default(),
            before_key,
            "空 manbo_api_key 不得改写注册表"
        );

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&temp_dir);
        println!("[PASS] test_save_with_empty_credentials_keeps_registry_untouched passed");
    }

    #[test]
    fn test_config_serialization_excludes_secrets() {
        let temp_dir = std::env::temp_dir().join("mh_test_config_secrets");
        let _ = fs::create_dir_all(&temp_dir);
        let path = temp_dir.join("secret_configs.json");

        let mut cfg = AppConfig::default();
        cfg.id_code = "SECRET_ID_CODE".into();
        cfg.app_id = "SECRET_APP_ID".into();
        cfg.access_key_id = "SECRET_AK_ID".into();
        cfg.access_key_secret = "SECRET_AK_SECRET".into();
        cfg.manbo_api_key = "SECRET_MANBO".into();
        cfg.mimo_api_key = "SECRET_MIMO".into();
        cfg.deepseek_api_key = "SECRET_DEEPSEEK".into();
        cfg.opacity = 77;

        let secret_keys = [
            "id_code", "app_id", "access_key_id", "access_key_secret",
            "manbo_api_key", "mimo_api_key", "deepseek_api_key",
        ];
        let secret_values = [
            "SECRET_ID_CODE", "SECRET_APP_ID", "SECRET_AK_ID", "SECRET_AK_SECRET",
            "SECRET_MANBO", "SECRET_MIMO", "SECRET_DEEPSEEK",
        ];

        // 1. 序列化结果（返回前端 / 落盘的同一来源）不含敏感键与明文
        let json_text = serde_json::to_string_pretty(&cfg).unwrap();
        for key in secret_keys {
            assert!(!json_text.contains(key), "序列化结果不应包含敏感键 {}: {}", key, json_text);
        }
        for secret in secret_values {
            assert!(!json_text.contains(secret), "序列化结果不应包含明文凭据 {}", secret);
        }
        assert!(json_text.contains("opacity") && json_text.contains("77"), "常规字段必须保留: {}", json_text);

        // 2. save() 落盘文件同样不含敏感键与明文。
        //    注：id_code / manbo_api_key 非空时 save() 会同步写注册表，为避免与 registry 测试并行竞争，
        //    落盘路径改用空值（其"键不出现"由同一 skip_serializing 机制保证），值断言由非注册表字段承担
        let mut save_cfg = cfg.clone();
        save_cfg.id_code = String::new();
        save_cfg.manbo_api_key = String::new();
        save_cfg.save(Some(&path)).unwrap();
        let file_text = fs::read_to_string(&path).unwrap();
        for key in secret_keys {
            assert!(!file_text.contains(key), "configs.json 不应包含敏感键 {}: {}", key, file_text);
        }
        for secret in ["SECRET_APP_ID", "SECRET_AK_ID", "SECRET_AK_SECRET", "SECRET_MIMO", "SECRET_DEEPSEEK"] {
            assert!(!file_text.contains(secret), "落盘文件不应包含明文凭据 {}: {}", secret, file_text);
        }

        // 3. 反序列化兼容：历史配置中的敏感键仍可读取（只读不回写）
        let legacy = r#"{"id_code":"LEGACY_ID","mimo_api_key":"LEGACY_MIMO","opacity":60}"#;
        let parsed = AppConfig::parse_content(legacy, None);
        assert_eq!(parsed.id_code, "LEGACY_ID");
        assert_eq!(parsed.mimo_api_key, "LEGACY_MIMO");
        assert_eq!(parsed.opacity, 60);

        // 4. sanitized()：清空全部凭据字段、保留 id_code 与常规字段
        let s = cfg.sanitized();
        assert_eq!(s.id_code, "SECRET_ID_CODE");
        assert!(s.app_id.is_empty() && s.access_key_id.is_empty() && s.access_key_secret.is_empty());
        assert!(s.manbo_api_key.is_empty() && s.mimo_api_key.is_empty() && s.deepseek_api_key.is_empty());
        assert_eq!(s.opacity, 77);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&temp_dir);
        println!("[PASS] test_config_serialization_excludes_secrets passed");
    }

    #[test]
    fn test_partial_config_uses_field_defaults() {
        let temp_dir = std::env::temp_dir().join("mh_test_config_partial");
        let _ = fs::create_dir_all(&temp_dir);
        let path = temp_dir.join("partial_configs.json");

        // 仅写入 opacity 一个键，其余字段必须逐字段取默认值而非整份重置
        fs::write(&path, r#"{ "opacity": 80 }"#).unwrap();

        let cfg = AppConfig::load(Some(&path));
        let def = AppConfig::default();
        assert_eq!(cfg.opacity, 80);
        assert_eq!(cfg.tts_engine, def.tts_engine);
        assert_eq!(cfg.enable_voice, def.enable_voice);
        assert_eq!(cfg.speech_volume, def.speech_volume);
        assert_eq!(cfg.penetrating_mode_opacity, def.penetrating_mode_opacity);
        assert_eq!(cfg.top_pos_x, def.top_pos_x);
        assert_eq!(cfg.default_marquee_text, def.default_marquee_text);
        assert_eq!(cfg.checkin_trigger_words, def.checkin_trigger_words);
        assert_eq!(cfg.is_lite_mode, def.is_lite_mode);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&temp_dir);
        println!("[PASS] test_partial_config_uses_field_defaults passed");
    }

    #[test]
    fn test_empty_object_config_is_all_defaults() {
        // 直接测试解析层，避免受本机注册表（idCode / ManboApiKey）注入影响
        let cfg = AppConfig::parse_content("{}", None);
        assert_eq!(cfg, AppConfig::default());
        println!("[PASS] test_empty_object_config_is_all_defaults passed");
    }

    #[test]
    fn test_invalid_config_dumps_diagnostic_copy() {
        let temp_dir = std::env::temp_dir().join("mh_test_config_invalid");
        let _ = fs::create_dir_all(&temp_dir);
        let path = temp_dir.join("broken_configs.json");
        let mut invalid_name = path.as_os_str().to_os_string();
        invalid_name.push(".invalid");
        let invalid_path = PathBuf::from(&invalid_name);
        let _ = fs::remove_file(&invalid_path);

        let broken = "{ this is not valid json";
        fs::write(&path, broken).unwrap();

        // 解析失败不 panic，回退默认值，并把原文保留到 .invalid 副本
        let cfg = AppConfig::parse_content(broken, Some(&path));
        assert_eq!(cfg, AppConfig::default());
        assert!(invalid_path.exists(), "应生成 {:?} 诊断副本", invalid_path);
        assert!(path.exists(), "原文件必须保留，不得删除或覆盖");
        let dumped = fs::read_to_string(&invalid_path).unwrap();
        assert_eq!(dumped, broken);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&invalid_path);
        let _ = fs::remove_dir(&temp_dir);
        println!("[PASS] test_invalid_config_dumps_diagnostic_copy passed");
    }

    #[test]
    fn test_load_legacy_mainconfig_format() {
        let temp_dir = std::env::temp_dir().join("mh_test_config_legacy");
        let _ = fs::create_dir_all(&temp_dir);
        let path = temp_dir.join("MainConfig.cfg");

        // 原工程 MainConfig.cfg 旧格式（SCREAMING_SNAKE + TopPos 嵌套对象）
        let legacy = r#"{
            "ONLY_MEDAL_ORDER": false,
            "ENABLE_VOICE": true,
            "SPEECH_VOLUME": 50,
            "OPACITY": 80,
            "PENETRATING_MODE_OPACITY": 50,
            "TTS_ENGINE": "sapi",
            "MANBO_VOICE": "曼波",
            "MIMO_VOICE": "mimo_default",
            "CHECKIN_TRIGGER_WORDS": "打卡,签到",
            "DEFAULT_MARQUEE_TEXT": "发送'点怪 xxx'进行点怪",
            "TTS_CACHE_DAYS_TO_KEEP": 7,
            "TopPos": { "X": 913.0, "Y": 105.0 }
        }"#;
        fs::write(&path, legacy).unwrap();

        let cfg = AppConfig::load(Some(&path));
        assert!(!cfg.only_medal_order);
        assert!(cfg.enable_voice);
        assert_eq!(cfg.speech_volume, 50);
        assert_eq!(cfg.opacity, 80);
        assert_eq!(cfg.penetrating_mode_opacity, 50);
        assert_eq!(cfg.tts_engine, "sapi");
        assert_eq!(cfg.manbo_voice, "曼波");
        assert_eq!(cfg.mimo_voice, "mimo_default");
        assert_eq!(cfg.checkin_trigger_words, "打卡,签到");
        assert_eq!(cfg.default_marquee_text, "发送'点怪 xxx'进行点怪");
        assert_eq!(cfg.top_pos_x, 913.0);
        assert_eq!(cfg.top_pos_y, 105.0);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&temp_dir);
        println!("[PASS] test_load_legacy_mainconfig_format passed");
    }
}
