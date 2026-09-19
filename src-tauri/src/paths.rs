//! 统一资源与数据目录解析。
//! 安装版资源随包分发（tauri.conf.json `bundle.resources`），绿色版/开发环境直接使用仓库目录。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 配置/数据子目录名（与仓库布局及原工程一致）
pub const CONFIG_DIR_NAME: &str = "MonsterOrderWilds_configs";

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

/// 可写数据目录（MonsterOrderWilds_configs）解析顺序：
/// 1. exe 同级（安装版：用户可编辑的随包数据目录；对齐原工程「恒定取 exe 同级」语义）
/// 2. cwd 下（绿色版 / 在仓库根目录直接运行）
/// 3. cwd/.. 下（`tauri dev` / `cargo test` 时 cwd = src-tauri）
/// 4. 兜底：创建 exe 同级目录
///
/// 关键点：按「是否已存在」逐级判定，故开发态 exe 同级不存在时会正确回退到仓库根目录，
/// 而安装版则恒定使用 exe 同级 —— 避免以不同工作目录启动同一 exe 时读写到不同数据。
pub fn config_dir() -> PathBuf {
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
/// 不播种数据库与用户配置，避免覆盖历史数据
pub fn ensure_seeded() {
    let target_dir = config_dir();
    let Some(root) = resource_root() else { return };

    for rel in [
        "monster_list.json",
        "local_voices.zip",
        "dict/stop_words.utf8",
        "dict/user.dict.utf8",
    ] {
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
}
