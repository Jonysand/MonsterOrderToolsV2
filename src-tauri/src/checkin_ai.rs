//! 打卡 AI 个性化回复与弹幕关键词学习（对齐原工程 CaptainCheckInModule）
//!
//! - 弹幕学习：仅舰长的弹幕参与学习（ShouldLearn 的 guardLevel > 0 门槛），
//!   同一用户 5 秒窗口内不重复学习（LEARN_TIME_WINDOW_MS）；
//!   指令类弹幕（打卡触发词、补签/查询词）由调用方在学习前排除（lib.rs::is_command_message），
//!   不计入关键词与发言历史，避免污染 AI 提示词（原工程无此过滤，属有意差异）；
//! - 关键词：jieba 分词（HMM 模式）→ 停用词过滤 → #标签# 排除 → 词频统计（上限 50，按频次降序）；
//! - 发言历史：最近 100 条（danmu_history_json，JSON 格式与原工程一致）；
//! - BuildPrompt / 兜底文案：用户消息与兜底文案逐字对齐原工程 BuildPrompt / GetFallbackAnswer；
//!   本工程另由 `ai::SYSTEM_PROMPT_CHECKIN` 注入随从猫人设系统提示词（有意增强，非原工程行为）。

use crate::checkin::{CheckinManager, KeywordRecord, LearningProfile};
use jieba_rs::Jieba;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

pub const MAX_KEYWORDS_COUNT: usize = 50;
pub const MAX_DANMU_HISTORY_SIZE: usize = 100;
/// 原工程按 UTF-8 字节长度比较：中文单字 3 字节可通过，单个 ASCII 字符会被过滤
pub const MIN_WORD_BYTES: usize = 2;
pub const LEARN_TIME_WINDOW_MS: i64 = 5_000;
pub const MAX_SAME_CONTENT_SKIP: i32 = 3;
/// 用户自定义词典缺省词频（对齐原工程《弹幕习惯词黑白名单配置.txt》：词频可省略，默认为 10）
pub const DEFAULT_USER_WORD_FREQ: usize = 10;

/// 内置停用词兜底（对齐原工程 STOP_WORDS；dict/stop_words.utf8 存在时以文件为准）
const BUILTIN_STOP_WORDS: &[&str] = &[
    "的", "了", "在", "是", "我", "你", "他", "她", "它",
    "这", "那", "都", "和", "与", "或", "一", "一下",
    "吗", "呢", "吧", "啊", "哦", "嗯", "哈哈", "嘿嘿",
    "可以", "什么", "怎么", "为什么", "有没有", "但是",
    "然后", "所以", "因为", "如果", "虽然",
];

fn hashtag_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"#([^#]+)#").unwrap())
}

/// 打卡弹幕学习器：jieba 分词 + 词频统计 + 同内容防刷屏
pub struct CheckinLearner {
    jieba: Jieba,
    stop_words: HashSet<String>,
    /// uid -> (上一条内容, 连续相同次数)，仅运行时记忆（对齐原工程 profile 内存字段）
    same_content: Mutex<HashMap<String, (String, i32)>>,
}

impl CheckinLearner {
    /// 从随包资源构建：内嵌 jieba 主词典 + dict/user.dict.utf8 自定义词 + dict/stop_words.utf8 停用词
    pub fn from_resources() -> Self {
        let mut jieba = Jieba::new();
        if let Some(path) = crate::paths::find_resource("dict/user.dict.utf8") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                Self::apply_user_dict(&mut jieba, &text);
            }
        }
        Self {
            jieba,
            stop_words: Self::load_stop_words(),
            same_content: Mutex::new(HashMap::new()),
        }
    }

    /// 载入用户自定义词典（主播黑话），逐行解析「词语 [词频] [词性]」。
    /// 对齐原工程《弹幕习惯词黑白名单配置.txt》约定：词频可省略，默认为 10；
    /// 词频位置写成非数字（如把词性误填在此）时按词性处理并回退默认词频。
    /// 注意：词频 0 在动态规划分词中等于禁用该词，故此处统一钳制到最小 1。
    fn apply_user_dict(jieba: &mut Jieba, text: &str) {
        for raw_line in text.lines() {
            let line = raw_line.trim().trim_start_matches('\u{feff}');
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(word) = parts.next() else { continue };

            let mut freq = DEFAULT_USER_WORD_FREQ;
            let mut tag: Option<&str> = None;
            if let Some(second) = parts.next() {
                match second.parse::<usize>() {
                    Ok(value) => {
                        freq = value;
                        tag = parts.next();
                    }
                    Err(_) => tag = Some(second),
                }
            }

            let _ = jieba.add_word(word, Some(freq.max(1)), tag);
        }
    }

    /// 加载停用词：文件非空时以文件为准，否则回退内置表（对齐原工程 IsStopWord）
    fn load_stop_words() -> HashSet<String> {
        if let Some(path) = crate::paths::find_resource("dict/stop_words.utf8") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                let set: HashSet<String> = text
                    .lines()
                    .map(|line| line.trim().to_string())
                    .filter(|line| !line.is_empty())
                    .collect();
                if !set.is_empty() {
                    return set;
                }
            }
        }
        BUILTIN_STOP_WORDS.iter().map(|s| s.to_string()).collect()
    }

    fn cut(&self, text: &str) -> Vec<String> {
        self.jieba
            .cut(text, true)
            .into_iter()
            .map(|token| token.word.to_string())
            .collect()
    }

    /// 单条弹幕学习（对齐原工程 PushDanmuEvent 的学习段）
    /// 仅舰长参与；5 秒窗口内不重复学习；返回是否发生了学习
    pub fn learn(
        &self,
        mgr: &CheckinManager,
        uid: &str,
        username: &str,
        guard_level: i32,
        content: &str,
        timestamp_secs: i64,
    ) -> bool {
        if guard_level <= 0 {
            return false;
        }

        let mut state = mgr.load_learning(uid);
        // 以上一条被学习弹幕的时间做 5s 节流（时间戳为服务器秒，统一换算为毫秒比较）
        if (timestamp_secs - state.last_danmu_timestamp) * 1000 < LEARN_TIME_WINDOW_MS {
            return false;
        }

        state.last_danmu_timestamp = timestamp_secs;
        state.danmu_history.push((timestamp_secs, content.to_string()));
        if state.danmu_history.len() > MAX_DANMU_HISTORY_SIZE {
            state.danmu_history.remove(0);
        }
        self.extract_keywords(&mut state, content, timestamp_secs * 1000);

        mgr.save_learning(uid, username, &state).is_ok()
    }

    /// 关键词抽取：分词 → 过滤（字节长度 / 停用词 / #标签# 词）→ 词频统计（对齐原工程 ExtractKeywords）
    fn extract_keywords(&self, state: &mut LearningProfile, content: &str, now_ms: i64) {
        let mut hashtag_words: HashSet<String> = HashSet::new();
        for cap in hashtag_regex().captures_iter(content) {
            if let Some(label) = cap.get(1) {
                for word in self.cut(label.as_str()) {
                    if word.len() >= MIN_WORD_BYTES {
                        hashtag_words.insert(word);
                    }
                }
            }
        }

        for word in self.cut(content) {
            if word.len() < MIN_WORD_BYTES {
                continue;
            }
            if self.stop_words.contains(&word) {
                continue;
            }
            if hashtag_words.contains(&word) {
                continue;
            }

            match state.keywords.iter_mut().find(|k| k.word == word) {
                Some(record) => {
                    record.freq += 1;
                    record.ts = now_ms;
                }
                None => {
                    if state.keywords.len() >= MAX_KEYWORDS_COUNT {
                        // 淘汰频次最低的词（与原工程一致）
                        if let Some((idx, _)) = state
                            .keywords
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, k)| k.freq)
                        {
                            state.keywords.remove(idx);
                        }
                    }
                    state.keywords.push(KeywordRecord {
                        word,
                        freq: 1,
                        ts: now_ms,
                    });
                }
            }
        }

        state.keywords.sort_by(|a, b| b.freq.cmp(&a.freq));
    }

    /// 同内容防刷屏：连续相同内容达到阈值后跳过（对齐原工程 ShouldSkipDuplicateContent）
    pub fn should_skip_duplicate(&self, uid: &str, content: &str) -> bool {
        let mut map = self.same_content.lock().unwrap();
        let entry = map
            .entry(uid.to_string())
            .or_insert_with(|| (String::new(), 0));

        if entry.0.is_empty() {
            entry.0 = content.to_string();
            entry.1 = 1;
            return false;
        }
        if entry.0 == content {
            entry.1 += 1;
            if entry.1 >= MAX_SAME_CONTENT_SKIP {
                return true;
            }
        } else {
            entry.0 = content.to_string();
            entry.1 = 1;
        }
        false
    }
}

/// 打卡事件上下文（BuildPrompt 入参）
pub struct CheckinContext<'a> {
    pub username: &'a str,
    pub continuous_days: i32,
    pub cumulative_days: i32,
    pub checkin_date: i32,
    pub last_checkin_date: i32,
    pub profile: &'a LearningProfile,
}

/// 构造 AI 提示词（逐字对齐原工程 CaptainCheckInModule::BuildPrompt）
pub fn build_prompt(ctx: &CheckinContext) -> String {
    let keywords = if ctx.profile.keywords.is_empty() {
        "（暂无发言习惯数据）".to_string()
    } else {
        ctx.profile
            .keywords
            .iter()
            .take(5)
            .map(|k| k.word.clone())
            .collect::<Vec<_>>()
            .join("、")
    };

    let recent_messages = if ctx.profile.danmu_history.is_empty() {
        "（暂无历史发言）".to_string()
    } else {
        ctx.profile
            .danmu_history
            .iter()
            .rev()
            .take(3)
            .map(|(_, content)| content.clone())
            .collect::<Vec<_>>()
            .join("；")
    };

    let last_checkin_info = build_last_checkin_info(ctx.last_checkin_date, ctx.checkin_date);

    format!(
        "用户{}是一位舰长，连续第{}天打卡，累计打卡{}天{}。\n他的发言习惯包含：{}\n最近发言：{}\n请在回复中明确提到用户{}的姓名，用轻松友好且有点皮的语气回复他的打卡，控制在20字以内。\n回复内容需要适合TTS语音播报，避免生僻字和复杂句式。",
        ctx.username,
        ctx.continuous_days,
        ctx.cumulative_days,
        last_checkin_info,
        keywords,
        recent_messages,
        ctx.username
    )
}

/// 上次打卡信息：\"，上次打卡是M月D日\" + 可选\"（N天前）\"
/// 天数按日期差精确计算（覆盖跨月/跨年）；原工程在跨月且非 1 日时恒不显示天数，此处按语义补全
fn build_last_checkin_info(last_checkin_date: i32, checkin_date: i32) -> String {
    if last_checkin_date <= 0 {
        return String::new();
    }
    let last_month = (last_checkin_date % 10000) / 100;
    let last_day = last_checkin_date % 100;
    let mut info = format!("，上次打卡是{}月{}日", last_month, last_day);

    let days = match (
        CheckinManager::int_to_date(last_checkin_date),
        CheckinManager::int_to_date(checkin_date),
    ) {
        (Some(last), Some(cur)) if cur > last => (cur - last).num_days(),
        _ => 0,
    };
    if days > 0 {
        info.push_str(&format!("（{}天前）", days));
    }
    info
}

/// AI 失败兜底文案（逐字对齐原工程 GetFallbackAnswer）
pub fn fallback_answer(username: &str, continuous_days: i32, cumulative_days: i32) -> String {
    format!(
        "{}连续第{}天打卡！累计{}天",
        username, continuous_days, cumulative_days
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_learner() -> CheckinLearner {
        CheckinLearner::from_resources()
    }

    #[test]
    fn test_learn_gate_window_and_history_cap() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let learner = new_learner();

        // 非舰长不学习
        assert!(!learner.learn(&mgr, "u1", "水友1", 0, "太刀真好玩", 1000));
        assert!(mgr.load_learning("u1").danmu_history.is_empty());

        // 舰长学习成功
        assert!(learner.learn(&mgr, "u1", "水友1", 3, "太刀真好玩", 1000));
        let s1 = mgr.load_learning("u1");
        assert_eq!(s1.danmu_history.len(), 1);
        assert_eq!(s1.last_danmu_timestamp, 1000);

        // 5 秒窗口内不重复学习
        assert!(!learner.learn(&mgr, "u1", "水友1", 3, "大剑也很强", 1004));
        assert_eq!(mgr.load_learning("u1").danmu_history.len(), 1);

        // 超过窗口后恢复学习
        assert!(learner.learn(&mgr, "u1", "水友1", 3, "大剑也很强", 1006));
        assert_eq!(mgr.load_learning("u1").danmu_history.len(), 2);

        // 历史条数上限 100（超出丢弃最旧一条）
        for i in 0..120 {
            let ts = 2000 + i * 6;
            learner.learn(&mgr, "u2", "水友2", 1, &format!("第{}条发言", i), ts);
        }
        let s2 = mgr.load_learning("u2");
        assert_eq!(s2.danmu_history.len(), MAX_DANMU_HISTORY_SIZE);
        assert_eq!(s2.danmu_history.first().unwrap().1, "第20条发言");
        println!("[PASS] test_learn_gate_window_and_history_cap passed");
    }

    #[test]
    fn test_stopword_min_length_and_hashtag_exclusion() {
        let mgr = CheckinManager::new_in_memory().unwrap();
        let learner = new_learner();

        // 用户自定义词典（dict/user.dict.utf8）词语命中 + 停用词过滤 + 单字节字符过滤
        assert!(learner.learn(&mgr, "u1", "水友1", 3, "a 的 区块链 云计算", 1000));
        let s = mgr.load_learning("u1");
        let words: Vec<&str> = s.keywords.iter().map(|k| k.word.as_str()).collect();
        assert!(words.contains(&"区块链"), "应包含自定义词典词: {:?}", words);
        assert!(words.contains(&"云计算"), "应包含自定义词典词: {:?}", words);
        assert!(!words.contains(&"的"), "停用词应被过滤: {:?}", words);
        assert!(!words.iter().any(|w| w.len() < MIN_WORD_BYTES), "单字节词应被过滤: {:?}", words);

        // #标签# 中的词在统计时排除
        assert!(learner.learn(&mgr, "u2", "水友2", 3, "#区块链# 云计算", 1000));
        let s2 = mgr.load_learning("u2");
        let words2: Vec<&str> = s2.keywords.iter().map(|k| k.word.as_str()).collect();
        assert!(!words2.contains(&"区块链"), "标签词应被排除: {:?}", words2);
        assert!(words2.contains(&"云计算"));

        // 重复词频次累加（跨学习窗口）
        assert!(learner.learn(&mgr, "u3", "水友3", 3, "云计算 云计算", 1000));
        let s3 = mgr.load_learning("u3");
        let cloud = s3.keywords.iter().find(|k| k.word == "云计算").unwrap();
        assert!(cloud.freq >= 2, "同段内重复词频次应累加: {}", cloud.freq);
        println!("[PASS] test_stopword_min_length_and_hashtag_exclusion passed");
    }

    #[test]
    fn test_same_content_skip() {
        let learner = new_learner();

        assert!(!learner.should_skip_duplicate("u1", "打卡"));
        assert!(!learner.should_skip_duplicate("u1", "打卡"));
        // 第 3 条连续相同内容起跳过
        assert!(learner.should_skip_duplicate("u1", "打卡"));
        assert!(learner.should_skip_duplicate("u1", "打卡"));

        // 内容变化后重置计数
        assert!(!learner.should_skip_duplicate("u1", "优先"));
        assert!(!learner.should_skip_duplicate("u1", "优先"));
        assert!(learner.should_skip_duplicate("u1", "优先"));

        // 用户之间互不影响
        assert!(!learner.should_skip_duplicate("u2", "打卡"));

        // 首次（空内容边界）直接记录，不跳过
        assert!(!learner.should_skip_duplicate("u3", ""));
        assert!(!learner.should_skip_duplicate("u3", ""));
        println!("[PASS] test_same_content_skip passed");
    }

    #[test]
    fn test_build_prompt_and_fallback() {
        let profile = LearningProfile {
            keywords: (1..=6)
                .map(|i| KeywordRecord {
                    word: format!("习惯{}", i),
                    freq: 10 - i,
                    ts: 0,
                })
                .collect(),
            danmu_history: vec![
                (100, "第一条".into()),
                (200, "第二条".into()),
                (300, "第三条".into()),
                (400, "第四条".into()),
            ],
            last_danmu_timestamp: 400,
        };

        let ctx = CheckinContext {
            username: "测试水友",
            continuous_days: 9,
            cumulative_days: 20,
            checkin_date: 20260919,
            last_checkin_date: 20260910,
            profile: &profile,
        };
        let prompt = build_prompt(&ctx);

        // Top5 习惯词 + 近 3 条发言（倒序、分号连接）
        assert!(prompt.contains("习惯1、习惯2、习惯3、习惯4、习惯5"), "{}", prompt);
        assert!(!prompt.contains("习惯6"), "仅取 Top5: {}", prompt);
        assert!(prompt.contains("最近发言：第四条；第三条；第二条"), "{}", prompt);
        assert!(prompt.contains("上次打卡是9月10日（9天前）"), "{}", prompt);
        assert!(prompt.contains("连续第9天打卡，累计打卡20天"), "{}", prompt);
        assert!(prompt.contains("控制在20字以内"), "{}", prompt);

        // 跨月边界（8/31 → 9/1 = 1 天）
        let cross_month = CheckinContext {
            username: "测试水友",
            continuous_days: 1,
            cumulative_days: 1,
            checkin_date: 20260901,
            last_checkin_date: 20260831,
            profile: &profile,
        };
        assert!(build_prompt(&cross_month).contains("（1天前）"));

        // 跨年边界（2025-12-31 → 2026-01-01 = 1 天）
        let cross_year = CheckinContext {
            username: "测试水友",
            continuous_days: 1,
            cumulative_days: 1,
            checkin_date: 20260101,
            last_checkin_date: 20251231,
            profile: &profile,
        };
        assert!(build_prompt(&cross_year).contains("（1天前）"));

        // 首次打卡（无历史）不显示上次打卡信息；无习惯数据/无发言时使用占位文案
        let empty_profile = LearningProfile::default();
        let first = CheckinContext {
            username: "新舰长",
            continuous_days: 1,
            cumulative_days: 1,
            checkin_date: 20260919,
            last_checkin_date: 0,
            profile: &empty_profile,
        };
        let first_prompt = build_prompt(&first);
        assert!(!first_prompt.contains("上次打卡是"), "{}", first_prompt);
        assert!(first_prompt.contains("（暂无发言习惯数据）") && first_prompt.contains("（暂无历史发言）"));

        // 兜底文案
        assert_eq!(
            fallback_answer("测试水友", 9, 20),
            "测试水友连续第9天打卡！累计20天"
        );
        println!("[PASS] test_build_prompt_and_fallback passed");
    }

    #[test]
    fn test_learning_profile_json_format_matches_legacy() {
        // 落库 JSON 必须与原工程 ProfileManager::KeywordsToJson / DanmuHistoryToJson 完全同格式，
        // 保证旧库（captain_profiles.db）中的历史学习数据可直接反序列化
        let profile = LearningProfile {
            keywords: vec![KeywordRecord {
                word: "区块链".into(),
                freq: 1,
                ts: 1000,
            }],
            danmu_history: vec![(1000, "区块链 云计算".into())],
            last_danmu_timestamp: 1000,
        };

        assert_eq!(
            serde_json::to_string(&profile.keywords).unwrap(),
            r#"[{"word":"区块链","freq":1,"ts":1000}]"#
        );
        assert_eq!(
            serde_json::to_string(&profile.danmu_history).unwrap(),
            r#"[[1000,"区块链 云计算"]]"#
        );

        // 反序列化旧库格式
        let parsed: Vec<KeywordRecord> =
            serde_json::from_str(r#"[{"word":"太刀","freq":3,"ts":123}]"#).unwrap();
        assert_eq!(parsed[0].word, "太刀");
        assert_eq!(parsed[0].freq, 3);
        assert_eq!(parsed[0].ts, 123);
        println!("[PASS] test_learning_profile_json_format_matches_legacy passed");
    }
}
