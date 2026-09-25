//! 点怪禁点名单（黑名单）管理。
//!
//! 名单语义：名单内的怪物**不可被点单** —— 弹幕点怪与「点单排队管理」选怪面板同时生效；
//! 空名单表示不做任何限制（默认状态），因此没有额外的总开关。
//!
//! 并发模型：`RwLock` 读多写少 —— 弹幕热路径只读 `is_blocked`，编辑写入为低频操作。
//! 全部写操作经**同一把编辑互斥锁**串行："取旧快照 → 计算目标 → 原子落盘 → 提交内存 → 递增版本"
//! 是一段不可分割的临界区，因此两个编辑不会各自基于旧快照写回而互相覆盖。
//! 落盘采用「同目录唯一临时文件 + rename」原子替换，避免写入中断产生半截 JSON。
//!
//! `revision` 只存在于内存中（不写入用户名单文件），供 IPC 做 CAS：
//! 前端拿着旧版本提交整表时会被拒绝，而不是把后端的新名单抹掉。

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

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

/// 名单权威快照（IPC 用）：数据 + 逻辑版本号。
/// 用户导入/导出的 JSON 仍是纯 `{items: [...]}`，版本号不写入用户文件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RosterSnapshot {
    pub data: RosterData,
    /// 每次真实写入递增；顺序未变或写入失败时不递增
    pub revision: u64,
}

/// 名单管理器
pub struct MonsterRoster {
    data: RwLock<RosterData>,
    path: PathBuf,
    /// 低频编辑互斥锁：所有写操作共用，保证"落盘 → 提交内存 → 递增版本"原子
    edit_lock: Mutex<()>,
    revision: RwLock<u64>,
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
            edit_lock: Mutex::new(()),
            revision: RwLock::new(0),
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

    fn read_revision(&self) -> u64 {
        *self.revision.read().unwrap_or_else(|e| e.into_inner())
    }

    /// 拿编辑锁（中毒时取回，不因一次 panic 永久停写）
    fn lock_edit(&self) -> std::sync::MutexGuard<'_, ()> {
        self.edit_lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 串行编辑原语：先落盘、成功后才提交内存并递增版本。
    /// 任一步失败时内存与磁盘都保持不变，不会出现「内存已改、磁盘没改」的假成功。
    fn commit_locked(&self, next: RosterData) -> Result<u64, String> {
        let next = next.normalized();
        if *self.read_data() == next {
            // 内容未变化：不写盘、不递增版本（重复的整表提交不该产生版本漂移）
            return Ok(self.read_revision());
        }
        Self::write_atomic(&self.path, &next)?;
        *self.write_data() = next;
        let mut rev = self.revision.write().unwrap_or_else(|e| e.into_inner());
        *rev += 1;
        Ok(*rev)
    }

    fn write_atomic(path: &Path, data: &RosterData) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建名单目录失败: {}", e))?;
        }
        // JSON 配置统一 UTF-8 无 BOM（见 scripts/check_encoding.py）
        let json =
            serde_json::to_string_pretty(data).map_err(|e| format!("名单序列化失败: {}", e))?;

        // 同目录唯一临时名：直接调用公有 API 的多个线程不会互相踩同一个 .tmp
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_file_name(format!(
            "{}.{}.{}.tmp",
            ROSTER_FILE_NAME,
            std::process::id(),
            seq
        ));
        let write_result = (|| -> std::io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(e) = write_result {
            let _ = fs::remove_file(&tmp);
            return Err(format!("写入名单临时文件失败: {}", e));
        }
        if let Err(e) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(format!("替换名单文件失败: {}（原文件保持不变）", e));
        }
        Ok(())
    }

    /// 持久化当前内存数据
    pub fn save(&self) -> Result<(), String> {
        let _guard = self.lock_edit();
        let snapshot = self.read_data().clone();
        Self::write_atomic(&self.path, &snapshot)
    }

    /// 禁点判定：命中名单即拦截；空名单表示不做限制
    pub fn is_blocked(&self, monster: &str) -> bool {
        self.read_data().items.iter().any(|i| i == monster)
    }

    /// 当前名单数据
    pub fn snapshot(&self) -> RosterData {
        self.read_data().clone()
    }

    /// 名单只读 guard。
    ///
    /// 供调用方在**同一临界区**内完成"检查 → 动作"（例如选怪面板的
    /// "禁点校验 → 入队"），使一次名单编辑无法插到检查与动作之间。
    /// 调用方必须遵守锁序 `queue_mutex → 字典读 → 名单读`，不得反向获取。
    pub fn read_guard(&self) -> RwLockReadGuard<'_, RosterData> {
        self.read_data()
    }

    /// 当前权威快照（数据 + 版本），供 IPC 做 CAS
    pub fn snapshot_versioned(&self) -> RosterSnapshot {
        RosterSnapshot {
            data: self.read_data().clone(),
            revision: self.read_revision(),
        }
    }

    /// 整表替换（编辑器批量操作后落盘）
    pub fn replace(&self, data: RosterData) -> Result<(), String> {
        let _guard = self.lock_edit();
        self.commit_locked(data)?;
        Ok(())
    }

    /// 整表替换（CAS）：版本不符时**不写盘、不改内存**，明确报"名单已变化"
    pub fn replace_if_revision(
        &self,
        data: RosterData,
        expected_revision: u64,
    ) -> Result<RosterSnapshot, String> {
        let _guard = self.lock_edit();
        let current = self.read_revision();
        if current != expected_revision {
            return Err(format!(
                "名单已变化，请刷新后重试（后端版本 {}，请求基于 {}）",
                current, expected_revision
            ));
        }
        self.commit_locked(data)?;
        Ok(self.snapshot_versioned())
    }

    /// 追加去重，返回新增条目数
    pub fn add_all(&self, names: Vec<String>) -> Result<usize, String> {
        let _guard = self.lock_edit();
        let mut next = self.read_data().clone();
        let before = next.items.len();
        next.items.extend(names);
        let next = next.normalized();
        let added = next.items.len().saturating_sub(before);
        self.commit_locked(next)?;
        Ok(added)
    }

    /// 移除单个条目（不存在时为空操作）
    pub fn remove(&self, name: &str) -> Result<(), String> {
        let _guard = self.lock_edit();
        let mut next = self.read_data().clone();
        next.items.retain(|i| i != name);
        self.commit_locked(next)?;
        Ok(())
    }

    /// 清空名单（清空后不再限制任何点怪）
    pub fn clear(&self) -> Result<(), String> {
        let _guard = self.lock_edit();
        let mut next = self.read_data().clone();
        next.items.clear();
        self.commit_locked(next)?;
        Ok(())
    }

    /// 字典改原名时同步名单（保持原位置），返回是否发生替换
    pub fn rename_item(&self, old: &str, new: &str) -> Result<bool, String> {
        let _guard = self.lock_edit();
        let mut next = self.read_data().clone();
        if !next.items.iter().any(|i| i == old) {
            return Ok(false);
        }
        for item in next.items.iter_mut() {
            if item == old {
                *item = new.to_string();
            }
        }
        self.commit_locked(next)?;
        Ok(true)
    }
}

/// 临时文件序号：与进程 ID 一起构成同目录唯一临时文件名
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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

    // ---------------- B4：版本 CAS 与多线程编辑 ----------------

    /// 顺序未变的整表提交不递增版本（重复提交不该产生版本漂移）
    #[test]
    fn test_revision_does_not_drift_on_noop_commit() {
        let path = temp_path("rev_noop");
        let roster = MonsterRoster::load(Some(&path));
        assert_eq!(roster.snapshot_versioned().revision, 0);

        let data = RosterData {
            items: vec!["黑龙".into()],
        };
        let snap = roster.replace_if_revision(data.clone(), 0).unwrap();
        assert_eq!(snap.revision, 1, "真实变更应递增版本");

        let again = roster.replace_if_revision(data, 1).unwrap();
        assert_eq!(again.revision, 1, "内容未变化不得递增版本");

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_revision_does_not_drift_on_noop_commit passed");
    }

    /// 旧版本提交必须被拒绝，且不写盘、不改内存（前端旧快照不得抹掉后端新名单）
    #[test]
    fn test_stale_revision_commit_is_rejected() {
        let path = temp_path("rev_cas");
        let roster = MonsterRoster::load(Some(&path));
        roster
            .replace_if_revision(
                RosterData {
                    items: vec!["黑龙".into()],
                },
                0,
            )
            .unwrap();
        let old_revision = roster.snapshot_versioned().revision;

        // 后端又发生了变更（模拟字典改名联动的名单更新）
        roster.rename_item("黑龙", "黑龙改名").unwrap();
        let current = roster.snapshot_versioned();

        // 前端拿着旧版本提交旧内容
        let err = roster
            .replace_if_revision(
                RosterData {
                    items: vec!["黑龙".into()],
                },
                old_revision,
            )
            .unwrap_err();
        assert!(err.contains("名单已变化"), "{}", err);

        assert_eq!(
            roster.snapshot_versioned(),
            current,
            "被拒绝的提交不得改动内存名单"
        );
        assert!(
            fs::read_to_string(&path).unwrap().contains("黑龙改名"),
            "被拒绝的提交不得改动磁盘名单"
        );

        // 用当前版本提交新意图可以成功
        let ok = roster
            .replace_if_revision(
                RosterData {
                    items: vec!["麒麟".into()],
                },
                current.revision,
            )
            .unwrap();
        assert_eq!(ok.data.items, vec!["麒麟"]);
        assert!(ok.revision > current.revision);

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_stale_revision_commit_is_rejected passed");
    }

    /// 两个 Rust 线程并发 replace/add_all：磁盘与内存必须一致，且两次追加都不丢
    #[test]
    fn test_concurrent_edits_keep_disk_and_memory_consistent() {
        let path = temp_path("concurrent");
        let roster = std::sync::Arc::new(MonsterRoster::load(Some(&path)));
        roster.replace(RosterData::default()).unwrap();

        let mut handles = Vec::new();
        for name in ["甲龙", "乙龙", "丙龙", "丁龙"] {
            let r = roster.clone();
            handles.push(std::thread::spawn(move || r.add_all(vec![name.to_string()])));
        }
        for h in handles {
            h.join().unwrap().expect("并发追加不应失败");
        }

        let in_memory = roster.snapshot();
        let on_disk = MonsterRoster::load(Some(&path)).snapshot();
        assert_eq!(
            in_memory.items.len(),
            4,
            "四次并发追加都必须保留: {:?}",
            in_memory.items
        );
        assert_eq!(on_disk, in_memory, "磁盘与内存必须一致");

        // 目录内不得残留临时文件
        let leftovers: Vec<String> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件: {:?}", leftovers);

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_concurrent_edits_keep_disk_and_memory_consistent passed");
    }

    /// 并发整表替换：最终磁盘必须等于最终内存（不得出现磁盘乙、内存甲）
    #[test]
    fn test_concurrent_replace_matches_disk() {
        let path = temp_path("concurrent_replace");
        let roster = std::sync::Arc::new(MonsterRoster::load(Some(&path)));
        roster.replace(RosterData::default()).unwrap();

        let mut handles = Vec::new();
        for name in ["甲", "乙"] {
            let r = roster.clone();
            let n = name.to_string();
            handles.push(std::thread::spawn(move || {
                r.replace(RosterData { items: vec![n] })
            }));
        }
        for h in handles {
            h.join().unwrap().expect("并发整表替换不应失败");
        }

        let in_memory = roster.snapshot();
        let on_disk = MonsterRoster::load(Some(&path)).snapshot();
        assert_eq!(on_disk, in_memory, "磁盘与内存必须一致，不得错写");

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_concurrent_replace_matches_disk passed");
    }
}
