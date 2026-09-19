use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// 单个怪物配置数据
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// 怪物数据与别名匹配管理器
pub struct MonsterDataManager {
    monsters: HashMap<String, MonsterConfig>,
    patterns: Vec<CompiledPattern>,
    pub loaded: bool,
}

impl Default for MonsterDataManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MonsterDataManager {
    pub fn new() -> Self {
        Self {
            monsters: HashMap::new(),
            patterns: Vec::new(),
            loaded: false,
        }
    }

    /// 正则特殊符号转义
    fn escape_regex(input: &str) -> String {
        regex::escape(input)
    }

    /// 加载并解析 monster_list.json
    pub fn load_from_file(&mut self, config_path: Option<&Path>) -> Result<usize, String> {
        let path = match config_path {
            Some(p) => p.to_path_buf(),
            None => Self::find_monster_list_path(),
        };

        if !path.exists() {
            return Err(format!("Monster list file not found: {:?}", path));
        }

        let mut file = File::open(&path).map_err(|e| format!("Open error {:?}: {}", path, e))?;
        let mut content = String::new();
        file.read_to_string(&mut content).map_err(|e| format!("Read error: {}", e))?;

        // 剔除可能的 UTF-8 BOM
        let clean = if content.starts_with('\u{FEFF}') {
            &content[3..]
        } else {
            &content
        };

        let raw_data: HashMap<String, MonsterConfig> =
            serde_json::from_str(clean).map_err(|e| format!("JSON parse error: {}", e))?;

        self.monsters.clear();
        self.patterns.clear();

        // 按怪物名（key）字典序遍历，保证共享别名时匹配结果确定（与原工程 std::map 行为一致）
        let mut ordered_names: Vec<&String> = raw_data.keys().collect();
        ordered_names.sort();

        for name in ordered_names {
            let cfg = &raw_data[name];
            self.monsters.insert(name.clone(), cfg.clone());

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
                let pattern_str = format!("^{}$", Self::escape_regex(trimmed));
                if let Ok(reg) = Regex::new(&pattern_str) {
                    self.patterns.push(CompiledPattern {
                        regex: reg,
                        monster_name: name.clone(),
                        default_tempered: cfg.default_tempered_level,
                        icon_url: cfg.icon_url.clone(),
                    });
                }
            }
        }

        self.loaded = true;
        Ok(self.monsters.len())
    }

    /// 查找默认 monster_list.json 路径（统一经 paths.rs：数据目录 → 安装资源 → 开发目录）
    pub fn find_monster_list_path() -> PathBuf {
        crate::paths::find_resource("monster_list.json").unwrap_or_else(|| {
            crate::paths::config_dir().join("monster_list.json")
        })
    }

    /// 原文精确匹配（不做修饰词剥离），命中时返回该条目默认历战等级
    fn match_exact(&self, text: &str) -> Option<MonsterMatchResult> {
        if text.is_empty() {
            return None;
        }

        for cp in &self.patterns {
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
        if !self.loaded || input_text.is_empty() {
            return None;
        }

        let trimmed = input_text.trim();

        if let Some(res) = self.match_exact(trimmed) {
            return Some(res);
        }

        let (forced_tempered, cleaned) = Self::strip_tempered_modifiers(trimmed);
        if cleaned.is_empty() {
            return None;
        }

        self.match_exact(&cleaned).map(|mut res| {
            if forced_tempered > 0 {
                res.tempered_level = forced_tempered;
            }
            res
        })
    }

    /// 获取所有怪物列表元数据
    pub fn get_all_monsters(&self) -> HashMap<String, MonsterConfig> {
        self.monsters.clone()
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
}
