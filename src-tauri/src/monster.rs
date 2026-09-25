use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// 单个怪物配置数据
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MonsterConfig {
    #[serde(rename = "默认历战等级", default)]
    pub default_tempered_level: i32,
    #[serde(rename = "图标地址", default)]
    pub icon_url: String,
    #[serde(rename = "别称", default)]
    pub nicknames: Vec<String>,
}

/// 怪物匹配命中结果
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MonsterMatchResult {
    pub monster_name: String,
    pub tempered_level: i32, // 0-普通, 1-历战, 2-历战王
    pub icon_url: String,
}

/// 编译后的单条别名正则规则
struct CompiledPattern {
    regex: Regex,
    monster_name: String,
    default_tempered: i32,
    icon_url: String,
}

/// 编译后的字典状态（匹配热路径只读，字典编辑需整体热重载）
struct MonsterInner {
    monsters: HashMap<String, MonsterConfig>,
    patterns: Vec<CompiledPattern>,
    loaded: bool,
}

impl Default for MonsterInner {
    fn default() -> Self {
        Self {
            monsters: HashMap::new(),
            patterns: Vec::new(),
            loaded: false,
        }
    }
}

/// 怪物数据与别名匹配管理器
/// 内部状态以 `RwLock` 承载：匹配是读多写少的热路径，而字典编辑（增删条目）需要运行期热重载
pub struct MonsterDataManager {
    inner: RwLock<MonsterInner>,
    /// 低频编辑互斥锁：串行化"读文件 → 校验 → 修改 → 写盘 → 提交"整段，
    /// 避免两个编辑各自基于旧文件写回而互相覆盖。匹配热路径不经过它。
    edit_lock: Mutex<()>,
}

impl Default for MonsterDataManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MonsterDataManager {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(MonsterInner::default()),
            edit_lock: Mutex::new(()),
        }
    }

    /// 锁中毒（持锁线程 panic）时取回内部数据，避免点怪匹配永久失效
    fn read_inner(&self) -> RwLockReadGuard<'_, MonsterInner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_inner(&self) -> RwLockWriteGuard<'_, MonsterInner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_loaded(&self) -> bool {
        self.read_inner().loaded
    }

    /// 由原始字典编译出可匹配状态。
    /// 按怪物名（key）字典序遍历，保证共享别名时匹配结果确定（与原工程 std::map 行为一致）
    fn compile(raw_data: HashMap<String, MonsterConfig>) -> MonsterInner {
        let mut monsters = HashMap::new();
        let mut patterns = Vec::new();

        let mut ordered_names: Vec<&String> = raw_data.keys().collect();
        ordered_names.sort();

        for name in ordered_names {
            let cfg = &raw_data[name];
            monsters.insert(name.clone(), cfg.clone());

            // 为怪物原名及每一个别称构建全字匹配正则
            let mut all_aliases = cfg.nicknames.clone();
            if !all_aliases.contains(name) {
                all_aliases.push(name.clone());
            }

            for alias in all_aliases {
                let trimmed = alias.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let pattern_str = format!("^{}$", regex::escape(trimmed));
                if let Ok(reg) = Regex::new(&pattern_str) {
                    patterns.push(CompiledPattern {
                        regex: reg,
                        monster_name: name.clone(),
                        default_tempered: cfg.default_tempered_level,
                        icon_url: cfg.icon_url.clone(),
                    });
                }
            }
        }

        MonsterInner {
            monsters,
            patterns,
            loaded: true,
        }
    }

    /// 加载并解析 monster_list.json
    pub fn load_from_file(&mut self, config_path: Option<&Path>) -> Result<usize, String> {
        let path = match config_path {
            Some(p) => p.to_path_buf(),
            None => Self::find_monster_list_path(),
        };

        let raw_data = Self::read_dict_file(&path)?;
        let count = raw_data.len();
        *self.write_inner() = Self::compile(raw_data);
        Ok(count)
    }

    /// 读取字典文件为 {怪物名: 配置}（自动剔除 UTF-8 BOM）
    fn read_dict_file(path: &Path) -> Result<HashMap<String, MonsterConfig>, String> {
        if !path.exists() {
            return Err(format!("未找到怪物列表文件: {}", path.display()));
        }

        let mut file = File::open(path).map_err(|e| format!("打开怪物列表文件失败: {}", e))?;
        let mut content = String::new();
        file.read_to_string(&mut content)
            .map_err(|e| format!("读取怪物列表文件失败: {}", e))?;

        let clean = content.strip_prefix('\u{FEFF}').unwrap_or(&content);
        serde_json::from_str(clean).map_err(|e| format!("怪物列表内容不是合法 JSON: {}", e))
    }

    /// 读取字典原始 JSON（保序），供编辑命令在原文件顺序上增删改
    pub fn read_ordered_dict(
        path: &Path,
    ) -> Result<serde_json::Map<String, serde_json::Value>, String> {
        if !path.exists() {
            return Err(format!("未找到怪物列表文件: {}", path.display()));
        }
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("读取怪物列表文件失败: {}", e))?;
        let clean = content.strip_prefix('\u{FEFF}').unwrap_or(&content);
        let value: serde_json::Value = serde_json::from_str(clean)
            .map_err(|e| format!("怪物列表内容不是合法 JSON: {}", e))?;
        match value {
            serde_json::Value::Object(map) => Ok(map),
            _ => Err("怪物列表文件格式不正确（顶层需为对象）".to_string()),
        }
    }

    /// 编辑字典并写回，成功后热重载匹配器。
    /// 内存数据在落盘成功前保持不变 —— 任一步失败即等同回滚。
    ///
    /// 整个「读当前文件 → 校验 → 修改 → 写盘 → 热重载提交」在**同一把编辑锁**内完成：
    /// 两个编辑不会各自基于旧文件写回、互相覆盖（匹配热路径仍只短暂读 `RwLock`）。
    pub fn edit_and_save<F>(&self, path: &Path, mutate: F) -> Result<usize, String>
    where
        F: FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> Result<(), String>,
    {
        let _edit_guard = self
            .edit_lock
            .lock()
            .map_err(|_| "字典编辑锁异常".to_string())?;

        let mut raw = Self::read_ordered_dict(path)?;
        mutate(&mut raw)?;

        let json_text = serde_json::to_string_pretty(&serde_json::Value::Object(raw.clone()))
            .map_err(|e| format!("条目序列化失败: {}", e))?;

        // 落盘前先自校验：确保新内容可解析且能编译出匹配器，
        // 避免「写入成功但热重载失败」导致磁盘与内存背离
        let parsed: HashMap<String, MonsterConfig> =
            serde_json::from_value(serde_json::Value::Object(raw))
                .map_err(|e| format!("条目内容非法: {}", e))?;
        let next = Self::compile(parsed);

        Self::write_atomic(path, &json_text)?;
        let count = next.monsters.len();
        *self.write_inner() = next;
        Ok(count)
    }

    /// 原子写入（临时文件 + 重命名）；JSON 统一 UTF-8 无 BOM
    fn write_atomic(path: &Path, content: &str) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建数据目录失败: {}", e))?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut file =
                File::create(&tmp).map_err(|e| format!("写入数据文件失败: {}", e))?;
            file.write_all(content.as_bytes())
                .map_err(|e| format!("写入数据文件失败: {}", e))?;
            file.flush()
                .map_err(|e| format!("写入数据文件失败: {}", e))?;
        }
        std::fs::rename(&tmp, path).map_err(|e| format!("替换数据文件失败: {}", e))
    }

    /// 查找默认 monster_list.json 路径（统一经 paths.rs：数据目录 → 安装资源 → 开发目录）
    pub fn find_monster_list_path() -> PathBuf {
        crate::paths::find_resource("monster_list.json").unwrap_or_else(|| {
            crate::paths::config_dir().join("monster_list.json")
        })
    }

    /// 原文精确匹配（不做修饰词剥离），命中时返回该条目默认历战等级
    fn match_exact(&self, inner: &MonsterInner, text: &str) -> Option<MonsterMatchResult> {
        if text.is_empty() {
            return None;
        }

        for cp in inner.patterns.iter() {
            if cp.regex.is_match(text) {
                return Some(MonsterMatchResult {
                    monster_name: cp.monster_name.clone(),
                    tempered_level: cp.default_tempered,
                    icon_url: cp.icon_url.clone(),
                });
            }
        }

        None
    }

    /// 剥离“历战王/歷戰王/AT”“历战/歷戰”修饰词，返回 (强制历战等级, 清理后文本)
    /// 未含修饰词时返回 (0, 去除首尾空白后的原文)
    fn strip_tempered_modifiers(text: &str) -> (i32, String) {
        let mut tempered = 0;
        let mut clean_text = text.to_string();

        if clean_text.contains("历战王") || clean_text.contains("歷戰王") || clean_text.contains("AT") {
            tempered = 2;
            clean_text = clean_text.replace("历战王", "").replace("歷戰王", "").replace("AT", "");
        } else if clean_text.contains("历战") || clean_text.contains("歷戰") {
            tempered = 1;
            clean_text = clean_text.replace("历战", "").replace("歷戰", "");
        }

        (tempered, clean_text.trim().to_string())
    }

    /// 根据输入文本匹配怪物与历战等级
    /// 阶段 1 原文精确匹配（优先命中自带历战/历战王前缀的专有别称，保留专属图标）；
    /// 阶段 2 剥离历战修饰词后重试，并以修饰词强制历战等级
    pub fn match_monster(&self, input_text: &str) -> Option<MonsterMatchResult> {
        let inner = self.read_inner();
        if !inner.loaded || input_text.is_empty() {
            return None;
        }

        let trimmed = input_text.trim();

        if let Some(res) = self.match_exact(&inner, trimmed) {
            return Some(res);
        }

        let (forced_tempered, cleaned) = Self::strip_tempered_modifiers(trimmed);
        if cleaned.is_empty() {
            return None;
        }

        self.match_exact(&inner, &cleaned).map(|mut res| {
            if forced_tempered > 0 {
                res.tempered_level = forced_tempered;
            }
            res
        })
    }

    /// 获取所有怪物列表元数据
    pub fn get_all_monsters(&self) -> HashMap<String, MonsterConfig> {
        self.read_inner().monsters.clone()
    }

    /// 字典条目写入的**权威校验 + 落点**（在任何 `remove`/`insert` 之前判定）。
    ///
    /// 拒绝的三种情况：
    /// - 新增时目标名已存在 → 会覆盖原条目的别称与图标；
    /// - 改名时旧名已不存在 → 基于已过期的界面状态操作；
    /// - 改名时目标名已存在 → 既覆盖别人的条目、又删掉旧条目。
    ///
    /// 原名不变的正常编辑允许通过。返回 `Err` 时调用方**不得**改动 `raw`。
    /// 前端预检查只作提示，这里才是最后一道校验。
    pub fn upsert_entry(
        raw: &mut serde_json::Map<String, serde_json::Value>,
        name: &str,
        config: &MonsterConfig,
        renamed_from: Option<&str>,
    ) -> Result<(), String> {
        match renamed_from {
            // 改名：旧名必须在、目标名必须没被占用
            Some(old) if old != name => {
                if !raw.contains_key(old) {
                    return Err(format!(
                        "原名「{}」已不存在（可能已被其他编辑删除），请刷新后重试",
                        old
                    ));
                }
                if raw.contains_key(name) {
                    return Err(format!(
                        "已存在名为「{}」的怪物，改名会覆盖它的别称与图标，已拒绝",
                        name
                    ));
                }
                raw.remove(old);
            }
            // 原名不变的正常编辑：允许覆盖自身
            Some(_) => {}
            // 新增：目标名不得已存在
            None => {
                if raw.contains_key(name) {
                    return Err(format!(
                        "已存在名为「{}」的怪物，新增会覆盖它的别称与图标，已拒绝",
                        name
                    ));
                }
            }
        }

        raw.insert(
            name.to_string(),
            serde_json::to_value(config).map_err(|e| format!("条目序列化失败: {}", e))?,
        );
        Ok(())
    }

    /// **按原名精确取键**（只查 `inner.monsters`，不跑别名匹配）。
    ///
    /// 选怪面板展示的就是字典原名，因此选中后必须按原名取值：
    /// 若改走 `match_monster`，别的怪只要把该原名登记成自己的别称，
    /// 面板上明明点的是「黑龙」也会被换成另一只怪。
    pub fn exact_entry(&self, name: &str) -> Option<MonsterMatchResult> {
        let inner = self.read_inner();
        inner.monsters.get(name).map(|cfg| MonsterMatchResult {
            monster_name: name.to_string(),
            tempered_level: cfg.default_tempered_level,
            icon_url: cfg.icon_url.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_and_match_real_monster_data() {
        let mut mgr = MonsterDataManager::new();
        // 查找实际的 monster_list.json
        let path = MonsterDataManager::find_monster_list_path();
        let count_res = mgr.load_from_file(Some(&path));
        assert!(count_res.is_ok(), "Failed to load monster list from {:?}", path);
        let count = count_res.unwrap();
        assert!(count > 100, "Monster list should contain over 100 monsters, found: {}", count);

        // 测试别名匹配 1: 霸主太太 -> 霸主雌火龙
        let res1 = mgr.match_monster("霸主太太");
        assert!(res1.is_some());
        let m1 = res1.unwrap();
        assert_eq!(m1.monster_name, "霸主雌火龙");
        assert_eq!(m1.tempered_level, 0);
        assert!(m1.icon_url.contains("Apex_Rathian"));

        // 测试别名匹配 2: 大胖虎 / 明日香 -> 嗟怨震天怨虎龙
        let res2 = mgr.match_monster("大胖虎");
        assert!(res2.is_some());
        assert_eq!(res2.unwrap().monster_name, "嗟怨震天怨虎龙");

        let res2_sub = mgr.match_monster("明日香");
        assert!(res2_sub.is_some());
        assert_eq!(res2_sub.unwrap().monster_name, "嗟怨震天怨虎龙");

        // 测试修饰词：历战大胖虎 -> tempered_level = 1
        let res3 = mgr.match_monster("历战大胖虎");
        assert!(res3.is_some());
        let m3 = res3.unwrap();
        assert_eq!(m3.monster_name, "嗟怨震天怨虎龙");
        assert_eq!(m3.tempered_level, 1);

        // 测试修饰词：历战王黑龙 -> tempered_level = 2
        let res4 = mgr.match_monster("历战王黑龙");
        assert!(res4.is_some());
        assert_eq!(res4.unwrap().tempered_level, 2);

        // 测试不存在的怪
        let res_none = mgr.match_monster("超级赛亚人小怪兽");
        assert!(res_none.is_none());

        println!("[PASS] test_load_and_match_real_monster_data passed");
    }

    #[test]
    fn test_all_aliases_exact_match() {
        let mut mgr = MonsterDataManager::new();
        let path = MonsterDataManager::find_monster_list_path();
        let count = mgr
            .load_from_file(Some(&path))
            .unwrap_or_else(|e| panic!("load monster list failed: {}", e));
        assert!(count > 100, "Monster list should contain over 100 monsters, found: {}", count);

        // 构建 别名 -> 归属条目集合（跨条目重复别称时命中任一归属方均可，与字典序加载规则一致）
        let all = mgr.get_all_monsters();
        let mut owners: HashMap<String, Vec<String>> = HashMap::new();
        for (name, cfg) in &all {
            let mut aliases = cfg.nicknames.clone();
            if !aliases.contains(name) {
                aliases.push(name.clone());
            }
            for alias in aliases {
                let trimmed = alias.trim();
                if !trimmed.is_empty() {
                    owners.entry(trimmed.to_string()).or_default().push(name.clone());
                }
            }
        }

        let mut total = 0usize;
        let mut tempered_checked = 0usize;
        for (alias, owner_list) in &owners {
            let res = mgr
                .match_monster(alias)
                .unwrap_or_else(|| panic!("alias {:?} should match some monster", alias));
            assert!(
                owner_list.contains(&res.monster_name),
                "alias {:?} matched {:?}, expected one of {:?}",
                alias,
                res.monster_name,
                owner_list
            );

            // 自带历战/历战王前缀的专有别称：必须命中归属条目并保留其默认历战等级与专属图标
            if alias.contains("历战") || alias.contains("歷戰") {
                let owner_cfg = all
                    .get(&res.monster_name)
                    .unwrap_or_else(|| panic!("owner {:?} missing", res.monster_name));
                assert_eq!(
                    res.tempered_level, owner_cfg.default_tempered_level,
                    "alias {:?} should keep owner default tempered level",
                    alias
                );
                assert!(
                    res.tempered_level >= 1 && res.icon_url.contains("Tempered"),
                    "alias {:?} should carry tempered icon (level {}, icon {})",
                    alias,
                    res.tempered_level,
                    res.icon_url
                );
                tempered_checked += 1;
            }
            total += 1;
        }

        // 数据含 42 个唯一历战别称（原始 44 条，其中“零式游星欧米茄”有 2 条重复）
        assert!(
            tempered_checked >= 42,
            "expected at least 42 unique tempered aliases, got {}",
            tempered_checked
        );
        println!(
            "[PASS] test_all_aliases_exact_match passed ({} aliases, {} tempered)",
            total, tempered_checked
        );
    }

    #[test]
    fn test_tempered_strip_fallback() {
        let mut mgr = MonsterDataManager::new();
        let path = MonsterDataManager::find_monster_list_path();
        mgr.load_from_file(Some(&path)).expect("load monster list");

        // 无专属历战条目的写法：剥离后回退匹配，等级被修饰词强制覆盖
        let m = mgr.match_monster("历战大胖虎").expect("历战大胖虎 should match");
        assert_eq!(m.monster_name, "嗟怨震天怨虎龙");
        assert_eq!(m.tempered_level, 1);

        let m = mgr.match_monster("历战王黑龙").expect("历战王黑龙 should match");
        assert_eq!(m.monster_name, "黑龙");
        assert_eq!(m.tempered_level, 2);

        // 繁体写法
        let m = mgr.match_monster("歷戰大胖虎").expect("歷戰大胖虎 should match");
        assert_eq!(m.tempered_level, 1);

        // 自带历战前缀的专有别称优先精确命中：历战钢龙 -> 风暴的棺材（专属图标）
        let m = mgr.match_monster("历战钢龙").expect("历战钢龙 should match");
        assert_eq!(m.monster_name, "风暴的棺材");
        assert_eq!(m.tempered_level, 1);
        assert!(m.icon_url.contains("Tempered_Kushala"));

        // 无修饰词时保持原文匹配行为
        let m = mgr.match_monster("钢龙").expect("钢龙 should match");
        assert_eq!(m.monster_name, "钢龙");
        assert_eq!(m.tempered_level, 0);

        println!("[PASS] test_tempered_strip_fallback passed");
    }

    #[test]
    fn test_monster_alias_deterministic_order() {
        let dir = std::env::temp_dir().join("mh_test_monster_order");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("monster_list.json");

        // 两个怪物共享同一别名，用于验证遍历顺序确定
        let data = r#"{
            "bbb怪": { "默认历战等级": 0, "图标地址": "b.png", "别称": ["共享怪"] },
            "aaa怪": { "默认历战等级": 0, "图标地址": "a.png", "别称": ["共享怪"] }
        }"#;
        std::fs::write(&path, data).unwrap();

        let mut first_result = String::new();
        for _ in 0..8 {
            let mut mgr = MonsterDataManager::new();
            mgr.load_from_file(Some(&path)).unwrap();
            let res = mgr.match_monster("共享怪").unwrap();
            if first_result.is_empty() {
                first_result = res.monster_name.clone();
            } else {
                assert_eq!(res.monster_name, first_result, "别名冲突时匹配结果必须稳定");
            }
        }
        // 字典序下 "aaa怪" 在 "bbb怪" 之前，首命中应为 aaa怪
        assert_eq!(first_result, "aaa怪");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
        println!("[PASS] test_monster_alias_deterministic_order passed");
    }

    /// 字典编辑写回：热重载立即生效、失败不落盘不回滚内存、条目顺序与文件格式保持不变
    #[test]
    fn test_edit_and_save_hot_reloads_and_keeps_order() {
        let source = MonsterDataManager::find_monster_list_path();
        let dir = std::env::temp_dir().join("mh_test_monster_edit");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("monster_list.json");
        std::fs::copy(&source, &path).unwrap();

        let mut mgr = MonsterDataManager::new();
        let before_count = mgr.load_from_file(Some(&path)).unwrap();

        // 1. 追加别称 → 新别称立即可匹配（无需重启，热重载生效）
        let mut hei = mgr.get_all_monsters().get("黑龙").unwrap().clone();
        hei.nicknames.push("测试专属别称".into());
        let count = mgr
            .edit_and_save(&path, |raw| {
                raw.insert("黑龙".into(), serde_json::to_value(&hei).unwrap());
                Ok(())
            })
            .unwrap();
        assert_eq!(count, before_count, "编辑已有条目不应改变条目总数");

        let hit = mgr.match_monster("测试专属别称").expect("新别称应立即生效");
        assert_eq!(hit.monster_name, "黑龙");

        // 2. 新增条目 → 原名与别称均可匹配，默认历战等级随条目
        let new_cfg = MonsterConfig {
            default_tempered_level: 1,
            icon_url: "MHWilds/新怪.png".into(),
            nicknames: vec!["自定义怪".into()],
        };
        let count = mgr
            .edit_and_save(&path, |raw| {
                raw.insert("香蕉龙".into(), serde_json::to_value(&new_cfg).unwrap());
                Ok(())
            })
            .unwrap();
        assert_eq!(count, before_count + 1);
        let hit = mgr.match_monster("自定义怪").expect("新增条目应可匹配");
        assert_eq!(hit.monster_name, "香蕉龙");
        assert_eq!(hit.tempered_level, 1);
        assert_eq!(mgr.match_monster("香蕉龙").unwrap().monster_name, "香蕉龙");

        // 3. 改名 → 旧名不再命中，新名与别称照常命中
        mgr.edit_and_save(&path, |raw| {
            raw.remove("香蕉龙");
            raw.insert("火龙果".into(), serde_json::to_value(&new_cfg).unwrap());
            Ok(())
        })
        .unwrap();
        assert!(mgr.match_monster("香蕉龙").is_none(), "改名后旧名不应再命中");
        assert_eq!(mgr.match_monster("火龙果").unwrap().monster_name, "火龙果");
        assert_eq!(mgr.match_monster("自定义怪").unwrap().monster_name, "火龙果");

        // 4. 删除条目 → 原名与别称全部失效
        let count = mgr
            .edit_and_save(&path, |raw| {
                raw.remove("火龙果");
                Ok(())
            })
            .unwrap();
        assert_eq!(count, before_count);
        assert!(mgr.match_monster("火龙果").is_none());
        assert!(mgr.match_monster("自定义怪").is_none());

        // 5. 文件格式与顺序：无 BOM、2 空格缩进、条目顺序与原文件完全一致
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.starts_with('\u{FEFF}'), "字典 JSON 不得含 BOM");
        assert!(text.contains("\n  \"黑龙\""), "应保持 2 空格缩进");
        let edited_keys: Vec<String> =
            MonsterDataManager::read_ordered_dict(&path).unwrap().keys().cloned().collect();
        let original_keys: Vec<String> =
            MonsterDataManager::read_ordered_dict(&source).unwrap().keys().cloned().collect();
        assert_eq!(edited_keys, original_keys, "编辑后条目顺序必须与原文件一致");

        // 6. 非法内容 → 拒绝落盘且内存不变（等同回滚）
        let bad = mgr.edit_and_save(&path, |raw| {
            raw.insert("坏条目".into(), serde_json::json!("不是对象"));
            Ok(())
        });
        assert!(bad.is_err(), "非法条目应被拒绝");
        assert!(mgr.match_monster("坏条目").is_none());
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("坏条目"), "校验失败不得写入文件");

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_edit_and_save_hot_reloads_and_keeps_order passed");
    }

    /// 字典图标地址必须同时满足两点：文件真实存在、且不含 URL 转义字符。
    /// 曾出现「冥赤龙 / 冥灯龙」图标文件名字面含 %27，浏览器按 URL 规则解码为单引号后 404，
    /// 表现为这两个怪永远无图标（前端 onError 静默隐藏）—— 本测试守护同类失配。
    #[test]
    fn test_icon_urls_resolve_to_real_files() {
        let mut mgr = MonsterDataManager::new();
        let path = MonsterDataManager::find_monster_list_path();
        mgr.load_from_file(Some(&path)).expect("load monster list");

        // 前端静态资源目录：src-tauri/../public/monster_icons
        let icons_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("public")
            .join("monster_icons");
        assert!(icons_dir.is_dir(), "图标静态目录不存在: {:?}", icons_dir);

        let mut missing = Vec::new();
        for (name, cfg) in mgr.get_all_monsters() {
            if cfg.icon_url.is_empty() {
                continue;
            }
            assert!(
                !cfg.icon_url.contains('%'),
                "图标地址含 URL 转义字符，浏览器解码后将找不到文件: {} -> {}",
                name,
                cfg.icon_url
            );
            if !icons_dir.join(&cfg.icon_url).is_file() {
                missing.push(format!("{} -> {}", name, cfg.icon_url));
            }
        }

        assert!(missing.is_empty(), "以下条目的图标文件缺失:\n{}", missing.join("\n"));
        println!(
            "[PASS] test_icon_urls_resolve_to_real_files passed ({} 个条目图标全部就位)",
            mgr.get_all_monsters().len()
        );
    }

    // ---------------- B1：同名/撞名必须拒绝 ----------------

    /// 隔离的临时字典：复制真实词库做副本，绝不对项目真实 monster_list.json 做破坏性测试
    fn temp_dict(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mh_test_monster_{}", tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = MonsterDataManager::find_monster_list_path();
        let dst = dir.join("monster_list.json");
        std::fs::copy(&src, &dst).expect("应能复制真实词库作为测试副本");
        (dir, dst)
    }

    fn cfg(tempered: i32, icon: &str, nicknames: &[&str]) -> MonsterConfig {
        MonsterConfig {
            default_tempered_level: tempered,
            icon_url: icon.to_string(),
            nicknames: nicknames.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// 新增一个已存在的原名必须被拒绝，且文件字节与内存状态不变
    #[test]
    fn test_upsert_rejects_existing_name_on_add() {
        let (dir, path) = temp_dict("dup_add");
        let mut mgr = MonsterDataManager::new();
        mgr.load_from_file(Some(&path)).unwrap();

        let before_bytes = std::fs::read(&path).unwrap();
        // 「黑龙」是出厂词库里真实存在的条目
        let original = mgr.get_all_monsters().get("黑龙").cloned().expect("应存在黑龙");
        assert!(!original.nicknames.is_empty(), "出厂黑龙应带有别称");

        let err = mgr
            .edit_and_save(&path, |raw| {
                MonsterDataManager::upsert_entry(
                    raw,
                    "黑龙",
                    &cfg(0, "", &[]),
                    None,
                )
            })
            .expect_err("新增已存在的原名必须被拒绝");
        assert!(err.contains("已存在名为"), "{}", err);

        // 文件与内存都不得被改动
        assert_eq!(std::fs::read(&path).unwrap(), before_bytes, "拒绝后文件字节必须不变");
        let after = mgr.get_all_monsters().get("黑龙").cloned().unwrap();
        assert_eq!(after, original, "拒绝后内存条目必须不变（别称与图标不得被空草稿取代）");

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_upsert_rejects_existing_name_on_add passed");
    }

    /// 改名撞上另一个已有原名必须被拒绝（否则会覆盖别人并删掉自己）
    #[test]
    fn test_upsert_rejects_rename_into_existing_name() {
        let (dir, path) = temp_dict("dup_rename");
        let mut mgr = MonsterDataManager::new();
        mgr.load_from_file(Some(&path)).unwrap();

        let names: Vec<String> = mgr.get_all_monsters().keys().cloned().collect();
        assert!(names.len() >= 2, "词库应至少两条");
        let target = mgr.get_all_monsters().get("黑龙").cloned().unwrap();
        let before_bytes = std::fs::read(&path).unwrap();

        // 把「雌火龙」改名为「黑龙」
        let victim = "雌火龙";
        assert!(mgr.get_all_monsters().contains_key(victim), "词库应存在雌火龙");
        let err = mgr
            .edit_and_save(&path, |raw| {
                MonsterDataManager::upsert_entry(raw, "黑龙", &cfg(0, "", &[]), Some(victim))
            })
            .expect_err("改名撞名必须被拒绝");
        assert!(err.contains("已存在名为"), "{}", err);

        assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
        let all = mgr.get_all_monsters();
        assert_eq!(all.get("黑龙").cloned().unwrap(), target, "被撞的条目不得被覆盖");
        assert!(all.contains_key(victim), "改名失败后原条目必须保留（不得凭空删掉一条）");

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_upsert_rejects_rename_into_existing_name passed");
    }

    /// 改名时旧名已被删除必须被拒绝（基于过期界面状态的操作不得生效）
    #[test]
    fn test_upsert_rejects_rename_when_original_missing() {
        let (dir, path) = temp_dict("rename_missing");
        let mut mgr = MonsterDataManager::new();
        mgr.load_from_file(Some(&path)).unwrap();

        let before_bytes = std::fs::read(&path).unwrap();
        let err = mgr
            .edit_and_save(&path, |raw| {
                MonsterDataManager::upsert_entry(
                    raw,
                    "全新名字",
                    &cfg(0, "", &[]),
                    Some("这个怪早就不在了"),
                )
            })
            .expect_err("旧名不存在时必须拒绝");
        assert!(err.contains("已不存在"), "{}", err);
        assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
        assert!(mgr.get_all_monsters().get("全新名字").is_none(), "不得凭空新增条目");

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_upsert_rejects_rename_when_original_missing passed");
    }

    /// 原名不变的正常编辑必须放行；两个编辑依次改不同条目都保留
    #[test]
    fn test_upsert_allows_normal_edit_and_sequential_edits() {
        let (dir, path) = temp_dict("normal_edit");
        let mut mgr = MonsterDataManager::new();
        mgr.load_from_file(Some(&path)).unwrap();
        let before_count = mgr.get_all_monsters().len();

        // 原名不变：改别称与图标
        mgr.edit_and_save(&path, |raw| {
            MonsterDataManager::upsert_entry(
                raw,
                "黑龙",
                &cfg(2, "MHRise/MHRS-BlackDragon_Icon.png", &["小黑龙"]),
                Some("黑龙"),
            )
        })
        .expect("原名不变的编辑应放行");
        assert_eq!(
            mgr.get_all_monsters().len(),
            before_count,
            "原名不变的编辑不得增减条目"
        );
        assert_eq!(mgr.get_all_monsters()["黑龙"].default_tempered_level, 2);
        assert!(mgr.match_monster("小黑龙").is_some(), "新别称应立即可匹配");

        // 第二个编辑改另一个条目：不得丢掉第一个编辑的结果
        mgr.edit_and_save(&path, |raw| {
            MonsterDataManager::upsert_entry(
                raw,
                "新怪甲",
                &cfg(0, "", &["新怪甲别称"]),
                None,
            )
        })
        .expect("新增未占用的名字应放行");
        assert!(mgr.get_all_monsters().contains_key("新怪甲"));
        assert!(mgr.get_all_monsters()["黑龙"].nicknames.contains(&"小黑龙".to_string()));

        // 改名到未占用的名字：旧条目被移除、新条目出现
        mgr.edit_and_save(&path, |raw| {
            MonsterDataManager::upsert_entry(raw, "新怪乙", &cfg(0, "", &[]), Some("新怪甲"))
        })
        .expect("改名到未占用的名字应放行");
        assert!(!mgr.get_all_monsters().contains_key("新怪甲"));
        assert!(mgr.get_all_monsters().contains_key("新怪乙"));

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_upsert_allows_normal_edit_and_sequential_edits passed");
    }

    // ---------------- B2：选怪面板按原名精确取键 ----------------

    /// 别的怪把某个原名登记成自己的别称时，面板仍必须精确点中该原名对应的怪
    #[test]
    fn test_exact_entry_ignores_alias_hijack() {
        let (dir, path) = temp_dict("exact_entry");
        let mut mgr = MonsterDataManager::new();
        mgr.load_from_file(Some(&path)).unwrap();

        // 给「雌火龙」加上只属于它自己的别称「太太」，使其成为「黑龙」原文的别名命中者
        let mut fire = mgr.get_all_monsters()["雌火龙"].clone();
        fire.nicknames.push("黑龙".to_string());
        mgr.edit_and_save(&path, |raw| {
            MonsterDataManager::upsert_entry(raw, "雌火龙", &fire, Some("雌火龙"))
        })
        .expect("给已有条目加别称应放行");

        // 弹幕路径按别名匹配：可能命中排在前面的一方（这里只断言它与精确取值可能不同）
        let matched = mgr.match_monster("黑龙").expect("别名匹配应命中");
        // 面板路径按原名精确取键：必须仍然是黑龙本体
        let picked = mgr.exact_entry("黑龙").expect("精确取键应命中黑龙");
        assert_eq!(picked.monster_name, "黑龙", "面板不得被别名劫持到别的怪");
        // 精确取键取的就是条目自身字段
        assert_eq!(
            picked.tempered_level,
            mgr.get_all_monsters()["黑龙"].default_tempered_level
        );
        assert_eq!(picked.icon_url, mgr.get_all_monsters()["黑龙"].icon_url);
        println!(
            "[PASS] test_exact_entry_ignores_alias_hijack passed (别名匹配命中 {}, 面板精确命中 {})",
            matched.monster_name, picked.monster_name
        );

        // 未知名字：精确取键必须返回 None（面板入口据此拒绝）
        assert!(mgr.exact_entry("这个词库里没有的怪").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
