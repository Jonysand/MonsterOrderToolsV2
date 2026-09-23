use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// 点怪排队条目
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueueItem {
    pub id: String,
    pub user_id: String,
    pub user_name: String,
    pub monster_name: String,
    pub is_priority: bool,
    #[serde(default)]
    pub guard_level: i32, // 1=总督, 2=提督, 3=舰长, 0=普通
    #[serde(default)]
    pub tempered_level: i32, // 0=普通, 1=历战, 2=历战王
    pub timestamp: i64,
    #[serde(default)]
    pub icon_url: String,
}

impl QueueItem {
    /// 优先级比较算法：
    /// 1. 优先 (is_priority = true) 排在非优先前面
    /// 2. 两者都是优先：
    ///    舰长等级 1 (总督) > 2 (提督) > 3 (舰长) > 0 (普通提权)
    ///    若等级相同，则先来后到 (timestamp 较小者排在前面)
    /// 3. 两者都是非优先：先来后到 (timestamp 较小者排在前面)
    pub fn compare_priority(&self, other: &Self) -> Ordering {
        if self.is_priority != other.is_priority {
            return if self.is_priority {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }

        if self.is_priority && other.is_priority {
            let g1 = if self.guard_level > 0 { self.guard_level } else { 999 };
            let g2 = if other.guard_level > 0 { other.guard_level } else { 999 };
            if g1 != g2 {
                return g1.cmp(&g2);
            }
            return self.timestamp.cmp(&other.timestamp);
        }

        self.timestamp.cmp(&other.timestamp)
    }
}

/// 排队队列管理器
#[derive(Debug, Default, Clone)]
pub struct QueueManager {
    pub items: Vec<QueueItem>,
    /// 脏标记：变更后置位，由后台任务按 500ms 节流落盘（对齐原工程 PriorityQueueManager::Tick）
    pub dirty: bool,
    /// user_id 索引：O(1) 判重/定位（对齐原工程 queue-performance spec 的 HashSet 要求）
    user_index: std::collections::HashSet<String>,
}

impl QueueManager {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            dirty: false,
            user_index: std::collections::HashSet::new(),
        }
    }

    /// O(1) 判定用户是否已在队中（替代原先的线性扫描）
    pub fn contains(&self, user_id: &str) -> bool {
        self.user_index.contains(user_id)
    }

    /// 重建 user_id 索引（整体替换 items 后调用）
    fn rebuild_index(&mut self) {
        self.user_index.clear();
        self.user_index.reserve(self.items.len());
        for it in &self.items {
            self.user_index.insert(it.user_id.clone());
        }
    }

    /// 添加新点单或对已存在用户提权
    pub fn add_or_update(&mut self, new_item: QueueItem) -> bool {
        if let Some(pos) = self.items.iter().position(|i| i.user_id == new_item.user_id) {
            // 已在队列中：如果新请求带有优先且原项非优先，或者带来更高舰长等级，则提权
            let existing = &mut self.items[pos];
            let mut changed = false;
            if new_item.is_priority && !existing.is_priority {
                existing.is_priority = true;
                changed = true;
            }
            if new_item.guard_level > 0 && (existing.guard_level == 0 || new_item.guard_level < existing.guard_level) {
                existing.guard_level = new_item.guard_level;
                changed = true;
            }
            if !new_item.monster_name.is_empty() && existing.monster_name != new_item.monster_name {
                existing.monster_name = new_item.monster_name;
                existing.tempered_level = new_item.tempered_level;
                existing.icon_url = new_item.icon_url;
                changed = true;
            }

            if changed {
                self.dirty = true;
                self.sort_queue();
            }
            return false;
        }

        // 新项加入，并执行稳定排序
        self.user_index.insert(new_item.user_id.clone());
        self.items.push(new_item);
        self.sort_queue();
        self.dirty = true;
        true
    }

    /// 二次“优先”置前：根据 user_id 提权。
    /// 对齐原工程 DanmuProcessor.cpp:364-368：**本条弹幕必须自带舰长身份**（`guardLevel > 0`）才允许提权；
    /// 且只翻转 priority，不修改队列项的 guard_level（避免低等级弹幕把高等级条目降级）。
    pub fn update_priority(&mut self, user_id: &str, guard_level: i32) -> bool {
        if guard_level <= 0 {
            return false;
        }
        if let Some(pos) = self.items.iter().position(|i| i.user_id == user_id) {
            let item = &mut self.items[pos];
            if !item.is_priority {
                item.is_priority = true;
                self.dirty = true;
                self.sort_queue();
                return true;
            }
        }
        false
    }

    /// 根据 user_id 保序删除指定条目
    pub fn dequeue_by_user_id(&mut self, user_id: &str) -> Option<QueueItem> {
        if let Some(pos) = self.items.iter().position(|i| i.user_id == user_id) {
            let removed = self.items.remove(pos);
            self.user_index.remove(&removed.user_id);
            self.dirty = true;
            Some(removed)
        } else {
            None
        }
    }

    /// 根据索引保序删除
    pub fn dequeue_by_index(&mut self, index: usize) -> Option<QueueItem> {
        if index < self.items.len() {
            let removed = self.items.remove(index);
            self.user_index.remove(&removed.user_id);
            self.dirty = true;
            Some(removed)
        } else {
            None
        }
    }

    /// 清空队列
    pub fn clear(&mut self) {
        if !self.items.is_empty() {
            self.items.clear();
            self.user_index.clear();
            self.dirty = true;
        }
    }

    /// 手动重新排序（主播拖拽调整顺序，保留手动排序次序）
    pub fn reorder(&mut self, new_items: Vec<QueueItem>) {
        self.items = new_items;
        self.rebuild_index();
        self.dirty = true;
    }

    /// 撤销完成：把条目**原样**插回指定下标。
    /// 保留原 id 与 timestamp —— 若改用 add_order 重建，新时间戳会让该条目在后续
    /// 优先条目插入时排到错误位置（优先级相同时按 timestamp 先来后到）。
    /// 与 reorder 一致不做重排，保留主播的手动次序。返回 false 表示该用户已在队中。
    pub fn restore(&mut self, item: QueueItem, index: usize) -> bool {
        if self.user_index.contains(&item.user_id) {
            return false;
        }
        let at = index.min(self.items.len());
        self.user_index.insert(item.user_id.clone());
        self.items.insert(at, item);
        self.dirty = true;
        true
    }

    /// 稳定排序（保证相同优先级下的先后次序）
    pub fn sort_queue(&mut self) {
        self.items.sort_by(|a, b| a.compare_priority(b));
    }

    /// 序列化为 JSON 文本（可在持锁期间调用，落盘动作由调用方在锁外执行）
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(&self.items).map_err(|e| e.to_string())
    }

    /// 原子写入 JSON 文本（临时文件 + 重命名），不涉及内存队列状态
    pub fn write_json(path: &Path, json: &str) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
        }

        let temp_path = path.with_extension("tmp");
        {
            let mut file = File::create(&temp_path).map_err(|e| e.to_string())?;
            file.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
            file.flush().map_err(|e| e.to_string())?;
        }
        fs::rename(&temp_path, path).map_err(|e| e.to_string())
    }

    /// 持久化保存至 JSON 文件（原子写临时文件再重命名）
    pub fn save_to_file(&mut self, path: &Path) -> Result<(), String> {
        let json_data = self.to_json()?;
        Self::write_json(path, &json_data)?;
        self.dirty = false;
        Ok(())
    }

    /// 从 JSON 文件恢复队列
    pub fn load_from_file(&mut self, path: &Path) -> Result<usize, String> {
        if !path.exists() {
            return Ok(0);
        }

        let mut file = File::open(path).map_err(|e| e.to_string())?;
        let mut content = String::new();
        file.read_to_string(&mut content).map_err(|e| e.to_string())?;

        // 移除可能存在的 BOM
        let clean_content = content.strip_prefix('\u{FEFF}').unwrap_or(&content);

        if clean_content.trim().is_empty() {
            return Ok(0);
        }

        // 优先按 V2 格式解析；失败则回退解析原工程 OrderList.list 旧格式
        let (loaded_items, from_legacy) = match serde_json::from_str::<Vec<QueueItem>>(clean_content) {
            Ok(items) => (items, false),
            Err(_) => (Self::parse_legacy_items(clean_content)?, true),
        };
        let count = loaded_items.len();
        self.items = loaded_items;
        self.rebuild_index();
        if from_legacy {
            // 旧格式迁移：按优先级归一化一次（后续以文件顺序为准，保留主播手动拖拽次序）
            self.sort_queue();
        }
        self.dirty = false;
        Ok(count)
    }

    /// 解析原工程 OrderList.list 旧格式（PascalCase 键，缺 id / icon_url）。
    /// 字段映射：UserId/TimeStamp/Priority/UserName/MonsterName/GuardLevel/TemperedLevel。
    fn parse_legacy_items(content: &str) -> Result<Vec<QueueItem>, String> {
        #[derive(Deserialize)]
        struct LegacyQueueItem {
            #[serde(rename = "UserId", default)]
            user_id: String,
            #[serde(rename = "TimeStamp", default)]
            timestamp: i64,
            #[serde(rename = "Priority", default)]
            priority: bool,
            #[serde(rename = "UserName", default)]
            user_name: String,
            #[serde(rename = "MonsterName", default)]
            monster_name: String,
            #[serde(rename = "GuardLevel", default)]
            guard_level: i32,
            #[serde(rename = "TemperedLevel", default)]
            tempered_level: i32,
        }

        let legacy: Vec<LegacyQueueItem> = serde_json::from_str(content)
            .map_err(|e| format!("解析队列文件失败（新旧格式均不匹配）: {}", e))?;

        Ok(legacy
            .into_iter()
            .map(|l| QueueItem {
                id: format!("legacy-{}", l.user_id),
                user_id: l.user_id,
                user_name: l.user_name,
                monster_name: l.monster_name,
                is_priority: l.priority,
                guard_level: l.guard_level,
                tempered_level: l.tempered_level,
                timestamp: l.timestamp,
                icon_url: String::new(),
            })
            .collect())
    }
}

/// 队列写入路径：恒为 V2 自有文件 `order_list.json`。
/// 旧 `OrderList.list` 仅用于首次读取迁移，不再被覆写（避免把原工程能读的
/// PascalCase 格式改写成 V2 格式，导致原工程读到空队列）。
pub fn get_order_list_path() -> PathBuf {
    crate::paths::config_dir().join("order_list.json")
}

/// 队列读取路径：优先 V2 自有文件，其次原工程 `OrderList.list`（首次迁移）
pub fn resolve_queue_read_path() -> PathBuf {
    let dir = crate::paths::config_dir();
    let v2 = dir.join("order_list.json");
    if v2.exists() {
        return v2;
    }
    let legacy = dir.join("OrderList.list");
    if legacy.exists() {
        return legacy;
    }
    v2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_priority_rules_complete() {
        let mut qm = QueueManager::new();

        // 1. 普通用户 A (最早, ts=100)
        let item_a = QueueItem {
            id: "1".into(),
            user_id: "u_a".into(),
            user_name: "普通水友A".into(),
            monster_name: "土砂龙".into(),
            is_priority: false,
            guard_level: 0,
            tempered_level: 0,
            timestamp: 100,
            icon_url: "".into(),
        };

        // 2. 舰长 B (非优先, ts=110, guard=3)
        let item_b = QueueItem {
            id: "2".into(),
            user_id: "u_b".into(),
            user_name: "舰长B".into(),
            monster_name: "火龙".into(),
            is_priority: false,
            guard_level: 3,
            tempered_level: 0,
            timestamp: 110,
            icon_url: "".into(),
        };

        // 3. 普通优先 C (优先, ts=120, guard=0)
        let item_c = QueueItem {
            id: "3".into(),
            user_id: "u_c".into(),
            user_name: "普通提权C".into(),
            monster_name: "灭尽龙".into(),
            is_priority: true,
            guard_level: 0,
            tempered_level: 0,
            timestamp: 120,
            icon_url: "".into(),
        };

        // 4. 舰长优先 D (优先, ts=130, guard=3)
        let item_d = QueueItem {
            id: "4".into(),
            user_id: "u_d".into(),
            user_name: "舰长优先D".into(),
            monster_name: "煌黑龙".into(),
            is_priority: true,
            guard_level: 3,
            tempered_level: 1,
            timestamp: 130,
            icon_url: "".into(),
        };

        // 5. 提督优先 E (优先, ts=140, guard=2)
        let item_e = QueueItem {
            id: "5".into(),
            user_id: "u_e".into(),
            user_name: "提督优先E".into(),
            monster_name: "黑龙".into(),
            is_priority: true,
            guard_level: 2,
            tempered_level: 2,
            timestamp: 140,
            icon_url: "".into(),
        };

        // 6. 总督优先 F (优先, ts=150, guard=1)
        let item_f = QueueItem {
            id: "6".into(),
            user_id: "u_f".into(),
            user_name: "总督优先F".into(),
            monster_name: "历战王冰呪龙".into(),
            is_priority: true,
            guard_level: 1,
            tempered_level: 2,
            timestamp: 150,
            icon_url: "".into(),
        };

        qm.add_or_update(item_a);
        qm.add_or_update(item_b);
        qm.add_or_update(item_c);
        qm.add_or_update(item_d);
        qm.add_or_update(item_e);
        qm.add_or_update(item_f);

        // 预期相对次序：
        // 1. 总督 F (guard=1, 优先)
        // 2. 提督 E (guard=2, 优先)
        // 3. 舰长 D (guard=3, 优先)
        // 4. 普通提权 C (guard=0, 优先)
        // 5. 普通水友 A (非优先, ts=100)
        // 6. 舰长 B (非优先, ts=110)
        let ids: Vec<String> = qm.items.iter().map(|i| i.user_id.clone()).collect();
        assert_eq!(ids, vec!["u_f", "u_e", "u_d", "u_c", "u_a", "u_b"]);
        println!("[PASS] test_priority_rules_complete passed");
    }

    #[test]
    fn test_preserved_order_dequeue() {
        let mut qm = QueueManager::new();
        for i in 1..=5 {
            qm.add_or_update(QueueItem {
                id: format!("id-{}", i),
                user_id: format!("u-{}", i),
                user_name: format!("User{}", i),
                monster_name: "轰龙".into(),
                is_priority: false,
                guard_level: 0,
                tempered_level: 0,
                timestamp: i as i64 * 10,
                icon_url: "".into(),
            });
        }

        // 删除中间条目 u-3
        let removed = qm.dequeue_by_user_id("u-3");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().user_id, "u-3");

        // 验证剩余元素顺序依然为 u-1, u-2, u-4, u-5
        let remaining: Vec<String> = qm.items.iter().map(|i| i.user_id.clone()).collect();
        assert_eq!(remaining, vec!["u-1", "u-2", "u-4", "u-5"]);
        println!("[PASS] test_preserved_order_dequeue passed");
    }

    #[test]
    fn test_two_stage_priority_promotion() {
        let mut qm = QueueManager::new();
        qm.add_or_update(QueueItem {
            id: "1".into(),
            user_id: "u_normal".into(),
            user_name: "普通水友".into(),
            monster_name: "角龙".into(),
            is_priority: false,
            guard_level: 0,
            tempered_level: 0,
            timestamp: 100,
            icon_url: "".into(),
        });
        qm.add_or_update(QueueItem {
            id: "2".into(),
            user_id: "u_captain".into(),
            user_name: "舰长水友".into(),
            monster_name: "轰龙".into(),
            is_priority: false,
            guard_level: 3,
            tempered_level: 0,
            timestamp: 200,
            icon_url: "".into(),
        });

        // 初始顺序：u_normal (100) 排在 u_captain (200) 前
        assert_eq!(qm.items[0].user_id, "u_normal");
        assert_eq!(qm.items[1].user_id, "u_captain");

        // 舰长单独发送“优先”触发提权
        let promoted = qm.update_priority("u_captain", 3);
        assert!(promoted);

        // 普通非舰长用户尝试“优先”提权，应被拒绝
        let normal_promoted = qm.update_priority("u_normal", 0);
        assert!(!normal_promoted);
        assert!(!qm.items[1].is_priority);

        // 提权后 u_captain 升至首位
        assert_eq!(qm.items[0].user_id, "u_captain");
        assert_eq!(qm.items[1].user_id, "u_normal");
        println!("[PASS] test_two_stage_priority_promotion passed");
    }

    /// 提权门槛与副作用（对齐原工程 DanmuProcessor.cpp:364-368）：
    /// ① 本条弹幕必须自带舰长身份，队列项昔日的等级不能代为满足门槛；
    /// ② 提权只翻转 priority，不改写 guard_level（不得把高等级条目降级）；
    /// ③ 已优先的条目重复提权返回 false（幂等）。
    #[test]
    fn test_update_priority_guard_and_no_demotion() {
        let mut qm = QueueManager::new();
        qm.add_or_update(QueueItem {
            id: "1".into(),
            user_id: "u_expired".into(),
            user_name: "过期舰长".into(),
            monster_name: "轰龙".into(),
            is_priority: false,
            guard_level: 1, // 队列中曾是总督
            tempered_level: 0,
            timestamp: 100,
            icon_url: "".into(),
        });

        // ① 本人当前弹幕 guard_level = 0（舰长身份已到期）→ 提权被拒绝
        assert!(
            !qm.update_priority("u_expired", 0),
            "无舰长身份的弹幕不得依赖队列项旧等级提权"
        );
        assert!(!qm.items[0].is_priority);

        // ② 舰长本人提权成功，且不得被新弹幕的低等级降级
        assert!(qm.update_priority("u_expired", 3));
        assert!(qm.items[0].is_priority);
        assert_eq!(qm.items[0].guard_level, 1, "提权不得改写队列项的 guard_level");

        // ③ 幂等：已优先的条目再次提权返回 false
        assert!(!qm.update_priority("u_expired", 3), "重复提权应返回 false");

        // 不存在的用户与非法等级均拒绝
        assert!(!qm.update_priority("not_in_queue", 3));
        assert!(!qm.update_priority("u_expired", -1));
        println!("[PASS] test_update_priority_guard_and_no_demotion passed");
    }

    #[test]
    fn test_order_list_persistence() {
        let temp_dir = std::env::temp_dir().join("mh_test_queue");
        let _ = fs::create_dir_all(&temp_dir);
        let file_path = temp_dir.join("order_list_test.json");

        let mut qm1 = QueueManager::new();
        qm1.add_or_update(QueueItem {
            id: "save-1".into(),
            user_id: "u_save".into(),
            user_name: "测试持久化".into(),
            monster_name: "泡狐龙".into(),
            is_priority: true,
            guard_level: 3,
            tempered_level: 1,
            timestamp: 123456789,
            icon_url: "MHRise/MHRS-Mizutsune_Icon.png".into(),
        });

        assert!(qm1.save_to_file(&file_path).is_ok());

        let mut qm2 = QueueManager::new();
        let loaded_count = qm2.load_from_file(&file_path).unwrap();
        assert_eq!(loaded_count, 1);
        assert_eq!(qm2.items[0].user_id, "u_save");
        assert_eq!(qm2.items[0].monster_name, "泡狐龙");
        assert_eq!(qm2.items[0].tempered_level, 1);

        let _ = fs::remove_file(&file_path);
        let _ = fs::remove_dir(&temp_dir);
        println!("[PASS] test_order_list_persistence passed");
    }

    #[test]
    fn test_manual_reorder_queue() {
        let mut qm = QueueManager::new();
        let item1 = QueueItem {
            id: "1".into(),
            user_id: "u1".into(),
            user_name: "猎人A".into(),
            monster_name: "黑龙".into(),
            is_priority: false,
            guard_level: 0,
            tempered_level: 0,
            timestamp: 100,
            icon_url: "".into(),
        };
        let item2 = QueueItem {
            id: "2".into(),
            user_id: "u2".into(),
            user_name: "猎人B".into(),
            monster_name: "煌黑龙".into(),
            is_priority: false,
            guard_level: 0,
            tempered_level: 0,
            timestamp: 200,
            icon_url: "".into(),
        };
        qm.items = vec![item1.clone(), item2.clone()];

        // 手动将 item2 拖动排在 item1 前面
        qm.reorder(vec![item2.clone(), item1.clone()]);
        assert_eq!(qm.items.len(), 2);
        assert_eq!(qm.items[0].user_id, "u2");
        assert_eq!(qm.items[1].user_id, "u1");
        assert!(qm.dirty);
        println!("[PASS] test_manual_reorder_queue passed");
    }

    #[test]
    fn test_load_legacy_order_list_format() {
        let temp_dir = std::env::temp_dir().join("mh_test_queue_legacy");
        let _ = fs::create_dir_all(&temp_dir);
        let file_path = temp_dir.join("OrderList.list");

        // 原工程 OrderList.list 旧格式（PascalCase 键）
        let legacy_json = r#"[
            { "UserId": "u_old_1", "TimeStamp": 1730000000000, "Priority": false, "UserName": "老猎人甲", "MonsterName": "土砂龙", "GuardLevel": 3, "TemperedLevel": 1 },
            { "UserId": "u_old_2", "TimeStamp": 1730000000500, "Priority": true, "UserName": "老猎人乙", "MonsterName": "火龙", "GuardLevel": 1, "TemperedLevel": 0 }
        ]"#;
        fs::write(&file_path, legacy_json).unwrap();

        let mut qm = QueueManager::new();
        let loaded = qm.load_from_file(&file_path).unwrap();
        assert_eq!(loaded, 2);
        // 优先项应排在最前
        assert_eq!(qm.items[0].user_id, "u_old_2");
        assert!(qm.items[0].is_priority);
        assert_eq!(qm.items[0].guard_level, 1);
        assert_eq!(qm.items[1].user_id, "u_old_1");
        assert_eq!(qm.items[1].tempered_level, 1);
        // 旧格式无 id，应自动补 legacy id
        assert!(qm.items[0].id.starts_with("legacy-"));

        let _ = fs::remove_file(&file_path);
        let _ = fs::remove_dir(&temp_dir);
        println!("[PASS] test_load_legacy_order_list_format passed");
    }

    /// P1-12：user_id 索引与 items 必须始终一致（O(1) 判重依赖该索引）
    #[test]
    fn test_user_index_stays_in_sync() {
        let mut q = QueueManager::new();
        let mk = |id: &str, prio: bool, ts: i64| QueueItem {
            id: format!("item-{}", id),
            user_id: id.to_string(),
            user_name: format!("水友{}", id),
            monster_name: "火龙".into(),
            is_priority: prio,
            guard_level: 0,
            tempered_level: 0,
            timestamp: ts,
            icon_url: String::new(),
        };

        assert!(!q.contains("u1"));
        q.add_or_update(mk("u1", false, 1));
        q.add_or_update(mk("u2", true, 2));
        assert!(q.contains("u1") && q.contains("u2"), "新入队用户应在索引中");

        // 重复入队（改怪）不改索引
        q.add_or_update(mk("u1", false, 1));
        assert_eq!(q.items.iter().filter(|i| i.user_id == "u1").count(), 1);

        // 删除后索引同步移除
        q.dequeue_by_user_id("u1");
        assert!(!q.contains("u1"), "删除后索引应同步移除");
        assert!(q.contains("u2"));

        // 清空后索引为空
        q.clear();
        assert!(!q.contains("u2"));

        // reorder 后索引重建
        q.reorder(vec![mk("u3", false, 3), mk("u4", false, 4)]);
        assert!(q.contains("u3") && q.contains("u4"));

        // dequeue_by_index 后索引同步
        q.dequeue_by_index(0);
        assert!(!q.contains("u3") && q.contains("u4"));

        // 索引与 items 完全一致（无残留/无误标）
        let expected: std::collections::HashSet<String> =
            q.items.iter().map(|i| i.user_id.clone()).collect();
        assert_eq!(expected.len(), q.items.len(), "不应存在重复 user_id");
        for uid in ["u1", "u2", "u3"] {
            assert!(!q.contains(uid), "{} 应为已删除状态", uid);
        }
        println!("[PASS] test_user_index_stays_in_sync passed");
    }

    /// P1-12：落盘路径拆分 —— 写入恒为 order_list.json，旧 OrderList.list 仅用于首次读取迁移
    #[test]
    fn test_queue_paths_separate_read_and_write() {
        let dir = crate::paths::config_dir();
        assert_eq!(
            get_order_list_path(),
            dir.join("order_list.json"),
            "写入路径必须恒为 V2 自有文件，避免覆写原工程可读的 OrderList.list"
        );

        // 读取路径：V2 文件存在时优先它（此处仅断言解析规则，不创建真实文件）
        let resolved = resolve_queue_read_path();
        assert!(
            resolved == dir.join("order_list.json") || resolved == dir.join("OrderList.list"),
            "读取路径必须落在数据目录内: {:?}",
            resolved
        );
        println!("[PASS] test_queue_paths_separate_read_and_write passed");
    }

    /// P1-12：脏标记语义 —— 变更置位、落盘后清除
    #[test]
    fn test_dirty_flag_lifecycle() {
        let temp_dir = std::env::temp_dir().join("mh_test_queue_dirty");
        let _ = fs::create_dir_all(&temp_dir);
        let path = temp_dir.join("order_list.json");
        let _ = fs::remove_file(&path);

        let mut q = QueueManager::new();
        assert!(!q.dirty, "初始应为干净状态");
        q.add_or_update(QueueItem {
            id: "item-1".into(),
            user_id: "u1".into(),
            user_name: "水友".into(),
            monster_name: "火龙".into(),
            is_priority: false,
            guard_level: 0,
            tempered_level: 0,
            timestamp: 1,
            icon_url: String::new(),
        });
        assert!(q.dirty, "变更后应置脏");

        q.save_to_file(&path).unwrap();
        assert!(!q.dirty, "落盘后应清除脏标记");

        // to_json / write_json 拆分：序列化不改变脏标记，写盘由调用方在锁外完成
        q.add_or_update(QueueItem {
            id: "item-2".into(),
            user_id: "u2".into(),
            user_name: "水友2".into(),
            monster_name: "雷狼龙".into(),
            is_priority: true,
            guard_level: 3,
            tempered_level: 0,
            timestamp: 2,
            icon_url: String::new(),
        });
        let json = q.to_json().unwrap();
        assert!(q.dirty, "仅序列化不应清除脏标记");
        QueueManager::write_json(&path, &json).unwrap();
        let reloaded = fs::read_to_string(&path).unwrap();
        assert!(reloaded.contains("u2"));

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&temp_dir);
        println!("[PASS] test_dirty_flag_lifecycle passed");
    }

    /// 撤销完成（restore_order）：原样插回原下标，保留 id 与 timestamp
    fn mk_item(uid: &str, prio: bool, ts: i64) -> QueueItem {
        QueueItem {
            id: format!("item-{}", uid),
            user_id: uid.into(),
            user_name: format!("水友{}", uid),
            monster_name: "火龙".into(),
            is_priority: prio,
            guard_level: 0,
            tempered_level: 0,
            timestamp: ts,
            icon_url: String::new(),
        }
    }

    #[test]
    fn test_restore_preserves_index_and_identity() {
        let mut q = QueueManager::new();
        for (i, uid) in ["a", "b", "c", "d"].iter().enumerate() {
            q.add_or_update(mk_item(uid, false, (i as i64 + 1) * 10));
        }
        let removed = q.dequeue_by_user_id("c").unwrap();
        assert_eq!(
            q.items.iter().map(|i| i.user_id.clone()).collect::<Vec<_>>(),
            vec!["a", "b", "d"]
        );

        // 按删除时的下标 2 原样插回
        assert!(q.restore(removed.clone(), 2));
        assert_eq!(
            q.items.iter().map(|i| i.user_id.clone()).collect::<Vec<_>>(),
            vec!["a", "b", "c", "d"],
            "应插回原下标而非队尾"
        );
        // id 与 timestamp 均不得被改写
        assert_eq!(q.items[2].id, removed.id);
        assert_eq!(q.items[2].timestamp, removed.timestamp);
        assert!(q.contains("c"), "索引应同步恢复");
        assert!(q.dirty);
        println!("[PASS] test_restore_preserves_index_and_identity passed");
    }

    #[test]
    fn test_restore_timestamp_semantics_for_later_priority() {
        // 撤销不得改变时间戳语义：插回后新来的优先条目仍应排在其后
        let mut q = QueueManager::new();
        q.add_or_update(mk_item("a", false, 100));
        q.add_or_update(mk_item("b", false, 200));
        let removed = q.dequeue_by_user_id("a").unwrap();
        assert!(q.restore(removed, 0));

        // 同优先级（均非优先）按 timestamp 先来后到：a(100) 仍应排在 b(200) 之前
        q.add_or_update(mk_item("c", false, 300));
        assert_eq!(
            q.items.iter().map(|i| i.user_id.clone()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        let restamp = q.items[0].timestamp;
        assert_eq!(restamp, 100, "恢复项时间戳必须保持原值，否则后续插入位置会错");

        // 重复恢复同一用户应被拒绝
        assert!(!q.restore(q.items[0].clone(), 0));
        // 下标越界时夹紧到队尾
        let mut q2 = QueueManager::new();
        q2.add_or_update(mk_item("x", false, 1));
        assert!(q2.restore(mk_item("y", false, 2), 99));
        assert_eq!(q2.items[1].user_id, "y");
        println!("[PASS] test_restore_timestamp_semantics_for_later_priority passed");
    }
}
