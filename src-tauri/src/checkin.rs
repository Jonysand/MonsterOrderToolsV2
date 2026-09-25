use chrono::{Datelike, Duration, Local, NaiveDate, Utc};
use rusqlite::{params, Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 单行可选查询：**只有** `QueryReturnedNoRows` 返回 `None`（"无行"），
/// 其它 SQL 错误一律传播 —— 把 SQL 故障伪装成"无行"会让资产被当成 0 覆盖掉。
fn query_optional<T, P, F>(conn: &Connection, sql: &str, params: P, f: F) -> Result<Option<T>, String>
where
    P: rusqlite::Params,
    F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
{
    match conn.query_row(sql, params, f) {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

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
    /// 本周首破领取标记（周起始日 YYYYMMDD），持久化于 retroactive_cards.weekly_first_claimed
    pub weekly_first_claimed: i32,
    pub last_earned_date: i32,
}

/// 批量补签执行结果
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BatchCheckinResult {
    pub success: bool,
    pub total_users: i32,
    pub patched_users: i32,
    pub skipped_users: i32,
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

/// 常规打卡的完整结果：档案 + "本次是否属于重复打卡"的内部判定。
///
/// 判定与 upsert 在同一事务内的同一条连接上完成，调用方不再需要在事务外先猜重复再决定文案。
#[derive(Debug, Clone)]
pub struct CheckinOutcome {
    pub already_checked_in: bool,
    pub profile: UserProfile,
}

/// 补签执行结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetroExecResult {
    /// 成功扣卡补签，附带剩余卡数
    Done { remaining_cards: i32 },
    /// 同一 (uid, msg_id) 的重复投递：未扣卡、未写明细
    Duplicate,
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

/// CSV 字段转义：含逗号/双引号/换行时用双引号包裹并翻倍内部引号；
/// 对以 = + - @ 开头的字段前置单引号，避免 Excel 等表格软件公式注入
fn csv_escape(field: &str) -> String {
    let mut s = field.to_string();
    if s.chars()
        .next()
        .map_or(false, |c| matches!(c, '=' | '+' | '-' | '@'))
    {
        s.insert(0, '\'');
    }
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

/// JSON 字符串字面量（含外层引号），复用 serde_json 保证转义与序列化器一致
fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// 舰长周打卡与补签系统管理器
pub struct CheckinManager {
    conn: Mutex<Connection>,
}

/// 原工程历史打卡库文件名（恒定目标：新建库也用这个名字，避免继续裂库）
pub const LEGACY_DB_FILE_NAME: &str = "captain_profiles.db";
/// V2 早期版本使用过的打卡库文件名（仅作为历史来源存在，不再新建）
pub const V2_DB_FILE_NAME: &str = "checkin.db";
/// 活动库标记文件名：记录「上一次实际使用的库」，避免新增/移动文件后静默换库
pub const ACTIVE_DB_MARKER_FILE_NAME: &str = "checkin_active_db.json";

/// 活动库标记内容（只存文件名，不存绝对路径：数据目录可被整体搬迁）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActiveDbMarker {
    active: String,
}

/// 打卡库来源解析结果。
///
/// 解析只做「读目录 + 读标记」，不打开数据库、不写任何文件，因此可对四种文件组合直接单测。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckinDbResolution {
    /// 本次实际使用的库文件完整路径
    pub active_path: PathBuf,
    /// 同目录同时存在、但本次**未展示也未删除**的另一份库
    pub shadow_path: Option<PathBuf>,
    /// 两份文件都不存在 → 本次会新建
    pub created_new: bool,
    /// 需要向用户展示的高可见警告（None = 无冲突）
    pub warning: Option<String>,
}

impl CheckinDbResolution {
    /// 活动库文件名（用于状态展示，不暴露完整路径）
    pub fn active_file_name(&self) -> String {
        self.active_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    }
}

/// 打开默认库后返回的只读报告（供 `get_checkin_status` 展示，不含用户数据）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckinOpenReport {
    pub active_file_name: String,
    pub has_shadow_db: bool,
    pub warning: Option<String>,
}

/// 解析数据目录中的打卡库来源。
///
/// 规则（刻意保持"读取 + 决策"纯粹，便于对四种文件组合直接单测）：
/// 1. **活动库标记优先**：标记存在且指向的文件仍在 → 用它，即使另一份库后出现也不换。
///    标记指向的文件缺失 → 返回 `Err` 报错，**不悄悄改库**（否则用户会以为资产丢了）。
/// 2. 无标记时两份文件都在 → 沿用 `captain_profiles.db` 优先的兼容规则，但必须显著告警：
///    另一份 V2 数据"未展示、未删除"，并给出离线恢复步骤。
/// 3. 只有一份 → 用它。两份都没有 → 新建 `captain_profiles.db`（不再新建 `checkin.db`）。
///
/// 本函数**不合并、不删除、不复制**任何库文件。
pub fn resolve_db_path(dir: &Path) -> Result<CheckinDbResolution, String> {
    let legacy = dir.join(LEGACY_DB_FILE_NAME);
    let v2 = dir.join(V2_DB_FILE_NAME);
    let legacy_exists = legacy.is_file();
    let v2_exists = v2.is_file();

    let shadow_of = |active: &Path| -> Option<PathBuf> {
        if active == legacy && v2_exists {
            Some(v2.clone())
        } else if active == v2 && legacy_exists {
            Some(legacy.clone())
        } else {
            None
        }
    };

    let conflict_hint = |active: &Path, shadow: &Path| -> String {
        format!(
            "检测到两份打卡库：本次使用「{}」，另一份「{}」既未展示也未删除。\
             如需查看其中的数据，请先退出应用、备份整个数据目录，再离线比对合并后恢复。",
            active.file_name().unwrap_or_default().to_string_lossy(),
            shadow.file_name().unwrap_or_default().to_string_lossy()
        )
    };

    // 1. 活动库标记优先
    if let Some(marked) = read_active_db_marker(dir)? {
        if !marked.is_file() {
            return Err(format!(
                "活动打卡库标记指向的文件「{}」已不存在。\
                 为避免把资产静默切到另一份库，本次不启动打卡子系统；\
                 请把该文件放回数据目录，或删除 {} 后重启以重新选择。",
                marked.file_name().unwrap_or_default().to_string_lossy(),
                ACTIVE_DB_MARKER_FILE_NAME
            ));
        }
        let shadow = shadow_of(&marked);
        let warning = shadow.as_ref().map(|s| conflict_hint(&marked, s));
        return Ok(CheckinDbResolution {
            active_path: marked,
            shadow_path: shadow,
            created_new: false,
            warning,
        });
    }

    // 2. 无标记：两份都在 → 沿用旧库优先，但必须可见
    if legacy_exists && v2_exists {
        return Ok(CheckinDbResolution {
            active_path: legacy.clone(),
            shadow_path: Some(v2.clone()),
            created_new: false,
            warning: Some(conflict_hint(&legacy, &v2)),
        });
    }

    // 3. 只有一份
    if legacy_exists {
        return Ok(CheckinDbResolution {
            active_path: legacy,
            shadow_path: None,
            created_new: false,
            warning: None,
        });
    }
    if v2_exists {
        return Ok(CheckinDbResolution {
            active_path: v2,
            shadow_path: None,
            created_new: false,
            warning: None,
        });
    }

    // 4. 两份都没有 → 新建旧库名，避免未来继续裂库
    Ok(CheckinDbResolution {
        active_path: legacy,
        shadow_path: None,
        created_new: true,
        warning: None,
    })
}

/// 读取活动库标记。文件缺失（首次运行）返回 `Ok(None)`；损坏/非法返回 `Err`，
/// 因为无法判断"上次在用哪一份"时静默选择正是 D06 要修的静默性。
fn read_active_db_marker(dir: &Path) -> Result<Option<PathBuf>, String> {
    let path = dir.join(ACTIVE_DB_MARKER_FILE_NAME);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let marker: ActiveDbMarker = serde_json::from_str(raw.trim())
        .map_err(|e| format!("{} 内容损坏: {}", ACTIVE_DB_MARKER_FILE_NAME, e))?;
    // 只接受纯文件名：标记不得指到数据目录之外
    if marker.active.is_empty()
        || marker.active.contains('/')
        || marker.active.contains('\\')
        || marker.active.contains("..")
    {
        return Err(format!(
            "{} 中的活动库文件名非法，已拒绝使用",
            ACTIVE_DB_MARKER_FILE_NAME
        ));
    }
    Ok(Some(dir.join(marker.active)))
}

/// 原子写入活动库标记（临时文件 + rename，避免中途崩溃留下半截 JSON）
fn write_active_db_marker(dir: &Path, active: &Path) -> Result<(), String> {
    let Some(name) = active.file_name().map(|n| n.to_string_lossy().to_string()) else {
        return Err("活动库路径缺少文件名，无法记录标记".to_string());
    };
    let marker = ActiveDbMarker { active: name };
    let json = serde_json::to_string(&marker).map_err(|e| e.to_string())?;
    let target = dir.join(ACTIVE_DB_MARKER_FILE_NAME);
    let tmp = dir.join(format!(
        "{}.{}.tmp",
        ACTIVE_DB_MARKER_FILE_NAME,
        std::process::id()
    ));
    std::fs::write(&tmp, json).map_err(|e| format!("写入活动库标记失败: {}", e))?;
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("提交活动库标记失败: {}", e));
    }
    Ok(())
}

impl CheckinManager {
    /// 初始化 SQLite 数据库及数据表（显式路径；`None` 走 `resolve_db_path` 的默认解析）
    pub fn new(db_path: Option<&Path>) -> Result<Self, String> {
        let path = match db_path {
            Some(p) => p.to_path_buf(),
            None => Self::resolve_default_db_path()?.active_path,
        };
        let conn = Connection::open(&path).map_err(|e| e.to_string())?;
        let mgr = Self {
            conn: Mutex::new(conn),
        };
        mgr.init_schema()?;
        Ok(mgr)
    }

    /// 生产入口：解析默认库来源 → 打开 → 建表迁移 → 原子写活动库标记。
    ///
    /// 返回 `(manager, report)`；任何一步失败都返回 `Err`，**不退回内存库**：
    /// 静默的内存兜底会让打卡与卡片在界面上"成功"，重启后资产凭空消失。
    pub fn open_default() -> Result<(Self, CheckinOpenReport), String> {
        Self::open_at(&crate::paths::config_dir())
    }

    /// 在指定目录打开打卡库（生产走 `open_default`；测试用隔离临时目录）
    pub fn open_at(dir: &Path) -> Result<(Self, CheckinOpenReport), String> {
        let resolution = resolve_db_path(dir)?;
        if let Some(parent) = resolution.active_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建打卡数据目录失败: {}", e))?;
        }
        let conn = Connection::open(&resolution.active_path)
            .map_err(|e| format!("打开打卡数据库失败: {}", e))?;
        let mgr = Self {
            conn: Mutex::new(conn),
        };
        // 建表 + 缺列迁移在一次事务里完成，避免半迁移的中间状态
        mgr.init_schema()?;
        // 成功打开后才记录活动库：标记写着"上次真在用哪一份"
        write_active_db_marker(dir, &resolution.active_path)?;

        let report = CheckinOpenReport {
            active_file_name: resolution.active_file_name(),
            has_shadow_db: resolution.shadow_path.is_some(),
            warning: resolution.warning.clone(),
        };
        if let Some(w) = &report.warning {
            crate::log_warn!("[Checkin] {}", w);
        }
        Ok((mgr, report))
    }

    /// 解析默认数据目录中的打卡库来源
    pub fn resolve_default_db_path() -> Result<CheckinDbResolution, String> {
        resolve_db_path(&crate::paths::config_dir())
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

    /// 测试专用：在库上执行任意 SQL，用于注入 `RAISE(ABORT)` trigger 制造 SQL 故障。
    /// 生产构建不包含该方法。
    #[cfg(test)]
    pub fn inject_sql_for_test(&self, sql: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(sql).map_err(|e| e.to_string())
    }

    /// 建表语句与原工程 captain_profiles.db 完全一致（表名/列名/可空性/默认值逐一对齐），
    /// 老库直接可用；除 V2 专用的补签幂等表外不创建原工程没有的表
    fn init_schema(&self) -> Result<(), String> {
        let mut conn = self.conn.lock().unwrap();
        // 建表与缺列迁移放在同一事务：中途失败整体回滚，不留下"建了一半"的库
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        tx.execute_batch(
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

            -- 列名对齐原工程 v40+ 权威结构：weekly_first_claimed 承载“本周首破领取日”（周起始日）
            CREATE TABLE IF NOT EXISTS retroactive_cards (
                uid TEXT PRIMARY KEY,
                card_count INTEGER DEFAULT 0,
                total_earned INTEGER DEFAULT 0,
                weekly_first_claimed INTEGER DEFAULT 0,
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

            -- V2 专用补签指令幂等表（原工程五张业务表之外唯一新增的表）。
            -- 用途：同一条「补签」弹幕被服务端重投时，只允许扣一张卡、只写一条缺日明细；
            -- 只在**真正扣卡**的路径上写键，无卡/满勤/无需补签等只读反馈不写，
            -- 因此重投仍能按当前资产给出正常提示。
            -- 键为 (uid, msg_id)：msg_id 为主站消息 ID，此处按"同一用户不重复处理同一条消息"落地；
            -- 空 msg_id 不写键（不得用消息文字+时间伪造键，否则会挡掉合法的多次补签）。
            CREATE TABLE IF NOT EXISTS processed_retro_commands (
                uid TEXT NOT NULL,
                msg_id TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (uid, msg_id)
            );
            "#,
        )
        .map_err(|e| e.to_string())?;

        Self::migrate_schema(&tx)?;
        tx.commit().map_err(|e| e.to_string())?;

        Ok(())
    }

    /// 结构迁移（与原工程 ProfileManager.cpp 的迁移逻辑逐条对齐）：
    /// ① user_profiles 缺 cumulative_days → ALTER 追加；
    /// ② retroactive_cards 缺 weekly_first_claimed → ALTER 追加（老库由月度列升级为周维度列）。
    /// 迁移只在缺列时执行，已迁移/新库为空操作，不触碰任何既有数据。
    fn migrate_schema(conn: &Connection) -> Result<(), String> {
        if !Self::table_has_column(conn, "user_profiles", "cumulative_days")? {
            conn.execute(
                "ALTER TABLE user_profiles ADD COLUMN cumulative_days INTEGER DEFAULT 0",
                [],
            )
            .map_err(|e| e.to_string())?;
            crate::log_info!("[Checkin] user_profiles 追加 cumulative_days 列");
        }

        if !Self::table_has_column(conn, "retroactive_cards", "weekly_first_claimed")? {
            conn.execute(
                "ALTER TABLE retroactive_cards ADD COLUMN weekly_first_claimed INTEGER DEFAULT 0",
                [],
            )
            .map_err(|e| e.to_string())?;
            crate::log_info!("[Checkin] retroactive_cards 追加 weekly_first_claimed 列");
        }

        Ok(())
    }

    /// 判定表是否已含指定列（PRAGMA table_info 逐列匹配）
    fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, String> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({})", table))
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let name: String = row.get(1).map_err(|e| e.to_string())?;
            if name == column {
                return Ok(true);
            }
        }
        Ok(false)
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

    /// 用户常规打卡（返回档案）。
    ///
    /// 明细写入、连续/累计重算、档案更新在**同一事务**内完成，任一 SQL 失败整体回滚并返回 `Err`，
    /// 因此不会出现"明细没写进去、档案却显示连续 1 天"的假成功。
    /// 回读真实档案也在提交前于事务内完成：只有读值与提交都成功才把结果交回调用方。
    pub fn record_checkin(
        &self,
        uid: &str,
        username: &str,
        date: NaiveDate,
    ) -> Result<UserProfile, String> {
        self.record_checkin_with_flag(uid, username, date)
            .map(|o| o.profile)
    }

    /// 用户常规打卡的完整结果：档案 + **在同一事务内**判定的「今天是否已打卡」。
    /// 调用方据此选择"首次打卡"或"重复打卡"文案，不再在事务外先猜重复。
    pub fn record_checkin_with_flag(
        &self,
        uid: &str,
        username: &str,
        date: NaiveDate,
    ) -> Result<CheckinOutcome, String> {
        let mut conn = self.conn.lock().unwrap();
        let date_int = Self::date_to_int(date);
        // created_at/updated_at 统一用毫秒纪元，对齐原工程 GetCurrentTimestamp
        // （ProfileManager.cpp:21-24，旧库历史数据均为 13 位毫秒，秒级会使单位混用）
        let now = Utc::now().timestamp_millis();

        let tx = conn.transaction().map_err(|e| e.to_string())?;

        // 当天是否已打卡的判定与 upsert 在同一事务内进行，避免并发下先猜重复再决定文案
        let already: bool = tx
            .query_row(
                "SELECT COUNT(1) FROM checkin_records WHERE uid = ?1 AND checkin_date = ?2",
                params![uid, date_int],
                |row| row.get::<_, i64>(0),
            )
            .map(|c| c > 0)
            .map_err(|e| e.to_string())?;

        // 插入打卡明细：同日重复打卡时按时间戳更新（对齐原工程
        // ProfileManager.cpp:31 的 ON CONFLICT ... WHERE created_at != excluded.created_at）
        // 失败必须传播：早期实现吞掉该错误，导致明细缺失却被当作打卡成功
        tx.execute(
            r#"
            INSERT INTO checkin_records (uid, username, checkin_date, created_at) VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(uid, checkin_date) DO UPDATE SET
                created_at = excluded.created_at,
                username = excluded.username
            WHERE checkin_records.created_at != excluded.created_at
            "#,
            params![uid, username, date_int, now],
        )
        .map_err(|e| format!("写入打卡明细失败: {}", e))?;

        // 从明细表重新计算真实连续天数与累计天数
        let continuous = Self::internal_calc_continuous(&tx, uid)?;
        // 读失败不得写成 0：把 SQL 错误当作"累计 0"会把正确信息覆盖掉
        let cumulative: i32 = tx
            .query_row(
                "SELECT COUNT(DISTINCT checkin_date) FROM checkin_records WHERE uid = ?1",
                params![uid],
                |row| row.get(0),
            )
            .map_err(|e| format!("统计累计打卡天数失败: {}", e))?;

        // 更新或创建 Profile（不触碰学习字段 last_danmu_timestamp / keywords_json / danmu_history_json，
        // 该三列仅由弹幕学习链路写入 —— C5：原工程打卡路径同样不覆写 lastDanmuTimestamp）
        tx.execute(
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
        ).map_err(|e| format!("更新打卡档案失败: {}", e))?;

        // 回读真实行：created_at / last_danmu_timestamp 以库内既有值为准（C5）
        let profile = Self::get_profile_by_conn(&tx, uid)?;
        tx.commit().map_err(|e| format!("提交打卡事务失败: {}", e))?;

        Ok(CheckinOutcome {
            already_checked_in: already,
            profile,
        })
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
        // updated_at 毫秒纪元，对齐原工程 SaveProfileToDb 的 GetCurrentTimestamp
        let now = Utc::now().timestamp_millis();
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

        // 无打卡记录时返回 1（对齐原工程 ProfileManager::CalculateContinuousDaysFromRecords）
        if dates.is_empty() {
            return Ok(1);
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

    /// 从明细表一次性重算 (连续天数, 累计天数)。
    /// 补签有效性判定统一走此入口，避免多处口径漂移
    fn calc_streak_and_cumulative(conn: &Connection, uid: &str) -> Result<(i32, i32), String> {
        let continuous = Self::internal_calc_continuous(conn, uid)?;
        let cumulative: i32 = conn
            .query_row(
                "SELECT COUNT(DISTINCT checkin_date) FROM checkin_records WHERE uid = ?1",
                params![uid],
                |row| row.get(0),
            )
            .map_err(|e| format!("统计累计打卡天数失败: {}", e))?;
        Ok((continuous, cumulative))
    }

    /// 自然周点赞满 30 奖卡逻辑
    /// 点赞累加与奖卡逻辑：
    /// 规则 1：连续 7 天点赞，奖励 1 张补签卡
    /// 规则 2：自然周内首次「当日点赞累计」达 30 次，奖励 1 张补签卡（每周限领 1 次）
    /// 返回两条规则的触发情况（供 C3 分别播报）
    ///
    /// 整个事件在同一 `IMMEDIATE` 事务内完成：本日点赞累加 → 连续天数与第 7 天奖卡及标记 →
    /// 自然周 30 赞判定、周标记与卡数。任一 SQL 或提交失败则整体回滚，
    /// 因此不会出现"卡已发、领取标记没保存"从而重复发卡的情况。
    /// 卡数与其对应的领取标记**总是**共同成败。
    pub fn add_likes(
        &self,
        uid: &str,
        likes: i32,
        date: NaiveDate,
    ) -> Result<LikeRewards, String> {
        let mut conn = self.conn.lock().unwrap();
        // IMMEDIATE：进入即取写锁，避免并发同 ID 事件各自读到旧标记后各发一张卡
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| format!("开启点赞事务失败: {}", e))?;
        let date_int = Self::date_to_int(date);
        let mut rewards = LikeRewards::default();

        // 1. 规则 1：连续 7 天点赞奖励
        let streak_row: Option<(i32, i32, i32)> = query_optional(
            &tx,
            "SELECT current_streak, last_like_date, streak_reward_issued FROM user_like_streaks WHERE uid = ?1",
            params![uid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|e| format!("读取连续点赞记录失败: {}", e))?;

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
            tx.execute(
                r#"
                INSERT INTO retroactive_cards (uid, card_count, total_earned, weekly_first_claimed, last_earned_date)
                VALUES (?1, 1, 1, 0, ?2)
                ON CONFLICT(uid) DO UPDATE SET
                    card_count = card_count + 1,
                    total_earned = total_earned + 1,
                    last_earned_date = ?2
                "#,
                params![uid, date_int],
            ).map_err(|e| format!("发放连续点赞奖卡失败: {}", e))?;
        }

        // 连赞标记与卡数在同一事务提交：标记写失败时上面的加卡会一起回滚
        tx.execute(
            r#"
            INSERT INTO user_like_streaks (uid, current_streak, last_like_date, streak_reward_issued)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(uid) DO UPDATE SET
                current_streak = ?2,
                last_like_date = ?3,
                streak_reward_issued = ?4
            "#,
            params![uid, current_streak, date_int, streak_reward_issued],
        ).map_err(|e| format!("更新连续点赞标记失败: {}", e))?;

        // 2. 规则 2：自然周内首次「当日点赞累计」达 30 奖卡（每周限 1 张）
        //    与原子工程语义一致：阈值比较的是当日累计（user_daily_likes），而非周累计。
        tx.execute(
            r#"
            INSERT INTO user_daily_likes (uid, like_date, total_likes)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(uid, like_date) DO UPDATE SET
                total_likes = total_likes + ?3
            "#,
            params![uid, date_int, likes],
        ).map_err(|e| format!("累加当日点赞失败: {}", e))?;

        let total_likes: i32 = tx
            .query_row(
                "SELECT total_likes FROM user_daily_likes WHERE uid = ?1 AND like_date = ?2",
                params![uid, date_int],
                |row| row.get(0),
            )
            .map_err(|e| format!("读取当日点赞累计失败: {}", e))?;

        let week_start = Self::get_week_start_date(date);
        // 周首破标记存于 weekly_first_claimed（与原工程 RetroactiveCheckInModule::IssueWeeklyFirstReward 一致）
        let weekly_first_claimed: i32 = query_optional(
            &tx,
            "SELECT weekly_first_claimed FROM retroactive_cards WHERE uid = ?1",
            params![uid],
            |row| row.get(0),
        )
        .map_err(|e| format!("读取周奖卡领取标记失败: {}", e))?
        .unwrap_or(0);

        if total_likes >= 30 && weekly_first_claimed != week_start {
            tx.execute(
                r#"
                INSERT INTO retroactive_cards (uid, card_count, total_earned, weekly_first_claimed, last_earned_date)
                VALUES (?1, 1, 1, ?2, ?3)
                ON CONFLICT(uid) DO UPDATE SET
                    card_count = card_count + 1,
                    total_earned = total_earned + 1,
                    weekly_first_claimed = ?2,
                    last_earned_date = ?3
                "#,
                params![uid, week_start, date_int],
            ).map_err(|e| format!("发放周奖卡失败: {}", e))?;

            rewards.weekly_reward = true;
        }

        rewards.daily_total = total_likes;
        // 提交成功后才把奖励结果交回调用方：调用方据此 emit / 入 TTS 队列
        tx.commit().map_err(|e| format!("提交点赞事务失败: {}", e))?;
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

    /// 读取补签卡资产。**行不存在与 SQL 故障是两件事**：
    /// 无行 → `Ok(None)`（新用户还没卡，应显示 0 张而不是报系统错误）；
    /// SQL 故障 → `Err`（必须记录并提示，不得伪装成"无行"把资产显示成 0）。
    fn load_cards(conn: &Connection, uid: &str) -> Result<Option<RetroactiveCardData>, String> {
        query_optional(
            conn,
            "SELECT uid, card_count, total_earned, weekly_first_claimed, last_earned_date FROM retroactive_cards WHERE uid = ?1",
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
    }

    /// 补签指令完整流程与回复文案
    /// 对齐原工程 RetroactiveCheckInModule::HandleRetroactiveCommand：
    /// 无卡档案 → 按 0 张处理；卡数为 0 → 无补签卡提示；无需补签 → 提示；无缺失日期 → 提示；
    /// 执行成功 → “已成功补签 X月X日，剩余补签卡N张，连续打卡恢复为M天！”
    ///
    /// `msg_id`：非空时用于幂等去重（同一用户同一条补签弹幕只扣一张卡）。
    /// 整个过程持有**同一个** IMMEDIATE 事务：查缺日与扣卡不会被另一条命令插到中间选中同一天。
    pub fn retro_command_outcome(
        &self,
        uid: &str,
        username: &str,
        date: NaiveDate,
        msg_id: Option<&str>,
    ) -> RetroCommandOutcome {
        let mut conn = self.conn.lock().unwrap();
        let tx = match conn.transaction_with_behavior(TransactionBehavior::Immediate) {
            Ok(t) => t,
            Err(e) => {
                crate::log_error!("[Checkin] 补签事务开启失败: {}", e);
                return RetroCommandOutcome {
                    reply: format!("{}，系统错误，请稍后再试。", username),
                    ..Default::default()
                };
            }
        };

        // 补签卡资产：无行 = 新用户还没卡（按 0 张处理），SQL 故障才报系统错误
        let cards = match Self::load_cards(&tx, uid) {
            Ok(Some(c)) => c,
            Ok(None) => RetroactiveCardData {
                uid: uid.to_string(),
                ..Default::default()
            },
            Err(e) => {
                crate::log_error!("[Checkin] 读取补签卡失败: {}", e);
                return RetroCommandOutcome {
                    reply: format!("{}，系统错误，请稍后再试。", username),
                    ..Default::default()
                };
            }
        };

        if cards.card_count <= 0 {
            return RetroCommandOutcome {
                reply: format!("{}，你没有补签卡哦~", username),
                remaining_cards: cards.card_count,
                ..Default::default()
            };
        }

        // 有效性判定与缺日查找共用本事务，口径不会在两次加锁之间漂移
        let (continuous, cumulative) = match Self::calc_streak_and_cumulative(&tx, uid) {
            Ok(v) => v,
            Err(e) => {
                crate::log_error!("[Checkin] 补签有效性校验失败: {}", e);
                return RetroCommandOutcome {
                    reply: format!("{}，系统错误，请稍后再试。", username),
                    ..Default::default()
                };
            }
        };
        if continuous >= cumulative {
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

        let target = match Self::find_last_missing_checkin_date_on(&tx, uid, date) {
            Ok(Some(d)) => d,
            Ok(None) => {
                return RetroCommandOutcome {
                    reply: format!("{}，当前没有需要补签的日期。", username),
                    remaining_cards: cards.card_count,
                    ..Default::default()
                };
            }
            Err(e) => {
                crate::log_error!("[Checkin] 查找缺失打卡日期失败: {}", e);
                return RetroCommandOutcome {
                    reply: format!("{}，系统错误，请稍后再试。", username),
                    ..Default::default()
                };
            }
        };
        let target_int = Self::date_to_int(target);

        let executed = Self::retro_execute_on(&tx, uid, username, target, msg_id, &cards);
        let exec = match executed {
            Ok(v) => v,
            Err(e) => {
                crate::log_error!("[Checkin] 补签执行失败: {}", e);
                return RetroCommandOutcome {
                    reply: format!("{}，补签失败，请稍后再试。", username),
                    remaining_cards: cards.card_count,
                    ..Default::default()
                };
            }
        };

        let RetroExecResult::Done { remaining_cards } = exec else {
            // 同一条弹幕重投：不重复扣卡、不重复播报
            return RetroCommandOutcome {
                reply: format!("{}，这条补签已经处理过啦~", username),
                remaining_cards: cards.card_count,
                ..Default::default()
            };
        };

        // 提交成功后才组装成功文案；读值在事务内完成，不会出现"报未提交但实际已提交"
        let new_continuous = match Self::internal_calc_continuous(&tx, uid) {
            Ok(v) => v,
            Err(e) => {
                crate::log_error!("[Checkin] 补签后重算连续天数失败: {}", e);
                return RetroCommandOutcome {
                    reply: format!("{}，补签失败，请稍后再试。", username),
                    remaining_cards: cards.card_count,
                    ..Default::default()
                };
            }
        };
        let display_cards = match Self::load_cards(&tx, uid) {
            Ok(Some(c)) => c.card_count.max(remaining_cards),
            _ => remaining_cards,
        };
        if let Err(e) = tx.commit() {
            crate::log_error!("[Checkin] 补签事务提交失败: {}", e);
            return RetroCommandOutcome {
                reply: format!("{}，补签失败，请稍后再试。", username),
                remaining_cards: cards.card_count,
                ..Default::default()
            };
        }

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

    /// 补签查询回复文案（对齐原工程 HandleQueryCommand；仅气泡不朗读，v24 决策）。
    /// 三份数据（卡档案 / 连续点赞 / 当日点赞）全无时按 0 张 / 0 天 / 0 次回显，
    /// 只有真正的 SQL 故障才返回系统错误提示。
    pub fn query_reply(&self, uid: &str, username: &str, date: NaiveDate) -> String {
        let conn = self.conn.lock().unwrap();
        let cards = match Self::load_cards(&conn, uid) {
            Ok(v) => v,
            Err(e) => {
                crate::log_error!("[Checkin] 补签查询读取卡片失败: {}", e);
                return format!("{}，系统错误，请稍后再试。", username);
            }
        };
        let streak = match query_optional(
            &conn,
            "SELECT current_streak FROM user_like_streaks WHERE uid = ?1",
            params![uid],
            |row| row.get::<_, i32>(0),
        ) {
            Ok(v) => v,
            Err(e) => {
                crate::log_error!("[Checkin] 补签查询读取连续点赞失败: {}", e);
                return format!("{}，系统错误，请稍后再试。", username);
            }
        };
        let daily_total = match query_optional(
            &conn,
            "SELECT total_likes FROM user_daily_likes WHERE uid = ?1 AND like_date = ?2",
            params![uid, Self::date_to_int(date)],
            |row| row.get::<_, i32>(0),
        ) {
            Ok(v) => v,
            Err(e) => {
                crate::log_error!("[Checkin] 补签查询读取当日点赞失败: {}", e);
                return format!("{}，系统错误，请稍后再试。", username);
            }
        };

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
        let (continuous, cumulative) = Self::calc_streak_and_cumulative(&conn, uid)?;

        if continuous >= cumulative {
            return Err("当前没有断签断档，连续打卡天数已达到累计打卡天数，无需补签".into());
        }
        Ok(())
    }

    /// 查找最近一个缺失的打卡日期（用于补签）
    /// 对齐原工程 ProfileManager::FindLastMissingCheckinDate：从当前日期（含）向前逐日检查，
    /// 返回第一个无打卡记录的日期；无任何记录时返回当前日期。
    pub fn find_last_missing_checkin_date(
        &self,
        uid: &str,
        current_date: NaiveDate,
    ) -> Option<NaiveDate> {
        let conn = self.conn.lock().unwrap();
        Self::find_last_missing_checkin_date_on(&conn, uid, current_date).ok().flatten()
    }

    /// 缺日查找的连接级实现：补签流程在**同一个事务**内调用它，
    /// 与后续扣卡之间不会被另一条命令插进来选中同一天。
    fn find_last_missing_checkin_date_on(
        conn: &Connection,
        uid: &str,
        current_date: NaiveDate,
    ) -> Result<Option<NaiveDate>, String> {
        let mut stmt = conn
            .prepare("SELECT DISTINCT checkin_date FROM checkin_records WHERE uid = ?1 ORDER BY checkin_date ASC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![uid], |r| r.get::<_, i32>(0))
            .map_err(|e| e.to_string())?;

        let mut existing = HashSet::new();
        for r in rows {
            if let Some(d) = Self::int_to_date(r.map_err(|e| e.to_string())?) {
                existing.insert(d);
            }
        }

        let mut cursor = current_date;

        // 倒序寻找从当前日期往前的第一个缺漏日期（下界：合法日期范围起点）
        loop {
            if !existing.contains(&cursor) {
                return Ok(Some(cursor));
            }
            match cursor.pred_opt() {
                Some(prev) if prev.year() >= 1970 => cursor = prev,
                _ => return Ok(None),
            }
        }
    }

    /// 在**调用方提供的事务/连接**上执行一次补签：
    /// 校验 → 扣卡（含 `card_count > 0` 条件并核对受影响行数）→ 插入明细 → 重算 → 更新档案
    /// → 写幂等键。任一步失败返回 `Err`，由调用方回滚整个事务。
    ///
    /// `msg_id` 非空时先查幂等表：已处理过则直接返回 `Duplicate`（不扣卡、不写明细）。
    /// 幂等键只在真正扣卡的路径上写入，无卡/无需补签等只读反馈不写键。
    fn retro_execute_on(
        conn: &Connection,
        uid: &str,
        username: &str,
        target_date: NaiveDate,
        msg_id: Option<&str>,
        cards: &RetroactiveCardData,
    ) -> Result<RetroExecResult, String> {
        let target_date_int = Self::date_to_int(target_date);
        let idem_key = msg_id.map(str::trim).filter(|s| !s.is_empty());

        // 1. 幂等预检：同一条指令重投时不再扣卡
        if let Some(key) = idem_key {
            let seen = conn
                .query_row(
                    "SELECT COUNT(1) FROM processed_retro_commands WHERE uid = ?1 AND msg_id = ?2",
                    params![uid, key],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|e| format!("查询补签幂等表失败: {}", e))?;
            if seen > 0 {
                return Ok(RetroExecResult::Duplicate);
            }
        }

        // 2. 目标日期已存在打卡记录时直接拒绝：原 INSERT OR REPLACE 会重写该行 id/created_at，
        //    且在无效补签上白白扣卡（正常入口由缺日查找保证缺失，此处兜底直接调用本 API 的场景）
        let already_exists = conn
            .query_row(
                "SELECT COUNT(1) FROM checkin_records WHERE uid = ?1 AND checkin_date = ?2",
                params![uid, target_date_int],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|e| format!("查询目标日期打卡记录失败: {}", e))?;
        if already_exists > 0 {
            return Err(format!("目标日期 {} 已存在打卡记录，无需补签", target_date_int));
        }

        // 3. 补签有效性校验（实时从记录动态重算，防脏数据或并发误差）
        let (cur_continuous, cur_cumulative) = Self::calc_streak_and_cumulative(conn, uid)?;
        if cur_continuous >= cur_cumulative {
            return Err("连续打卡天数已等于累计打卡天数，无需补签".into());
        }

        if cards.card_count <= 0 {
            return Err("补签卡不足，无法补签".into());
        }

        // 4. 扣 1 张卡：条件里带 card_count > 0，并核对确实改到 1 行
        let affected = conn
            .execute(
                "UPDATE retroactive_cards SET card_count = card_count - 1 WHERE uid = ?1 AND card_count > 0",
                params![uid],
            )
            .map_err(|e| format!("扣减补签卡失败: {}", e))?;
        if affected != 1 {
            return Err(format!("扣减补签卡未命中唯一一行（受影响 {} 行）", affected));
        }
        let new_card_count = cards.card_count - 1;

        // 5. 插入目标打卡记录（前置校验已确认该日期缺失，故用普通 INSERT 而非 OR REPLACE）
        // created_at 毫秒纪元，对齐原工程 ExecuteRetroactiveCheckin 的 GetCurrentTimestamp
        let now = Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO checkin_records (uid, username, checkin_date, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![uid, username, target_date_int, now],
        ).map_err(|e| format!("插入补签记录失败: {}", e))?;

        // 6. 重算连续天数与累计天数
        let (new_continuous, new_cumulative) = Self::calc_streak_and_cumulative(conn, uid)?;

        // 7. 更新 UserProfile（补签日期若大于原打卡日则推进 last_checkin_date，对齐原工程 ProfileManager.cpp:1453）
        let profile_affected = conn
            .execute(
                r#"
                UPDATE user_profiles SET
                    last_checkin_date = MAX(last_checkin_date, ?1),
                    continuous_days = ?2,
                    cumulative_days = ?3,
                    updated_at = ?4
                WHERE uid = ?5
                "#,
                params![target_date_int, new_continuous, new_cumulative, now, uid],
            )
            .map_err(|e| format!("更新补签档案失败: {}", e))?;
        if profile_affected != 1 {
            return Err(format!(
                "更新补签档案未命中唯一一行（受影响 {} 行）",
                profile_affected
            ));
        }

        // 8. 幂等占位与扣卡、明细同一事务提交：写键失败则本次补签整体回滚，重投可重试
        if let Some(key) = idem_key {
            if let Err(e) = conn.execute(
                "INSERT INTO processed_retro_commands (uid, msg_id, created_at) VALUES (?1, ?2, ?3)",
                params![uid, key, now],
            ) {
                // 唯一键冲突 = 并发的同一条指令已经提交过：本次按重复处理
                if Self::is_unique_violation(&e) {
                    return Ok(RetroExecResult::Duplicate);
                }
                return Err(format!("记录补签幂等键失败: {}", e));
            }
        }

        Ok(RetroExecResult::Done {
            remaining_cards: new_card_count,
        })
    }

    /// 判定 SQLite 唯一约束冲突（用于把并发重复投递识别为 Duplicate 而非故障）
    fn is_unique_violation(e: &rusqlite::Error) -> bool {
        matches!(
            e,
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ErrorCode::ConstraintViolation,
                    ..
                },
                _
            )
        )
    }

    /// 原子事务执行补签 (ExecuteRetroactiveCheckin)
    /// 步骤：校验有效性 -> 扣减补签卡 -> 插入打卡明细 -> 重算连续/累计天数 -> 更新 Profile -> 写幂等键
    ///
    /// `msg_id`：非空时启用幂等去重（同一条补签弹幕重投不再扣卡）
    pub fn execute_retroactive_checkin(
        &self,
        uid: &str,
        username: &str,
        target_date: NaiveDate,
        msg_id: Option<&str>,
    ) -> Result<RetroExecResult, String> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| format!("开启补签事务失败: {}", e))?;

        let cards = Self::load_cards(&tx, uid)?.unwrap_or(RetroactiveCardData {
            uid: uid.to_string(),
            ..Default::default()
        });
        let outcome = Self::retro_execute_on(&tx, uid, username, target_date, msg_id, &cards)?;
        tx.commit().map_err(|e| format!("提交补签事务失败: {}", e))?;
        Ok(outcome)
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

    /// 查询补签卡资产。
    /// 无行（新用户还没卡）按 0 张返回；SQL 故障同样退化为 0 张但会记录 ERROR，
    /// 因此状态读取不会因单次查询异常而让界面崩掉。
    pub fn get_cards(&self, uid: &str) -> RetroactiveCardData {
        let conn = self.conn.lock().unwrap();
        match Self::load_cards(&conn, uid) {
            Ok(Some(c)) => c,
            Ok(None) => RetroactiveCardData {
                uid: uid.to_string(),
                ..Default::default()
            },
            Err(e) => {
                crate::log_error!("[Checkin] 读取补签卡失败（按 0 张展示）: {}", e);
                RetroactiveCardData {
                    uid: uid.to_string(),
                    ..Default::default()
                }
            }
        }
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
            INSERT INTO retroactive_cards (uid, card_count, total_earned, weekly_first_claimed, last_earned_date)
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
    /// 对齐原工程 ProfileManager::BatchCheckin：
    /// 候选集为全部累计打卡 > 0 的用户；补签区间 = [最早打卡日（无明细时按 今天-(累计-1) 反推）, max(last_checkin_date, 今天)]；
    /// 补齐后连续天数直接置为累计天数，并记录跳过（无缺失）的用户数
    ///
    /// 整批在**一个事务**内执行：任何一条明细插入或档案更新失败都整体回滚，
    /// 计数只在 `commit()` 成功后才组装返回，因此不会出现"报告 N 条、实际 0 条"。
    pub fn batch_checkin(&self) -> Result<BatchCheckinResult, String> {
        let mut conn = self.conn.lock().unwrap();
        let today = Local::now().date_naive();
        let today_int = Self::date_to_int(today);
        // created_at 毫秒纪元，对齐原工程 BatchCheckin 的 GetCurrentTimestamp
        let now = Utc::now().timestamp_millis();

        let users: Vec<(String, String, i32, i32)> = {
            let mut stmt = conn
                .prepare("SELECT uid, username, last_checkin_date, cumulative_days FROM user_profiles WHERE cumulative_days > 0")
                .map_err(|e| e.to_string())?;

            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
                .map_err(|e| e.to_string())?;

            // 逐行取数：解析错误必须传播，不得用 flatten() 静默丢掉用户
            let mut list = Vec::new();
            for r in rows {
                list.push(r.map_err(|e| format!("读取批量补签候选用户失败: {}", e))?);
            }
            list
        };

        let total_users = users.len() as i32;
        let mut patched_users = 0;
        let mut skipped_users = 0;
        let mut total_inserted = 0;

        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;

        for (uid, username, last_checkin_date, cumulative_days) in &users {
            let existing: HashSet<NaiveDate> = {
                let mut rec_stmt = tx
                    .prepare("SELECT DISTINCT checkin_date FROM checkin_records WHERE uid = ?1 ORDER BY checkin_date ASC")
                    .map_err(|e| e.to_string())?;

                let rows = rec_stmt
                    .query_map(params![uid], |r| r.get::<_, i32>(0))
                    .map_err(|e| e.to_string())?;

                let mut set = HashSet::new();
                for r in rows {
                    let d = r.map_err(|e| format!("读取用户打卡明细失败: {}", e))?;
                    if let Some(d) = Self::int_to_date(d) {
                        set.insert(d);
                    }
                }
                set
            };

            // 起始日期：无明细时按累计天数反推（今天往前推 cumulative-1 天）
            let start = match existing.iter().min() {
                Some(d) => *d,
                None => today - Duration::days((*cumulative_days as i64 - 1).max(0)),
            };
            // 结束日期：最后打卡日若在未来则补到该日，否则补到今天
            let end = if *last_checkin_date > 0 && *last_checkin_date > today_int {
                Self::int_to_date(*last_checkin_date).unwrap_or(today)
            } else {
                today
            };

            let mut missing: Vec<NaiveDate> = Vec::new();
            let mut cursor = start;
            while cursor <= end {
                if !existing.contains(&cursor) {
                    missing.push(cursor);
                }
                cursor = cursor + Duration::days(1);
            }

            for d in &missing {
                let d_int = Self::date_to_int(*d);
                // 只统计**真实插入**：唯一键已存在时 INSERT OR IGNORE 受影响行数为 0
                let inserted = tx
                    .execute(
                        "INSERT OR IGNORE INTO checkin_records (uid, username, checkin_date, created_at) VALUES (?1, ?2, ?3, ?4)",
                        params![uid, username, d_int, now],
                    )
                    .map_err(|e| format!("批量补签写入打卡明细失败: {}", e))?;
                total_inserted += inserted as i32;
            }

            // 更新 Profile：连续天数置为累计天数（对齐原工程 newContinuous = user.cumulativeDays）
            let new_last_checkin = Self::date_to_int(end);
            let new_continuous = *cumulative_days;
            let affected = tx
                .execute(
                    "UPDATE user_profiles SET last_checkin_date = ?1, continuous_days = ?2, updated_at = ?3 WHERE uid = ?4",
                    params![new_last_checkin, new_continuous, now, uid],
                )
                .map_err(|e| format!("批量补签更新档案失败: {}", e))?;
            if affected != 1 {
                return Err(format!(
                    "批量补签更新档案未命中唯一一行（受影响 {} 行），整批已回滚",
                    affected
                ));
            }

            // 统计口径：有缺口才计入「补签用户数」，无缺口计入「跳过用户数」（两者互斥，不重叠）
            if missing.is_empty() {
                skipped_users += 1;
            } else {
                patched_users += 1;
            }
        }

        tx.commit().map_err(|e| format!("提交批量补签事务失败: {}", e))?;

        Ok(BatchCheckinResult {
            success: true,
            total_users,
            patched_users,
            skipped_users,
            total_inserted,
            message: format!(
                "操作完成\n总用户数: {}\n补签用户数: {}\n跳过用户数: {} (已连续到今天)\n插入打卡记录数: {}",
                total_users, patched_users, skipped_users, total_inserted
            ),
        })
    }

    /// 导出所有用户打卡总览汇总数据（CSV / JSON，UTF-8 BOM 前置）
    /// 对齐原工程 DataBridgeExports::ProfileManager_ExportUsersSummary 与 ProfileManager::GetAllUsersSummary
    pub fn export_users_summary(&self, format: &str) -> Result<String, String> {
        if format != "csv" && format != "json" {
            return Err("不支持的导出格式（请选择 CSV 或 JSON）".into());
        }

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT uid, username, continuous_days, cumulative_days FROM user_profiles WHERE cumulative_days > 0 ORDER BY cumulative_days DESC")
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i32>(2)?,
                    row.get::<_, i32>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let users: Vec<(String, String, i32, i32)> = rows.flatten().collect();

        let body = match format {
            "csv" => {
                let mut s = String::from("uid,username,continuous_days,cumulative_days\n");
                for (uid, name, continuous, cumulative) in &users {
                    s.push_str(&format!(
                        "{},{},{},{}\n",
                        csv_escape(uid),
                        csv_escape(name),
                        continuous,
                        cumulative
                    ));
                }
                s
            }
            _ => {
                let mut s = String::from("[\n");
                for (i, (uid, name, continuous, cumulative)) in users.iter().enumerate() {
                    s.push_str("  {\n");
                    s.push_str(&format!("    \"uid\": {},\n", json_string(uid)));
                    s.push_str(&format!("    \"username\": {},\n", json_string(name)));
                    s.push_str(&format!("    \"continuousDays\": {},\n", continuous));
                    s.push_str(&format!("    \"cumulativeDays\": {}\n", cumulative));
                    s.push_str("  }");
                    if i < users.len() - 1 {
                        s.push(',');
                    }
                    s.push('\n');
                }
                s.push_str("]\n");
                s
            }
        };

        Ok(format!("\u{FEFF}{}", body))
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
            return Err("不支持的导出格式（请选择 CSV 或 JSON）".into());
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
                return Err("未找到该用户".into());
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
                    s.push_str(&format!(
                        "{},{},{},{}\n",
                        csv_escape(uid),
                        csv_escape(name),
                        date,
                        created
                    ));
                }
                s
            }
            _ => {
                let mut s = String::from("[\n");
                for (i, (uid, name, date, created)) in records.iter().enumerate() {
                    s.push_str("  {\n");
                    s.push_str(&format!("    \"uid\": {},\n", json_string(uid)));
                    s.push_str(&format!("    \"username\": {},\n", json_string(name)));
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

    /// 生成与真实旧库 captain_profiles.db 完全一致的表结构（pre-v40：retroactive_cards 只有月度列）
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

        // 用 V2 的 CheckinManager 直接打开旧库（与原工程一致：缺列时 ALTER 追加 weekly_first_claimed）
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

        // 旧库无 weekly_first_claimed（迁移后默认 0）视为本周未领取：同日达 30 正常发卡
        assert!(mgr.add_likes("old_u", 30, today).unwrap().weekly_reward);
        assert_eq!(mgr.get_cards("old_u").card_count, 3);

        // 手动发卡正常
        assert_eq!(mgr.grant_card("old_u", 1).unwrap(), 4);

        // 关键断言：迁移后 retroactive_cards 拥有 weekly_first_claimed；
        // 原五张业务表结构逐字不变，只允许新增一张声明的 V2 补签幂等表
        {
            let conn = Connection::open(&path).unwrap();
            assert!(
                CheckinManager::table_has_column(&conn, "retroactive_cards", "weekly_first_claimed").unwrap(),
                "老库应被追加 weekly_first_claimed 列"
            );
            let after = schema_snapshot(&conn);
            assert_eq!(
                after.len(),
                before_schema.len() + 1,
                "只允许新增一张 V2 专用表，原有表不得增删"
            );
            // 按表名比对原五张表（新表插入排序后下标会错位，不能用 zip）
            let after_map: std::collections::HashMap<String, String> = after.iter().cloned().collect();
            for (name, sql_before) in before_schema.iter() {
                let Some(sql_after) = after_map.get(name) else {
                    panic!("原表 {} 不得被删除", name);
                };
                if name == "retroactive_cards" {
                    continue; // 该表仅追加列，CREATE 语句必然变化
                }
                assert_eq!(sql_before, sql_after, "表 {} 结构不得变更", name);
            }
            let names: Vec<String> = after.iter().map(|(n, _)| n.clone()).collect();
            assert!(
                names.iter().any(|n| n == "processed_retro_commands"),
                "新增的表必须是补签幂等表，实际: {:?}",
                names
            );
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

        // retroactive_cards 必须使用原工程 v40+ 权威列名 weekly_first_claimed
        let mut stmt = conn.prepare("PRAGMA table_info(retroactive_cards)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .flatten()
            .collect();
        assert!(cols.contains(&"weekly_first_claimed".to_string()), "cols: {:?}", cols);

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

    /// P0 回归：模拟「由原工程 v40+ 全新建库」——retroactive_cards 只有 weekly_first_claimed、无月度列。
    /// 修复前该场景下 add_likes / get_cards / grant_card 全部报 "no such column" 并被静默吞掉。
    #[test]
    fn test_v40_plus_schema_weekly_card_chain() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        {
            let conn = mgr.conn.lock().unwrap();
            // 还原为原工程 v40+ 的建表结果（无 monthly_first_claimed 列）
            conn.execute_batch(
                r#"
                DROP TABLE retroactive_cards;
                CREATE TABLE retroactive_cards (
                    uid TEXT PRIMARY KEY,
                    card_count INTEGER DEFAULT 0,
                    total_earned INTEGER DEFAULT 0,
                    weekly_first_claimed INTEGER DEFAULT 0,
                    last_earned_date INTEGER DEFAULT 0
                );
                "#,
            )
            .unwrap();
        }

        let uid = "v40_user";
        let today = NaiveDate::from_ymd_opt(2026, 3, 18).unwrap(); // 周三
        let week_start = CheckinManager::get_week_start_date(today);

        // 点赞奖卡链路必须畅通（修复前此处为 no such column 被吞）
        let rewards = mgr.add_likes(uid, 30, today).expect("add_likes 不得报错");
        assert!(rewards.weekly_reward, "当日达 30 应发周首破奖卡");

        let cards = mgr.get_cards(uid);
        assert_eq!(cards.card_count, 1);
        assert_eq!(cards.weekly_first_claimed, week_start, "周首破标记应写入周起始日");

        // 同周再次达 30 不得重复发卡
        assert!(!mgr.add_likes(uid, 30, today).unwrap().weekly_reward);
        assert_eq!(mgr.get_cards(uid).card_count, 1);

        // 次周可再次领取
        let next_week = today + Duration::days(7);
        assert!(mgr.add_likes(uid, 30, next_week).unwrap().weekly_reward);
        assert_eq!(mgr.get_cards(uid).card_count, 2);

        // GM 发卡与查询链路同样畅通（按最近领取的那一周查询 → 已领取）
        assert_eq!(mgr.grant_card(uid, 1).unwrap(), 3);
        let reply = mgr.query_reply(uid, "周卡水友", next_week);
        assert!(reply.contains("补签卡3张"), "reply: {}", reply);
        assert!(reply.contains("每周点赞30：已领取"), "reply: {}", reply);

        drop(mgr);
        println!("[PASS] test_v40_plus_schema_weekly_card_chain passed");
    }

    /// 结构迁移：pre-v40 老库（只有 monthly_first_claimed）打开后被追加 weekly_first_claimed，
    /// 且迁移幂等、不触碰其它表
    #[test]
    fn test_schema_migration_adds_weekly_column_idempotently() {
        let temp_dir = std::env::temp_dir().join("mh_test_schema_migration");
        let _ = std::fs::create_dir_all(&temp_dir);
        let path = temp_dir.join("legacy.db");
        let _ = std::fs::remove_file(&path);

        {
            let conn = Connection::open(&path).unwrap();
            create_legacy_schema(&conn);
        }

        let mgr = CheckinManager::new(Some(&path)).unwrap();
        {
            let conn = mgr.conn.lock().unwrap();
            assert!(CheckinManager::table_has_column(&conn, "retroactive_cards", "weekly_first_claimed").unwrap());
            // 老列保留（不破坏旧数据）
            assert!(CheckinManager::table_has_column(&conn, "retroactive_cards", "monthly_first_claimed").unwrap());
        }
        let first = {
            let conn = Connection::open(&path).unwrap();
            schema_snapshot(&conn)
        };
        drop(mgr);

        let mgr = CheckinManager::new(Some(&path)).unwrap();
        let second = {
            let conn = Connection::open(&path).unwrap();
            schema_snapshot(&conn)
        };
        assert_eq!(first, second, "重复打开不得再次改动结构");
        drop(mgr);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&temp_dir);
        println!("[PASS] test_schema_migration_adds_weekly_column_idempotently passed");
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
        assert!(mgr.add_likes(uid, 30, today).unwrap().weekly_reward, "迁移后的 weekly_first_claimed 列应承载周首破标记并正常发卡");
        assert_eq!(mgr.get_cards(uid).card_count, 1);
        assert_eq!(mgr.grant_card(uid, 2).unwrap(), 3);

        let missing = mgr.find_last_missing_checkin_date(uid, today);
        assert_eq!(missing, Some(today - Duration::days(1)));
        let remaining = mgr
            .execute_retroactive_checkin(uid, "兼容探针", today - Duration::days(1), None)
            .unwrap();
        assert_eq!(remaining, RetroExecResult::Done { remaining_cards: 2 });

        // 关键断言：迁移仅向 retroactive_cards 追加 weekly_first_claimed，其余表结构逐字不变
        let after_schema = {
            let conn = Connection::open(&dst).unwrap();
            assert!(
                CheckinManager::table_has_column(&conn, "retroactive_cards", "weekly_first_claimed").unwrap(),
                "真实旧库应被追加 weekly_first_claimed 列"
            );
            schema_snapshot(&conn)
        };
        assert_eq!(before_schema.len(), after_schema.len(), "迁移不得增删表");
        for (before, after_one) in before_schema.iter().zip(after_schema.iter()) {
            if before.0 == "retroactive_cards" {
                continue; // 该表仅追加列，CREATE 语句必然变化
            }
            assert_eq!(before, after_one, "表 {} 结构不得变更", before.0);
        }
        drop(mgr);

        // 迁移幂等：再次打开同一库不产生任何结构变化
        let mgr = CheckinManager::new(Some(&dst)).unwrap();
        drop(mgr);
        {
            let conn = Connection::open(&dst).unwrap();
            assert_eq!(after_schema, schema_snapshot(&conn), "重复打开不得再次改动结构");
        }

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
        let remaining_cards = mgr.execute_retroactive_checkin("u_retro", "断签舰长", target, None).unwrap();
        assert_eq!(remaining_cards, RetroExecResult::Done { remaining_cards: 0 });

        // 检查补签后连续天数已重新计算提升
        let updated = mgr.get_profile("u_retro").unwrap();
        assert!(updated.continuous_days >= 2);
        assert_eq!(updated.cumulative_days, 3);

        // 连续天数达到累计天数后拦截补签
        // 人工将连续天数调平并测试拦截
        let _ = mgr.grant_card("u_retro", 1);
        // 执行批量补签使连续天数拉满，并断言最近打卡日期更新为今日（v29 语义）
        let _ = mgr.batch_checkin().unwrap();
        let full = mgr.get_profile("u_retro").unwrap();
        assert_eq!(full.continuous_days, full.cumulative_days);
        assert_eq!(full.last_checkin_date, CheckinManager::date_to_int(today));

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

        // 补签 d5 后，再检查应为 d4 (2026-04-23)，并断言 last_checkin_date 推进至 d5 (20260424)
        let _ = mgr.grant_card("u_test_missing", 2);
        let _ = mgr.execute_retroactive_checkin("u_test_missing", "水友", d5, None).unwrap();
        let prof_d5 = mgr.get_profile("u_test_missing").unwrap();
        assert_eq!(prof_d5.last_checkin_date, 20260424);
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

        // 三份数据全无 → 按 0 张 / 0 天 / 0 次如实回显。
        // 早期实现把"无行"当成 SQL 故障回"系统错误"，新用户第一次查询就会看到报错
        assert_eq!(
            mgr.query_reply("nobody", "路人", today),
            "路人，补签卡0张\n连续点赞7天：已0天，差7天\n每周点赞30：0/30，差30"
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

        // 无卡档案（新用户还没有卡行）→ 按 0 张处理，提示"没有补签卡"而不是系统错误。
        // 早期实现把"无行"当成 SQL 故障，普通新用户第一条补签就被回"系统错误"
        let r = mgr.retro_command_outcome("u_none", "无档案", today, None);
        assert!(!r.success);
        assert_eq!(r.reply, "无档案，你没有补签卡哦~");

        // 有档案但卡数为 0（先消费唯一一张卡）
        let two_days_ago = today - Duration::days(2);
        assert_eq!(mgr.grant_card("u_zero", 1).unwrap(), 1);
        let _ = mgr.record_checkin("u_zero", "零卡", two_days_ago).unwrap();
        let _ = mgr.record_checkin("u_zero", "零卡", today).unwrap();
        let first = mgr.retro_command_outcome("u_zero", "零卡", today, None);
        assert!(first.success, "{}", first.reply);
        assert_eq!(mgr.get_cards("u_zero").card_count, 0);
        let r = mgr.retro_command_outcome("u_zero", "零卡", today, None);
        assert_eq!(r.reply, "零卡，你没有补签卡哦~");

        // 满勤（记录连续，连续 == 累计）→ 无需补签
        let yesterday = today - Duration::days(1);
        let _ = mgr.record_checkin("u_full", "满勤", yesterday).unwrap();
        let _ = mgr.record_checkin("u_full", "满勤", today).unwrap();
        assert_eq!(mgr.grant_card("u_full", 1).unwrap(), 1);
        let r = mgr.retro_command_outcome("u_full", "满勤", today, None);
        assert_eq!(r.reply, "满勤，当前连续打卡2天、累计2天，无需补签哦~");
        assert!(!r.success);
        assert_eq!(mgr.get_cards("u_full").card_count, 1, "无需补签不应扣卡");

        // 断签场景 → 成功补签（扣卡 + 恢复连续 + 原工程文案）
        let _ = mgr.record_checkin("u_gap", "断签", three_days_ago).unwrap();
        let _ = mgr.record_checkin("u_gap", "断签", today).unwrap();
        assert_eq!(mgr.grant_card("u_gap", 1).unwrap(), 1);
        let r = mgr.retro_command_outcome("u_gap", "断签", today, None);
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

        // 无命中用户 → 未找到该用户；非法格式 → 不支持的导出格式
        assert_eq!(
            mgr.export_records_content("csv", Some("不存在的昵称"), None, None)
                .unwrap_err(),
            "未找到该用户"
        );
        assert!(mgr
            .export_records_content("xml", None, None, None)
            .unwrap_err()
            .contains("不支持的导出格式"));
        println!("[PASS] test_export_records_content_formats_and_filters passed");
    }

    #[test]
    fn test_export_users_summary_formats() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let d1 = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2026, 9, 11).unwrap();

        let _ = mgr.record_checkin("u_s1", "总览甲", d1).unwrap();
        let _ = mgr.record_checkin("u_s1", "总览甲", d2).unwrap();
        let _ = mgr.record_checkin("u_s2", "总览乙", d2).unwrap();

        // 验证 CSV 格式：表头为 uid,username,continuous_days,cumulative_days
        let csv = mgr.export_users_summary("csv").unwrap();
        assert!(csv.starts_with('\u{feff}'), "应带 UTF-8 BOM");
        assert!(csv.contains("uid,username,continuous_days,cumulative_days\n"));
        assert!(csv.contains("u_s1,总览甲,2,2"));
        assert!(csv.contains("u_s2,总览乙,1,1"));

        // 验证 JSON 格式：键名为 uid, username, continuousDays, cumulativeDays
        let json = mgr.export_users_summary("json").unwrap();
        assert!(json.starts_with("\u{feff}[\n"));
        assert!(json.contains("\"uid\": \"u_s1\","));
        assert!(json.contains("\"continuousDays\": 2,"));
        assert!(json.contains("\"cumulativeDays\": 2"));

        // 非法格式报错
        assert!(mgr.export_users_summary("yaml").is_err());
        println!("[PASS] test_export_users_summary_formats passed");
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

    /// 批量补签统计口径回归：patched 与 skipped 互斥、不重叠，且合计等于总用户数
    #[test]
    fn test_batch_checkin_reports_patched_and_skipped_counts() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let today = Local::now().date_naive();
        let d3 = today - Duration::days(3);

        // 用户 A：3 天前与今天打卡 → 中间缺 2 天，应计入 patched
        let _ = mgr.record_checkin("u_batch_gap", "断签水友", d3).unwrap();
        let _ = mgr.record_checkin("u_batch_gap", "断签水友", today).unwrap();
        // 用户 B：昨天与今天打卡 → 已连续，应计入 skipped
        let _ = mgr.record_checkin("u_batch_full", "满勤水友", today - Duration::days(1)).unwrap();
        let _ = mgr.record_checkin("u_batch_full", "满勤水友", today).unwrap();

        let res = mgr.batch_checkin().unwrap();
        assert_eq!(res.total_users, 2);
        assert_eq!(res.patched_users, 1, "仅存在断签的用户应计入补签用户数");
        assert_eq!(res.skipped_users, 1, "已连续到今天的用户应计入跳过数");
        assert_eq!(
            res.patched_users + res.skipped_users,
            res.total_users,
            "两个口径必须互斥且覆盖全部用户"
        );
        assert_eq!(res.total_inserted, 2, "A 需补前天与昨天两条");

        let a = mgr.get_profile("u_batch_gap").unwrap();
        assert_eq!(a.continuous_days, a.cumulative_days, "补签后连续天数应拉平到累计天数");
        println!("[PASS] test_batch_checkin_reports_patched_and_skipped_counts passed");
    }

    /// 导出字段转义回归：昵称含逗号/双引号/换行时 CSV 不串列、JSON 仍可被标准解析
    #[test]
    fn test_export_escapes_special_usernames() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 9, 19).unwrap();
        let tricky = "逗号,引号\"换行\n水友";
        let _ = mgr.record_checkin("u_esc", tricky, date).unwrap();

        // CSV：逗号/引号/换行字段必须被双引号包裹且内部引号翻倍
        let csv = mgr.export_records_content("csv", None, None, None).unwrap();
        let csv_body = csv.trim_start_matches('\u{feff}');
        assert!(
            csv_body.contains("\"逗号,引号\"\"换行\n水友\""),
            "CSV 未正确转义: {}",
            csv_body
        );

        // JSON：明细与汇总都必须能被标准解析器解析，且往返后昵称不变
        let json = mgr.export_records_content("json", None, None, None).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(json.trim_start_matches('\u{feff}')).expect("明细 JSON 必须合法");
        assert_eq!(parsed[0]["username"], serde_json::json!(tricky));

        let summary = mgr.export_users_summary("json").unwrap();
        let parsed_summary: serde_json::Value = serde_json::from_str(
            summary.trim_start_matches('\u{feff}'),
        )
        .expect("汇总 JSON 必须合法");
        assert_eq!(parsed_summary[0]["username"], serde_json::json!(tricky));
        println!("[PASS] test_export_escapes_special_usernames passed");
    }

    /// 补签兜底校验：目标日期已存在记录时拒绝，且不得扣卡、不得覆盖原记录
    #[test]
    fn test_execute_retroactive_rejects_existing_date_without_charging_card() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 9, 19).unwrap();
        let d3 = today - Duration::days(3);

        let _ = mgr.record_checkin("u_exist", "水友", d3).unwrap();
        let _ = mgr.record_checkin("u_exist", "水友", today - Duration::days(1)).unwrap();
        let _ = mgr.record_checkin("u_exist", "水友", today).unwrap();
        assert_eq!(mgr.grant_card("u_exist", 1).unwrap(), 1);

        // 目标日期（昨天）已存在：拒绝且卡数不变
        let err = mgr
            .execute_retroactive_checkin("u_exist", "水友", today - Duration::days(1), None)
            .unwrap_err();
        assert!(err.contains("已存在打卡记录"), "{}", err);
        assert_eq!(mgr.get_cards("u_exist").card_count, 1, "拒绝补签不得扣卡");

        // 真正缺失的前天仍可正常补签并扣 1 张卡
        let remaining = mgr
            .execute_retroactive_checkin("u_exist", "水友", today - Duration::days(2), None)
            .unwrap();
        assert_eq!(remaining, RetroExecResult::Done { remaining_cards: 0 });
        println!("[PASS] test_execute_retroactive_rejects_existing_date_without_charging_card passed");
    }

    /// created_at/updated_at 单位回归：4 条写入链路（常规打卡 / 学习档案 / 补签 / 批量补签）
    /// 全部必须为毫秒纪元，对齐原工程 GetCurrentTimestamp 与旧库 13 位历史数据
    #[test]
    fn test_created_at_uses_millisecond_epoch() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        // 毫秒纪元下界（2001-09）：秒级 now（~1.7e9）远小于此，可稳定区分两种单位
        let ms_floor: i64 = 1_000_000_000_000;
        let today = NaiveDate::from_ymd_opt(2026, 9, 19).unwrap();

        // 1. 常规打卡：user_profiles 的 created_at/updated_at 与 checkin_records.created_at 均为毫秒
        let p = mgr.record_checkin("u_ms", "水友", today).unwrap();
        assert!(p.created_at >= ms_floor, "profile.created_at 应为毫秒: {}", p.created_at);
        assert!(p.updated_at >= ms_floor, "profile.updated_at 应为毫秒: {}", p.updated_at);
        let rec_created: i64 = {
            let conn = mgr.conn.lock().unwrap();
            conn.query_row(
                "SELECT created_at FROM checkin_records WHERE uid = 'u_ms'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(rec_created >= ms_floor, "明细 created_at 应为毫秒: {}", rec_created);

        // 2. 学习档案写入：user_profiles.created_at/updated_at 为毫秒
        mgr.save_learning("u_ms2", "水友2", &LearningProfile::default()).unwrap();
        let p2 = mgr.get_profile("u_ms2").unwrap();
        assert!(
            p2.created_at >= ms_floor && p2.updated_at >= ms_floor,
            "学习链路 created_at/updated_at 应为毫秒: {}/{}",
            p2.created_at,
            p2.updated_at
        );

        // 3. 补签插入：checkin_records.created_at 为毫秒
        //    先制造断档（打 today-3 与 today，缺 today-1/today-2），否则连续=累计会被拦截
        let _ = mgr.record_checkin("u_ms", "水友", today - Duration::days(3)).unwrap();
        let _ = mgr.grant_card("u_ms", 1).unwrap();
        let _ = mgr
            .execute_retroactive_checkin("u_ms", "水友", today - Duration::days(1), None)
            .unwrap();
        let retro_created: i64 = {
            let conn = mgr.conn.lock().unwrap();
            conn.query_row(
                "SELECT created_at FROM checkin_records WHERE uid = 'u_ms' AND checkin_date = ?1",
                params![CheckinManager::date_to_int(today - Duration::days(1))],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(retro_created >= ms_floor, "补签 created_at 应为毫秒: {}", retro_created);

        // 4. 批量补签插入：checkin_records.created_at 为毫秒
        let _ = mgr.record_checkin("u_ms3", "水友3", today - Duration::days(3)).unwrap();
        mgr.batch_checkin().unwrap();
        let batch_created: i64 = {
            let conn = mgr.conn.lock().unwrap();
            conn.query_row(
                "SELECT MIN(created_at) FROM checkin_records WHERE uid = 'u_ms3'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(batch_created >= ms_floor, "批量补签 created_at 应为毫秒: {}", batch_created);

        println!("[PASS] test_created_at_uses_millisecond_epoch passed");
    }

    // ===== publish_v2 原包体真实库兼容测试 =====
    // 环境变量 MH_PUBLISH_DB 指向原工程发布环境（如 D:\VisualStudioProjects\publish_v2）
    // captain_profiles.db 的「副本」；未设置时跳过，全程不触碰原件。

    /// 将真实库复制为独立临时副本（各测试互不干扰）
    fn publish_db_copy(tag: &str) -> Option<PathBuf> {
        let src = std::env::var_os("MH_PUBLISH_DB").map(PathBuf::from)?;
        let dst = std::env::temp_dir().join(format!("mh_publish_{}_{}.db", tag, std::process::id()));
        let _ = std::fs::remove_file(&dst);
        std::fs::copy(&src, &dst).expect("复制真实库副本失败");
        Some(dst)
    }

    /// 统计五张表的行数（打开前 / 打开后必须一致，证明迁移与读取不增删数据行）
    fn count_all_rows(conn: &Connection) -> [i64; 5] {
        let q = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        [
            q("SELECT COUNT(*) FROM user_profiles"),
            q("SELECT COUNT(*) FROM checkin_records"),
            q("SELECT COUNT(*) FROM retroactive_cards"),
            q("SELECT COUNT(*) FROM user_like_streaks"),
            q("SELECT COUNT(*) FROM user_daily_likes"),
        ]
    }

    /// 结构迁移：真实 pre-v40 库（仅有 monthly_first_claimed）打开后追加 weekly_first_claimed、
    /// 保留原月度列、其余表结构逐字不变，且重复打开幂等
    #[test]
    fn test_publish_db_schema_migration_on_real_legacy() {
        let Some(dst) = publish_db_copy("schema") else {
            println!("[SKIP] test_publish_db_schema_migration_on_real_legacy: 未设置 MH_PUBLISH_DB");
            return;
        };
        let before = {
            let conn = Connection::open(&dst).unwrap();
            schema_snapshot(&conn)
        };

        {
            let mgr = CheckinManager::new(Some(&dst)).unwrap();
            let conn = mgr.conn.lock().unwrap();
            assert!(
                CheckinManager::table_has_column(&conn, "retroactive_cards", "weekly_first_claimed")
                    .unwrap(),
                "真实库应被追加 weekly_first_claimed"
            );
            assert!(
                CheckinManager::table_has_column(&conn, "retroactive_cards", "monthly_first_claimed")
                    .unwrap(),
                "原月度列必须保留，不得删除"
            );
        }

        // 重复打开幂等：除 retroactive_cards（仅追加列）外，其余表结构逐字不变
        {
            let _ = CheckinManager::new(Some(&dst)).unwrap();
            let conn = Connection::open(&dst).unwrap();
            let after = schema_snapshot(&conn);
            assert_eq!(before.len(), after.len(), "迁移不得增删表");
            for (b, a) in before.iter().zip(after.iter()) {
                if b.0 == "retroactive_cards" {
                    continue;
                }
                assert_eq!(b, a, "表 {} 结构不得变更", b.0);
            }
        }
        let _ = std::fs::remove_file(&dst);
        println!("[PASS] test_publish_db_schema_migration_on_real_legacy passed");
    }

    /// 数据行保真：打开 + 迁移前后五张表行数一致；奖卡抽样读取与 SQL 直查一致
    #[test]
    fn test_publish_db_row_counts_unchanged_after_open() {
        let Some(dst) = publish_db_copy("rows") else {
            println!("[SKIP] test_publish_db_row_counts_unchanged_after_open: 未设置 MH_PUBLISH_DB");
            return;
        };
        let before = {
            let conn = Connection::open(&dst).unwrap();
            count_all_rows(&conn)
        };
        assert!(before[0] > 0 && before[1] > 0, "真实库应有数据: {:?}", before);

        {
            let mgr = CheckinManager::new(Some(&dst)).unwrap();
            // 抽样 5 个持卡用户：V2 读取的 card_count 必须与 SQL 直查一致
            let conn = Connection::open(&dst).unwrap();
            let mut stmt = conn
                .prepare("SELECT uid, card_count FROM retroactive_cards WHERE card_count > 0 LIMIT 5")
                .unwrap();
            let samples: Vec<(String, i32)> = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i32>(1)?)))
                .unwrap()
                .flatten()
                .collect();
            drop(stmt);
            drop(conn);
            for (uid, sql_count) in samples {
                assert_eq!(mgr.get_cards(&uid).card_count, sql_count, "uid={} 卡数读取不一致", uid);
            }
        }

        let after = {
            let conn = Connection::open(&dst).unwrap();
            count_all_rows(&conn)
        };
        assert_eq!(before, after, "打开 + 迁移不得增删任何数据行");
        let _ = std::fs::remove_file(&dst);
        println!("[PASS] test_publish_db_row_counts_unchanged_after_open passed");
    }

    /// 连续天数重算：对真实用户的明细日期用独立实现倒推，与 calculate_continuous_days_from_records 对照；
    /// 同时观察原工程「一键黑幕遗留形态」（profile 累计 < 明细实数）是否存在于该库
    #[test]
    fn test_publish_db_continuous_recalc_matches_independent_walk() {
        let Some(dst) = publish_db_copy("streak") else {
            println!("[SKIP] test_publish_db_continuous_recalc_matches_independent_walk: 未设置 MH_PUBLISH_DB");
            return;
        };
        let mgr = CheckinManager::new(Some(&dst)).unwrap();
        let conn = mgr.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT uid FROM checkin_records LIMIT 30",
            )
            .unwrap();
        let uids: Vec<String> = stmt.query_map([], |r| r.get(0)).unwrap().flatten().collect();
        drop(stmt);
        assert!(!uids.is_empty(), "真实库应有打卡明细");
        drop(conn); // 释放锁：循环内短作用域重新取锁 + 调用 mgr 方法

        let mut dirty_forms = 0;
        for uid in &uids {
            // 短作用域取数（锁内只做查询，随后释放，避免与下方 mgr 方法重复加锁死锁）
            let (dates, stored, real): (Vec<NaiveDate>, i32, i32) = {
                let conn = mgr.conn.lock().unwrap();
                let mut stmt = conn
                    .prepare(
                        "SELECT DISTINCT checkin_date FROM checkin_records WHERE uid = ?1 ORDER BY checkin_date DESC",
                    )
                    .unwrap();
                let dates: Vec<NaiveDate> = stmt
                    .query_map(params![uid], |r| r.get::<_, i32>(0))
                    .unwrap()
                    .flatten()
                    .filter_map(CheckinManager::int_to_date)
                    .collect();
                drop(stmt);
                let stored: i32 = conn
                    .query_row(
                        "SELECT cumulative_days FROM user_profiles WHERE uid = ?1",
                        params![uid],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                let real: i32 = conn
                    .query_row(
                        "SELECT COUNT(DISTINCT checkin_date) FROM checkin_records WHERE uid = ?1",
                        params![uid],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                (dates, stored, real)
            };

            // 独立实现：日期降序，逐日比较 +1 天
            let mut expected = 1;
            for i in 1..dates.len() {
                if dates[i] + Duration::days(1) == dates[i - 1] {
                    expected += 1;
                } else {
                    break;
                }
            }
            assert_eq!(
                mgr.calculate_continuous_days_from_records(uid),
                expected,
                "uid={} 连续天数重算与独立倒推不一致",
                uid
            );

            // 脏形态观察：profile 存的累计 < 明细实数（原工程黑幕只改连续不重算累计的遗留）
            if real > stored {
                dirty_forms += 1;
            }
        }
        println!(
            "[INFO] 独立倒推对照通过；原工程黑幕遗留形态（明细实数 > profile 累计）用户数: {}",
            dirty_forms
        );
        drop(mgr); // 先释放 SQLite 连接，否则 Windows 上文件被占用导致 remove_file 静默失败
        let _ = std::fs::remove_file(&dst);
        println!("[PASS] test_publish_db_continuous_recalc_matches_independent_walk passed");
    }

    /// 补签 + 周卡全链路（真实用户，副本上执行）：
    /// 断签真实用户凭卡补签成功、扣卡、明细 +1；pre-v40 库迁移后周首破 30 正常发卡
    #[test]
    fn test_publish_db_retro_flow_and_weekly_card_on_real_user() {
        let Some(dst) = publish_db_copy("retro") else {
            println!("[SKIP] test_publish_db_retro_flow_and_weekly_card_on_real_user: 未设置 MH_PUBLISH_DB");
            return;
        };
        let mgr = CheckinManager::new(Some(&dst)).unwrap();

        // 用生产链路判定入口（实时重算连续/累计）挑选真正可补签的真实用户：
        // 连续 >= 累计（满勤或黑幕拉平）会被拦截，跳过；找第一个可补签者
        let candidates: Vec<(String, String)> = {
            let conn = mgr.conn.lock().unwrap();
            let rows: Vec<(String, String)> = conn
                .prepare("SELECT uid, username FROM user_profiles WHERE cumulative_days > 0")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .flatten()
                .collect();
            rows
        };
        let (uid, username) = candidates
            .iter()
            .find(|(uid, _)| mgr.check_retroactive_validity(uid).is_ok())
            .map(|(uid, name)| (uid.clone(), name.clone()))
            .expect("真实库应存在可补签的断签用户");

        let records_before: i64 = {
            let conn = mgr.conn.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM checkin_records WHERE uid = ?1",
                params![uid],
                |r| r.get(0),
            )
            .unwrap()
        };

        assert_eq!(mgr.grant_card(&uid, 2).unwrap(), 2);
        let today = Local::now().date_naive();
        let outcome = mgr.retro_command_outcome(&uid, &username, today, None);
        assert!(outcome.success, "断签真实用户补签应成功: {}", outcome.reply);
        assert!(outcome.reply.contains("已成功补签"), "{}", outcome.reply);
        assert_eq!(mgr.get_cards(&uid).card_count, 1, "补签一次应扣 1 张卡");

        let records_after: i64 = {
            let conn = mgr.conn.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM checkin_records WHERE uid = ?1",
                params![uid],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(records_after, records_before + 1, "补签应恰好新增一条明细");

        // pre-v40 库迁移后 weekly_first_claimed 默认 0 → 本周首次当日达 30 必发周卡
        let rewards = mgr.add_likes(&uid, 30, today).expect("add_likes 不得报错");
        assert!(rewards.weekly_reward, "迁移后周首破标记为 0，当日 30 应发卡");
        assert_eq!(mgr.get_cards(&uid).card_count, 2);
        drop(mgr); // 先释放 SQLite 连接，否则 Windows 上文件被占用导致 remove_file 静默失败
        let _ = std::fs::remove_file(&dst);
        println!("[PASS] test_publish_db_retro_flow_and_weekly_card_on_real_user passed");
    }

    /// 一键黑幕统计口径 + 导出格式（对照 publish_v2 真实导出样例）：
    /// patched + skipped == 总用户数、插入数 == 明细前后差、补后连续 == 黑幕前档案累计（对齐原工程黑幕语义）、
    /// 导出 CSV 带 BOM 且表头与原样例逐字一致
    #[test]
    fn test_publish_db_batch_checkin_stats_and_export_header() {
        let Some(dst) = publish_db_copy("batch") else {
            println!("[SKIP] test_publish_db_batch_checkin_stats_and_export_header: 未设置 MH_PUBLISH_DB");
            return;
        };
        let mgr = CheckinManager::new(Some(&dst)).unwrap();

        let (before, stored_cums): (i64, Vec<(String, i32)>) = {
            let conn = mgr.conn.lock().unwrap();
            let cums: Vec<(String, i32)> = conn
                .prepare("SELECT uid, cumulative_days FROM user_profiles WHERE cumulative_days > 0")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .flatten()
                .collect();
            let total: i64 = conn
                .query_row("SELECT COUNT(*) FROM checkin_records", [], |r| r.get(0))
                .unwrap();
            (total, cums)
        };

        let res = mgr.batch_checkin().unwrap();
        assert_eq!(res.total_users, stored_cums.len() as i32, "候选集应为 cumulative>0 用户");
        assert_eq!(
            res.patched_users + res.skipped_users,
            res.total_users,
            "patched 与 skipped 必须互斥且覆盖全部候选"
        );

        let after: i64 = {
            let conn = mgr.conn.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM checkin_records", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(res.total_inserted as i64, after - before, "插入数应等于明细增量");

        // 黑幕语义（对齐原工程 BatchCheckin）：连续天数置为黑幕前档案累计值（可能小于明细实数，
        // 由后续打卡的实时重算自愈）
        let (uid, stored_cum) = stored_cums.first().unwrap().clone();
        let profile = mgr.get_profile(&uid).unwrap();
        assert_eq!(
            profile.continuous_days, stored_cum,
            "黑幕后连续天数应等于黑幕前档案累计（原工程语义）"
        );

        // 导出格式与 publish_v2\checkin_records_20260527.csv 真实样例对照（BOM + 表头逐字一致）
        let csv = mgr.export_records_content("csv", None, None, None).unwrap();
        assert!(csv.starts_with('\u{feff}'), "明细导出应带 UTF-8 BOM");
        assert!(
            csv.trim_start_matches('\u{feff}').starts_with("uid,username,checkin_date,created_at\n"),
            "明细导出表头必须与原工程样例一致"
        );
        let summary = mgr.export_users_summary("csv").unwrap();
        assert!(summary.starts_with('\u{feff}'), "汇总导出应带 UTF-8 BOM");
        assert_eq!(
            summary.trim_start_matches('\u{feff}').lines().count() - 1,
            res.total_users as usize,
            "汇总导出行数应等于候选用户数"
        );
        drop(mgr); // 先释放 SQLite 连接，否则 Windows 上文件被占用导致 remove_file 静默失败
        let _ = std::fs::remove_file(&dst);
        println!("[PASS] test_publish_db_batch_checkin_stats_and_export_header passed");
    }

    // ---------------- C2：打卡库来源解析与双库可见性 ----------------

    /// 隔离临时目录 + 半截库文件（内容不重要，解析只看"文件是否存在"）
    fn c2_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mh_test_c2_{}", name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch_db(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"stub").unwrap();
        p
    }

    /// 可被 SQLite 打开的"空库"：零字节文件即为合法空库（解析只关心是否存在）
    fn touch_empty_db(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"").unwrap();
        p
    }

    /// D06：新装先把数据写进 checkin.db，之后旧库出现 —— 有活动库标记时必须继续用原库
    #[test]
    fn test_active_db_marker_prevents_silent_switch() {
        let dir = c2_dir("marker_priority");

        // 只有一个 V2 库：解析选中它并（在 open_at 中）写入标记
        let v2 = touch_db(&dir, V2_DB_FILE_NAME);
        let r1 = resolve_db_path(&dir).unwrap();
        assert_eq!(r1.active_path, v2);
        assert!(r1.warning.is_none());
        write_active_db_marker(&dir, &v2).unwrap();

        // 后来出现旧库：标记仍然指向 checkin.db，且必须告警另一份未被展示
        let legacy = touch_db(&dir, LEGACY_DB_FILE_NAME);
        let r2 = resolve_db_path(&dir).unwrap();
        assert_eq!(r2.active_path, v2, "有标记时不得静默改读另一份库");
        assert_eq!(r2.shadow_path.as_ref(), Some(&legacy));
        let warning = r2.warning.expect("双库必须产生可见警告");
        assert!(warning.contains("未展示"), "警告需说明另一份未展示: {}", warning);
        assert!(warning.contains("未删除"), "警告需说明另一份未被删除: {}", warning);

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_active_db_marker_prevents_silent_switch passed");
    }

    /// 四种文件组合的解析结果
    #[test]
    fn test_resolve_db_path_four_combinations() {
        // ① 只有旧库
        let d1 = c2_dir("only_legacy");
        let legacy = touch_db(&d1, LEGACY_DB_FILE_NAME);
        let r = resolve_db_path(&d1).unwrap();
        assert_eq!(r.active_path, legacy);
        assert!(r.shadow_path.is_none() && !r.created_new && r.warning.is_none());
        let _ = std::fs::remove_dir_all(&d1);

        // ② 只有 V2 库
        let d2 = c2_dir("only_v2");
        let v2 = touch_db(&d2, V2_DB_FILE_NAME);
        let r = resolve_db_path(&d2).unwrap();
        assert_eq!(r.active_path, v2);
        assert!(r.shadow_path.is_none() && !r.created_new && r.warning.is_none());
        let _ = std::fs::remove_dir_all(&d2);

        // ③ 两者都在且无标记：沿用旧库优先（兼容过渡），但必须显著告警
        let d3 = c2_dir("both_no_marker");
        let legacy3 = touch_db(&d3, LEGACY_DB_FILE_NAME);
        let v2_3 = touch_db(&d3, V2_DB_FILE_NAME);
        let r = resolve_db_path(&d3).unwrap();
        assert_eq!(r.active_path, legacy3, "无标记双库时沿用旧库优先规则");
        assert_eq!(r.shadow_path.as_ref(), Some(&v2_3));
        assert!(r.warning.is_some(), "无标记双库必须告警");
        let _ = std::fs::remove_dir_all(&d3);

        // ④ 两者都没有：新建旧库名（不再裂出 checkin.db）
        let d4 = c2_dir("neither");
        let r = resolve_db_path(&d4).unwrap();
        assert!(r.created_new);
        assert_eq!(r.active_path.file_name().unwrap(), LEGACY_DB_FILE_NAME);
        assert!(!r.active_path.exists(), "解析阶段不得创建文件");
        assert!(r.warning.is_none());
        let _ = std::fs::remove_dir_all(&d4);

        println!("[PASS] test_resolve_db_path_four_combinations passed");
    }

    /// 活动库标记指向的文件缺失时必须报错，不得悄悄换另一份库
    #[test]
    fn test_missing_marked_db_reports_error_without_switching() {
        let dir = c2_dir("marker_missing");
        let v2 = touch_db(&dir, V2_DB_FILE_NAME);
        write_active_db_marker(&dir, &dir.join(LEGACY_DB_FILE_NAME)).unwrap();

        let err = resolve_db_path(&dir).expect_err("标记目标缺失必须报错");
        assert!(err.contains("已不存在"), "错误需说明文件缺失: {}", err);
        assert!(v2.exists(), "另一份库不得被删除或改名");

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_missing_marked_db_reports_error_without_switching passed");
    }

    /// 标记里的文件名不得跳出数据目录
    #[test]
    fn test_marker_rejects_path_traversal_and_garbage() {
        let dir = c2_dir("marker_bad");
        for bad in ["../../etc/passwd", "sub/dir.db", "..\\win.db", ""] {
            std::fs::write(
                dir.join(ACTIVE_DB_MARKER_FILE_NAME),
                format!("{{\"active\":\"{}\"}}", bad.replace('\\', "\\\\")),
            )
            .unwrap();
            assert!(
                resolve_db_path(&dir).is_err(),
                "非法标记内容必须被拒绝: {:?}",
                bad
            );
        }

        std::fs::write(dir.join(ACTIVE_DB_MARKER_FILE_NAME), b"{ not json").unwrap();
        assert!(resolve_db_path(&dir).is_err(), "损坏的标记必须报错而不是静默选择");

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_marker_rejects_path_traversal_and_garbage passed");
    }

    /// 解析与打开都不得改动另一份库文件（不合并、不删除、不复制）
    #[test]
    fn test_shadow_db_file_is_never_modified() {
        let dir = c2_dir("shadow_untouched");
        let legacy = touch_empty_db(&dir, LEGACY_DB_FILE_NAME);
        let v2 = touch_empty_db(&dir, V2_DB_FILE_NAME);
        let v2_bytes = std::fs::read(&v2).unwrap();
        let (mgr, report) = CheckinManager::open_at(&dir).expect("应能打开旧库");
        assert!(report.has_shadow_db, "双库必须报告存在未展示的另一份");
        assert_eq!(report.active_file_name, LEGACY_DB_FILE_NAME);
        drop(mgr);

        assert_eq!(std::fs::read(&v2).unwrap(), v2_bytes, "另一份 V2 库字节不得变化");
        // 另一份库根本没被打开过：不应留下 SQLite 的 -wal/-shm sidecar
        assert!(
            !dir.join(format!("{}-wal", V2_DB_FILE_NAME)).exists()
                && !dir.join(format!("{}-shm", V2_DB_FILE_NAME)).exists(),
            "未展示的库不得被打开（不应产生 WAL sidecar）"
        );
        assert!(legacy.exists() && v2.exists(), "两份文件都必须保留");
        // 活动库标记已写入并指向旧库
        let marked = read_active_db_marker(&dir).unwrap().unwrap();
        assert_eq!(marked, legacy);
        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_shadow_db_file_is_never_modified passed");
    }

    /// 全新目录：打开后创建旧库名的库 + 活动库标记，且不产生 checkin.db
    #[test]
    fn test_open_at_fresh_dir_creates_legacy_named_db_only() {
        let dir = c2_dir("fresh");
        let (mgr, report) = CheckinManager::open_at(&dir).expect("应能新建库");
        assert_eq!(report.active_file_name, LEGACY_DB_FILE_NAME);
        assert!(!report.has_shadow_db);
        // 新库可正常写入（结构完整）
        let today = NaiveDate::from_ymd_opt(2026, 3, 20).unwrap();
        assert!(mgr.record_checkin("fresh_u", "新水友", today).is_ok());
        drop(mgr);

        assert!(dir.join(LEGACY_DB_FILE_NAME).is_file());
        assert!(!dir.join(V2_DB_FILE_NAME).exists(), "不得再新建 checkin.db");
        assert!(dir.join(ACTIVE_DB_MARKER_FILE_NAME).is_file());
        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_open_at_fresh_dir_creates_legacy_named_db_only passed");
    }

    /// 标记写入是原子的：不残留临时文件
    #[test]
    fn test_marker_write_is_atomic_and_leaves_no_temp() {
        let dir = c2_dir("marker_atomic");
        write_active_db_marker(&dir, &dir.join(LEGACY_DB_FILE_NAME)).unwrap();
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件: {:?}", leftovers);

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_marker_write_is_atomic_and_leaves_no_temp passed");
    }

    // ---------------- D1/D2/D3/D5：SQL 故障注入反测 ----------------
    //
    // 全部在内存库上用 RAISE(ABORT) trigger 制造 SQL 失败，不接触任何真实库。

    /// 给指定表加一个"总是拒绝写入"的 trigger（故障注入用）
    fn refuse_writes_on(mgr: &CheckinManager, table: &str, op: &str) {
        let conn = mgr.conn.lock().unwrap();
        conn.execute_batch(&format!(
            r#"
            CREATE TRIGGER refuse_{table}_{op} BEFORE {op} ON {table}
            BEGIN
                SELECT RAISE(ABORT, 'injected {op} failure on {table}');
            END;
            "#,
            table = table,
            op = op
        ))
        .unwrap();
    }

    fn drop_trigger(mgr: &CheckinManager, name: &str) {
        let conn = mgr.conn.lock().unwrap();
        let _ = conn.execute_batch(&format!("DROP TRIGGER IF EXISTS {}", name));
    }

    /// D1：打卡明细写入失败必须整体回滚 —— 不新建档案、不回复成功
    #[test]
    fn test_record_checkin_rolls_back_when_detail_insert_fails() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 3, 20).unwrap();
        refuse_writes_on(&mgr, "checkin_records", "INSERT");

        let err = mgr
            .record_checkin("d1_user", "打卡水友", today)
            .expect_err("明细写入失败必须返回 Err");
        assert!(err.contains("写入打卡明细失败"), "错误需指明失败环节: {}", err);

        drop_trigger(&mgr, "refuse_checkin_records_INSERT");
        assert!(
            mgr.get_profile("d1_user").is_err(),
            "回滚后不得留下档案（否则界面会显示已打卡）"
        );
        {
            let conn = mgr.conn.lock().unwrap();
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM checkin_records WHERE uid = 'd1_user'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0, "回滚后明细必须为 0 条");
        }

        // 故障解除后重试：明细、连续、累计同时正确
        let outcome = mgr.record_checkin_with_flag("d1_user", "打卡水友", today).unwrap();
        assert!(!outcome.already_checked_in, "首次打卡不算重复");
        assert_eq!(outcome.profile.continuous_days, 1);
        assert_eq!(outcome.profile.cumulative_days, 1);
        assert_eq!(mgr.get_cards("d1_user").card_count, 0);

        // 当天第二条真实打卡：档案不再变化，且被判定为重复
        let again = mgr.record_checkin_with_flag("d1_user", "打卡水友", today).unwrap();
        assert!(again.already_checked_in, "当天第二条应判定为重复打卡");
        assert_eq!(again.profile.cumulative_days, 1);
        println!("[PASS] test_record_checkin_rolls_back_when_detail_insert_fails passed");
    }

    /// D2：批量补签遇到一条失败必须整体回滚，且不得报告已插入
    #[test]
    fn test_batch_checkin_rolls_back_entire_batch_on_failure() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let today = Local::now().date_naive();
        let d3 = today - Duration::days(3);

        // 两个候选用户：各自有一条 3 天前的打卡，中间留有缺口
        for uid in ["batch_a", "batch_b"] {
            mgr.record_checkin(uid, "批量水友", d3).unwrap();
        }
        let before: i64 = {
            let conn = mgr.conn.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM checkin_records", [], |r| r.get(0))
                .unwrap()
        };

        // 注入：第二个用户的档案更新失败（按 uid 精确命中，保证不是"没命中"才失败）
        {
            let conn = mgr.conn.lock().unwrap();
            conn.execute_batch(
                r#"
                CREATE TRIGGER refuse_batch_profile BEFORE UPDATE ON user_profiles
                WHEN OLD.uid = 'batch_b'
                BEGIN
                    SELECT RAISE(ABORT, 'injected profile update failure');
                END;
                "#,
            )
            .unwrap();
        }

        let err = mgr.batch_checkin().expect_err("任一用户失败必须整体回滚");
        assert!(err.contains("更新档案失败"), "错误需指明失败环节: {}", err);

        drop_trigger(&mgr, "refuse_batch_profile");
        let after: i64 = {
            let conn = mgr.conn.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM checkin_records", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(after, before, "失败批次不得留下任何部分插入的明细");

        // 故障解除后整批成功，计数与真实增量一致
        let res = mgr.batch_checkin().unwrap();
        assert!(res.success);
        let final_count: i64 = {
            let conn = mgr.conn.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM checkin_records", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(
            res.total_inserted as i64,
            final_count - before,
            "报告插入数必须等于真实明细增量"
        );
        assert_eq!(res.patched_users + res.skipped_users, res.total_users);

        // INSERT OR IGNORE 被唯一键跳过时，受影响行数为 0，不得计入
        let res2 = mgr.batch_checkin().unwrap();
        assert_eq!(res2.total_inserted, 0, "已补齐的用户再跑一次不得报告插入");
        println!("[PASS] test_batch_checkin_rolls_back_entire_batch_on_failure passed");
    }

    /// D3：连赞标记 UPSERT 失败时，同事务里已发出的卡必须一起回滚（不得"卡已发、标记没存"）
    #[test]
    fn test_add_likes_rolls_back_card_when_streak_marker_fails() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 3, 20).unwrap();
        let six_days_ago = today - Duration::days(6);

        // 先造出"连赞 6 天"的状态：第 1 天写入，之后逐日推进
        for day in 0..6 {
            let d = six_days_ago + Duration::days(day);
            mgr.add_likes("streak_fail", 1, d).unwrap();
        }
        assert_eq!(mgr.get_cards("streak_fail").card_count, 0, "前 6 天不发卡");

        // 注入：连赞标记写入失败（第 7 天本应先加卡、再写标记）
        refuse_writes_on(&mgr, "user_like_streaks", "INSERT");

        let err = mgr
            .add_likes("streak_fail", 1, today)
            .expect_err("标记写入失败必须整体回滚");
        assert!(err.contains("连续点赞标记失败") || err.contains("连续点赞奖卡失败"), "{}", err);

        drop_trigger(&mgr, "refuse_user_like_streaks_INSERT");
        assert_eq!(
            mgr.get_cards("streak_fail").card_count,
            0,
            "奖励卡必须随标记失败一起回滚，否则会重复发卡"
        );

        // 故障解除后重试：卡与标记同时生效，且只发一张
        let rewards = mgr.add_likes("streak_fail", 1, today).unwrap();
        assert!(rewards.streak_reward, "第 7 天应发连赞卡");
        assert_eq!(mgr.get_cards("streak_fail").card_count, 1);

        // 同一天再来一条不同 msg_id 的赞：标记已保存，不得重复发卡
        let again = mgr.add_likes("streak_fail", 1, today).unwrap();
        assert!(!again.streak_reward, "同一领取日不得重复发卡");
        assert_eq!(mgr.get_cards("streak_fail").card_count, 1);
        println!("[PASS] test_add_likes_rolls_back_card_when_streak_marker_fails passed");
    }

    /// D3：周奖卡更新失败时，本日点赞累加与连赞状态一起回滚
    #[test]
    fn test_add_likes_rolls_back_weekly_card_failure() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 3, 20).unwrap();

        // 周奖卡写入失败（retroactive_cards 的 INSERT）
        refuse_writes_on(&mgr, "retroactive_cards", "INSERT");

        let err = mgr
            .add_likes("weekly_fail", 30, today)
            .expect_err("周奖卡写入失败必须整体回滚");
        assert!(err.contains("周奖卡"), "{}", err);

        drop_trigger(&mgr, "refuse_retroactive_cards_INSERT");
        assert_eq!(mgr.get_cards("weekly_fail").card_count, 0, "回滚后不得留下卡");
        assert_eq!(
            mgr.get_daily_like_total("weekly_fail", today),
            None,
            "整事件回滚后当日点赞也不得留下半截状态"
        );

        // 重试成功：一次发一张，再次突破 30 不再发
        let r = mgr.add_likes("weekly_fail", 30, today).unwrap();
        assert!(r.weekly_reward);
        assert_eq!(mgr.get_cards("weekly_fail").card_count, 1);
        let again = mgr.add_likes("weekly_fail", 30, today).unwrap();
        assert!(!again.weekly_reward, "同一自然周限领 1 张");
        assert_eq!(mgr.get_cards("weekly_fail").card_count, 1);
        println!("[PASS] test_add_likes_rolls_back_weekly_card_failure passed");
    }

    /// D5/I01：同一 msg_id 的补签重投只能扣一张卡、只写一条明细
    #[test]
    fn test_retro_same_msg_id_is_idempotent() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 3, 20).unwrap();
        let three_days_ago = today - Duration::days(3);

        // 两张卡、两个缺日（today-1 与 today-2 都没打）
        mgr.record_checkin("retro_dup", "补签水友", three_days_ago).unwrap();
        mgr.record_checkin("retro_dup", "补签水友", today).unwrap();
        mgr.grant_card("retro_dup", 2).unwrap();

        let first = mgr.retro_command_outcome("retro_dup", "补签水友", today, Some("msg_dup_1"));
        assert!(first.success, "{}", first.reply);
        assert_eq!(mgr.get_cards("retro_dup").card_count, 1);

        // 同一条弹幕重投：不再扣卡
        let replay = mgr.retro_command_outcome("retro_dup", "补签水友", today, Some("msg_dup_1"));
        assert!(!replay.success, "重投不得再次补签: {}", replay.reply);
        assert_eq!(
            mgr.get_cards("retro_dup").card_count,
            1,
            "同一 msg_id 重投只能扣一张卡"
        );

        // 不同 msg_id 的合法补签不受影响
        let second = mgr.retro_command_outcome("retro_dup", "补签水友", today, Some("msg_dup_2"));
        assert!(second.success, "{}", second.reply);
        assert_eq!(mgr.get_cards("retro_dup").card_count, 0);

        // 每条只写一条明细
        let (records, dup_keys): (i64, i64) = {
            let conn = mgr.conn.lock().unwrap();
            (
                conn.query_row(
                    "SELECT COUNT(*) FROM checkin_records WHERE uid = 'retro_dup'",
                    [],
                    |r| r.get(0),
                )
                .unwrap(),
                conn.query_row("SELECT COUNT(*) FROM processed_retro_commands", [], |r| r.get(0))
                    .unwrap(),
            )
        };
        assert_eq!(records, 4, "两次补签应各写一条明细（2 原始 + 2 补签）");
        assert_eq!(dup_keys, 2, "只对真正扣卡的两次补签写幂等键");
        println!("[PASS] test_retro_same_msg_id_is_idempotent passed");
    }

    /// D5：空 msg_id 不写幂等键 —— 不得用伪造键挡掉合法的多次补签
    #[test]
    fn test_retro_empty_msg_id_keeps_legacy_semantics() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 3, 20).unwrap();
        let three_days_ago = today - Duration::days(3);

        // 两张卡、两个缺日
        mgr.record_checkin("retro_no_id", "无ID水友", three_days_ago).unwrap();
        mgr.record_checkin("retro_no_id", "无ID水友", today).unwrap();
        mgr.grant_card("retro_no_id", 2).unwrap();

        for _ in 0..2 {
            let r = mgr.retro_command_outcome("retro_no_id", "无ID水友", today, None);
            assert!(r.success, "{}", r.reply);
        }
        assert_eq!(mgr.get_cards("retro_no_id").card_count, 0);
        let keys: i64 = {
            let conn = mgr.conn.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM processed_retro_commands", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(keys, 0, "空 msg_id 不得写入幂等键");
        println!("[PASS] test_retro_empty_msg_id_keeps_legacy_semantics passed");
    }

    /// D5：只读反馈（无卡 / 无需补签 / 无缺日）不写幂等键，重投仍能给出正常提示
    #[test]
    fn test_retro_readonly_outcomes_do_not_write_idempotency_key() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 3, 20).unwrap();

        // 无卡
        let r1 = mgr.retro_command_outcome("ro_none", "无卡水友", today, Some("ro_msg_1"));
        assert_eq!(r1.reply, "无卡水友，你没有补签卡哦~");
        // 满勤（连续 == 累计）
        mgr.record_checkin("ro_full", "满勤水友", today).unwrap();
        mgr.grant_card("ro_full", 1).unwrap();
        let r2 = mgr.retro_command_outcome("ro_full", "满勤水友", today, Some("ro_msg_2"));
        assert!(!r2.success);
        assert_eq!(mgr.get_cards("ro_full").card_count, 1, "无需补签不得扣卡");

        let keys: i64 = {
            let conn = mgr.conn.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM processed_retro_commands", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(keys, 0, "只读反馈不得写幂等键");

        // 重投同样的 msg_id 仍能拿到同样的正常提示，而不是"已处理过"
        let r1_again = mgr.retro_command_outcome("ro_none", "无卡水友", today, Some("ro_msg_1"));
        assert_eq!(r1_again.reply, "无卡水友，你没有补签卡哦~");
        println!("[PASS] test_retro_readonly_outcomes_do_not_write_idempotency_key passed");
    }

    /// D5：补签失败时幂等占位必须一起回滚，重投可以重试
    #[test]
    fn test_retro_failure_rolls_back_idempotency_placeholder() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 3, 20).unwrap();
        let two_days_ago = today - Duration::days(2);

        mgr.record_checkin("retro_retry", "重试水友", two_days_ago).unwrap();
        mgr.record_checkin("retro_retry", "重试水友", today).unwrap();
        mgr.grant_card("retro_retry", 1).unwrap();

        // 注入：明细插入失败 → 整个补签（含幂等键）回滚
        refuse_writes_on(&mgr, "checkin_records", "INSERT");
        let r = mgr.retro_command_outcome("retro_retry", "重试水友", today, Some("retry_msg_1"));
        assert!(!r.success, "失败不得报告成功: {}", r.reply);
        drop_trigger(&mgr, "refuse_checkin_records_INSERT");

        assert_eq!(mgr.get_cards("retro_retry").card_count, 1, "失败不得扣卡");
        let keys: i64 = {
            let conn = mgr.conn.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM processed_retro_commands", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(keys, 0, "失败的补签不得留下幂等占位（否则重投永远被挡）");

        // 重投同一 msg_id 可以正常补签
        let ok = mgr.retro_command_outcome("retro_retry", "重试水友", today, Some("retry_msg_1"));
        assert!(ok.success, "{}", ok.reply);
        assert_eq!(mgr.get_cards("retro_retry").card_count, 0);
        println!("[PASS] test_retro_failure_rolls_back_idempotency_placeholder passed");
    }

    /// D5：卡数扣减必须真正命中一行（并发/脏数据下不得静默成功）
    #[test]
    fn test_retro_card_deduction_requires_exactly_one_row() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 3, 20).unwrap();
        let two_days_ago = today - Duration::days(2);

        mgr.record_checkin("retro_norow", "无卡行水友", two_days_ago).unwrap();
        mgr.record_checkin("retro_norow", "无卡行水友", today).unwrap();
        // 直接调用底层执行器：卡片行不存在时不得凭空"补签成功"
        let err = mgr
            .execute_retroactive_checkin("retro_norow", "无卡行水友", today - Duration::days(1), None)
            .expect_err("无卡行时不得补签成功");
        assert!(err.contains("补签卡不足"), "{}", err);

        let records: i64 = {
            let conn = mgr.conn.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM checkin_records WHERE uid = 'retro_norow'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(records, 2, "失败的补签不得写入明细");
        println!("[PASS] test_retro_card_deduction_requires_exactly_one_row passed");
    }
}
