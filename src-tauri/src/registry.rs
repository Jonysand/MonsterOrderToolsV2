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

#[cfg(not(windows))]
mod platform {
    use std::collections::HashMap;
    use std::sync::Mutex;

    static MEM_REG: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

    pub fn read_reg_string(subkey: &str, value_name: &str) -> Result<String, String> {
        let key = format!("{}\\{}", subkey, value_name);
        let guard = MEM_REG.lock().unwrap();
        if let Some(map) = guard.as_ref() {
            Ok(map.get(&key).cloned().unwrap_or_default())
        } else {
            Ok(String::new())
        }
    }

    pub fn write_reg_string(subkey: &str, value_name: &str, value: &str) -> Result<(), String> {
        let key = format!("{}\\{}", subkey, value_name);
        let mut guard = MEM_REG.lock().unwrap();
        if guard.is_none() {
            *guard = Some(HashMap::new());
        }
        guard.as_mut().unwrap().insert(key, value.to_string());
        Ok(())
    }

    pub fn delete_reg_value(subkey: &str, value_name: &str) -> Result<(), String> {
        let key = format!("{}\\{}", subkey, value_name);
        let mut guard = MEM_REG.lock().unwrap();
        if let Some(map) = guard.as_mut() {
            map.remove(&key);
        }
        Ok(())
    }
}

/// 从 Windows 注册表 HKCU\Software\MonsterOrderWilds 读取开播身份码 IdCode
pub fn read_id_code() -> Result<String, String> {
    platform::read_reg_string(REG_SUBKEY, REG_VALUE_ID_CODE)
}

/// 将开播身份码 IdCode 写入 Windows 注册表 HKCU\Software\MonsterOrderWilds
pub fn write_id_code(id_code: &str) -> Result<(), String> {
    platform::write_reg_string(REG_SUBKEY, REG_VALUE_ID_CODE, id_code)
}

/// 删除注册表中的 IdCode（主要用于单元测试与环境清理）
pub fn delete_id_code() -> Result<(), String> {
    platform::delete_reg_value(REG_SUBKEY, REG_VALUE_ID_CODE)
}

/// 从 Windows 注册表 HKCU\Software\MonsterOrderWilds 读取 Manbo API Key
/// 遵循原工程规范：ManboApiKey 与 IdCode 同级独立持久化，不落入常规 JSON 配置
pub fn read_manbo_api_key() -> Result<String, String> {
    platform::read_reg_string(REG_SUBKEY, REG_VALUE_MANBO_API_KEY)
}

/// 将 Manbo API Key 写入 Windows 注册表 HKCU\Software\MonsterOrderWilds
pub fn write_manbo_api_key(key: &str) -> Result<(), String> {
    platform::write_reg_string(REG_SUBKEY, REG_VALUE_MANBO_API_KEY, key)
}

/// 删除注册表中的 ManboApiKey（主要用于单元测试与环境清理）
pub fn delete_manbo_api_key() -> Result<(), String> {
    platform::delete_reg_value(REG_SUBKEY, REG_VALUE_MANBO_API_KEY)
}

#[cfg(test)]
mod tests {
    use super::*;

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
