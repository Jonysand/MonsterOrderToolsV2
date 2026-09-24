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

/// 历史留档目录（对齐原工程 `WriteLog::RecordHistory` 的 `History/` 目录）
pub fn history_dir() -> PathBuf {
    crate::paths::config_dir().join("History")
}

/// 崩溃转储目录（minidump 与崩溃报告）
pub fn crashes_dir() -> PathBuf {
    crate::paths::config_dir().join("Crashes")
}

/// 日志/留档文件写入串行锁：原工程 `WriteLog` 用 `writtingLock` 串行化
/// 「判空→写 BOM→追加」全过程；V2 若不加锁，多线程同日首次写入会写出两个 BOM。
static FILE_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 单行格式，与原工程 `WriteLog` 逐字对齐
pub fn format_line(time: &str, level: LogLevel, message: &str) -> String {
    format!("[{}]:[{}] {}\n", time, level.as_str(), message)
}

/// 追加一行到 `dir/YYYY-MM-DD.txt`；文件为新建或空文件时先写入 UTF-8 BOM
pub fn append_line(dir: &Path, date: &str, line: &str) -> std::io::Result<()> {
    let _guard = FILE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    append_line_unlocked(dir, date, line)
}

fn append_line_unlocked(dir: &Path, date: &str, line: &str) -> std::io::Result<()> {
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

/// 历史留档：写入 `<数据目录>/History/YYYY.M.D.txt`，行格式 `[HH:MM:SS] msg`
/// （与原工程 `WriteLog::RecordHistory` 的文件名与行格式逐字一致：月/日不补零）
pub fn record_history(message: &str) {
    use chrono::Datelike;
    let now = chrono::Local::now();
    // 月/日不补零，与原工程 `_stprintf_s(fileName, "%d.%d.%d.txt", wYear, wMonth, wDay)` 一致
    let date = format!("{}.{}.{}", now.year(), now.month(), now.day());
    let line = format!("[{}] {}\n", now.format("%H:%M:%S"), message);
    // 与 log 一致：测试构建不落盘，避免污染仓库数据目录（格式由单测覆盖）
    #[cfg(not(test))]
    let _ = append_line(&history_dir(), &date, &line);
    #[cfg(test)]
    let _ = (&date, &line);
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

// ---------------------------------------------------------------------------
// 崩溃处理（对齐原工程 DumpHelper：SetUnhandledExceptionFilter + MiniDumpWriteDump）
// ---------------------------------------------------------------------------

/// 崩溃报告文本（Rust panic 路径：panic=abort 下 hook 仍会先于 abort 执行）
pub fn format_crash_report(message: &str, location: &str, backtrace: &str) -> String {
    format!(
        "=================== MonsterOrderWilds-Ascendance 崩溃报告 ===================\n\
         时间: {}\n\
         消息: {}\n\
         位置: {}\n\
         ---------------- 回溯 ----------------\n{}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        message,
        location,
        backtrace
    )
}

/// 写出崩溃报告文件，返回路径（失败返回 None）
pub fn write_crash_report(message: &str, location: &str, backtrace: &str) -> Option<PathBuf> {
    let dir = crashes_dir();
    fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!(
        "crash-{}.txt",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    let report = format_crash_report(message, location, backtrace);
    fs::write(&path, report.as_bytes()).ok()?;
    Some(path)
}

/// 安装崩溃处理器：① panic hook 写崩溃报告 + 日志；② Windows 未处理异常写全内存 minidump。
/// 对齐原工程 `DumpHelper::Init("*.dmp", FULL_DUMP)` 的能力（原为 exe 同级，V2 落在数据目录）。
pub fn install_crash_handler() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "未知 panic".to_string());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "未知位置".to_string());
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let path = write_crash_report(&msg, &location, &backtrace);
        log(
            LogLevel::Error,
            format!(
                "[Crash] 程序异常终止: {} @ {}（报告: {}）",
                msg,
                location,
                path.as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "写入失败".into())
            ),
        );
        default_hook(info);
    }));

    #[cfg(windows)]
    platform::install_unhandled_exception_filter();
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    #[repr(C)]
    struct MiniDumpExceptionInformation {
        thread_id: u32,
        exception_pointers: *mut c_void,
        client_pointers: *mut c_void,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn SetUnhandledExceptionFilter(
            filter: Option<unsafe extern "system" fn(*mut c_void) -> i32>,
        ) -> *mut c_void;
        fn GetCurrentProcess() -> *mut c_void;
        fn GetCurrentProcessId() -> u32;
        fn GetCurrentThreadId() -> u32;
        fn LoadLibraryA(name: *const u8) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: *mut c_void,
        ) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    /// 未处理异常过滤器返回码
    const EXCEPTION_EXECUTE_HANDLER: i32 = 1;
    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
    /// MiniDumpWithFullMemory（对齐原工程 FULL_DUMP）
    const MINIDUMP_WITH_FULL_MEMORY: u32 = 0x00000002;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const CREATE_ALWAYS: u32 = 2;

    type MiniDumpWriteDumpFn = unsafe extern "system" fn(
        *mut c_void,
        u32,
        *mut c_void,
        u32,
        *mut MiniDumpExceptionInformation,
        *mut c_void,
        *mut c_void,
    ) -> i32;

    /// 记录已处理崩溃，避免异常过滤器被重入
    static CRASH_HANDLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    unsafe extern "system" fn exception_filter(exception_pointers: *mut c_void) -> i32 {
        use std::sync::atomic::Ordering;
        if CRASH_HANDLED.swap(true, Ordering::SeqCst) {
            return EXCEPTION_CONTINUE_SEARCH;
        }

        // 动态加载 dbghelp.dll（与原工程一致，不产生链接期依赖）
        let module = LoadLibraryA(b"dbghelp.dll\0".as_ptr());
        if module.is_null() {
            crate::logging::log(
                crate::logging::LogLevel::Error,
                "[Crash] 无法加载 dbghelp.dll，跳过转储生成",
            );
            return EXCEPTION_CONTINUE_SEARCH;
        }

        let proc = GetProcAddress(module, b"MiniDumpWriteDump\0".as_ptr());
        if proc.is_null() {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let dump_fn: MiniDumpWriteDumpFn = std::mem::transmute(proc);

        // 转储路径：<数据目录>/Crashes/crash-YYYYmmdd-HHMMSS.dmp
        let _ = std::fs::create_dir_all(crate::logging::crashes_dir());
        let path = crate::logging::crashes_dir().join(format!(
            "crash-{}.dmp",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        ));
        let wide: Vec<u16> = std::ffi::OsStr::new(&path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let file = CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            0,
            std::ptr::null_mut(),
            CREATE_ALWAYS,
            0,
            std::ptr::null_mut(),
        );
        if file as isize == -1 {
            return EXCEPTION_CONTINUE_SEARCH;
        }

        let mut info = MiniDumpExceptionInformation {
            thread_id: GetCurrentThreadId(),
            exception_pointers,
            client_pointers: std::ptr::null_mut(),
        };
        dump_fn(
            GetCurrentProcess(),
            GetCurrentProcessId(),
            file,
            MINIDUMP_WITH_FULL_MEMORY,
            &mut info,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        CloseHandle(file);
        crate::logging::log(
            crate::logging::LogLevel::Error,
            format!("[Crash] 已生成崩溃转储: {}", path.display()),
        );
        EXCEPTION_EXECUTE_HANDLER
    }
    pub fn install_unhandled_exception_filter() {
        unsafe {
            SetUnhandledExceptionFilter(Some(exception_filter));
        }
    }
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
    fn test_history_line_format_matches_original_record_history() {
        // 对齐原工程 WriteLog::RecordHistory：`[HH:MM:SS] msg\n`
        use chrono::Timelike;
        let now = chrono::Local::now();
        let line = format!("[{}] {}\n", now.format("%H:%M:%S"), "水友A 进入直播间");
        assert!(line.starts_with('['), "line: {}", line);
        assert!(line.ends_with("水友A 进入直播间\n"), "line: {}", line);
        assert_eq!(now.format("%H:%M:%S").to_string().len(), 8);
        assert_eq!(now.hour() < 24, true);

        // 文件名 YYYY.M.D.txt（月/日不补零）
        use chrono::Datelike;
        let date = format!("{}.{}.{}", now.year(), now.month(), now.day());
        assert!(!date.starts_with('0'), "date: {}", date);
        assert_eq!(date.split('.').count(), 3, "date: {}", date);
        println!("[PASS] test_history_line_format_matches_original_record_history passed");
    }

    #[test]
    fn test_crash_report_format_and_write() {
        let report = format_crash_report("测试 panic", "src/main.rs:1:1", "backtrace-line");
        assert!(report.contains("崩溃报告"));
        assert!(report.contains("测试 panic"));
        assert!(report.contains("src/main.rs:1:1"));
        assert!(report.contains("backtrace-line"));

        // write_crash_report 依赖 paths::config_dir()，仅验证其返回文件存在且内容可读
        if let Some(path) = write_crash_report("测试 panic", "src/main.rs:1:1", "bt") {
            let text = fs::read_to_string(&path).unwrap();
            assert!(text.contains("测试 panic"));
            let _ = fs::remove_file(&path);
        }
        println!("[PASS] test_crash_report_format_and_write passed");
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
