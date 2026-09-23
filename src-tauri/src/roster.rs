//! 点怪禁点名单（黑名单）管理。
//!
//! 名单语义：名单内的怪物**不可被点单** —— 弹幕点怪与「点单排队管理」选怪面板同时生效；
//! 空名单表示不做任何限制（默认状态），因此没有额外的总开关。
//!
//! 并发模型：`RwLock` 读多写少 —— 弹幕热路径只读 `is_blocked`，编辑写入为低频操作。
//! 落盘采用「临时文件 + rename」原子替换，避免写入中断产生半截 JSON。

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// 名单文件名（与 monster_list.json 同目录）
pub const ROSTER_FILE_NAME: &str = "monster_roster.json";

/// 名单持久化数据结构
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RosterData {
    /// 禁点怪物原名列表（去重、按加入顺序保存）
    #[serde(default)]
    pub items: Vec<String>,
}

impl RosterData {
    /// 归一化：去除首尾空白、剔除空项、按首次出现顺序去重
    fn normalized(mut self) -> Self {
        let mut seen = HashSet::new();
        self.items = self
            .items
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .filter(|s| seen.insert(s.clone()))
            .collect();
        self
    }
}

/// 名单管理器
pub struct MonsterRoster {
    data: RwLock<RosterData>,
    path: PathBuf,
}

impl MonsterRoster {
    /// 从文件加载；缺失/损坏时回退默认值（空名单 = 不限制任何点怪）并告警，不阻断启动。
    /// 旧版「可选名单（白名单）」文件（带 `enabled` 字段）语义与黑名单相反，
    /// 沿用会把全部怪物误判为禁点，故判定为旧格式、丢弃内容并迁移为新的空名单文件
    pub fn load(path: Option<&Path>) -> Self {
        let path = path
            .map(|p| p.to_path_buf())
            .unwrap_or_else(Self::default_path);

        let mut legacy = false;
        let data = match fs::read_to_string(&path) {
            Ok(content) => {
                let clean = content.strip_prefix('\u{FEFF}').unwrap_or(&content);
                match serde_json::from_str::<serde_json::Value>(clean) {
                    Ok(serde_json::Value::Object(map)) => {
                        if map.contains_key("enabled") {
                            legacy = true;
                            RosterData::default()
                        } else {
                            match serde_json::from_value(serde_json::Value::Object(map)) {
                                Ok(d) => d,
                                Err(e) => {
                                    crate::log_warn!(
                                        "[Roster] 名单文件结构非法，回退空名单: {}（{}）",
                                        path.display(),
                                        e
                                    );
                                    RosterData::default()
                                }
                            }
                        }
                    }
                    Ok(_) => {
                        crate::log_warn!(
                            "[Roster] 名单文件顶层必须是对象（{{items}}），回退空名单: {}",
                            path.display()
                        );
                        RosterData::default()
                    }
                    Err(e) => {
                        crate::log_warn!(
                            "[Roster] 名单文件解析失败，回退空名单: {}（{}）",
                            path.display(),
                            e
                        );
                        RosterData::default()
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => RosterData::default(),
            Err(e) => {
                crate::log_warn!(
                    "[Roster] 名单文件读取失败，回退空名单: {}（{}）",
                    path.display(),
                    e
                );
                RosterData::default()
            }
        };

        let roster = Self {
            data: RwLock::new(data.normalized()),
            path,
        };

        if legacy {
            crate::log_warn!(
                "[Roster] 检测到旧版「可选名单（白名单）」文件，已按禁点名单机制重置为空: {}",
                roster.path.display()
            );
            // 迁移失败不阻断启动：内存已是空名单，下次修改时仍会重新落盘
            let _ = roster.save();
        }

        roster
    }

    /// 默认路径：可写数据目录下的 monster_roster.json（名单不随包播种，默认不存在）
    pub fn default_path() -> PathBuf {
        crate::paths::config_dir().join(ROSTER_FILE_NAME)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 锁中毒（持锁线程 panic）时取回内部数据，避免禁点判定永久失效
    fn read_data(&self) -> RwLockReadGuard<'_, RosterData> {
        self.data.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_data(&self) -> RwLockWriteGuard<'_, RosterData> {
        self.data.write().unwrap_or_else(|e| e.into_inner())
    }

    /// 先落盘、后提交内存：任一步失败时内存与磁盘保持一致，
    /// 不会出现「内存已改、磁盘没改」的假成功
    fn commit(&self, next: RosterData) -> Result<(), String> {
        Self::write_atomic(&self.path, &next)?;
        *self.write_data() = next;
        Ok(())
    }

    fn write_atomic(path: &Path, data: &RosterData) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建名单目录失败: {}", e))?;
        }
        // JSON 配置统一 UTF-8 无 BOM（见 scripts/check_encoding.py）
        let json =
            serde_json::to_string_pretty(data).map_err(|e| format!("名单序列化失败: {}", e))?;

        let tmp = path.with_extension("tmp");
        {
            let mut file =
                fs::File::create(&tmp).map_err(|e| format!("写入名单临时文件失败: {}", e))?;
            file.write_all(json.as_bytes())
                .map_err(|e| format!("写入名单临时文件失败: {}", e))?;
            file.flush()
                .map_err(|e| format!("写入名单临时文件失败: {}", e))?;
        }
        fs::rename(&tmp, path).map_err(|e| format!("替换名单文件失败: {}", e))
    }

    /// 持久化当前内存数据
    pub fn save(&self) -> Result<(), String> {
        let snapshot = self.read_data().clone();
        Self::write_atomic(&self.path, &snapshot)
    }

    /// 禁点判定：命中名单即拦截；空名单表示不做限制
    pub fn is_blocked(&self, monster: &str) -> bool {
        self.read_data().items.iter().any(|i| i == monster)
    }

    /// 当前名单快照
    pub fn snapshot(&self) -> RosterData {
        self.read_data().clone()
    }

    /// 整表替换（编辑器批量操作后落盘）
    pub fn replace(&self, data: RosterData) -> Result<(), String> {
        self.commit(data.normalized())
    }

    /// 追加去重，返回新增条目数
    pub fn add_all(&self, names: Vec<String>) -> Result<usize, String> {
        let mut next = self.read_data().clone();
        let before = next.items.len();
        next.items.extend(names);
        let next = next.normalized();
        let added = next.items.len().saturating_sub(before);
        self.commit(next)?;
        Ok(added)
    }

    /// 移除单个条目（不存在时为空操作）
    pub fn remove(&self, name: &str) -> Result<(), String> {
        let mut next = self.read_data().clone();
        next.items.retain(|i| i != name);
        self.commit(next.normalized())
    }

    /// 清空名单（清空后不再限制任何点怪）
    pub fn clear(&self) -> Result<(), String> {
        let mut next = self.read_data().clone();
        next.items.clear();
        self.commit(next)
    }

    /// 字典改原名时同步名单（保持原位置），返回是否发生替换
    pub fn rename_item(&self, old: &str, new: &str) -> Result<bool, String> {
        let mut next = self.read_data().clone();
        if !next.items.iter().any(|i| i == old) {
            return Ok(false);
        }
        for item in next.items.iter_mut() {
            if item == old {
                *item = new.to_string();
            }
        }
        self.commit(next.normalized())?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mh_test_roster_{}", tag));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join(ROSTER_FILE_NAME)
    }

    #[test]
    fn test_default_when_file_missing() {
        let path = temp_path("missing");
        let roster = MonsterRoster::load(Some(&path));

        assert!(roster.snapshot().items.is_empty());
        // 空名单不做任何限制
        assert!(!roster.is_blocked("任意怪物"));
        assert!(!roster.is_blocked(""));

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_default_when_file_missing passed");
    }

    #[test]
    fn test_replace_roundtrip_without_bom() {
        let path = temp_path("roundtrip");
        let roster = MonsterRoster::load(Some(&path));
        roster
            .replace(RosterData {
                items: vec!["黑龙".into(), "风暴的棺材".into()],
            })
            .expect("replace should succeed");

        // 落盘文件为 UTF-8 无 BOM 且可被重新加载解析
        let raw = fs::read(&path).unwrap();
        assert_ne!(&raw[..3], b"\xef\xbb\xbf", "JSON 配置不得含 BOM");

        let reloaded = MonsterRoster::load(Some(&path));
        assert_eq!(reloaded.snapshot(), roster.snapshot());
        assert_eq!(reloaded.snapshot().items, vec!["黑龙", "风暴的棺材"]);

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_replace_roundtrip_without_bom passed");
    }

    #[test]
    fn test_add_all_dedup_and_normalize() {
        let path = temp_path("dedup");
        let roster = MonsterRoster::load(Some(&path));
        roster.replace(RosterData::default()).unwrap();

        let added = roster
            .add_all(vec![
                "黑龙".into(),
                "  黑龙  ".into(),
                "".into(),
                "  ".into(),
                "嗟怨震天怨虎龙".into(),
            ])
            .expect("add_all should succeed");
        assert_eq!(added, 2, "重复项与空白项不应计入新增");
        assert_eq!(roster.snapshot().items, vec!["黑龙", "嗟怨震天怨虎龙"]);

        // 二次追加全为重复 → 新增 0
        let added_again = roster
            .add_all(vec!["黑龙".into(), "嗟怨震天怨虎龙".into()])
            .expect("add_all should succeed");
        assert_eq!(added_again, 0);
        assert_eq!(roster.snapshot().items.len(), 2);

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_add_all_dedup_and_normalize passed");
    }

    #[test]
    fn test_is_blocked_only_for_listed() {
        let path = temp_path("blocked");
        let roster = MonsterRoster::load(Some(&path));
        roster
            .replace(RosterData {
                items: vec!["黑龙".into()],
            })
            .unwrap();

        assert!(roster.is_blocked("黑龙"), "名单内的怪物应被禁点");
        assert!(!roster.is_blocked("金狮子"), "名单外的怪物不受限制");

        // 落盘后可重新读回同样的判定
        let reloaded = MonsterRoster::load(Some(&path));
        assert!(reloaded.is_blocked("黑龙"));
        assert!(!reloaded.is_blocked("金狮子"));

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_is_blocked_only_for_listed passed");
    }

    #[test]
    fn test_corrupt_file_falls_back_to_default() {
        let path = temp_path("corrupt");
        fs::write(&path, "{ 这不是合法 JSON").unwrap();

        let roster = MonsterRoster::load(Some(&path));
        assert!(roster.snapshot().items.is_empty());
        assert!(!roster.is_blocked("黑龙"));

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_corrupt_file_falls_back_to_default passed");
    }

    #[test]
    fn test_legacy_whitelist_file_is_discarded() {
        // 旧版「可选名单（白名单）」文件：带 enabled 字段（enabled 为真/假都必须清空，
        // 否则一份「全量白名单」会变成「全量禁点」，导致默认状态下什么都点不了）
        for (tag, enabled) in [("true", "true"), ("false", "false")] {
            let path = temp_path(&format!("legacy_{}", tag));
            fs::write(
                &path,
                format!(r#"{{"enabled": {}, "items": ["黑龙", "麒麟"]}}"#, enabled),
            )
            .unwrap();

            let roster = MonsterRoster::load(Some(&path));
            assert!(
                roster.snapshot().items.is_empty(),
                "旧白名单文件（enabled={}）的内容不得当作禁点名单沿用",
                enabled
            );
            assert!(!roster.is_blocked("黑龙"));
            assert!(!roster.is_blocked("金狮子"));

            // 已迁移落盘为新格式：不再含 enabled 键
            let raw = fs::read_to_string(&path).unwrap();
            assert!(!raw.contains("enabled"), "迁移后的文件不得保留 enabled 字段");
            assert!(raw.contains("items"));

            let _ = fs::remove_dir_all(path.parent().unwrap());
        }

        println!("[PASS] test_legacy_whitelist_file_is_discarded passed");
    }

    #[test]
    fn test_remove_clear_and_rename_item() {
        let path = temp_path("mutate");
        let roster = MonsterRoster::load(Some(&path));
        roster
            .replace(RosterData {
                items: vec!["黑龙".into(), "钢龙".into(), "麒麟".into()],
            })
            .unwrap();

        // 改名同步：保持原位置
        assert!(roster.rename_item("钢龙", "风暴的棺材").unwrap());
        assert_eq!(
            roster.snapshot().items,
            vec!["黑龙", "风暴的棺材", "麒麟"]
        );
        // 未命中的改名不产生变更
        assert!(!roster.rename_item("不存在的怪", "X").unwrap());

        roster.remove("黑龙").unwrap();
        assert_eq!(roster.snapshot().items, vec!["风暴的棺材", "麒麟"]);
        assert!(!roster.is_blocked("黑龙"));

        roster.clear().unwrap();
        assert!(roster.snapshot().items.is_empty());
        assert!(!roster.is_blocked("风暴的棺材"));

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_remove_clear_and_rename_item passed");
    }
}
