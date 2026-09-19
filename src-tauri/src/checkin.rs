use chrono::{Datelike, Duration, Local, NaiveDate, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 用户个人档案数据
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UserProfile {
    pub uid: String,
    pub username: String,
    pub last_checkin_date: i32,
    pub continuous_days: i32,
    pub cumulative_days: i32,
    pub last_danmu_timestamp: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 打卡明细记录
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckinRecord {
    pub id: i64,
    pub uid: String,
    pub username: String,
    pub checkin_date: i32,
    pub created_at: i64,
}

/// 补签卡资产数据
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RetroactiveCardData {
    pub uid: String,
    pub card_count: i32,
    pub total_earned: i32,
    /// 本周首破领取标记（周起始日 YYYYMMDD），持久化于旧库列 monthly_first_claimed
    pub weekly_first_claimed: i32,
    pub last_earned_date: i32,
}

/// 批量补签执行结果
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BatchCheckinResult {
    pub success: bool,
    pub total_users: i32,
    pub patched_users: i32,
    pub total_inserted: i32,
    pub message: String,
}

/// 关键词学习记录（JSON 字段与原工程 ProfileManager::KeywordsToJson 完全一致）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeywordRecord {
    pub word: String,
    pub freq: i32,
    pub ts: i64,
}

/// 弹幕学习档案（对应 user_profiles 的 keywords_json / danmu_history_json / last_danmu_timestamp 三列）
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct LearningProfile {
    pub keywords: Vec<KeywordRecord>,
    /// [时间戳(秒), 内容] 二元组序列，与原工程 danmu_history_json 格式一致
    pub danmu_history: Vec<(i64, String)>,
    pub last_danmu_timestamp: i64,
}

/// 连续点赞数据（user_like_streaks）
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct LikeStreakData {
    pub uid: String,
    pub current_streak: i32,
    pub last_like_date: i32,
    pub streak_reward_issued: i32,
}

/// 点赞累加与奖卡结果（C3：区分“连续 7 天”与“今日突破 30”两条播报）
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct LikeRewards {
    pub streak_reward: bool,
    pub weekly_reward: bool,
    pub daily_total: i32,
}

/// 补签指令处理结果（含完整回复文案，对齐原工程 HandleRetroactiveCommand）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RetroCommandOutcome {
    pub success: bool,
    pub reply: String,
    pub remaining_cards: i32,
    pub new_continuous_days: i32,
    pub checkin_date: i32,
}

/// GM 水友搜索结果条目（档案 + 当前补签卡数）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSearchItem {
    #[serde(flatten)]
    pub profile: UserProfile,
    pub card_count: i32,
}

/// 补签指令词表（对齐原工程 RetroactiveCheckInModule::Init 的 SetTriggerWords 原文）
/// 分号前为操作词，分号后为查询词
pub const RETRO_TRIGGER_WORDS: &str = "补签,补签卡;补签查询,补签卡查询,查询补签,查询补签卡,我的补签卡";

/// 解析补签词表：返回 (操作词, 查询词)。
/// 不含分号时，含“查询”的词归入查询词（兼容旧格式），与原工程一致
pub fn parse_retro_trigger_words(raw: &str) -> (Vec<String>, Vec<String>) {
    let (retro_part, query_part) = match raw.split_once(';') {
        Some((r, q)) => (r, Some(q)),
        None => (raw, None),
    };
    let split_trim = |s: &str| -> Vec<String> {
        s.split(',')
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty())
            .collect()
    };
    match query_part {
        Some(q) => (split_trim(retro_part), split_trim(q)),
        None => {
            let mut retro = Vec::new();
            let mut query = Vec::new();
            for w in split_trim(retro_part) {
                if w.contains("查询") {
                    query.push(w);
                } else {
                    retro.push(w);
                }
            }
            (retro, query)
        }
    }
}

/// 是否补签操作指令（"补签" / "补签卡"）
pub fn is_retro_command(msg: &str) -> bool {
    parse_retro_trigger_words(RETRO_TRIGGER_WORDS)
        .0
        .iter()
        .any(|w| w == msg)
}

/// 是否补签查询指令（"补签查询" / "我的补签卡" 等）
pub fn is_retro_query(msg: &str) -> bool {
    parse_retro_trigger_words(RETRO_TRIGGER_WORDS)
        .1
        .iter()
        .any(|w| w == msg)
}

/// 舰长周打卡与补签系统管理器
pub struct CheckinManager {
    conn: Mutex<Connection>,
}

impl CheckinManager {
    /// 初始化 SQLite 数据库及数据表
    pub fn new(db_path: Option<&Path>) -> Result<Self, String> {
        let conn = match db_path {
            Some(p) => Connection::open(p).map_err(|e| e.to_string())?,
            None => {
                let default_path = Self::get_default_db_path();
                if let Some(parent) = default_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                Connection::open(default_path).map_err(|e| e.to_string())?
            }
        };

        let mgr = Self {
            conn: Mutex::new(conn),
        };
        mgr.init_schema()?;
        Ok(mgr)
    }

    /// 基于内存的测试数据库构造器
    pub fn new_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        let mgr = Self {
            conn: Mutex::new(conn),
        };
        mgr.init_schema()?;
        Ok(mgr)
    }

    /// 获取默认数据库路径（统一经 paths::config_dir）。
    /// 优先沿用原工程的 captain_profiles.db，保证升级后历史打卡数据不丢失。
    pub fn get_default_db_path() -> PathBuf {
        let dir = crate::paths::config_dir();
        // 最高优先：原工程历史打卡库（含历史打卡 / 补签 / 奖卡数据）
        let legacy = dir.join("captain_profiles.db");
        if legacy.exists() {
            return legacy;
        }
        dir.join("checkin.db")
    }

    /// 建表语句与原工程 captain_profiles.db 完全一致（表名/列名/可空性/默认值逐一对齐），
    /// 老库直接可用；V2 不执行任何 ALTER，也不创建原工程没有的表
    fn init_schema(&self) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS user_profiles (
                uid TEXT PRIMARY KEY,
                username TEXT NOT NULL,
                last_checkin_date INTEGER DEFAULT 0,
                continuous_days INTEGER DEFAULT 0,
                cumulative_days INTEGER DEFAULT 0,
                last_danmu_timestamp INTEGER DEFAULT 0,
                created_at INTEGER DEFAULT 0,
                updated_at INTEGER DEFAULT 0,
                keywords_json TEXT DEFAULT '[]',
                danmu_history_json TEXT DEFAULT '[]'
            );

            CREATE TABLE IF NOT EXISTS checkin_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                uid TEXT NOT NULL,
                checkin_date INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                username TEXT,
                UNIQUE(uid, checkin_date)
            );

            -- 列名保持旧库 monthly_first_claimed，V2 以该列承载“本周首破领取日”标记
            CREATE TABLE IF NOT EXISTS retroactive_cards (
                uid TEXT PRIMARY KEY,
                card_count INTEGER DEFAULT 0,
                total_earned INTEGER DEFAULT 0,
                monthly_first_claimed INTEGER DEFAULT 0,
                last_earned_date INTEGER DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS user_daily_likes (
                uid TEXT NOT NULL,
                like_date INTEGER NOT NULL,
                total_likes INTEGER DEFAULT 0,
                PRIMARY KEY (uid, like_date)
            );

            CREATE TABLE IF NOT EXISTS user_like_streaks (
                uid TEXT PRIMARY KEY,
                current_streak INTEGER DEFAULT 0,
                last_like_date INTEGER DEFAULT 0,
                streak_reward_issued INTEGER DEFAULT 0
            );
            "#,
        )
        .map_err(|e| e.to_string())?;

        Ok(())
    }

    pub fn date_to_int(d: NaiveDate) -> i32 {
        d.year() * 10000 + d.month() as i32 * 100 + d.day() as i32
    }

    pub fn int_to_date(val: i32) -> Option<NaiveDate> {
        let y = val / 10000;
        let m = (val % 10000) / 100;
        let d = val % 100;
        NaiveDate::from_ymd_opt(y, m as u32, d as u32)
    }

    pub fn get_today_int() -> i32 {
        Self::date_to_int(Local::now().date_naive())
    }

    pub fn get_year_week(d: NaiveDate) -> i32 {
        d.year() * 100 + d.iso_week().week() as i32
    }

    /// 返回所在自然周周一的 YYYYMMDD 整数（与原工程 DateUtils::GetWeekStartDate 一致，周一为首日）
    pub fn get_week_start_date(d: NaiveDate) -> i32 {
        let days_back = d.weekday().num_days_from_monday() as i64;
        let monday = d - Duration::days(days_back);
        Self::date_to_int(monday)
    }

    /// 用户常规打卡（如果当天已打卡则忽略，未打卡则记录并重新计算连续天数）
    pub fn record_checkin(
        &self,
        uid: &str,
        username: &str,
        date: NaiveDate,
    ) -> Result<UserProfile, String> {
        let conn = self.conn.lock().unwrap();
        let date_int = Self::date_to_int(date);
        let now = Utc::now().timestamp();

        // 尝试插入打卡明细
        let _ = conn.execute(
            "INSERT OR IGNORE INTO checkin_records (uid, username, checkin_date, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![uid, username, date_int, now],
        );

        // 从明细表重新计算真实连续天数与累计天数
        let continuous = Self::internal_calc_continuous(&conn, uid)?;
        let cumulative: i32 = conn
            .query_row(
                "SELECT COUNT(DISTINCT checkin_date) FROM checkin_records WHERE uid = ?1",
                params![uid],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // 更新或创建 Profile（不触碰学习字段 last_danmu_timestamp / keywords_json / danmu_history_json，
        // 该三列仅由弹幕学习链路写入 —— C5：原工程打卡路径同样不覆写 lastDanmuTimestamp）
        conn.execute(
            r#"
            INSERT INTO user_profiles (uid, username, last_checkin_date, continuous_days, cumulative_days, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
            ON CONFLICT(uid) DO UPDATE SET
                username = ?2,
                last_checkin_date = ?3,
                continuous_days = ?4,
                cumulative_days = ?5,
                updated_at = ?6
            "#,
            params![uid, username, date_int, continuous, cumulative, now],
        ).map_err(|e| e.to_string())?;

        // 回读真实行：created_at / last_danmu_timestamp 以库内既有值为准（C5）
        Self::get_profile_by_conn(&conn, uid)
    }

    /// 指定日期是否已存在打卡记录（用于“今日已打卡”判定）
    pub fn has_checkin_record(&self, uid: &str, date: NaiveDate) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(1) FROM checkin_records WHERE uid = ?1 AND checkin_date = ?2",
            params![uid, Self::date_to_int(date)],
            |row| row.get::<_, i32>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false)
    }

    /// 读取弹幕学习档案（keywords_json / danmu_history_json / last_danmu_timestamp）
    /// 行不存在或 JSON 损坏时返回空档案（与原工程容错一致）
    pub fn load_learning(&self, uid: &str) -> LearningProfile {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT keywords_json, danmu_history_json, last_danmu_timestamp FROM user_profiles WHERE uid = ?1",
            params![uid],
            |row| {
                Ok((
                    row.get::<_, String>(0).unwrap_or_else(|_| "[]".into()),
                    row.get::<_, String>(1).unwrap_or_else(|_| "[]".into()),
                    row.get::<_, i64>(2).unwrap_or(0),
                ))
            },
        )
        .map(|(kw, hist, ts)| LearningProfile {
            keywords: serde_json::from_str(&kw).unwrap_or_default(),
            danmu_history: serde_json::from_str(&hist).unwrap_or_default(),
            last_danmu_timestamp: ts,
        })
        .unwrap_or_default()
    }

    /// 写入弹幕学习档案：行不存在时创建（对齐原工程 SaveProfileToDb 的插入语义），
    /// 仅更新学习相关列与 username，不触碰打卡字段
    pub fn save_learning(
        &self,
        uid: &str,
        username: &str,
        profile: &LearningProfile,
    ) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().timestamp();
        let keywords_json = serde_json::to_string(&profile.keywords).map_err(|e| e.to_string())?;
        let history_json =
            serde_json::to_string(&profile.danmu_history).map_err(|e| e.to_string())?;

        conn.execute(
            r#"
            INSERT INTO user_profiles (uid, username, last_danmu_timestamp, created_at, updated_at, keywords_json, danmu_history_json)
            VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6)
            ON CONFLICT(uid) DO UPDATE SET
                username = ?2,
                last_danmu_timestamp = ?3,
                updated_at = ?4,
                keywords_json = ?5,
                danmu_history_json = ?6
            "#,
            params![uid, username, profile.last_danmu_timestamp, now, keywords_json, history_json],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 从 checkin_records 倒推计算连续打卡天数（防误差算法）
    fn internal_calc_continuous(
        conn: &Connection,
        uid: &str,
    ) -> Result<i32, String> {
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT checkin_date FROM checkin_records WHERE uid = ?1 ORDER BY checkin_date DESC",
            )
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map(params![uid], |row| row.get::<_, i32>(0))
            .map_err(|e| e.to_string())?;

        let mut dates = Vec::new();
        for r in rows {
            if let Ok(d_int) = r {
                if let Some(d) = Self::int_to_date(d_int) {
                    dates.push(d);
                }
            }
        }

        if dates.is_empty() {
            return Ok(0);
        }

        let mut streak = 1;
        for i in 1..dates.len() {
            if dates[i] + Duration::days(1) == dates[i - 1] {
                streak += 1;
            } else {
                break;
            }
        }

        Ok(streak)
    }

    /// 外部接口：从数据库倒推计算连续打卡天数
    pub fn calculate_continuous_days_from_records(&self, uid: &str) -> i32 {
        let conn = self.conn.lock().unwrap();
        Self::internal_calc_continuous(&conn, uid).unwrap_or(0)
    }

    /// 自然周点赞满 30 奖卡逻辑
    /// 点赞累加与奖卡逻辑：
    /// 规则 1：连续 7 天点赞，奖励 1 张补签卡
    /// 规则 2：自然周内首次「当日点赞累计」达 30 次，奖励 1 张补签卡（每周限领 1 次）
    /// 返回两条规则的触发情况（供 C3 分别播报）
    pub fn add_likes(
        &self,
        uid: &str,
        likes: i32,
        date: NaiveDate,
    ) -> Result<LikeRewards, String> {
        let conn = self.conn.lock().unwrap();
        let date_int = Self::date_to_int(date);
        let mut rewards = LikeRewards::default();

        // 1. 规则 1：连续 7 天点赞奖励
        let streak_row: Option<(i32, i32, i32)> = conn
            .query_row(
                "SELECT current_streak, last_like_date, streak_reward_issued FROM user_like_streaks WHERE uid = ?1",
                params![uid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok();

        let (mut current_streak, last_like_date, mut streak_reward_issued) = streak_row.unwrap_or((0, 0, 0));
        let last_date_opt = Self::int_to_date(last_like_date);

        if let Some(ld) = last_date_opt {
            if date == ld + Duration::days(1) {
                current_streak += 1;
            } else if date != ld {
                current_streak = 1;
            }
        } else {
            current_streak = 1;
        }

        if current_streak >= 7 && (current_streak % 7) == 0 && streak_reward_issued != date_int {
            streak_reward_issued = date_int;
            rewards.streak_reward = true;
            conn.execute(
                r#"
                INSERT INTO retroactive_cards (uid, card_count, total_earned, monthly_first_claimed, last_earned_date)
                VALUES (?1, 1, 1, 0, ?2)
                ON CONFLICT(uid) DO UPDATE SET
                    card_count = card_count + 1,
                    total_earned = total_earned + 1,
                    last_earned_date = ?2
                "#,
                params![uid, date_int],
            ).map_err(|e| e.to_string())?;
        }

        conn.execute(
            r#"
            INSERT INTO user_like_streaks (uid, current_streak, last_like_date, streak_reward_issued)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(uid) DO UPDATE SET
                current_streak = ?2,
                last_like_date = ?3,
                streak_reward_issued = ?4
            "#,
            params![uid, current_streak, date_int, streak_reward_issued],
        ).map_err(|e| e.to_string())?;

        // 2. 规则 2：自然周内首次「当日点赞累计」达 30 奖卡（每周限 1 张）
        //    与原子工程语义一致：阈值比较的是当日累计（user_daily_likes），而非周累计。
        conn.execute(
            r#"
            INSERT INTO user_daily_likes (uid, like_date, total_likes)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(uid, like_date) DO UPDATE SET
                total_likes = total_likes + ?3
            "#,
            params![uid, date_int, likes],
        ).map_err(|e| e.to_string())?;

        let total_likes: i32 = conn
            .query_row(
                "SELECT total_likes FROM user_daily_likes WHERE uid = ?1 AND like_date = ?2",
                params![uid, date_int],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;

        let week_start = Self::get_week_start_date(date);
        // 旧库列名为 monthly_first_claimed，V2 以该列承载“本周首破领取日”标记（不修改旧库格式）
        let weekly_first_claimed: i32 = conn
            .query_row(
                "SELECT monthly_first_claimed FROM retroactive_cards WHERE uid = ?1",
                params![uid],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if total_likes >= 30 && weekly_first_claimed != week_start {
            conn.execute(
                r#"
                INSERT INTO retroactive_cards (uid, card_count, total_earned, monthly_first_claimed, last_earned_date)
                VALUES (?1, 1, 1, ?2, ?3)
                ON CONFLICT(uid) DO UPDATE SET
                    card_count = card_count + 1,
                    total_earned = total_earned + 1,
                    monthly_first_claimed = ?2,
                    last_earned_date = ?3
                "#,
                params![uid, week_start, date_int],
            ).map_err(|e| e.to_string())?;

            rewards.weekly_reward = true;
        }

        rewards.daily_total = total_likes;
        Ok(rewards)
    }

    /// 读取连续点赞数据（行不存在返回 None，用于区分“无记录”与“有记录但为 0”）
    pub fn get_like_streak(&self, uid: &str) -> Option<LikeStreakData> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT uid, current_streak, last_like_date, streak_reward_issued FROM user_like_streaks WHERE uid = ?1",
            params![uid],
            |row| {
                Ok(LikeStreakData {
                    uid: row.get(0)?,
                    current_streak: row.get(1)?,
                    last_like_date: row.get(2)?,
                    streak_reward_issued: row.get(3)?,
                })
            },
        )
        .ok()
    }

    /// 读取指定日期的当日点赞累计（行不存在返回 None）
    pub fn get_daily_like_total(&self, uid: &str, date: NaiveDate) -> Option<i32> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT total_likes FROM user_daily_likes WHERE uid = ?1 AND like_date = ?2",
            params![uid, Self::date_to_int(date)],
            |row| row.get::<_, i32>(0),
        )
        .ok()
    }

    /// 读取补签卡资产（行不存在返回 None）
    fn load_cards(conn: &Connection, uid: &str) -> Option<RetroactiveCardData> {
        conn.query_row(
            "SELECT uid, card_count, total_earned, monthly_first_claimed, last_earned_date FROM retroactive_cards WHERE uid = ?1",
            params![uid],
            |row| {
                Ok(RetroactiveCardData {
                    uid: row.get(0)?,
                    card_count: row.get(1)?,
                    total_earned: row.get(2)?,
                    weekly_first_claimed: row.get(3)?,
                    last_earned_date: row.get(4)?,
                })
            },
        )
        .ok()
    }

    /// 补签指令完整流程与回复文案
    /// 对齐原工程 RetroactiveCheckInModule::HandleRetroactiveCommand：
    /// 无卡档案 → 系统错误；卡数为 0 → 无补签卡提示；无需补签 → 提示；无缺失日期 → 提示；
    /// 执行成功 → “已成功补签 X月X日，剩余补签卡N张，连续打卡恢复为M天！”
    pub fn retro_command_outcome(
        &self,
        uid: &str,
        username: &str,
        date: NaiveDate,
    ) -> RetroCommandOutcome {
        let conn = self.conn.lock().unwrap();

        let Some(cards) = Self::load_cards(&conn, uid) else {
            return RetroCommandOutcome {
                reply: format!("{}，系统错误，请稍后再试。", username),
                ..Default::default()
            };
        };

        if cards.card_count <= 0 {
            return RetroCommandOutcome {
                reply: format!("{}，你没有补签卡哦~", username),
                remaining_cards: cards.card_count,
                ..Default::default()
            };
        }

        let continuous = Self::internal_calc_continuous(&conn, uid).unwrap_or(0);
        let cumulative: i32 = conn
            .query_row(
                "SELECT COUNT(DISTINCT checkin_date) FROM checkin_records WHERE uid = ?1",
                params![uid],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if cumulative > 0 && continuous >= cumulative {
            return RetroCommandOutcome {
                reply: format!(
                    "{}，当前连续打卡{}天、累计{}天，无需补签哦~",
                    username, continuous, cumulative
                ),
                remaining_cards: cards.card_count,
                new_continuous_days: continuous,
                ..Default::default()
            };
        }

        // 先释放连接锁：find_last_missing_checkin_date / execute_retroactive_checkin 会再次加锁
        drop(conn);

        let Some(target) = self.find_last_missing_checkin_date(uid, date) else {
            return RetroCommandOutcome {
                reply: format!("{}，当前没有需要补签的日期。", username),
                remaining_cards: cards.card_count,
                ..Default::default()
            };
        };
        let target_int = Self::date_to_int(target);

        match self.execute_retroactive_checkin(uid, username, target) {
            Ok(remaining_cards) => {
                let new_continuous = self.calculate_continuous_days_from_records(uid);
                let display_cards = self
                    .get_cards(uid)
                    .card_count
                    .max(remaining_cards);
                RetroCommandOutcome {
                    success: true,
                    reply: format!(
                        "{}，已成功补签{}月{}日，剩余补签卡{}张，连续打卡恢复为{}天！",
                        username,
                        (target_int / 100) % 100,
                        target_int % 100,
                        display_cards,
                        new_continuous
                    ),
                    remaining_cards: display_cards,
                    new_continuous_days: new_continuous,
                    checkin_date: target_int,
                }
            }
            Err(_) => RetroCommandOutcome {
                reply: format!("{}，补签失败，请稍后再试。", username),
                remaining_cards: cards.card_count,
                ..Default::default()
            },
        }
    }

    /// 补签查询回复文案（对齐原工程 HandleQueryCommand；仅气泡不朗读，v24 决策）
    /// 三份数据（卡档案 / 连续点赞 / 当日点赞）全无时，与原工程一致地返回系统错误提示
    pub fn query_reply(&self, uid: &str, username: &str, date: NaiveDate) -> String {
        let conn = self.conn.lock().unwrap();
        let cards = Self::load_cards(&conn, uid);
        let streak = conn
            .query_row(
                "SELECT current_streak FROM user_like_streaks WHERE uid = ?1",
                params![uid],
                |row| row.get::<_, i32>(0),
            )
            .ok();
        let daily_total = conn
            .query_row(
                "SELECT total_likes FROM user_daily_likes WHERE uid = ?1 AND like_date = ?2",
                params![uid, Self::date_to_int(date)],
                |row| row.get::<_, i32>(0),
            )
            .ok();

        if cards.is_none() && streak.is_none() && daily_total.is_none() {
            return format!("{}，系统错误，请稍后再试。", username);
        }

        let card_count = cards.as_ref().map(|c| c.card_count).unwrap_or(0);
        let mut out = format!("{}，补签卡{}张", username, card_count);

        let current_streak = streak.unwrap_or(0);
        let remaining_streak = 7 - current_streak;
        if remaining_streak <= 0 {
            out.push_str("\n连续点赞7天：已满足，下次领取");
        } else {
            out.push_str(&format!(
                "\n连续点赞7天：已{}天，差{}天",
                current_streak, remaining_streak
            ));
        }

        let week_start = Self::get_week_start_date(date);
        let weekly_claimed = cards
            .as_ref()
            .map(|c| c.weekly_first_claimed > 0 && c.weekly_first_claimed == week_start)
            .unwrap_or(false);
        if weekly_claimed {
            out.push_str("\n每周点赞30：已领取");
        } else {
            let current_likes = daily_total.unwrap_or(0);
            let remaining_likes = 30 - current_likes;
            if remaining_likes <= 0 {
                out.push_str("\n每周点赞30：已满足，可领取");
            } else {
                out.push_str(&format!("\n每周点赞30：{}/30，差{}", current_likes, remaining_likes));
            }
        }

        out
    }

    /// 补签有效性校验：
    /// 实时从 checkin_records 重算，当连续打卡天数 >= 累计打卡天数时，说明用户从未漏打，无需补签，予以拦截
    pub fn check_retroactive_validity(&self, uid: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let continuous = Self::internal_calc_continuous(&conn, uid)?;
        let cumulative: i32 = conn
            .query_row(
                "SELECT COUNT(DISTINCT checkin_date) FROM checkin_records WHERE uid = ?1",
                params![uid],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if continuous >= cumulative && cumulative > 0 {
            return Err("拦截校验失败：连续打卡天数已达到或超过累计打卡天数，当前没有断签断档，无需补签".into());
        }
        Ok(())
    }

    /// 查找最近一个缺失的打卡日期（用于补签）
    /// 从当前日期（含当前日期）向前倒序检查，找到最近的缺失日期
    pub fn find_last_missing_checkin_date(
        &self,
        uid: &str,
        current_date: NaiveDate,
    ) -> Option<NaiveDate> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT DISTINCT checkin_date FROM checkin_records WHERE uid = ?1 ORDER BY checkin_date ASC")
            .ok()?;
        let rows = stmt.query_map(params![uid], |r| r.get::<_, i32>(0)).ok()?;

        let mut existing = HashSet::new();
        let mut min_date = None;
        for r in rows.flatten() {
            if let Some(d) = Self::int_to_date(r) {
                existing.insert(d);
                if min_date.is_none() || Some(d) < min_date {
                    min_date = Some(d);
                }
            }
        }

        let start = min_date?;
        let mut cursor = current_date;

        // 倒序寻找从当前日期到最早打卡日之间的第一个缺漏日期
        while cursor >= start {
            if !existing.contains(&cursor) {
                return Some(cursor);
            }
            cursor = cursor - Duration::days(1);
        }

        None
    }

    /// 原子事务执行补签 (ExecuteRetroactiveCheckin)
    /// 步骤：校验有效性 -> 扣减补签卡 -> 插入打卡明细 -> 重算连续/累计天数 -> 更新 Profile
    pub fn execute_retroactive_checkin(
        &self,
        uid: &str,
        username: &str,
        target_date: NaiveDate,
    ) -> Result<i32, String> {
        let mut conn = self.conn.lock().unwrap();

        // 1. 补签有效性校验（实时从记录动态重算，防脏数据或并发误差）
        let cur_continuous = Self::internal_calc_continuous(&conn, uid)?;
        let cur_cumulative: i32 = conn
            .query_row(
                "SELECT COUNT(DISTINCT checkin_date) FROM checkin_records WHERE uid = ?1",
                params![uid],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if cur_continuous >= cur_cumulative && cur_cumulative > 0 {
            return Err("拦截：连续打卡天数已等于累计打卡天数，无需补签".into());
        }

        // 2. 开启原子事务
        let tx = conn.transaction().map_err(|e| e.to_string())?;

        // 3. 校验卡片数量
        let card_count: i32 = tx
            .query_row(
                "SELECT card_count FROM retroactive_cards WHERE uid = ?1",
                params![uid],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if card_count <= 0 {
            return Err("补签卡不足，无法补签".into());
        }

        // 4. 扣除 1 张卡
        let new_card_count = card_count - 1;
        tx.execute(
            "UPDATE retroactive_cards SET card_count = ?1 WHERE uid = ?2",
            params![new_card_count, uid],
        )
        .map_err(|e| e.to_string())?;

        // 5. 插入目标打卡记录
        let target_date_int = Self::date_to_int(target_date);
        let now = Utc::now().timestamp();
        tx.execute(
            "INSERT INTO checkin_records (uid, username, checkin_date, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![uid, username, target_date_int, now],
        ).map_err(|e| format!("插入补签记录失败（可能已存在该日记录）: {}", e))?;

        // 6. 重算连续天数与累计天数
        let new_continuous = Self::internal_calc_continuous(&tx, uid)?;
        let new_cumulative: i32 = tx
            .query_row(
                "SELECT COUNT(DISTINCT checkin_date) FROM checkin_records WHERE uid = ?1",
                params![uid],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // 7. 更新 UserProfile
        tx.execute(
            r#"
            UPDATE user_profiles SET
                continuous_days = ?1,
                cumulative_days = ?2,
                updated_at = ?3
            WHERE uid = ?4
            "#,
            params![new_continuous, new_cumulative, now, uid],
        )
        .map_err(|e| e.to_string())?;

        // 8. 提交事务
        tx.commit().map_err(|e| e.to_string())?;

        Ok(new_card_count)
    }

    /// 查询用户档案
    pub fn get_profile(&self, uid: &str) -> Result<UserProfile, String> {
        let conn = self.conn.lock().unwrap();
        Self::get_profile_by_conn(&conn, uid)
    }

    fn get_profile_by_conn(conn: &Connection, uid: &str) -> Result<UserProfile, String> {
        conn.query_row(
            r#"
            SELECT uid, username, last_checkin_date, continuous_days, cumulative_days, last_danmu_timestamp, created_at, updated_at
            FROM user_profiles WHERE uid = ?1
            "#,
            params![uid],
            |row| {
                Ok(UserProfile {
                    uid: row.get(0)?,
                    username: row.get(1)?,
                    last_checkin_date: row.get(2)?,
                    continuous_days: row.get(3)?,
                    cumulative_days: row.get(4)?,
                    last_danmu_timestamp: row.get(5)?,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            },
        ).map_err(|_| format!("User profile not found: {}", uid))
    }

    /// 查询补签卡资产
    pub fn get_cards(&self, uid: &str) -> RetroactiveCardData {
        let conn = self.conn.lock().unwrap();
        Self::load_cards(&conn, uid).unwrap_or_else(|| RetroactiveCardData {
            uid: uid.to_string(),
            ..Default::default()
        })
    }

    /// 手动调整或发卡（GM 操作）
    pub fn grant_card(&self, uid: &str, count: i32) -> Result<i32, String> {
        if count <= 0 {
            return Err("发卡数量必须大于 0".into());
        }
        let conn = self.conn.lock().unwrap();
        let today_int = Self::get_today_int();
        conn.execute(
            r#"
            INSERT INTO retroactive_cards (uid, card_count, total_earned, monthly_first_claimed, last_earned_date)
            VALUES (?1, ?2, ?2, 0, ?3)
            ON CONFLICT(uid) DO UPDATE SET
                card_count = card_count + ?2,
                total_earned = total_earned + ?2,
                last_earned_date = ?3
            "#,
            params![uid, count, today_int],
        ).map_err(|e| e.to_string())?;

        let updated: i32 = conn
            .query_row(
                "SELECT card_count FROM retroactive_cards WHERE uid = ?1",
                params![uid],
                |r| r.get(0),
            )
            .unwrap_or(0);

        Ok(updated)
    }

    /// 模糊搜索水友 (GM 功能)
    /// 返回档案 + 当前补签卡数；LIKE 通配符已转义，避免用户输入 % / _ 造成全表匹配
    pub fn search_users(&self, keyword: &str) -> Result<Vec<UserSearchItem>, String> {
        let conn = self.conn.lock().unwrap();
        let escaped = keyword
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{}%", escaped);
        let mut stmt = conn
            .prepare(
                r#"
            SELECT p.uid, p.username, p.last_checkin_date, p.continuous_days, p.cumulative_days,
                   p.last_danmu_timestamp, p.created_at, p.updated_at,
                   COALESCE(c.card_count, 0)
            FROM user_profiles p
            LEFT JOIN retroactive_cards c ON c.uid = p.uid
            WHERE p.username LIKE ?1 ESCAPE '\' OR p.uid LIKE ?1 ESCAPE '\'
            ORDER BY p.updated_at DESC
            LIMIT 50
            "#,
            )
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map(params![pattern], |row| {
                Ok(UserSearchItem {
                    profile: UserProfile {
                        uid: row.get(0)?,
                        username: row.get(1)?,
                        last_checkin_date: row.get(2)?,
                        continuous_days: row.get(3)?,
                        cumulative_days: row.get(4)?,
                        last_danmu_timestamp: row.get(5)?,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                    },
                    card_count: row.get(8)?,
                })
            })
            .map_err(|e| e.to_string())?;

        let mut list = Vec::new();
        for r in rows.flatten() {
            list.push(r);
        }
        Ok(list)
    }

    /// 一键黑幕批量补签 (GM 功能)
    /// 为所有累计天数大于连续天数的用户，自动补齐缺失的历史日期，使连续天数拉满
    pub fn batch_checkin(&self) -> Result<BatchCheckinResult, String> {
        let mut conn = self.conn.lock().unwrap();
        let today = Local::now().date_naive();
        let now = Utc::now().timestamp();

        let mut stmt = conn
            .prepare("SELECT uid, username FROM user_profiles WHERE cumulative_days > continuous_days")
            .map_err(|e| e.to_string())?;

        let users: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| e.to_string())?
            .flatten()
            .collect();

        drop(stmt);

        let total_users = users.len() as i32;
        let mut patched_users = 0;
        let mut total_inserted = 0;

        let tx = conn.transaction().map_err(|e| e.to_string())?;

        for (uid, username) in &users {
            let mut rec_stmt = tx
                .prepare("SELECT DISTINCT checkin_date FROM checkin_records WHERE uid = ?1 ORDER BY checkin_date ASC")
                .map_err(|e| e.to_string())?;

            let existing: HashSet<NaiveDate> = rec_stmt
                .query_map(params![uid], |r| r.get::<_, i32>(0))
                .map_err(|e| e.to_string())?
                .flatten()
                .filter_map(Self::int_to_date)
                .collect();

            if existing.is_empty() {
                continue;
            }

            let start = *existing.iter().min().unwrap();
            let mut cursor = start;
            let mut user_inserted = 0;

            while cursor <= today {
                if !existing.contains(&cursor) {
                    let d_int = Self::date_to_int(cursor);
                    let _ = tx.execute(
                        "INSERT OR IGNORE INTO checkin_records (uid, username, checkin_date, created_at) VALUES (?1, ?2, ?3, ?4)",
                        params![uid, username, d_int, now],
                    );
                    user_inserted += 1;
                }
                cursor = cursor + Duration::days(1);
            }

            if user_inserted > 0 {
                total_inserted += user_inserted;
                patched_users += 1;

                // 重算该用户的连续天数
                let new_continuous = Self::internal_calc_continuous(&tx, uid)?;
                let new_cumulative: i32 = tx
                    .query_row(
                        "SELECT COUNT(DISTINCT checkin_date) FROM checkin_records WHERE uid = ?1",
                        params![uid],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);

                let _ = tx.execute(
                    "UPDATE user_profiles SET continuous_days = ?1, cumulative_days = ?2, updated_at = ?3 WHERE uid = ?4",
                    params![new_continuous, new_cumulative, now, uid],
                );
            }
        }

        tx.commit().map_err(|e| e.to_string())?;

        Ok(BatchCheckinResult {
            success: true,
            total_users,
            patched_users,
            total_inserted,
            message: format!("批量补签完成：覆盖 {} 位断签用户，补签记录 {} 条", patched_users, total_inserted),
        })
    }

    /// 导出打卡记录内容（CSV / JSON，UTF-8 BOM 前置，格式对齐原工程 DataBridgeExports）
    /// 支持按昵称模糊筛选（命中多个 UID 合并导出）与日期范围；记录按打卡日期降序
    pub fn export_records_content(
        &self,
        format: &str,
        username: Option<&str>,
        start: Option<NaiveDate>,
        end: Option<NaiveDate>,
    ) -> Result<String, String> {
        if format != "csv" && format != "json" {
            return Err("Unsupported format. Use 'csv' or 'json'".into());
        }

        let conn = self.conn.lock().unwrap();

        // 昵称筛选：部分匹配取全部 UID（对齐原工程 GetUidsByUsernamePartial）
        let mut uids: Vec<String> = Vec::new();
        if let Some(name) = username.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let escaped = name
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            let pattern = format!("%{}%", escaped);
            let mut stmt = conn
                .prepare("SELECT uid FROM user_profiles WHERE username LIKE ?1 ESCAPE '\\'")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![pattern], |row| row.get::<_, String>(0))
                .map_err(|e| e.to_string())?;
            uids = rows.flatten().collect();
            if uids.is_empty() {
                return Err("User not found".into());
            }
        }

        let mut sql =
            String::from("SELECT uid, username, checkin_date, created_at FROM checkin_records WHERE 1=1");
        let mut values: Vec<rusqlite::types::Value> = Vec::new();
        if !uids.is_empty() {
            let placeholders = vec!["?"; uids.len()].join(",");
            sql.push_str(&format!(" AND uid IN ({})", placeholders));
            for u in uids {
                values.push(rusqlite::types::Value::Text(u));
            }
        }
        if let Some(s) = start {
            sql.push_str(" AND checkin_date >= ?");
            values.push(rusqlite::types::Value::Integer(Self::date_to_int(s) as i64));
        }
        if let Some(e) = end {
            sql.push_str(" AND checkin_date <= ?");
            values.push(rusqlite::types::Value::Integer(Self::date_to_int(e) as i64));
        }
        sql.push_str(" ORDER BY checkin_date DESC");

        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(values), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i32>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let records: Vec<(String, String, i32, i64)> = rows.flatten().collect();

        let body = match format {
            "csv" => {
                let mut s = String::from("uid,username,checkin_date,created_at\n");
                for (uid, name, date, created) in &records {
                    s.push_str(&format!("{},{},{},{}\n", uid, name, date, created));
                }
                s
            }
            _ => {
                let mut s = String::from("[\n");
                for (i, (uid, name, date, created)) in records.iter().enumerate() {
                    s.push_str("  {\n");
                    s.push_str(&format!("    \"uid\": \"{}\",\n", uid));
                    s.push_str(&format!("    \"username\": \"{}\",\n", name));
                    s.push_str(&format!("    \"checkinDate\": {},\n", date));
                    s.push_str(&format!("    \"createdAt\": {}\n", created));
                    s.push_str("  }");
                    if i < records.len() - 1 {
                        s.push(',');
                    }
                    s.push('\n');
                }
                s.push_str("]\n");
                s
            }
        };

        // UTF-8 BOM 前置（Excel 直接打开不乱码，对齐原工程）
        Ok(format!("\u{FEFF}{}", body))
    }
}

use std::collections::HashSet;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_continuous_days_backward_calculation() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let base = NaiveDate::from_ymd_opt(2026, 3, 10).unwrap();

        // 打卡第 10 天
        let p1 = mgr.record_checkin("u1", "水友1", base).unwrap();
        assert_eq!(p1.continuous_days, 1);
        assert_eq!(p1.cumulative_days, 1);

        // 打卡第 9 天（模拟倒序补签或历史数据）
        let d9 = base - Duration::days(1);
        let _ = mgr.record_checkin("u1", "水友1", d9).unwrap();
        let c = mgr.calculate_continuous_days_from_records("u1");
        assert_eq!(c, 2);

        // 打卡第 8 天
        let d8 = base - Duration::days(2);
        let _ = mgr.record_checkin("u1", "水友1", d8).unwrap();
        let c = mgr.calculate_continuous_days_from_records("u1");
        assert_eq!(c, 3);

        // 打卡第 6 天（中间缺了第 7 天）
        let d6 = base - Duration::days(4);
        let _ = mgr.record_checkin("u1", "水友1", d6).unwrap();
        // 连续天数依然应为 3（8, 9, 10 连续；6 与 8 之间有断档）
        let c = mgr.calculate_continuous_days_from_records("u1");
        assert_eq!(c, 3);
        println!("[PASS] test_continuous_days_backward_calculation passed");
    }

    #[test]
    fn test_weekly_30_likes_card_reward() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 3, 10).unwrap();

        // 点赞 15 次，未到 30
        let r1 = mgr.add_likes("u_like", 15, date).unwrap();
        assert!(!r1.weekly_reward);
        assert_eq!(mgr.get_cards("u_like").card_count, 0);

        // 再点赞 15 次，达到 30 触发奖励
        let r2 = mgr.add_likes("u_like", 15, date).unwrap();
        assert!(r2.weekly_reward);
        assert_eq!(r2.daily_total, 30);
        let cards = mgr.get_cards("u_like");
        assert_eq!(cards.card_count, 1);
        assert_eq!(cards.total_earned, 1);

        // 同一周内继续点赞，不再重复发放
        let r3 = mgr.add_likes("u_like", 50, date).unwrap();
        assert!(!r3.weekly_reward);
        assert_eq!(mgr.get_cards("u_like").card_count, 1);
        println!("[PASS] test_weekly_30_likes_card_reward passed");
    }

    #[test]
    fn test_weekly_first_daily_30_rule() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        // 2026-03-10 为周二，同周；下周同一天为 2026-03-17
        let d1 = NaiveDate::from_ymd_opt(2026, 3, 10).unwrap();
        let d2 = d1 + Duration::days(1);
        let next_week = d1 + Duration::days(7);

        // 当日累计 20，未达标
        assert!(!mgr.add_likes("u_w", 20, d1).unwrap().weekly_reward);
        // 当日补 10 → 当日累计 30 → 发卡
        assert!(mgr.add_likes("u_w", 10, d1).unwrap().weekly_reward);
        assert_eq!(mgr.get_cards("u_w").card_count, 1);

        // 同一自然周的另一天再达 30 → 本周已领，不再发放
        assert!(!mgr.add_likes("u_w", 30, d2).unwrap().weekly_reward);
        assert_eq!(mgr.get_cards("u_w").card_count, 1);

        // 下一自然周当日达 30 → 可再领一张
        assert!(mgr.add_likes("u_w", 30, next_week).unwrap().weekly_reward);
        assert_eq!(mgr.get_cards("u_w").card_count, 2);

        println!("[PASS] test_weekly_first_daily_30_rule passed");
    }

    /// 生成与真实旧库 captain_profiles.db 完全一致的表结构（列名/可空性/默认值逐一对齐）
    fn create_legacy_schema(conn: &Connection) {
        conn.execute_batch(
            r#"
            CREATE TABLE user_profiles (uid TEXT PRIMARY KEY,username TEXT NOT NULL,last_checkin_date INTEGER DEFAULT 0,continuous_days INTEGER DEFAULT 0,last_danmu_timestamp INTEGER DEFAULT 0,created_at INTEGER DEFAULT 0,updated_at INTEGER DEFAULT 0,keywords_json TEXT DEFAULT '[]',danmu_history_json TEXT DEFAULT '[]', cumulative_days INTEGER DEFAULT 0);
            CREATE TABLE checkin_records (id INTEGER PRIMARY KEY AUTOINCREMENT,uid TEXT NOT NULL,checkin_date INTEGER NOT NULL,created_at INTEGER NOT NULL,username TEXT,UNIQUE(uid, checkin_date));
            CREATE TABLE user_daily_likes (uid TEXT NOT NULL,like_date INTEGER NOT NULL,total_likes INTEGER DEFAULT 0,PRIMARY KEY (uid, like_date));
            CREATE TABLE user_like_streaks (uid TEXT PRIMARY KEY,current_streak INTEGER DEFAULT 0,last_like_date INTEGER DEFAULT 0,streak_reward_issued INTEGER DEFAULT 0);
            CREATE TABLE retroactive_cards (uid TEXT PRIMARY KEY,card_count INTEGER DEFAULT 0,total_earned INTEGER DEFAULT 0,monthly_first_claimed INTEGER DEFAULT 0,last_earned_date INTEGER DEFAULT 0);
            "#,
        )
        .unwrap();
    }

    /// 读取表结构快照（表名 -> CREATE 语句），用于断言 V2 不修改旧库格式
    fn schema_snapshot(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT name, sql FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        rows.flatten().collect()
    }

    /// 定位仓库内的真实旧库（源码只读，测试使用副本）
    fn find_repo_legacy_db() -> Option<PathBuf> {
        crate::paths::find_resource("captain_profiles.db")
    }

    #[test]
    fn test_open_legacy_captain_profiles_db() {
        let temp_dir = std::env::temp_dir().join("mh_test_legacy_db");
        let _ = std::fs::create_dir_all(&temp_dir);
        let path = temp_dir.join("captain_profiles.db");
        let _ = std::fs::remove_file(&path);

        // 模拟真实旧库：retroactive_cards 仅有 monthly_first_claimed，无任何 V2 独有表
        let before_schema;
        {
            let conn = Connection::open(&path).unwrap();
            create_legacy_schema(&conn);
            conn.execute_batch(
                r#"
                INSERT INTO user_profiles (uid, username, cumulative_days) VALUES ('old_u', '老舰长', 5);
                INSERT INTO checkin_records (uid, checkin_date, created_at, username) VALUES ('old_u', 20260301, 111, '老舰长');
                INSERT INTO retroactive_cards (uid, card_count, total_earned, monthly_first_claimed, last_earned_date) VALUES ('old_u', 2, 2, 20260101, 20260101);
                "#,
            )
            .unwrap();
            before_schema = schema_snapshot(&conn);
        }

        // 用 V2 的 CheckinManager 直接打开旧库（无迁移、无 ALTER）
        let mgr = CheckinManager::new(Some(&path)).unwrap();

        // 旧数据（含旧列 monthly_first_claimed 的月度语义值）可正常读出
        let p = mgr.get_profile("old_u").unwrap();
        assert_eq!(p.username, "老舰长");
        let cards = mgr.get_cards("old_u");
        assert_eq!(cards.card_count, 2);
        assert_eq!(cards.total_earned, 2);

        // 新打卡写入不破坏旧表结构（keywords_json 等额外列由默认值兼容）
        let today = NaiveDate::from_ymd_opt(2026, 3, 20).unwrap();
        let p2 = mgr.record_checkin("old_u", "老舰长", today).unwrap();
        assert_eq!(p2.cumulative_days, 2);

        // 旧列 monthly_first_claimed（20260101）与本周起始日不同，视作本周未领取：同日达 30 正常发卡
        assert!(mgr.add_likes("old_u", 30, today).unwrap().weekly_reward);
        assert_eq!(mgr.get_cards("old_u").card_count, 3);

        // 手动发卡正常
        assert_eq!(mgr.grant_card("old_u", 1).unwrap(), 4);

        // 关键断言：V2 打开并读写后，旧库表结构完全未变（无 ALTER、无新表）
        {
            let conn = Connection::open(&path).unwrap();
            assert_eq!(before_schema, schema_snapshot(&conn), "V2 不得修改旧库表结构");
        }
        drop(mgr);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&temp_dir);
        println!("[PASS] test_open_legacy_captain_profiles_db passed");
    }

    #[test]
    fn test_v2_schema_matches_legacy_format() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let conn = mgr.conn.lock().unwrap();

        // retroactive_cards 必须使用旧库列名 monthly_first_claimed，且不存在 weekly_first_claimed
        let mut stmt = conn.prepare("PRAGMA table_info(retroactive_cards)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .flatten()
            .collect();
        assert!(cols.contains(&"monthly_first_claimed".to_string()), "cols: {:?}", cols);
        assert!(!cols.contains(&"weekly_first_claimed".to_string()), "cols: {:?}", cols);

        // 不创建原工程没有的 weekly_likes 表；原工程同构表齐全
        let names: Vec<String> = schema_snapshot(&conn).into_iter().map(|(n, _)| n).collect();
        assert!(!names.contains(&"weekly_likes".to_string()), "tables: {:?}", names);
        for t in ["user_profiles", "checkin_records", "user_daily_likes", "user_like_streaks", "retroactive_cards"] {
            assert!(names.contains(&t.to_string()), "missing table {}: {:?}", t, names);
        }

        // user_profiles 保持原工程同构列
        let mut stmt = conn.prepare("PRAGMA table_info(user_profiles)").unwrap();
        let up_cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .flatten()
            .collect();
        assert!(up_cols.contains(&"keywords_json".to_string()), "cols: {:?}", up_cols);
        assert!(up_cols.contains(&"danmu_history_json".to_string()), "cols: {:?}", up_cols);
        assert!(up_cols.contains(&"cumulative_days".to_string()), "cols: {:?}", up_cols);

        println!("[PASS] test_v2_schema_matches_legacy_format passed");
    }

    #[test]
    fn test_real_repo_legacy_db_copy_compatible() {
        let Some(src) = find_repo_legacy_db() else {
            println!("[SKIP] test_real_repo_legacy_db_copy_compatible: 未找到 MonsterOrderWilds_configs/captain_profiles.db");
            return;
        };

        let temp_dir = std::env::temp_dir().join("mh_test_real_legacy_db");
        let _ = std::fs::create_dir_all(&temp_dir);
        let dst = temp_dir.join("captain_profiles.db");
        let _ = std::fs::remove_file(&dst);
        std::fs::copy(&src, &dst).expect("copy legacy db");

        let before_schema = {
            let conn = Connection::open(&dst).unwrap();
            schema_snapshot(&conn)
        };

        // 在真实旧库副本上执行全链路读写（合成 uid，避免触碰历史数据）
        let mgr = CheckinManager::new(Some(&dst)).unwrap();
        let uid = "v2_compat_probe";
        let today = Local::now().date_naive();

        let _ = mgr.record_checkin(uid, "兼容探针", today).unwrap();
        let _ = mgr.record_checkin(uid, "兼容探针", today - Duration::days(2)).unwrap();
        assert!(mgr.add_likes(uid, 30, today).unwrap().weekly_reward, "旧库 monthly_first_claimed 列应承载周首破标记并正常发卡");
        assert_eq!(mgr.get_cards(uid).card_count, 1);
        assert_eq!(mgr.grant_card(uid, 2).unwrap(), 3);

        let missing = mgr.find_last_missing_checkin_date(uid, today);
        assert_eq!(missing, Some(today - Duration::days(1)));
        let remaining = mgr
            .execute_retroactive_checkin(uid, "兼容探针", today - Duration::days(1))
            .unwrap();
        assert_eq!(remaining, 2);

        // 关键断言：对真实旧库副本的全部读写均未改变表结构
        {
            let conn = Connection::open(&dst).unwrap();
            assert_eq!(before_schema, schema_snapshot(&conn), "V2 不得修改真实旧库的表结构");
        }
        drop(mgr);

        let _ = std::fs::remove_file(&dst);
        let _ = std::fs::remove_dir(&temp_dir);
        println!("[PASS] test_real_repo_legacy_db_copy_compatible passed (source: {:?})", src);
    }

    #[test]
    fn test_retroactive_checkin_atomic_flow_and_validation() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let today = Local::now().date_naive();
        let yesterday = today - Duration::days(1);
        let three_days_ago = today - Duration::days(3);

        // 注册并打卡 today 和 three_days_ago（产生断档）
        let _ = mgr.record_checkin("u_retro", "断签舰长", three_days_ago).unwrap();
        let _ = mgr.record_checkin("u_retro", "断签舰长", today).unwrap();

        let profile = mgr.get_profile("u_retro").unwrap();
        assert_eq!(profile.continuous_days, 1);
        assert_eq!(profile.cumulative_days, 2);

        // 赠予一张补签卡
        let _ = mgr.grant_card("u_retro", 1);
        assert_eq!(mgr.get_cards("u_retro").card_count, 1);

        // 查找缺失日期：应为 yesterday 或 two_days_ago
        let missing = mgr.find_last_missing_checkin_date("u_retro", today);
        assert!(missing.is_some());
        let target = missing.unwrap();
        assert_eq!(target, yesterday);

        // 执行原子补签
        let remaining_cards = mgr.execute_retroactive_checkin("u_retro", "断签舰长", target).unwrap();
        assert_eq!(remaining_cards, 0);

        // 检查补签后连续天数已重新计算提升
        let updated = mgr.get_profile("u_retro").unwrap();
        assert!(updated.continuous_days >= 2);
        assert_eq!(updated.cumulative_days, 3);

        // 连续天数达到累计天数后拦截补签
        // 人工将连续天数调平并测试拦截
        let _ = mgr.grant_card("u_retro", 1);
        // 执行批量补签使连续天数拉满
        let _ = mgr.batch_checkin().unwrap();
        let full = mgr.get_profile("u_retro").unwrap();
        assert_eq!(full.continuous_days, full.cumulative_days);

        // 再次尝试补签应当被拦截
        let val_res = mgr.check_retroactive_validity("u_retro");
        assert!(val_res.is_err());
        assert!(val_res.unwrap_err().contains("无需补签"));
        println!("[PASS] test_retroactive_checkin_atomic_flow_and_validation passed");
    }

    #[test]
    fn test_missing_current_date_detection() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let d1 = NaiveDate::from_ymd_opt(2026, 4, 20).unwrap();
        let d3 = NaiveDate::from_ymd_opt(2026, 4, 22).unwrap();
        let d5 = NaiveDate::from_ymd_opt(2026, 4, 24).unwrap(); // 今天，尚未打卡

        let _ = mgr.record_checkin("u_test_missing", "水友", d1).unwrap();
        let _ = mgr.record_checkin("u_test_missing", "水友", d3).unwrap();

        // 验证从今天 d5 倒推检查，应首先找到 d5 本身作为缺失日期
        let missing = mgr.find_last_missing_checkin_date("u_test_missing", d5);
        assert_eq!(missing, Some(d5));

        // 补签 d5 后，再检查应为 d4 (2026-04-23)
        let _ = mgr.grant_card("u_test_missing", 2);
        let _ = mgr.execute_retroactive_checkin("u_test_missing", "水友", d5).unwrap();
        let next_missing = mgr.find_last_missing_checkin_date("u_test_missing", d5);
        assert_eq!(next_missing, Some(NaiveDate::from_ymd_opt(2026, 4, 23).unwrap()));
        println!("[PASS] test_missing_current_date_detection passed");
    }

    #[test]
    fn test_streak_7_days_like_reward() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let start = NaiveDate::from_ymd_opt(2026, 5, 1).unwrap();

        // 连续 6 天点赞（每次 1 个赞，未满 30）
        for i in 0..6 {
            let d = start + Duration::days(i);
            let rewarded = mgr.add_likes("u_streak", 1, d).unwrap();
            assert!(!rewarded.streak_reward && !rewarded.weekly_reward);
            assert_eq!(mgr.get_cards("u_streak").card_count, 0);
        }

        // 第 7 天点赞触发连续 7 天奖卡
        let d7 = start + Duration::days(6);
        let rewarded = mgr.add_likes("u_streak", 1, d7).unwrap();
        assert!(rewarded.streak_reward);
        assert_eq!(mgr.get_cards("u_streak").card_count, 1);
        println!("[PASS] test_streak_7_days_like_reward passed");
    }

    #[test]
    fn test_record_checkin_preserves_learning_columns_and_created_at() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 9, 19).unwrap();
        let yesterday = today - Duration::days(1);

        // 学习链路先写入学习档案（模拟舰长发言学习）
        let learning = LearningProfile {
            keywords: vec![KeywordRecord {
                word: "太刀".into(),
                freq: 3,
                ts: 111,
            }],
            danmu_history: vec![(100, "第一条".into())],
            last_danmu_timestamp: 100,
        };
        mgr.save_learning("u_learn", "学习舰长", &learning).unwrap();
        let before = mgr.get_profile("u_learn").unwrap();

        // 打卡不应覆写 last_danmu_timestamp / keywords_json / danmu_history_json
        let p1 = mgr.record_checkin("u_learn", "学习舰长", yesterday).unwrap();
        assert_eq!(p1.last_danmu_timestamp, 100);
        let after = mgr.load_learning("u_learn");
        assert_eq!(after.keywords.len(), 1);
        assert_eq!(after.danmu_history.len(), 1);
        assert_eq!(after.last_danmu_timestamp, 100);

        // 多次打卡 created_at 保持首次落库值（回读真实值，而非当前时间）
        let p2 = mgr.record_checkin("u_learn", "学习舰长", today).unwrap();
        assert_eq!(p2.continuous_days, 2);
        assert_eq!(p2.created_at, before.created_at);
        println!("[PASS] test_record_checkin_preserves_learning_columns_and_created_at passed");
    }

    #[test]
    fn test_query_reply_texts() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 9, 19).unwrap();

        // 三份数据全无 → 系统错误提示（与原工程一致）
        assert_eq!(
            mgr.query_reply("nobody", "路人", today),
            "路人，系统错误，请稍后再试。"
        );

        // 有卡、有连赞进度、当日点赞 10/30
        mgr.grant_card("u_q", 2).unwrap();
        let _ = mgr.add_likes("u_q", 10, today).unwrap();
        let reply = mgr.query_reply("u_q", "查询水友", today);
        assert_eq!(
            reply,
            "查询水友，补签卡2张\n连续点赞7天：已1天，差6天\n每周点赞30：10/30，差20"
        );

        // 当日点赞突破 30 且本周已领取 → 已领取；连赞 7 天已满足
        let start = today - Duration::days(6);
        for i in 0..7 {
            let _ = mgr.add_likes("u_q2", 1, start + Duration::days(i)).unwrap();
        }
        let _ = mgr.add_likes("u_q2", 30, today).unwrap();
        let reply2 = mgr.query_reply("u_q2", "满勤水友", today);
        assert_eq!(
            reply2,
            "满勤水友，补签卡2张\n连续点赞7天：已满足，下次领取\n每周点赞30：已领取"
        );
        println!("[PASS] test_query_reply_texts passed");
    }

    #[test]
    fn test_retro_command_outcome_texts() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 9, 19).unwrap();
        let three_days_ago = today - Duration::days(3);

        // 无卡档案 → 系统错误
        let r = mgr.retro_command_outcome("u_none", "无档案", today);
        assert!(!r.success);
        assert_eq!(r.reply, "无档案，系统错误，请稍后再试。");

        // 有档案但卡数为 0（先消费唯一一张卡）
        let two_days_ago = today - Duration::days(2);
        assert_eq!(mgr.grant_card("u_zero", 1).unwrap(), 1);
        let _ = mgr.record_checkin("u_zero", "零卡", two_days_ago).unwrap();
        let _ = mgr.record_checkin("u_zero", "零卡", today).unwrap();
        let first = mgr.retro_command_outcome("u_zero", "零卡", today);
        assert!(first.success, "{}", first.reply);
        assert_eq!(mgr.get_cards("u_zero").card_count, 0);
        let r = mgr.retro_command_outcome("u_zero", "零卡", today);
        assert_eq!(r.reply, "零卡，你没有补签卡哦~");

        // 满勤（记录连续，连续 == 累计）→ 无需补签
        let yesterday = today - Duration::days(1);
        let _ = mgr.record_checkin("u_full", "满勤", yesterday).unwrap();
        let _ = mgr.record_checkin("u_full", "满勤", today).unwrap();
        assert_eq!(mgr.grant_card("u_full", 1).unwrap(), 1);
        let r = mgr.retro_command_outcome("u_full", "满勤", today);
        assert_eq!(r.reply, "满勤，当前连续打卡2天、累计2天，无需补签哦~");
        assert!(!r.success);
        assert_eq!(mgr.get_cards("u_full").card_count, 1, "无需补签不应扣卡");

        // 断签场景 → 成功补签（扣卡 + 恢复连续 + 原工程文案）
        let _ = mgr.record_checkin("u_gap", "断签", three_days_ago).unwrap();
        let _ = mgr.record_checkin("u_gap", "断签", today).unwrap();
        assert_eq!(mgr.grant_card("u_gap", 1).unwrap(), 1);
        let r = mgr.retro_command_outcome("u_gap", "断签", today);
        assert!(r.success, "{}", r.reply);
        assert_eq!(r.remaining_cards, 0);
        let target_int = CheckinManager::date_to_int(today - Duration::days(1));
        assert_eq!(r.checkin_date, target_int);
        // 记录集合 {今天-3, 今天-1(补签), 今天}：连续天数从今天往前只连到补签日，恢复为 2 天
        assert_eq!(r.new_continuous_days, 2);
        assert_eq!(
            r.reply,
            format!(
                "断签，已成功补签{}月{}日，剩余补签卡0张，连续打卡恢复为2天！",
                (target_int / 100) % 100,
                target_int % 100
            )
        );
        println!("[PASS] test_retro_command_outcome_texts passed");
    }

    #[test]
    fn test_grant_card_rejects_non_positive() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        assert!(mgr.grant_card("u_g", 0).is_err());
        assert!(mgr.grant_card("u_g", -3).is_err());
        assert_eq!(mgr.get_cards("u_g").card_count, 0);
        assert_eq!(mgr.grant_card("u_g", 2).unwrap(), 2);
        println!("[PASS] test_grant_card_rejects_non_positive passed");
    }

    #[test]
    fn test_search_users_card_count_and_like_escaping() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 9, 19).unwrap();

        // UID 不含下划线，避免干扰 LIKE 通配符转义断言
        let _ = mgr.record_checkin("userA", "太刀侠", today).unwrap();
        let _ = mgr.record_checkin("userB", "大_剑客", today).unwrap();
        let _ = mgr.record_checkin("userC", "百分%水友", today).unwrap();
        assert_eq!(mgr.grant_card("userA", 3).unwrap(), 3);

        // 昵称部分匹配 + 补签卡数
        let list = mgr.search_users("太刀").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].profile.uid, "userA");
        assert_eq!(list[0].card_count, 3);

        // LIKE 通配符转义：'%' 不再匹配任意串
        let list = mgr.search_users("%").unwrap();
        assert_eq!(list.len(), 1, "通配符应被转义: {:?}", list);
        assert_eq!(list[0].profile.uid, "userC");

        // LIKE 通配符转义：'_' 不再匹配单字符
        let list = mgr.search_users("_").unwrap();
        assert_eq!(list.len(), 1, "通配符应被转义: {:?}", list);
        assert_eq!(list[0].profile.uid, "userB");

        // 未配置卡档案的用户 card_count 为 0
        let list = mgr.search_users("userB").unwrap();
        assert_eq!(list[0].card_count, 0);
        println!("[PASS] test_search_users_card_count_and_like_escaping passed");
    }

    #[test]
    fn test_export_records_content_formats_and_filters() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let d1 = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2026, 9, 19).unwrap();

        let _ = mgr.record_checkin("u_e1", "导出甲", d1).unwrap();
        let _ = mgr.record_checkin("u_e1", "导出甲", d2).unwrap();
        let _ = mgr.record_checkin("u_e2", "导出乙", d2).unwrap();

        // CSV：UTF-8 BOM + 原工程表头 + 日期降序
        let csv = mgr.export_records_content("csv", None, None, None).unwrap();
        assert!(csv.starts_with('\u{feff}'), "应带 UTF-8 BOM");
        assert!(csv.contains("uid,username,checkin_date,created_at\n"));
        let body = csv.trim_start_matches('\u{feff}');
        let first_data_line = body.lines().nth(1).unwrap();
        assert!(first_data_line.contains("20260919"), "应按打卡日期降序: {}", first_data_line);

        // JSON：原工程字段名与缩进
        let json = mgr.export_records_content("json", None, None, None).unwrap();
        assert!(json.starts_with("\u{feff}[\n"));
        assert!(json.contains("\"uid\": \"u_e1\","));
        assert!(json.contains("\"checkinDate\": 20260910,"));

        // 昵称筛选（部分匹配合并多 UID）+ 日期范围
        let filtered = mgr
            .export_records_content("csv", Some("导出"), Some(d2), Some(d2))
            .unwrap();
        assert!(!filtered.contains("20260910"), "日期范围应生效");
        assert!(filtered.contains("u_e1") && filtered.contains("u_e2"));

        // 无命中用户 → User not found；非法格式 → Unsupported format
        assert_eq!(
            mgr.export_records_content("csv", Some("不存在的昵称"), None, None)
                .unwrap_err(),
            "User not found"
        );
        assert!(mgr
            .export_records_content("xml", None, None, None)
            .unwrap_err()
            .contains("Unsupported format"));
        println!("[PASS] test_export_records_content_formats_and_filters passed");
    }

    #[test]
    fn test_retro_trigger_words_parsing() {
        // 原工程 Init 原文：分号前操作词、分号后查询词
        let (retro, query) = parse_retro_trigger_words(RETRO_TRIGGER_WORDS);
        assert_eq!(retro, vec!["补签", "补签卡"]);
        assert_eq!(
            query,
            vec!["补签查询", "补签卡查询", "查询补签", "查询补签卡", "我的补签卡"]
        );
        assert!(is_retro_command("补签") && is_retro_command("补签卡"));
        assert!(!is_retro_command("补签查询"), "查询词不应被当作操作词");
        assert!(is_retro_query("我的补签卡") && is_retro_query("查询补签卡"));
        assert!(!is_retro_query("补签"));

        // 旧格式兼容：不含分号时，含「查询」的词归入查询词
        let (retro2, query2) = parse_retro_trigger_words("补签,补签查询");
        assert_eq!(retro2, vec!["补签"]);
        assert_eq!(query2, vec!["补签查询"]);
        println!("[PASS] test_retro_trigger_words_parsing passed");
    }
}
