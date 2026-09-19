//! 按日期分文件的运行日志与内存日志环（对齐原工程 `WriteLog`）。
//!
//! * 文件位置：`<数据目录>/Logs/YYYY-MM-DD.txt`（原工程为 exe 同级 `Logs`，V2 统一到可写数据目录）
//! * 文件编码：UTF-8 with BOM（首行写入时补 BOM，Excel/记事本直接打开不乱码）
//! * 行格式：`[YYYY-MM-DD HH:MM:SS]:[LEVEL] message`
//! * 内存环：保留最近 [`MAX_RECENT_ENTRIES`] 条供前端「运行日志」视图读取

use serde::Serialize;
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 日志级别（数值越大越详细，与原工程 `WriteLog::LogLevel` 顺序一致）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum LogLevel {
    Error = 0,
    Warning = 1,
    Info = 2,
    Debug = 3,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Warning => "WARNING",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
        }
    }

    /// 解析级别名（大小写不敏感，兼容前端传入的 "error"/"DEBUG"）
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "ERROR" => Some(LogLevel::Error),
            "WARNING" | "WARN" => Some(LogLevel::Warning),
            "INFO" => Some(LogLevel::Info),
            "DEBUG" => Some(LogLevel::Debug),
            _ => None,
        }
    }
}

/// 前端可见的日志条目
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub time: String,
    pub level: String,
    pub message: String,
}

/// 内存保留条数上限
pub const MAX_RECENT_ENTRIES: usize = 500;

struct LoggerState {
    recent: VecDeque<LogEntry>,
    /// 记录到文件/内存环的最高详细级别（Debug=最详细；对齐原工程 Release 下关闭 LOG_DEBUG）
    verbose_level: LogLevel,
}

static LOGGER: OnceLock<Mutex<LoggerState>> = OnceLock::new();

fn state() -> &'static Mutex<LoggerState> {
    LOGGER.get_or_init(|| {
        Mutex::new(LoggerState {
            recent: VecDeque::new(),
            // 与原工程一致：Debug 级别仅在调试构建输出
            verbose_level: if cfg!(debug_assertions) {
                LogLevel::Debug
            } else {
                LogLevel::Info
            },
        })
    })
}

/// 运行日志目录（不存在时由 [`append_line`] 自动创建）
pub fn logs_dir() -> PathBuf {
    crate::paths::config_dir().join("Logs")
}

/// 单行格式，与原工程 `WriteLog` 逐字对齐
pub fn format_line(time: &str, level: LogLevel, message: &str) -> String {
    format!("[{}]:[{}] {}\n", time, level.as_str(), message)
}

/// 追加一行到 `dir/YYYY-MM-DD.txt`；文件为新建或空文件时先写入 UTF-8 BOM
pub fn append_line(dir: &Path, date: &str, line: &str) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.txt", date));
    let need_bom = fs::metadata(&path).map(|m| m.len() == 0).unwrap_or(true);
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    if need_bom {
        f.write_all(&[0xEF, 0xBB, 0xBF])?;
    }
    f.write_all(line.as_bytes())?;
    f.flush()
}

/// 记录一条日志：写入内存环并落盘。落盘失败静默（日志本身不得影响主流程）
pub fn log(level: LogLevel, message: impl AsRef<str>) {
    let msg = message.as_ref().to_string();
    let now = chrono::Local::now();
    let time = now.format("%Y-%m-%d %H:%M:%S").to_string();
    let date = now.format("%Y-%m-%d").to_string();

    {
        let mut st = match state().lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        if level > st.verbose_level {
            return;
        }
        st.recent.push_back(LogEntry {
            time: time.clone(),
            level: level.as_str().to_string(),
            message: msg.clone(),
        });
        while st.recent.len() > MAX_RECENT_ENTRIES {
            st.recent.pop_front();
        }
    }

    let line = format_line(&time, level, &msg);
    // 测试构建不落盘，避免污染仓库数据目录；落盘路径由 append_line 单测覆盖
    #[cfg(not(test))]
    let _ = append_line(&logs_dir(), &date, &line);
    #[cfg(test)]
    let _ = (&date, &line);
}

/// 读取内存环中的日志（`limit` 为最多返回条数，返回按时间正序）；
/// `max_level` 表示最多返回到该详细级别（如 `Info` 时不返回 Debug 条目），`None` 为不过滤
pub fn recent_entries(limit: usize, max_level: Option<LogLevel>) -> Vec<LogEntry> {
    let st = match state().lock() {
        Ok(s) => s,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut out: Vec<LogEntry> = st
        .recent
        .iter()
        .filter(|e| match max_level {
            Some(lv) => LogLevel::parse(&e.level).map(|l| l <= lv).unwrap_or(true),
            None => true,
        })
        .cloned()
        .collect();
    if out.len() > limit {
        out.drain(..out.len() - limit);
    }
    out
}

/// 清空内存环（不影响已落盘日志文件）
pub fn clear_recent() {
    let mut st = match state().lock() {
        Ok(s) => s,
        Err(poisoned) => poisoned.into_inner(),
    };
    st.recent.clear();
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::LogLevel::Error, format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::LogLevel::Warning, format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::LogLevel::Info, format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::LogLevel::Debug, format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_line_matches_original_write_log() {
        let line = format_line("2026-09-19 21:03:07", LogLevel::Warning, "资源缺失: voices");
        assert_eq!(line, "[2026-09-19 21:03:07]:[WARNING] 资源缺失: voices\n");
        assert_eq!(LogLevel::parse("warning"), Some(LogLevel::Warning));
        assert_eq!(LogLevel::parse("ERROR"), Some(LogLevel::Error));
        assert_eq!(LogLevel::parse("nope"), None);
        println!("[PASS] test_format_line_matches_original_write_log passed");
    }

    #[test]
    fn test_append_line_writes_bom_once_and_appends() {
        let base = std::env::temp_dir().join("mh_test_logging");
        let _ = fs::remove_dir_all(&base);

        append_line(&base, "2026-09-19", &format_line("2026-09-19 10:00:00", LogLevel::Info, "第一条")).unwrap();
        append_line(&base, "2026-09-19", &format_line("2026-09-19 10:00:01", LogLevel::Error, "第二条")).unwrap();

        let path = base.join("2026-09-19.txt");
        let bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[..3], &[0xEF, 0xBB, 0xBF], "文件头应为 UTF-8 BOM");
        let text = String::from_utf8(bytes[3..].to_vec()).unwrap();
        assert_eq!(text, "[2026-09-19 10:00:00]:[INFO] 第一条\n[2026-09-19 10:00:01]:[ERROR] 第二条\n");
        // 仅首次写入 BOM（第二次追加后文件中 BOM 只出现一次）
        assert_eq!(fs::read(&path).unwrap().windows(3).filter(|w| *w == [0xEF, 0xBB, 0xBF]).count(), 1);

        let _ = fs::remove_dir_all(&base);
        println!("[PASS] test_append_line_writes_bom_once_and_appends passed");
    }

    #[test]
    fn test_recent_ring_buffer_and_level_filter() {
        let marker = format!("ring-{}", std::process::id());
        for i in 0..(MAX_RECENT_ENTRIES + 20) {
            log(LogLevel::Debug, format!("{}#{}", marker, i));
        }
        log(LogLevel::Error, format!("{}#错误", marker));

        let all = recent_entries(MAX_RECENT_ENTRIES * 2, None);
        assert!(all.len() <= MAX_RECENT_ENTRIES, "内存环长度不得超过上限");
        assert_eq!(all.last().unwrap().level, "ERROR");
        assert!(all.iter().any(|e| e.message.ends_with("#错误")));

        // 级别过滤：max_level=Info 时不返回 Debug 条目（Error/Warning/Info 保留）
        let filtered = recent_entries(MAX_RECENT_ENTRIES, Some(LogLevel::Info));
        assert!(filtered.iter().all(|e| e.level != "DEBUG"));
        assert!(filtered.iter().all(|e| matches!(e.level.as_str(), "ERROR" | "WARNING" | "INFO")));
        assert!(filtered.iter().any(|e| e.message.ends_with("#错误")));

        clear_recent();
        assert!(recent_entries(10, None).is_empty());
        println!("[PASS] test_recent_ring_buffer_and_level_filter passed");
    }
}
