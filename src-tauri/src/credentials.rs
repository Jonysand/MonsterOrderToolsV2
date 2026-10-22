use base64::prelude::*;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::fs;
use std::path::{Path, PathBuf};

type HmacSha256 = Hmac<Sha256>;

pub const FILE_MAGIC: &str = "@MonsterOrderSecret@";
pub const SALT: &str = "@M0nst3r$Alt@";

/// 加密配置文件凭据。
///
/// 除开播身份码（注册表）外的全部凭据均由发行方填写 `scripts/credentials.json`
/// 后生成 credentials.dat 并随安装包打包。Manbo 语音 Key 另提供 TTS 设置里的
/// 应急入口（仅写注册表），用于打包凭据缺失或轮换后的现场修复。
/// 旧文件中的遗留字段（minimax_tts_api_key 等）由 serde 默认忽略，读取兼容。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Credentials {
    #[serde(rename = "APP_ID", default)]
    pub app_id: String,
    #[serde(rename = "ACCESS_KEY_ID", default)]
    pub access_key_id: String,
    #[serde(rename = "ACCESS_KEY_SECRET", default)]
    pub access_key_secret: String,
    #[serde(default)]
    pub manbo_api_key: String,
    #[serde(default = "default_chat_provider")]
    pub chat_provider: String,
    #[serde(default)]
    pub chat_api_key: String,
}

fn default_chat_provider() -> String {
    "deepseek".to_string()
}

/// 识别 `scripts/credentials.example.json` 里的模板占位文本。
///
/// 占位值一旦被当成真实密钥：Manbo 会返回 401 并静默降级到本地 SAPI，
/// 让"只有默认语音"这种故障几乎没有线索可查。加载时一律按"未配置"处理，
/// 交给注册表 / 配置镜像兜底。
pub fn is_placeholder_value(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return false;
    }
    if v.to_ascii_uppercase().contains("REPLACE_ME") {
        return true;
    }
    const CN_MARKERS: [&str; 6] = [
        "在此填写",
        "在此输入",
        "在此輸入",
        "请填写",
        "請填寫",
        "模板占位",
    ];
    CN_MARKERS.iter().any(|m| v.contains(m))
}

/// 凭据状态摘要（用于前端安全脱敏展示）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialsStatus {
    pub loaded: bool,
    pub file_path: String,
    pub app_id: String,
    pub access_key_masked: String,
    pub chat_provider: String,
    pub has_chat_key: bool,
    pub has_manbo_key: bool,
}

impl Credentials {
    /// 可选密钥字段的占位值清洗：Manbo / 对话 Key 都有注册表/配置兜底，
    /// 不能拿模板说明文字去请求服务端。B 站三件套不在此处理 —— 它们没有兜底，
    /// 占位值应在打包生成 credentials.dat 时直接拦截（见 scripts/generate_credentials.py）。
    fn normalize_optional_placeholders(&mut self) {
        if is_placeholder_value(&self.manbo_api_key) {
            crate::log_warn!(
                "[Credentials] manbo_api_key 是模板占位值，按未配置处理（回退注册表/配置镜像）"
            );
            self.manbo_api_key.clear();
        }
        if is_placeholder_value(&self.chat_api_key) {
            crate::log_warn!(
                "[Credentials] chat_api_key 是模板占位值，按未配置处理（回退配置镜像）"
            );
            self.chat_api_key.clear();
        }
    }

    pub fn to_status(&self, loaded: bool, path: &str) -> CredentialsStatus {
        let access_key_masked = mask_access_key(&self.access_key_id);

        CredentialsStatus {
            loaded,
            file_path: path.to_string(),
            // app_id 非密钥，回显供用户核对；AccessKey 走掩码
            app_id: if self.app_id.is_empty() { "未配置".to_string() } else { self.app_id.clone() },
            access_key_masked,
            chat_provider: self.chat_provider.clone(),
            has_chat_key: !self.chat_api_key.is_empty(),
            has_manbo_key: !self.manbo_api_key.is_empty(),
        }
    }
}

/// AccessKey 掩码：仅保留首尾各 4 个**字符**（按字符边界切分，避免多字节 UTF-8 触发 panic）
pub fn mask_access_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() >= 8 {
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{}***{}", head, tail)
    } else if !chars.is_empty() {
        "******".to_string()
    } else {
        "未配置".to_string()
    }
}

/// 计算 HMAC-SHA256 十六进制字符串（小写）
pub fn compute_hmac_hex(data: &str, salt: &str) -> Result<String, String> {
    let mut mac = HmacSha256::new_from_slice(salt.as_bytes())
        .map_err(|e| format!("HMAC 初始化失败: {}", e))?;
    mac.update(data.as_bytes());
    let result = mac.finalize();
    Ok(hex::encode(result.into_bytes()))
}

/// 获取 credentials.dat 的规范路径（统一经 paths::config_dir）。
/// 注：即使文件尚不存在也返回规范位置，便于前端提示"应放置到哪里"与实际导入目标一致。
pub fn get_credentials_path() -> PathBuf {
    crate::paths::config_dir().join("credentials.dat")
}

/// 凭据来源裁决：优先使用可写数据目录中的文件（导入更新通道），
/// 数据目录不存在时回退到随安装包分发的 resources 副本（打包预置通道）。
/// 两个候选都不存在时返回 None。
pub fn pick_credentials_source(primary: &Path, resource: Option<&Path>) -> Option<PathBuf> {
    if primary.is_file() {
        return Some(primary.to_path_buf());
    }
    resource
        .filter(|p| p.is_file())
        .map(|p| p.to_path_buf())
}

/// 解析默认加载路径：数据目录 → 安装资源目录（`find_resource` 已覆盖两级候选）。
/// Windows 安装版资源与数据目录同为 exe 同级，天然命中第一级；
/// macOS 打包态数据目录无文件时回退 .app 包内资源，实现"凭据随安装包内置"。
pub fn resolve_default_credentials_path() -> Option<PathBuf> {
    let primary = get_credentials_path();
    let resource = crate::paths::find_resource("credentials.dat");
    pick_credentials_source(&primary, resource.as_deref())
}

/// 加载并解密 credentials.dat 文件
pub fn load_credentials(path: Option<&Path>) -> Result<Credentials, String> {
    let p = match path {
        Some(custom) => custom.to_path_buf(),
        None => resolve_default_credentials_path()
            .ok_or_else(|| format!("凭证文件不存在: {}", get_credentials_path().display()))?,
    };

    if !p.exists() {
        return Err(format!("凭证文件不存在: {}", p.display()));
    }

    let raw_bytes = fs::read(&p).map_err(|e| format!("无法读取凭据文件: {}", e))?;
    let raw_str = String::from_utf8_lossy(&raw_bytes).trim().to_string();

    let decoded_bytes = BASE64_STANDARD
        .decode(&raw_str)
        .map_err(|e| format!("Base64 解码凭据失败: {}", e))?;

    let content = String::from_utf8(decoded_bytes)
        .map_err(|e| format!("凭据文件内容非有效 UTF-8: {}", e))?;

    if !content.starts_with(FILE_MAGIC) {
        return Err("凭据文件格式不正确".to_string());
    }

    let after_magic = &content[FILE_MAGIC.len()..];
    const HMAC_HEX_LENGTH: usize = 64;
    if after_magic.len() < HMAC_HEX_LENGTH {
        return Err("凭据文件不完整".to_string());
    }

    let stored_hmac = &after_magic[..HMAC_HEX_LENGTH];
    let json_data = &after_magic[HMAC_HEX_LENGTH..];

    let computed_hmac = compute_hmac_hex(json_data, SALT)?;
    if !stored_hmac.eq_ignore_ascii_case(&computed_hmac) {
        return Err("凭据校验失败：文件可能已损坏或被非法篡改！".to_string());
    }

    let mut creds: Credentials = serde_json::from_str(json_data)
        .map_err(|e| format!("解析凭据 JSON 失败: {}", e))?;
    creds.normalize_optional_placeholders();

    Ok(creds)
}

/// 生成并加密凭证文件（可用于离线生成与单元测试）
pub fn save_credentials(creds: &Credentials, path: &Path) -> Result<(), String> {
    let json_data = serde_json::to_string(creds).map_err(|e| e.to_string())?;
    let hmac_hex = compute_hmac_hex(&json_data, SALT)?;
    let combined = format!("{}{}{}", FILE_MAGIC, hmac_hex, json_data);
    let encoded = BASE64_STANDARD.encode(combined.as_bytes());

    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    fs::write(path, encoded).map_err(|e| format!("写入凭据文件失败: {}", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_real_credentials_dat() {
        // 按统一资源解析定位真实凭据文件（cwd → cwd/.. → exe 同级 → 随包资源）
        let Some(p) = crate::paths::find_resource("credentials.dat") else {
            // 显式跳过并写明原因（避免"测试被静默跳过"导致格式回归无保护）
            println!("[SKIP] test_load_real_credentials_dat: 未找到 credentials.dat（仅验证格式往返）");
            return;
        };
        let res = load_credentials(Some(&p));
        assert!(res.is_ok(), "加载真实 credentials.dat 失败: {:?}", res.err());
        let cred = res.unwrap();
        // 仅校验解析结构与关键字段非空，禁止在源码中硬编码真实密钥
        assert!(!cred.app_id.is_empty(), "app_id 不应为空");
        assert!(!cred.access_key_id.is_empty(), "access_key_id 不应为空");
        assert!(!cred.access_key_secret.is_empty(), "access_key_secret 不应为空");
        assert_eq!(cred.chat_provider, "deepseek");
        // 可选密钥允许为空：模板占位值在加载时已按"未配置"清洗，不能断言非空
        println!(
            "[PASS] test_load_real_credentials_dat passed (source: {:?}, chat_key={}, manbo_key={})",
            p,
            !cred.chat_api_key.is_empty(),
            !cred.manbo_api_key.is_empty()
        );
    }

    /// 占位文本识别：模板说明文字绝不能当成真实密钥
    #[test]
    fn test_is_placeholder_value() {
        assert!(is_placeholder_value(
            "REPLACE_ME_Manbo语音接口密钥_留空则回退本机注册表历史值"
        ));
        assert!(is_placeholder_value("在此填写B站开放平台AccessKey ID"));
        assert!(is_placeholder_value("请填写真实的密钥"));
        assert!(!is_placeholder_value(""));
        assert!(!is_placeholder_value("   "));
        assert!(!is_placeholder_value("a1b2c3d4e5f6a7b8c9d0"));
        assert!(!is_placeholder_value("sk-proj-ABC123xyz"));

        println!("[PASS] test_is_placeholder_value passed");
    }

    /// 加载时占位密钥 → 空（注册表/配置兜底才有机会生效）；B 站三件套原样保留
    #[test]
    fn test_placeholder_keys_normalized_on_load() {
        let dir = std::env::temp_dir().join("mh_cred_placeholder");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("credentials.dat");

        let creds = Credentials {
            app_id: "1751077177719".into(),
            access_key_id: "AKIDEXAMPLE1234".into(),
            access_key_secret: "SECRETEXAMPLE".into(),
            manbo_api_key: "REPLACE_ME_Manbo语音接口密钥_留空则回退本机注册表历史值".into(),
            chat_provider: "deepseek".into(),
            chat_api_key: "在此填写DeepSeek对话模型密钥".into(),
        };
        save_credentials(&creds, &path).unwrap();

        let loaded = load_credentials(Some(&path)).unwrap();
        assert_eq!(loaded.manbo_api_key, "", "占位 Manbo Key 应按未配置处理");
        assert_eq!(loaded.chat_api_key, "", "占位对话 Key 应按未配置处理");
        assert_eq!(loaded.app_id, creds.app_id, "必填三件套原样保留");
        assert_eq!(loaded.access_key_id, creds.access_key_id);
        assert_eq!(loaded.access_key_secret, creds.access_key_secret);

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_placeholder_keys_normalized_on_load passed");
    }

    /// 格式往返：V2 生成的文件必须能被 V2 读回（原工程算法逐步骤对齐，故亦与原工程互通）
    #[test]
    fn test_credentials_roundtrip_and_mask() {
        let temp_dir = std::env::temp_dir().join("mh_cred_roundtrip");
        let _ = std::fs::create_dir_all(&temp_dir);
        let path = temp_dir.join("credentials.dat");
        let _ = std::fs::remove_file(&path);

        let creds = Credentials {
            app_id: "1751077177719".into(),
            access_key_id: "AKIDEXAMPLE1234".into(),
            access_key_secret: "SECRETEXAMPLE".into(),
            chat_provider: "deepseek".into(),
            chat_api_key: "sk-chat".into(),
            ..Default::default()
        };
        save_credentials(&creds, &path).unwrap();

        let loaded = load_credentials(Some(&path)).unwrap();
        assert_eq!(loaded.access_key_id, creds.access_key_id);
        assert_eq!(loaded.access_key_secret, creds.access_key_secret);
        assert_eq!(loaded.chat_api_key, creds.chat_api_key);

        // 篡改一字节 → HMAC 校验必须失败
        let mut raw = std::fs::read(&path).unwrap();
        let n = raw.len();
        raw[n - 4] = if raw[n - 4] == b'A' { b'B' } else { b'A' };
        std::fs::write(&path, &raw).unwrap();
        assert!(load_credentials(Some(&path)).is_err(), "篡改后必须校验失败");

        // 掩码不得 panic（含多字节字符的边界用例）
        assert_eq!(mask_access_key("AKIDEXAMPLE1234"), "AKID***1234");
        assert_eq!(mask_access_key("短"), "******");
        assert_eq!(mask_access_key(""), "未配置");
        assert_eq!(mask_access_key("中文密钥中文密钥中文"), "中文密钥***密钥中文");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&temp_dir);
        println!("[PASS] test_credentials_roundtrip_and_mask passed");
    }

    #[test]
    fn test_credentials_tampered_magic() {
        let temp_dir = std::env::temp_dir().join("mh_cred_test_magic");
        let _ = fs::create_dir_all(&temp_dir);
        let path = temp_dir.join("tampered_magic.dat");

        // 构造非法魔数
        let json_data = r#"{"APP_ID":"123"}"#;
        let hmac_hex = compute_hmac_hex(json_data, SALT).unwrap();
        let fake_content = format!("@BadSecret@{}{}", hmac_hex, json_data);
        let encoded = BASE64_STANDARD.encode(fake_content.as_bytes());
        fs::write(&path, encoded).unwrap();

        let res = load_credentials(Some(&path));
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("凭据文件格式不正确"));

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&temp_dir);
        println!("[PASS] test_credentials_tampered_magic passed");
    }

    #[test]
    fn test_credentials_tampered_hmac() {
        let temp_dir = std::env::temp_dir().join("mh_cred_test_hmac");
        let _ = fs::create_dir_all(&temp_dir);
        let path = temp_dir.join("tampered_hmac.dat");

        // 构造被篡改的内容（篡改 json 内容使签名失效）
        let json_data = r#"{"APP_ID":"123"}"#;
        let hmac_hex = compute_hmac_hex(json_data, SALT).unwrap();
        let tampered_json = r#"{"APP_ID":"hacked_999"}"#;
        let fake_content = format!("{}{}{}", FILE_MAGIC, hmac_hex, tampered_json);
        let encoded = BASE64_STANDARD.encode(fake_content.as_bytes());
        fs::write(&path, encoded).unwrap();

        let res = load_credentials(Some(&path));
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("凭据校验失败"));

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&temp_dir);
        println!("[PASS] test_credentials_tampered_hmac passed");
    }

    #[test]
    fn test_credentials_roundtrip() {
        let temp_dir = std::env::temp_dir().join("mh_cred_test_roundtrip");
        let _ = fs::create_dir_all(&temp_dir);
        let path = temp_dir.join("test_roundtrip.dat");

        let original = Credentials {
            app_id: "test_app_888".into(),
            access_key_id: "test_ak_id".into(),
            access_key_secret: "test_ak_sec".into(),
            manbo_api_key: "test_manbo_key".into(),
            chat_provider: "deepseek".into(),
            chat_api_key: "sk-test_ai_key".into(),
        };

        assert!(save_credentials(&original, &path).is_ok());

        let loaded = load_credentials(Some(&path)).unwrap();
        assert_eq!(loaded, original);

        let status = loaded.to_status(true, &path.to_string_lossy());
        assert!(status.loaded);
        assert_eq!(status.app_id, "test_app_888");
        assert!(status.access_key_masked.contains("***"));
        assert!(status.has_manbo_key);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&temp_dir);
        println!("[PASS] test_credentials_roundtrip passed");
    }

    /// 凭据来源裁决：数据目录优先（导入更新通道），缺失时回退安装资源（打包预置通道）
    #[test]
    fn test_pick_credentials_source_prefers_data_dir_over_resource() {
        let base = std::env::temp_dir().join("mh_cred_pick_source");
        let _ = fs::remove_dir_all(&base);
        let data_dir = base.join("data");
        let res_dir = base.join("resource");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&res_dir).unwrap();

        let primary = data_dir.join("credentials.dat");
        let resource = res_dir.join("credentials.dat");

        // 两侧都无文件 → None
        assert!(pick_credentials_source(&primary, Some(&resource)).is_none());

        // 仅资源侧有 → 用资源（首次安装：凭据随包内置）
        fs::write(&resource, b"BUILTIN").unwrap();
        assert_eq!(
            pick_credentials_source(&primary, Some(&resource)).unwrap(),
            resource
        );

        // 数据目录出现后优先（导入更新通道），且不被资源副本覆盖裁决
        fs::write(&primary, b"IMPORTED").unwrap();
        assert_eq!(
            pick_credentials_source(&primary, Some(&resource)).unwrap(),
            primary
        );

        // 资源候选为 None 时仅看数据目录
        assert_eq!(
            pick_credentials_source(&primary, None).unwrap(),
            primary
        );

        let _ = fs::remove_dir_all(&base);
        println!("[PASS] test_pick_credentials_source_prefers_data_dir_over_resource passed");
    }

    /// 跨端互通锁定：scripts/generate_credentials.py 产物必须能被 Rust 侧验签加载。
    /// 该向量为 Python 生成器对固定测试 JSON 的输出（无真实凭据），任何人重跑
    /// `python -c` 复算都应得到同一 Base64 —— 算法漂移（魔数/盐/序列化）会被此用例捕获
    #[test]
    fn test_load_python_generator_output() {
        let py_output = "QE1vbnN0ZXJPcmRlclNlY3JldEAwY2IzMTMwMWUzNmI5NzNkM2YwYjE0MDUyZTcwZGY5YzRjMDQ0OGVmYWU3ZDk1YzVkNmNhYTc3NTRlZWYxZmUyeyJBUFBfSUQiOiJQWUdFTl9URVNUIiwiQUNDRVNTX0tFWV9JRCI6ImFrX3B5IiwiQUNDRVNTX0tFWV9TRUNSRVQiOiJzZWNfcHkiLCJtaW1vX3R0c19hcGlfa2V5IjoiIiwibWFuYm9fYXBpX2tleSI6Im1hbmJvX3B5IiwiY2hhdF9wcm92aWRlciI6ImRlZXBzZWVrIiwiY2hhdF9hcGlfa2V5Ijoic2tfcHkifQ==";
        let temp_dir = std::env::temp_dir().join("mh_cred_pygen");
        let _ = fs::create_dir_all(&temp_dir);
        let path = temp_dir.join("pygen.dat");
        fs::write(&path, py_output).unwrap();

        let loaded = load_credentials(Some(&path)).unwrap();
        assert_eq!(loaded.app_id, "PYGEN_TEST");
        assert_eq!(loaded.manbo_api_key, "manbo_py");
        assert_eq!(loaded.chat_api_key, "sk_py");

        // 正向：Rust save_credentials 的产物同样符合该信封结构（往返已在其他用例覆盖）
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&temp_dir);
        println!("[PASS] test_load_python_generator_output passed");
    }

    /// 旧版凭据文件（含遗留字段 minimax/special_user_tts_api_key）必须继续可读，
    /// 未知字段由 serde 忽略 —— 保证升级安装不因凭据文件格式差异而失效
    #[test]
    fn test_load_legacy_credentials_with_removed_fields() {
        let temp_dir = std::env::temp_dir().join("mh_cred_legacy_fields");
        let _ = fs::create_dir_all(&temp_dir);
        let path = temp_dir.join("legacy.dat");

        let json_data = r#"{"APP_ID":"123","ACCESS_KEY_ID":"ak","ACCESS_KEY_SECRET":"sec","mimo_tts_api_key":"mimo","minimax_tts_api_key":"legacy_mm","special_user_tts_api_key":"legacy_vip","chat_provider":"deepseek","chat_api_key":"sk-x"}"#;
        let hmac_hex = compute_hmac_hex(json_data, SALT).unwrap();
        let combined = format!("{}{}{}", FILE_MAGIC, hmac_hex, json_data);
        fs::write(&path, BASE64_STANDARD.encode(combined.as_bytes())).unwrap();

        let loaded = load_credentials(Some(&path)).unwrap();
        assert_eq!(loaded.app_id, "123");
        assert_eq!(loaded.chat_api_key, "sk-x");
        assert!(loaded.manbo_api_key.is_empty(), "旧文件无 manbo 字段时应为空");

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&temp_dir);
        println!("[PASS] test_load_legacy_credentials_with_removed_fields passed");
    }
}
