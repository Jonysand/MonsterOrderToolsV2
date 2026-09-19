use base64::prelude::*;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::fs;
use std::path::{Path, PathBuf};

type HmacSha256 = Hmac<Sha256>;

pub const FILE_MAGIC: &str = "@MonsterOrderSecret@";
pub const SALT: &str = "@M0nst3r$Alt@";

/// 加密配置文件凭据
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Credentials {
    #[serde(rename = "APP_ID", default)]
    pub app_id: String,
    #[serde(rename = "ACCESS_KEY_ID", default)]
    pub access_key_id: String,
    #[serde(rename = "ACCESS_KEY_SECRET", default)]
    pub access_key_secret: String,
    #[serde(default)]
    pub mimo_tts_api_key: String,
    #[serde(default)]
    pub minimax_tts_api_key: String,
    #[serde(default)]
    pub special_user_tts_api_key: String,
    #[serde(default = "default_chat_provider")]
    pub chat_provider: String,
    #[serde(default)]
    pub chat_api_key: String,
}

fn default_chat_provider() -> String {
    "deepseek".to_string()
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
    pub has_mimo_key: bool,
    pub has_vip_tts_key: bool,
}

impl Credentials {
    pub fn to_status(&self, loaded: bool, path: &str) -> CredentialsStatus {
        let access_key_masked = if self.access_key_id.len() >= 8 {
            format!("{}***{}", &self.access_key_id[..4], &self.access_key_id[self.access_key_id.len() - 4..])
        } else if !self.access_key_id.is_empty() {
            "******".to_string()
        } else {
            "未配置".to_string()
        };

        CredentialsStatus {
            loaded,
            file_path: path.to_string(),
            app_id: if self.app_id.is_empty() { "未配置".to_string() } else { self.app_id.clone() },
            access_key_masked,
            chat_provider: self.chat_provider.clone(),
            has_chat_key: !self.chat_api_key.is_empty(),
            has_mimo_key: !self.mimo_tts_api_key.is_empty(),
            has_vip_tts_key: !self.special_user_tts_api_key.is_empty(),
        }
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

/// 获取 credentials.dat 默认路径（统一经 paths::config_dir）
pub fn get_credentials_path() -> PathBuf {
    let p = crate::paths::config_dir().join("credentials.dat");
    if p.exists() {
        return p;
    }
    PathBuf::from("credentials.dat")
}

/// 加载并解密 credentials.dat 文件
pub fn load_credentials(path: Option<&Path>) -> Result<Credentials, String> {
    let p = match path {
        Some(custom) => custom.to_path_buf(),
        None => get_credentials_path(),
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
        return Err("无效的凭据文件魔数 (FILE_MAGIC 不匹配)".to_string());
    }

    let after_magic = &content[FILE_MAGIC.len()..];
    const HMAC_HEX_LENGTH: usize = 64;
    if after_magic.len() < HMAC_HEX_LENGTH {
        return Err("凭据文件长度过短，缺少 HMAC 签名".to_string());
    }

    let stored_hmac = &after_magic[..HMAC_HEX_LENGTH];
    let json_data = &after_magic[HMAC_HEX_LENGTH..];

    let computed_hmac = compute_hmac_hex(json_data, SALT)?;
    if !stored_hmac.eq_ignore_ascii_case(&computed_hmac) {
        return Err("凭据 HMAC 校验失败：数据可能已被损坏或非法篡改！".to_string());
    }

    let creds: Credentials = serde_json::from_str(json_data)
        .map_err(|e| format!("解析凭据 JSON 失败: {}", e))?;

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
        let p = Path::new("MonsterOrderWilds_configs/credentials.dat");
        if p.exists() {
            let res = load_credentials(Some(p));
            assert!(res.is_ok(), "加载真实 credentials.dat 失败: {:?}", res.err());
            let cred = res.unwrap();
            // 仅校验解析结构与关键字段非空，禁止在源码中硬编码真实密钥
            assert!(!cred.app_id.is_empty(), "app_id 不应为空");
            assert!(!cred.access_key_id.is_empty(), "access_key_id 不应为空");
            assert!(!cred.access_key_secret.is_empty(), "access_key_secret 不应为空");
            assert_eq!(cred.chat_provider, "deepseek");
            assert!(!cred.chat_api_key.is_empty(), "chat_api_key 不应为空");
            println!("[PASS] test_load_real_credentials_dat passed");
        }
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
        assert!(res.unwrap_err().contains("FILE_MAGIC"));

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
        assert!(res.unwrap_err().contains("HMAC 校验失败"));

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
            mimo_tts_api_key: "test_mimo_key".into(),
            minimax_tts_api_key: String::new(),
            special_user_tts_api_key: "test_vip_tts".into(),
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

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&temp_dir);
        println!("[PASS] test_credentials_roundtrip passed");
    }
}
