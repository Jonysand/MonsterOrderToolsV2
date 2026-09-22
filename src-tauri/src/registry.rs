/// 注册表持久化模块：遵循原工程规范存储在 HKCU\Software\MonsterOrderWilds\IdCode
pub const REG_SUBKEY: &str = "Software\\MonsterOrderWilds";
pub const REG_VALUE_ID_CODE: &str = "IdCode";
pub const REG_VALUE_MANBO_API_KEY: &str = "ManboApiKey";

#[cfg(windows)]
mod platform {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    type HKEY = *mut std::ffi::c_void;
    type LSTATUS = i32;

    const HKEY_CURRENT_USER: HKEY = 0x80000001usize as HKEY;
    const KEY_READ: u32 = 0x20019;
    const KEY_WRITE: u32 = 0x20006;
    const REG_SZ: u32 = 1;
    const ERROR_SUCCESS: LSTATUS = 0;
    const REG_OPTION_NON_VOLATILE: u32 = 0;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegOpenKeyExW(
            hKey: HKEY,
            lpSubKey: *const u16,
            ulOptions: u32,
            samDesired: u32,
            phkResult: *mut HKEY,
        ) -> LSTATUS;

        fn RegCreateKeyExW(
            hKey: HKEY,
            lpSubKey: *const u16,
            Reserved: u32,
            lpClass: *mut u16,
            dwOptions: u32,
            samDesired: u32,
            lpSecurityAttributes: *mut std::ffi::c_void,
            phkResult: *mut HKEY,
            lpdwDisposition: *mut u32,
        ) -> LSTATUS;

        fn RegQueryValueExW(
            hKey: HKEY,
            lpValueName: *const u16,
            lpReserved: *mut u32,
            lpType: *mut u32,
            lpData: *mut u8,
            lpcbData: *mut u32,
        ) -> LSTATUS;

        fn RegSetValueExW(
            hKey: HKEY,
            lpValueName: *const u16,
            Reserved: u32,
            dwType: u32,
            lpData: *const u8,
            cbData: u32,
        ) -> LSTATUS;

        fn RegDeleteValueW(hKey: HKEY, lpValueName: *const u16) -> LSTATUS;

        fn RegCloseKey(hKey: HKEY) -> LSTATUS;
    }

    fn to_wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    pub fn read_reg_string(subkey: &str, value_name: &str) -> Result<String, String> {
        let wide_subkey = to_wide(subkey);
        let wide_val_name = to_wide(value_name);
        let mut h_key: HKEY = ptr::null_mut();

        unsafe {
            let status = RegOpenKeyExW(
                HKEY_CURRENT_USER,
                wide_subkey.as_ptr(),
                0,
                KEY_READ,
                &mut h_key,
            );
            if status != ERROR_SUCCESS {
                return Ok(String::new());
            }

            let mut data_type: u32 = 0;
            let mut byte_size: u32 = 0;
            let status = RegQueryValueExW(
                h_key,
                wide_val_name.as_ptr(),
                ptr::null_mut(),
                &mut data_type,
                ptr::null_mut(),
                &mut byte_size,
            );

            if status != ERROR_SUCCESS || byte_size == 0 {
                let _ = RegCloseKey(h_key);
                return Ok(String::new());
            }

            let mut buffer: Vec<u8> = vec![0u8; byte_size as usize];
            let status = RegQueryValueExW(
                h_key,
                wide_val_name.as_ptr(),
                ptr::null_mut(),
                &mut data_type,
                buffer.as_mut_ptr(),
                &mut byte_size,
            );
            let _ = RegCloseKey(h_key);

            if status != ERROR_SUCCESS {
                return Ok(String::new());
            }

            if data_type == REG_SZ {
                let u16_slice = std::slice::from_raw_parts(
                    buffer.as_ptr() as *const u16,
                    byte_size as usize / 2,
                );
                let trimmed = match u16_slice.iter().position(|&c| c == 0) {
                    Some(pos) => &u16_slice[..pos],
                    None => u16_slice,
                };
                String::from_utf16(trimmed).map_err(|e| e.to_string())
            } else {
                Ok(String::new())
            }
        }
    }

    pub fn write_reg_string(subkey: &str, value_name: &str, value: &str) -> Result<(), String> {
        let wide_subkey = to_wide(subkey);
        let wide_val_name = to_wide(value_name);
        let wide_val = to_wide(value);
        let mut h_key: HKEY = ptr::null_mut();
        let mut disp: u32 = 0;

        unsafe {
            let status = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                wide_subkey.as_ptr(),
                0,
                ptr::null_mut(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                ptr::null_mut(),
                &mut h_key,
                &mut disp,
            );
            if status != ERROR_SUCCESS {
                return Err(format!("RegCreateKeyExW 失败，错误码: {}", status));
            }

            let byte_size = (wide_val.len() * 2) as u32;
            let status = RegSetValueExW(
                h_key,
                wide_val_name.as_ptr(),
                0,
                REG_SZ,
                wide_val.as_ptr() as *const u8,
                byte_size,
            );
            let _ = RegCloseKey(h_key);

            if status != ERROR_SUCCESS {
                return Err(format!("RegSetValueExW 失败，错误码: {}", status));
            }

            Ok(())
        }
    }

    pub fn delete_reg_value(subkey: &str, value_name: &str) -> Result<(), String> {
        let wide_subkey = to_wide(subkey);
        let wide_val_name = to_wide(value_name);
        let mut h_key: HKEY = ptr::null_mut();

        unsafe {
            let status = RegOpenKeyExW(
                HKEY_CURRENT_USER,
                wide_subkey.as_ptr(),
                0,
                KEY_WRITE,
                &mut h_key,
            );
            if status != ERROR_SUCCESS {
                return Ok(());
            }

            let _ = RegDeleteValueW(h_key, wide_val_name.as_ptr());
            let _ = RegCloseKey(h_key);
            Ok(())
        }
    }
}

/// 非 Windows 平台的「注册表」等价物：数据目录下的加密键值文件。
///
/// 对齐原工程规范：身份码与 ManboApiKey 必须独立于常规 JSON 配置持久化，且受加密托管。
/// Windows 用 HKCU 注册表承载，macOS/Linux 则使用 `registry.dat` ——
/// 复用 credentials.dat 的信封格式（Base64 + `@MonsterOrderSecret@` + HMAC-SHA256 签名），
/// 因而同样具备篡改检测，且不落入 configs.json。
#[cfg(not(windows))]
mod platform {
    use base64::prelude::*;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard};

    const STORE_FILE: &str = "registry.dat";

    /// 串行化「读-改-写」，避免并发调用互相覆盖（Windows 注册表侧由系统保证原子性）
    static STORE_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> MutexGuard<'static, ()> {
        STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn store_path() -> PathBuf {
        crate::paths::config_dir().join(STORE_FILE)
    }

    fn value_key(subkey: &str, value_name: &str) -> String {
        format!("{}\\{}", subkey, value_name)
    }

    fn load_store() -> BTreeMap<String, String> {
        load_store_at(&store_path())
    }

    /// 读取并验签整份键值存储。文件缺失/损坏/被篡改一律按空存储处理并告警，
    /// 不阻断启动（与 Windows 侧「读取失败返回空串」的语义一致）。
    pub(super) fn load_store_at(path: &Path) -> BTreeMap<String, String> {
        let Ok(raw) = std::fs::read(path) else {
            // 首次运行尚无该文件属正常情况，不告警
            return BTreeMap::new();
        };
        let text = String::from_utf8_lossy(&raw).trim().to_string();

        let decoded = match BASE64_STANDARD.decode(&text) {
            Ok(d) => d,
            Err(e) => {
                crate::log_warn!("[Registry] {} Base64 解码失败，按空存储处理: {}", STORE_FILE, e);
                return BTreeMap::new();
            }
        };
        let Ok(content) = String::from_utf8(decoded) else {
            crate::log_warn!("[Registry] {} 内容非有效 UTF-8，按空存储处理", STORE_FILE);
            return BTreeMap::new();
        };
        let Some(rest) = content.strip_prefix(crate::credentials::FILE_MAGIC) else {
            crate::log_warn!("[Registry] {} 文件魔数不匹配，按空存储处理", STORE_FILE);
            return BTreeMap::new();
        };

        const HMAC_HEX_LENGTH: usize = 64;
        if rest.len() < HMAC_HEX_LENGTH {
            crate::log_warn!("[Registry] {} 缺少 HMAC 签名，按空存储处理", STORE_FILE);
            return BTreeMap::new();
        }
        let (stored_hmac, json_data) = rest.split_at(HMAC_HEX_LENGTH);
        match crate::credentials::compute_hmac_hex(json_data, crate::credentials::SALT) {
            Ok(computed) if computed.eq_ignore_ascii_case(stored_hmac) => {}
            _ => {
                crate::log_warn!(
                    "[Registry] {} HMAC 校验失败（数据损坏或被篡改），按空存储处理",
                    STORE_FILE
                );
                return BTreeMap::new();
            }
        }

        serde_json::from_str(json_data).unwrap_or_else(|e| {
            crate::log_warn!("[Registry] {} JSON 解析失败，按空存储处理: {}", STORE_FILE, e);
            BTreeMap::new()
        })
    }

    fn save_store(map: &BTreeMap<String, String>) -> Result<(), String> {
        save_store_at(&store_path(), map)
    }

    /// 加密并原子写入整份键值存储（临时文件 + rename，避免中途崩溃留下半截文件）
    pub(super) fn save_store_at(path: &Path, map: &BTreeMap<String, String>) -> Result<(), String> {
        let json_data = serde_json::to_string(map).map_err(|e| e.to_string())?;
        let hmac_hex = crate::credentials::compute_hmac_hex(&json_data, crate::credentials::SALT)?;
        let combined = format!("{}{}{}", crate::credentials::FILE_MAGIC, hmac_hex, json_data);
        let encoded = BASE64_STANDARD.encode(combined.as_bytes());

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建数据目录失败: {}", e))?;
        }
        let tmp = path.with_extension("dat.tmp");
        std::fs::write(&tmp, encoded).map_err(|e| format!("写入 {} 失败: {}", STORE_FILE, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("提交 {} 失败: {}", STORE_FILE, e))
    }

    pub fn read_reg_string(subkey: &str, value_name: &str) -> Result<String, String> {
        let _guard = lock();
        Ok(load_store()
            .get(&value_key(subkey, value_name))
            .cloned()
            .unwrap_or_default())
    }

    pub fn write_reg_string(subkey: &str, value_name: &str, value: &str) -> Result<(), String> {
        let _guard = lock();
        let key = value_key(subkey, value_name);
        let mut map = load_store();
        // 值未变化时不重复落盘：save() 会被悬浮窗位置防抖任务频繁调用
        if map.get(&key).map(|v| v.as_str()) == Some(value) {
            return Ok(());
        }
        map.insert(key, value.to_string());
        save_store(&map)
    }

    pub fn delete_reg_value(subkey: &str, value_name: &str) -> Result<(), String> {
        let _guard = lock();
        let mut map = load_store();
        if map.remove(&value_key(subkey, value_name)).is_none() {
            return Ok(());
        }
        save_store(&map)
    }
}

/// 读取开播身份码 IdCode
/// （Windows：HKCU\Software\MonsterOrderWilds；其他平台：加密的 registry.dat）
pub fn read_id_code() -> Result<String, String> {
    platform::read_reg_string(REG_SUBKEY, REG_VALUE_ID_CODE)
}

/// 写入开播身份码 IdCode
/// （Windows：HKCU\Software\MonsterOrderWilds；其他平台：加密的 registry.dat）
pub fn write_id_code(id_code: &str) -> Result<(), String> {
    platform::write_reg_string(REG_SUBKEY, REG_VALUE_ID_CODE, id_code)
}

/// 删除持久化的 IdCode（主要用于单元测试与环境清理）
pub fn delete_id_code() -> Result<(), String> {
    platform::delete_reg_value(REG_SUBKEY, REG_VALUE_ID_CODE)
}

/// 读取 Manbo API Key。
/// 遵循原工程规范：ManboApiKey 与 IdCode 同级独立持久化，不落入常规 JSON 配置
pub fn read_manbo_api_key() -> Result<String, String> {
    platform::read_reg_string(REG_SUBKEY, REG_VALUE_MANBO_API_KEY)
}

/// 写入 Manbo API Key
/// （Windows：HKCU\Software\MonsterOrderWilds；其他平台：加密的 registry.dat）
pub fn write_manbo_api_key(key: &str) -> Result<(), String> {
    platform::write_reg_string(REG_SUBKEY, REG_VALUE_MANBO_API_KEY, key)
}

/// 删除持久化的 ManboApiKey（主要用于单元测试与环境清理）
pub fn delete_manbo_api_key() -> Result<(), String> {
    platform::delete_reg_value(REG_SUBKEY, REG_VALUE_MANBO_API_KEY)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(windows))]
    use base64::prelude::*;

    #[test]
    fn test_registry_id_code_roundtrip() {
        // 保存原注册表值以便测试完成后还原
        let original_code = read_id_code().unwrap_or_default();

        let test_code = "TEST_ID_CODE_MHDANMU_987654";
        let write_res = write_id_code(test_code);
        assert!(write_res.is_ok(), "写入注册表失败: {:?}", write_res);

        let read_back = read_id_code().unwrap_or_default();
        assert_eq!(read_back, test_code, "注册表回读值与写入值不匹配");

        // 还原原注册表值或清理
        if original_code.is_empty() {
            let _ = delete_id_code();
        } else {
            let _ = write_id_code(&original_code);
        }

        println!("[PASS] test_registry_id_code_roundtrip passed");
    }

    /// 非 Windows：存储必须落盘且为密文 —— 这是「重启后身份码仍在」的根因验证
    #[cfg(not(windows))]
    #[test]
    fn test_registry_store_persists_encrypted_on_disk() {
        use std::collections::BTreeMap;

        let dir = std::env::temp_dir().join("mh_registry_store_test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("registry.dat");

        let secret = "TEST_ID_CODE_MHDANMU_987654";
        let mut map = BTreeMap::new();
        map.insert(
            format!("{}\\{}", REG_SUBKEY, REG_VALUE_ID_CODE),
            secret.to_string(),
        );
        super::platform::save_store_at(&path, &map).expect("加密落盘应成功");

        // 落盘内容不得暴露明文（区别于旧的内存 HashMap：此处必须真的有文件）
        let raw = std::fs::read_to_string(&path).expect("文件应存在");
        assert!(!raw.contains(secret), "存储文件不得包含明文身份码");
        assert!(
            base64::prelude::BASE64_STANDARD.decode(raw.trim()).is_ok(),
            "存储文件应为 Base64 信封"
        );

        // 重新从磁盘加载（等价于进程重启后的读取路径）
        let reloaded = super::platform::load_store_at(&path);
        assert_eq!(
            reloaded.get(&format!("{}\\{}", REG_SUBKEY, REG_VALUE_ID_CODE)),
            Some(&secret.to_string()),
            "重新加载后应取回原值"
        );

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_registry_store_persists_encrypted_on_disk passed");
    }

    /// 非 Windows：HMAC 被篡改时必须整体拒绝，而不是返回被污染的凭据
    #[cfg(not(windows))]
    #[test]
    fn test_registry_store_rejects_tampered_content() {
        use std::collections::BTreeMap;

        let dir = std::env::temp_dir().join("mh_registry_tamper_test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("registry.dat");

        let mut map = BTreeMap::new();
        map.insert(
            format!("{}\\{}", REG_SUBKEY, REG_VALUE_ID_CODE),
            "REAL_CODE".to_string(),
        );
        super::platform::save_store_at(&path, &map).unwrap();

        // 解出信封后替换 JSON 明文，签名保持不变 → 校验必须失败
        let raw = std::fs::read_to_string(&path).unwrap();
        let decoded = base64::prelude::BASE64_STANDARD.decode(raw.trim()).unwrap();
        let content = String::from_utf8(decoded).unwrap();
        let tampered = content.replace("REAL_CODE", "HACKED_XX");
        assert_ne!(content, tampered, "篡改替换应生效");
        std::fs::write(
            &path,
            base64::prelude::BASE64_STANDARD.encode(tampered.as_bytes()),
        )
        .unwrap();

        let reloaded = super::platform::load_store_at(&path);
        assert!(reloaded.is_empty(), "校验失败时必须按空存储处理");

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_registry_store_rejects_tampered_content passed");
    }

    #[test]
    fn test_registry_manbo_api_key_roundtrip() {
        // 保存原值以便测试完成后还原
        let original = read_manbo_api_key().unwrap_or_default();

        let test_key = "TEST_MANBO_API_KEY_123456";
        let write_res = write_manbo_api_key(test_key);
        assert!(write_res.is_ok(), "写入 ManboApiKey 失败: {:?}", write_res);

        let read_back = read_manbo_api_key().unwrap_or_default();
        assert_eq!(read_back, test_key, "ManboApiKey 回读值与写入值不匹配");

        if original.is_empty() {
            let _ = delete_manbo_api_key();
        } else {
            let _ = write_manbo_api_key(&original);
        }

        println!("[PASS] test_registry_manbo_api_key_roundtrip passed");
    }
}
