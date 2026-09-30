//! 随机抽选设置：作品筛选 / 排除名单 / 抽选模式与节奏档位。
//!
//! **抽选本身是纯前端行为**（不写队列、不占弹幕额度、零副作用），后端只负责把
//! 「主播的池子偏好」持久化下来，因此本模块没有任何业务算法，只有归一化与读写。
//!
//! 与「点怪禁点名单」（`roster.rs`）**相互独立**：那份的语义是"观众不能点"，
//! 是面向观众的业务规则；这份是"主播自己不想抽到"，是个人偏好。混用会让
//! 「把某只怪加进禁点名单防观众刷」意外变成「主播自己也抽不到它」。
//! 需要合并时由前端点「同步自禁点名单」一次性导入，不做隐式联动。
//!
//! Lite 形态**同样支持**（2026-09-30 决策）：随机抽选不依赖被摘掉的 TTS / 打卡 / AI，
//! 而纯点怪的 Lite 主播恰恰最常用「今天随机打什么」这个玩法，故不设 `ensure_not_lite` 守卫。
//!
//! 并发模型：写操作走同一把编辑互斥锁串行，**不做版本 CAS**。
//! 理由：写入方只有「随机抽选」这一个页签，且每次提交的都是**完整的目标状态**
//! （不是增量），串行化后天然 last-write-wins；而前端已按"同一时刻只允许一个
//! in-flight 提交、期间新意图合并为最新一份"发送（同 MainWindow 的 commitRoster），
//! 因此不存在"旧快照覆盖新名单"的窗口。

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// 设置文件名（与 monster_list.json / monster_roster.json 同目录）
pub const DRAW_SETTINGS_FILE_NAME: &str = "draw_settings.json";

/// 作品 id 白名单：必须与前端 `src/lib/monsterList.ts` 的 `GAME_ORDER` 保持一致
/// （由 `test_game_ids_match_frontend_order` 守护）。
///
/// 校验它的意义在于**可恢复性**：文件里若混入未知 id 并原样回传给前端，
/// 作品按钮里没有对应的项可供取消，主播会卡在"空池"里无法自救。
pub const GAME_IDS: [&str; 5] = ["MHWilds", "MHWorld", "MHWI", "MHRise", "MHRS"];

/// 抽选模式：两者都抽 / 仅武器 / 仅怪物
pub const MODES: [&str; 3] = ["both", "weapon", "monster"];

/// 节奏档位：快 / 标准 / 拖长（对应前端演出时长的三个倍率）
pub const PACES: [&str; 3] = ["fast", "normal", "long"];

const DEFAULT_MODE: &str = "both";
const DEFAULT_PACE: &str = "normal";

fn default_games() -> Vec<String> {
    GAME_IDS.iter().map(|s| s.to_string()).collect()
}

fn default_mode() -> String {
    DEFAULT_MODE.to_string()
}

fn default_pace() -> String {
    DEFAULT_PACE.to_string()
}

/// 抽选设置（持久化结构）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrawSettings {
    /// 参与抽取的作品（`GAME_IDS` 的子集，按 `GAME_IDS` 规范顺序存放）
    #[serde(default = "default_games")]
    pub games: Vec<String>,
    /// 排除的怪物原名（与 monster_list.json 的键一致）
    #[serde(default)]
    pub excluded_monsters: Vec<String>,
    /// 排除的武器文件名（用文件名而非中文名：改中文名不会让排除失效）
    #[serde(default)]
    pub excluded_weapons: Vec<String>,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_pace")]
    pub pace: String,
}

impl Default for DrawSettings {
    fn default() -> Self {
        Self {
            games: default_games(),
            excluded_monsters: Vec::new(),
            excluded_weapons: Vec::new(),
            mode: default_mode(),
            pace: default_pace(),
        }
    }
}

/// 字符串列表归一化：去首尾空白、丢空项、按首次出现顺序去重
fn normalize_list(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .filter(|s| seen.insert(s.clone()))
        .collect()
}

impl DrawSettings {
    /// 归一化非法/冗余输入。
    ///
    /// 三条规则解释：
    /// - `games` 只保留已知作品并**按 `GAME_IDS` 顺序重排** —— 顺序在这里没有功能含义
    ///   （多选即并集），规范化后同一份选择永远产出同一份文件，便于比对与手工核对；
    /// - `games` 过滤后为空（空数组、全是未知 id、解析缺字段）一律回退**全部作品**：
    ///   空作品列表若照原样解释成"一个作品都不选"，池子会恒为空且前端无从恢复；
    /// - `mode` / `pace` 非枚举值回退默认，不报错：这两个是纯演出偏好，
    ///   为一条脏数据拒绝整个设置文件不划算（其余字段仍照常生效）。
    fn normalized(mut self) -> Self {
        self.excluded_monsters = normalize_list(self.excluded_monsters);
        self.excluded_weapons = normalize_list(self.excluded_weapons);

        let picked: HashSet<String> = normalize_list(self.games).into_iter().collect();
        self.games = GAME_IDS
            .iter()
            .filter(|g| picked.contains(**g))
            .map(|g| g.to_string())
            .collect();
        if self.games.is_empty() {
            self.games = default_games();
        }

        if !MODES.contains(&self.mode.as_str()) {
            self.mode = default_mode();
        }
        if !PACES.contains(&self.pace.as_str()) {
            self.pace = default_pace();
        }
        self
    }
}

/// 设置管理器：内存权威 + 落盘副本
pub struct DrawSettingsManager {
    data: RwLock<DrawSettings>,
    path: PathBuf,
    /// 低频写互斥锁：保证"落盘 → 提交内存"原子，失败时内存与磁盘都保持不变
    edit_lock: Mutex<()>,
}

impl DrawSettingsManager {
    /// 从文件加载；缺失/损坏/结构非法时回退**全默认**并告警，不阻断启动。
    ///
    /// 与 `roster.rs` 的处理姿态一致：设置文件属于"锦上添花"的偏好数据，
    /// 读不出来时让主播照常开机点怪，比在启动阶段报错重要得多。
    pub fn load(path: Option<&Path>) -> Self {
        let path = path
            .map(|p| p.to_path_buf())
            .unwrap_or_else(Self::default_path);

        let data = match std::fs::read_to_string(&path) {
            Ok(content) => {
                let clean = content.strip_prefix('\u{FEFF}').unwrap_or(&content);
                match serde_json::from_str::<DrawSettings>(clean) {
                    Ok(d) => d,
                    Err(e) => {
                        crate::log_warn!(
                            "[Draw] 抽选设置解析失败，回退默认值: {}（{}）",
                            path.display(),
                            e
                        );
                        DrawSettings::default()
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DrawSettings::default(),
            Err(e) => {
                crate::log_warn!(
                    "[Draw] 抽选设置读取失败，回退默认值: {}（{}）",
                    path.display(),
                    e
                );
                DrawSettings::default()
            }
        };

        Self {
            data: RwLock::new(data.normalized()),
            path,
            edit_lock: Mutex::new(()),
        }
    }

    /// 默认路径：可写数据目录下的 draw_settings.json（不随包播种，默认不存在）
    pub fn default_path() -> PathBuf {
        crate::paths::config_dir().join(DRAW_SETTINGS_FILE_NAME)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 锁中毒（持锁线程 panic）时取回内部数据，避免设置页永久失去读写能力
    fn read_data(&self) -> RwLockReadGuard<'_, DrawSettings> {
        self.data.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_data(&self) -> RwLockWriteGuard<'_, DrawSettings> {
        self.data.write().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_edit(&self) -> std::sync::MutexGuard<'_, ()> {
        self.edit_lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 当前设置（归一化后的权威值）
    pub fn snapshot(&self) -> DrawSettings {
        self.read_data().clone()
    }

    /// 整表替换：先落盘、成功后才提交内存。
    /// 内容未变化时既不写盘也不报错（重复提交不该让文件时间戳漂移）。
    /// 返回落盘后的权威设置，供前端纠正自己的乐观状态。
    pub fn replace(&self, next: DrawSettings) -> Result<DrawSettings, String> {
        let _guard = self.lock_edit();
        let next = next.normalized();
        if *self.read_data() == next {
            return Ok(next);
        }
        crate::paths::write_json_atomic(&self.path, &next)?;
        *self.write_data() = next.clone();
        crate::log_info!(
            "[Draw] 抽选设置已更新（作品 {} 个 / 排除怪物 {} 只 / 排除武器 {} 把 / {} / {}）",
            next.games.len(),
            next.excluded_monsters.len(),
            next.excluded_weapons.len(),
            next.mode,
            next.pace
        );
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mh_test_draw_{}", tag));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join(DRAW_SETTINGS_FILE_NAME)
    }

    /// 默认值：全部作品、空排除名单、both / normal
    #[test]
    fn test_default_when_file_missing() {
        let path = temp_path("missing");
        let mgr = DrawSettingsManager::load(Some(&path));

        let s = mgr.snapshot();
        assert_eq!(s.games, GAME_IDS.to_vec(), "缺失文件应默认选中全部作品");
        assert!(s.excluded_monsters.is_empty());
        assert!(s.excluded_weapons.is_empty());
        assert_eq!(s.mode, "both");
        assert_eq!(s.pace, "normal");
        // 未写盘前不得凭空创建文件
        assert!(!path.exists(), "仅加载不应创建文件");

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_default_when_file_missing passed");
    }

    /// 往返：落盘无 BOM、可被重新加载解析、值与提交一致
    #[test]
    fn test_replace_roundtrip_without_bom() {
        let path = temp_path("roundtrip");
        let mgr = DrawSettingsManager::load(Some(&path));
        let saved = mgr
            .replace(DrawSettings {
                games: vec!["MHRS".into(), "MHWilds".into()],
                excluded_monsters: vec!["黑龙".into()],
                excluded_weapons: vec!["MHWilds-Bow_Icon_Base.png".into()],
                mode: "monster".into(),
                pace: "long".into(),
            })
            .expect("replace 应成功");

        let raw = fs::read(&path).unwrap();
        assert_ne!(&raw[..3], b"\xef\xbb\xbf", "JSON 配置不得含 BOM");

        let reloaded = DrawSettingsManager::load(Some(&path));
        assert_eq!(reloaded.snapshot(), saved, "重新加载应得到同一份设置");
        assert_eq!(
            saved.games,
            vec!["MHWilds".to_string(), "MHRS".to_string()],
            "作品应按 GAME_IDS 规范顺序落盘，与点击先后无关"
        );

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_replace_roundtrip_without_bom passed");
    }

    /// 损坏 / 结构非法 → 回退全默认，不阻断
    #[test]
    fn test_corrupt_file_falls_back_to_default() {
        let path = temp_path("corrupt");
        fs::write(&path, "{ 这不是合法 JSON").unwrap();
        let mgr = DrawSettingsManager::load(Some(&path));
        assert_eq!(mgr.snapshot(), DrawSettings::default());

        // 顶层是数组（结构非法）
        fs::write(&path, "[1, 2, 3]").unwrap();
        let mgr = DrawSettingsManager::load(Some(&path));
        assert_eq!(mgr.snapshot(), DrawSettings::default());

        // 字段类型不对
        fs::write(&path, r#"{"games": "MHWilds", "mode": 5}"#).unwrap();
        let mgr = DrawSettingsManager::load(Some(&path));
        assert_eq!(mgr.snapshot(), DrawSettings::default());

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_corrupt_file_falls_back_to_default passed");
    }

    /// 作品列表：未知 id 被剔除；剔除后为空则回退全部作品（可恢复性）
    #[test]
    fn test_games_normalized_to_known_ids() {
        let path = temp_path("games");

        // 混入未知 id → 只保留已知的
        fs::write(&path, r#"{"games": ["MHWilds", "MHNow", "  MHRS  "]}"#).unwrap();
        let mgr = DrawSettingsManager::load(Some(&path));
        assert_eq!(
            mgr.snapshot().games,
            vec!["MHWilds".to_string(), "MHRS".to_string()]
        );

        // 全部未知 → 回退全部作品，而不是"一个都不选"
        fs::write(&path, r#"{"games": ["MHNow", "MHFrontier"]}"#).unwrap();
        let mgr = DrawSettingsManager::load(Some(&path));
        assert_eq!(mgr.snapshot().games, GAME_IDS.to_vec());

        // 空数组同样回退全部
        fs::write(&path, r#"{"games": []}"#).unwrap();
        let mgr = DrawSettingsManager::load(Some(&path));
        assert_eq!(mgr.snapshot().games, GAME_IDS.to_vec());

        // 重复项被去重
        fs::write(&path, r#"{"games": ["MHRS", "MHRS", "MHRS"]}"#).unwrap();
        let mgr = DrawSettingsManager::load(Some(&path));
        assert_eq!(mgr.snapshot().games, vec!["MHRS".to_string()]);

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_games_normalized_to_known_ids passed");
    }

    /// mode / pace 非枚举值回退默认，且不影响其余字段
    #[test]
    fn test_invalid_mode_and_pace_fall_back() {
        let path = temp_path("enums");
        fs::write(
            &path,
            r#"{"mode": "武器", "pace": "turbo", "excluded_monsters": ["黑龙"]}"#,
        )
        .unwrap();
        let mgr = DrawSettingsManager::load(Some(&path));
        let s = mgr.snapshot();
        assert_eq!(s.mode, "both");
        assert_eq!(s.pace, "normal");
        assert_eq!(s.excluded_monsters, vec!["黑龙".to_string()], "其余字段照常生效");

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_invalid_mode_and_pace_fall_back passed");
    }

    /// 排除名单：去空白、去重、保序
    #[test]
    fn test_exclusions_dedup_and_trim() {
        let path = temp_path("exclusions");
        let mgr = DrawSettingsManager::load(Some(&path));
        let saved = mgr
            .replace(DrawSettings {
                excluded_monsters: vec![
                    "  黑龙  ".into(),
                    "黑龙".into(),
                    "".into(),
                    "   ".into(),
                    "风暴的棺材".into(),
                ],
                excluded_weapons: vec!["a.png".into(), "a.png".into(), "b.png".into()],
                ..DrawSettings::default()
            })
            .unwrap();

        assert_eq!(
            saved.excluded_monsters,
            vec!["黑龙".to_string(), "风暴的棺材".to_string()]
        );
        assert_eq!(
            saved.excluded_weapons,
            vec!["a.png".to_string(), "b.png".to_string()]
        );

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_exclusions_dedup_and_trim passed");
    }

    /// 内容未变化时不重复写盘（避免文件时间戳与 mtime 无意义漂移）
    #[test]
    fn test_noop_replace_does_not_rewrite() {
        let path = temp_path("noop");
        let mgr = DrawSettingsManager::load(Some(&path));
        assert!(!path.exists());

        // 提交一份"等于默认值"的设置 → 不写盘
        mgr.replace(DrawSettings::default()).unwrap();
        assert!(!path.exists(), "内容未变化不应写盘");

        // 真正变化才写
        mgr.replace(DrawSettings {
            excluded_monsters: vec!["黑龙".into()],
            ..DrawSettings::default()
        })
        .unwrap();
        assert!(path.exists());

        let _ = fs::remove_dir_all(path.parent().unwrap());
        println!("[PASS] test_noop_replace_does_not_rewrite passed");
    }

    /// 后端作品白名单必须与前端 `GAME_ORDER` 逐项一致。
    /// 二者漂移会让前端出现"后端认得的作品、按钮里却没有"的鬼状态，必须锁死。
    #[test]
    fn test_game_ids_match_frontend_order() {
        let raw = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../src/lib/monsterList.ts"))
            .expect("应能读取 src/lib/monsterList.ts");
        let line = raw
            .lines()
            .find(|l| l.contains("GAME_ORDER"))
            .expect("monsterList.ts 应导出 GAME_ORDER");
        for id in GAME_IDS {
            assert!(
                line.contains(&format!("\"{}\"", id)),
                "GAME_ORDER 缺少 {}（后端 GAME_IDS 与前端不一致）: {}",
                id,
                line.trim()
            );
        }
        // 反向：前端不得有后端不认识的作品
        let inside = line
            .split('[')
            .nth(1)
            .and_then(|s| s.split(']').next())
            .expect("应能截出 GAME_ORDER 数组体");
        let frontend_ids: Vec<String> = inside
            .split(',')
            .map(|s| s.trim().trim_matches('"').to_string())
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(
            frontend_ids,
            GAME_IDS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "GAME_IDS 与前端 GAME_ORDER 必须逐项一致（含顺序）"
        );
        println!("[PASS] test_game_ids_match_frontend_order passed");
    }

    /// 武器图标资源必须就位：随机抽选的武器池只有 14 项且图标随包，
    /// 少一张就会在抽选盘上留下一个空白格 —— 而空白格在暗色舞台上极不显眼，
    /// 靠肉眼验收容易漏掉，用测试钉死。
    #[test]
    fn test_weapon_icon_files_exist() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("public")
            .join("weapon_icons")
            .join("MHWilds");
        assert!(dir.is_dir(), "武器图标目录不存在: {:?}", dir);

        let expected = [
            "MHWilds-Great_Sword_Icon_Base.png",
            "MHWilds-Long_Sword_Icon_Base.png",
            "MHWilds-Sword_and_Shield_Icon_Base.png",
            "MHWilds-Dual_Blades_Icon_Base.png",
            "MHWilds-Hammer_Icon_Base.png",
            "MHWilds-Hunting_Horn_Icon_Base.png",
            "MHWilds-Lance_Icon_Base.png",
            "MHWilds-Gunlance_Icon_Base.png",
            "MHWilds-Switch_Axe_Icon_Base.png",
            "MHWilds-Charge_Blade_Icon_Base.png",
            "MHWilds-Insect_Glaive_Icon_Base.png",
            "MHWilds-Light_Bowgun_Icon_Base.png",
            "MHWilds-Heavy_Bowgun_Icon_Base.png",
            "MHWilds-Bow_Icon_Base.png",
        ];
        let missing: Vec<&str> = expected
            .iter()
            .copied()
            .filter(|n| !dir.join(n).is_file())
            .collect();
        assert!(missing.is_empty(), "以下武器图标文件缺失: {:?}", missing);
        assert_eq!(expected.len(), 14, "武器池应为 14 把");

        // 图标文件名不得含 URL 转义字符（浏览器会解码后 404，前端 onError 静默隐藏）
        for name in expected {
            assert!(!name.contains('%'), "图标文件名含 URL 转义字符: {}", name);
        }

        println!(
            "[PASS] test_weapon_icon_files_exist passed ({} 张武器图标全部就位)",
            expected.len()
        );
    }
}
