//! 统一资源与数据目录解析。
//! 安装版资源随包分发（tauri.conf.json `bundle.resources`），绿色版/开发环境直接使用仓库目录。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 配置/数据子目录名（与仓库布局及原工程一致）
pub const CONFIG_DIR_NAME: &str = "MonsterOrderWilds_configs";

/// macOS 用户数据根目录名，必须与 tauri.conf.json 的 `identifier` 一致。
/// 二者漂移会导致数据被写到一个"看似正确但永不被读取"的目录，
/// 故由 `test_macos_data_dir_name_matches_bundle_identifier` 单测守护。
#[cfg(target_os = "macos")]
pub const MACOS_APP_SUPPORT_DIR_NAME: &str = "com.jonysand.danmutools";

static RESOURCE_DIR: OnceLock<PathBuf> = OnceLock::new();

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
}

/// 由 setup() 注入 Tauri 资源目录；未注入时回退 exe 同级
pub fn init_resource_dir(dir: Option<PathBuf>) {
    if let Some(d) = dir.or_else(exe_dir) {
        let _ = RESOURCE_DIR.set(d);
    }
}

/// 资源根目录：Tauri resource_dir → exe 同级
pub fn resource_root() -> Option<PathBuf> {
    if let Some(d) = RESOURCE_DIR.get() {
        return Some(d.clone());
    }
    exe_dir()
}

/// 打包态判定：exe 所在目录是否为 macOS 应用包内的 `Contents/MacOS`。
/// 该判据同时覆盖 .app 与从只读 DMG 直接启动的场景，而开发态
/// （`tauri dev` / `cargo test`，exe 位于 `target/debug`）不受影响。
#[cfg(target_os = "macos")]
fn is_macos_bundle_exe_dir(dir: &Path) -> bool {
    dir.ends_with("Contents/MacOS")
}

/// macOS 规范用户数据目录：`~/Library/Application Support/<identifier>/MonsterOrderWilds_configs`
#[cfg(target_os = "macos")]
fn macos_user_config_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join(MACOS_APP_SUPPORT_DIR_NAME)
            .join(CONFIG_DIR_NAME),
    )
}

/// 可写数据目录（MonsterOrderWilds_configs）解析顺序：
///
/// macOS 打包态优先：exe 同级位于 `.app/Contents/MacOS` 包体内，写入会破坏签名密封，
/// 且从只读 DMG 启动时静默失败（数据/日志/数据库全部丢失），故改用系统规范数据目录。
///
/// 其余情况：
/// 1. exe 同级（安装版：用户可编辑的随包数据目录；对齐原工程「恒定取 exe 同级」语义）
/// 2. cwd 下（绿色版 / 在仓库根目录直接运行）
/// 3. cwd/.. 下（`tauri dev` / `cargo test` 时 cwd = src-tauri）
/// 4. 兜底：创建 exe 同级目录
///
/// 关键点：按「是否已存在」逐级判定，故开发态 exe 同级不存在时会正确回退到仓库根目录，
/// 而安装版则恒定使用 exe 同级 —— 避免以不同工作目录启动同一 exe 时读写到不同数据。
pub fn config_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    if exe_dir().map(|d| is_macos_bundle_exe_dir(&d)).unwrap_or(false) {
        if let Some(dir) = macos_user_config_dir() {
            let _ = std::fs::create_dir_all(&dir);
            return dir;
        }
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = exe_dir() {
        candidates.push(dir.join(CONFIG_DIR_NAME));
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(CONFIG_DIR_NAME));
        candidates.push(cwd.join("..").join(CONFIG_DIR_NAME));
    }

    for c in &candidates {
        if c.is_dir() {
            return c.clone();
        }
    }

    let fallback = exe_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(CONFIG_DIR_NAME);
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

/// 在「可写数据目录 → 安装资源目录 → 开发目录」中查找资源文件。
/// `rel` 为相对数据目录的路径，如 `monster_list.json`、`voices/manbo.mp3`
pub fn find_resource(rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    let mut candidates = vec![config_dir().join(rel_path)];

    if let Some(root) = resource_root() {
        // 安装版资源映射目标（resource_dir/MonsterOrderWilds_configs/...）
        candidates.push(root.join(CONFIG_DIR_NAME).join(rel_path));
        // 资源直接落在资源根目录
        candidates.push(root.join(rel_path));
    }

    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(rel_path));
        candidates.push(cwd.join(CONFIG_DIR_NAME).join(rel_path));
        candidates.push(cwd.join("..").join(CONFIG_DIR_NAME).join(rel_path));
    }

    candidates.into_iter().find(|p| p.exists())
}

/// 首次运行播种：exe 同级配置目录缺失静态资源（怪物表 / 本地语音包 / 分词词典）时从安装资源复制。
/// 不播种数据库与用户配置，避免覆盖历史数据。
///
/// Lite 形态只播种点怪核心资源（怪物表）：本地语音包、分词词典与 voices/ 属 TTS 与打卡 AI
/// 素材，Lite 下既不加载也不应复制，避免在与完整版共用数据目录时凭空写入无用文件。
pub fn ensure_seeded() {
    let target_dir = config_dir();
    let Some(root) = resource_root() else { return };
    seed_into(&target_dir, &root, crate::IS_LITE);
}

/// 播种的实际实现（目录可注入，便于对 Lite / 完整版两条分支直接断言）。
///
/// `lite=true` 时**只**处理怪物表：语音包、分词词典、voices/ 一律不复制，
/// 也不读取它们 —— 共用数据目录时不会写入 Lite 用不到的文件。
pub fn seed_into(target_dir: &Path, root: &Path, lite: bool) {
    let mut required: Vec<&str> = vec!["monster_list.json"];
    if !lite {
        required.extend_from_slice(&[
            "local_voices.zip",
            "dict/stop_words.utf8",
            "dict/user.dict.utf8",
        ]);
    }

    for rel in required {
        let target = target_dir.join(rel);
        if target.exists() {
            continue;
        }
        for src in [
            root.join(CONFIG_DIR_NAME).join(rel),
            root.join(rel),
        ] {
            if src.is_file() {
                if let Some(parent) = target.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::copy(&src, &target);
                break;
            }
        }
    }

    if lite {
        return;
    }

    let voices_target = target_dir.join("voices");
    if !voices_target.is_dir() {
        for src in [
            root.join(CONFIG_DIR_NAME).join("voices"),
            root.join("voices"),
        ] {
            if src.is_dir() {
                copy_dir_recursive(&src, &voices_target);
                break;
            }
        }
    }
}

/// 递归复制目录（播种用，失败静默 —— 缺失资源由启动检查统一告警）
fn copy_dir_recursive(src: &Path, dst: &Path) {
    if std::fs::create_dir_all(dst).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(src) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &target);
        } else {
            let _ = std::fs::copy(&path, &target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_dir_prefers_run_directory() {
        // cargo test / tauri dev：cwd = src-tauri，应回退到仓库根目录的 MonsterOrderWilds_configs，
        // 而非 exe 同级（target/debug）的资源副本
        let cwd = std::env::current_dir().unwrap();
        let expected = [cwd.join(CONFIG_DIR_NAME), cwd.join("..").join(CONFIG_DIR_NAME)]
            .into_iter()
            .find(|p| p.is_dir())
            .expect("开发环境应存在仓库配置目录");
        assert_eq!(config_dir(), expected);
    }

    #[test]
    fn test_find_resource_resolves_real_files() {
        let p = find_resource("monster_list.json").expect("应能找到 monster_list.json");
        assert!(p.is_file());
        assert!(p.to_string_lossy().contains(CONFIG_DIR_NAME));

        let v = find_resource("voices").expect("应能找到 voices 目录");
        assert!(v.is_dir());

        assert!(find_resource("不存在的资源_xyz.bin").is_none());
        println!("[PASS] test_find_resource_resolves_real_files passed");
    }

    #[test]
    fn test_resource_root_falls_back_to_exe_dir() {
        let root = resource_root().expect("资源根目录应可解析");
        assert!(root.exists());
        let exe = std::env::current_exe().unwrap();
        assert_eq!(root, exe.parent().unwrap().to_path_buf());
        println!("[PASS] test_resource_root_falls_back_to_exe_dir passed");
    }

    /// 打包态判据只认 `.app/Contents/MacOS`，不得把开发态 target 目录误判为包内
    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_bundle_exe_dir_detection() {
        assert!(is_macos_bundle_exe_dir(Path::new(
            "/Applications/MonsterOrderWilds-Ascendance.app/Contents/MacOS"
        )));
        assert!(is_macos_bundle_exe_dir(Path::new(
            "/Volumes/MonsterOrderWilds-Ascendance/MonsterOrderWilds-Ascendance.app/Contents/MacOS"
        )));
        // 开发态：cargo test / tauri dev 的 exe 同级
        assert!(!is_macos_bundle_exe_dir(Path::new(
            "/Users/x/proj/src-tauri/target/debug"
        )));
        // 仅外层是 .app 但并非可执行目录
        assert!(!is_macos_bundle_exe_dir(Path::new(
            "/Applications/MonsterOrderWilds-Ascendance.app/Contents"
        )));
        println!("[PASS] test_macos_bundle_exe_dir_detection passed");
    }

    /// 数据目录名与 bundle identifier 漂移会导致数据写入"永不被读取"的目录，必须锁死
    #[cfg(target_os = "macos")]
    #[test]
    fn test_macos_data_dir_name_matches_bundle_identifier() {
        let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tauri.conf.json"))
            .expect("应能读取 tauri.conf.json");
        let val: serde_json::Value = serde_json::from_str(&raw).expect("tauri.conf.json 应为合法 JSON");
        assert_eq!(
            val["identifier"].as_str(),
            Some(MACOS_APP_SUPPORT_DIR_NAME),
            "MACOS_APP_SUPPORT_DIR_NAME 与 tauri.conf.json 的 identifier 不一致"
        );
        println!("[PASS] test_macos_data_dir_name_matches_bundle_identifier passed");
    }

    #[test]
    fn test_copy_dir_recursive() {
        let base = std::env::temp_dir().join("mh_test_paths_copy");
        let _ = std::fs::remove_dir_all(&base);
        let src_root = base.join("src_dir");
        std::fs::create_dir_all(src_root.join("sub")).unwrap();
        std::fs::write(src_root.join("a.mp3"), b"a").unwrap();
        std::fs::write(src_root.join("sub").join("b.mp3"), b"b").unwrap();

        let dst = base.join("dst_dir");
        copy_dir_recursive(&src_root, &dst);

        assert!(dst.join("a.mp3").is_file());
        assert!(dst.join("sub").join("b.mp3").is_file());

        let _ = std::fs::remove_dir_all(&base);
        println!("[PASS] test_copy_dir_recursive passed");
    }

    /// 播种分支：Lite 只复制怪物表；完整版还会复制语音包、分词词典与 voices/
    #[test]
    fn test_seed_into_lite_only_copies_core_resource() {
        let base = std::env::temp_dir().join("mh_test_paths_seed");
        let _ = std::fs::remove_dir_all(&base);
        // 伪造"安装资源目录"：包含全部四类资源
        let root = base.join("root");
        std::fs::create_dir_all(root.join(CONFIG_DIR_NAME).join("dict")).unwrap();
        std::fs::write(root.join(CONFIG_DIR_NAME).join("monster_list.json"), b"{}").unwrap();
        std::fs::write(root.join(CONFIG_DIR_NAME).join("local_voices.zip"), b"zip").unwrap();
        std::fs::write(
            root.join(CONFIG_DIR_NAME).join("dict").join("stop_words.utf8"),
            b"sw",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(CONFIG_DIR_NAME).join("voices")).unwrap();
        std::fs::write(root.join(CONFIG_DIR_NAME).join("voices").join("a.mp3"), b"a").unwrap();

        // Lite：只有怪物表被播种
        let lite_target = base.join("lite");
        std::fs::create_dir_all(&lite_target).unwrap();
        seed_into(&lite_target, &root, true);
        assert!(lite_target.join("monster_list.json").is_file(), "核心资源应播种");
        assert!(!lite_target.join("local_voices.zip").exists(), "Lite 不得复制语音包");
        assert!(!lite_target.join("dict").exists(), "Lite 不得复制分词词典");
        assert!(!lite_target.join("voices").exists(), "Lite 不得复制 voices/");

        // 完整版：全部播种
        let full_target = base.join("full");
        std::fs::create_dir_all(&full_target).unwrap();
        seed_into(&full_target, &root, false);
        assert!(full_target.join("monster_list.json").is_file());
        assert!(full_target.join("local_voices.zip").is_file());
        assert!(full_target.join("dict").join("stop_words.utf8").is_file());
        assert!(full_target.join("voices").join("a.mp3").is_file());

        // 已存在的目标文件不得被覆盖（避免覆盖用户编辑过的词库）
        std::fs::write(full_target.join("monster_list.json"), b"USER_EDIT").unwrap();
        seed_into(&full_target, &root, false);
        assert_eq!(
            std::fs::read(full_target.join("monster_list.json")).unwrap(),
            b"USER_EDIT",
            "已存在的词库不得被安装资源覆盖"
        );

        let _ = std::fs::remove_dir_all(&base);
        println!("[PASS] test_seed_into_lite_only_copies_core_resource passed");
    }
}
