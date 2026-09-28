//! 手动预览工具：用真实打卡库数据渲染打卡提示词，并**实际调用一次** DeepSeek 看真实回复。
//!
//! 调优提示词时用它替代「打包启动应用 + 真开播」的验证方式：数据取自真实库，
//! 提示词与请求体走生产同一条代码路径（`checkin_ai::build_prompt` /
//! `ai::DeepSeekAIChatProvider::call_api`），因此看到的回复就是线上会播报的内容。
//!
//! 日常 `cargo test` 不执行（`#[ignore]`），需要真实库与 API Key，手动运行：
//!
//! ```bash
//! PREVIEW_DB=/tmp/mo_preview/A/captain_profiles.db \
//! PREVIEW_UID=<uid> \
//! PREVIEW_CRED="$PWD/scripts/credentials.json" \
//! cargo test --manifest-path src-tauri/Cargo.toml --test preview_checkin_reply -- --ignored --nocapture
//! ```
//!
//! 注意：
//! - 集成测试的工作目录是包根 `src-tauri/`，`PREVIEW_DB` / `PREVIEW_CRED` 请传绝对路径；
//! - 会向 `PREVIEW_DB` **写入一次真实打卡记录**（与线上同路径 `record_checkin_with_flag`），
//!   因此务必传库副本，不要指向正在使用的库；同一副本不可重复运行
//!   （第二次会命中 `already_checked_in`，而线上该分支不调 AI）；
//! - 省略 `PREVIEW_CRED` 时只渲染提示词、不发网络请求（无 Key 也能做提示词 diff）；
//! - API Key 只从凭据文件读取，不打印、不落日志。

use mhdanmutoolsv2_lib::{ai, checkin, checkin_ai};

#[test]
#[ignore = "手动预览工具：需要真实打卡库与网络，日常 CI 不跑"]
fn preview_checkin_reply_from_real_db() {
    let db_path = std::env::var("PREVIEW_DB").expect("需要 PREVIEW_DB 指向打卡库副本");
    let uid = std::env::var("PREVIEW_UID").expect("需要 PREVIEW_UID");

    let mgr = checkin::CheckinManager::new(Some(std::path::Path::new(&db_path)))
        .expect("打开打卡库失败");

    // 取值顺序与线上一致：先读「上次打卡日期」，再走真实的落库路径
    // （连续/累计天数由 checkin_records 明细重算，不能自行推算）
    let before = mgr.get_profile(&uid).expect("读取档案失败");
    let today = chrono::Local::now().date_naive();
    let outcome = mgr
        .record_checkin_with_flag(&uid, &before.username, today)
        .expect("打卡落库失败");

    println!(
        "\n=====档案=====\n昵称：{}\n连续：{} 天\n累计：{} 天\n上次打卡（落库前）：{}\n今日日期：{}\nalready_checked_in：{}",
        outcome.profile.username,
        outcome.profile.continuous_days,
        outcome.profile.cumulative_days,
        before.last_checkin_date,
        checkin::CheckinManager::date_to_int(today),
        outcome.already_checked_in
    );

    let learning = mgr.load_learning(&uid);
    println!(
        "\n=====学习档案=====\n关键词：{} 个\n发言历史：{} 条",
        learning.keywords.len(),
        learning.danmu_history.len()
    );

    let prompt = checkin_ai::build_prompt(&checkin_ai::CheckinContext {
        username: &outcome.profile.username,
        continuous_days: outcome.profile.continuous_days,
        cumulative_days: outcome.profile.cumulative_days,
        checkin_date: checkin::CheckinManager::date_to_int(today),
        last_checkin_date: before.last_checkin_date,
        profile: &learning,
    });

    println!(
        "\n=====SYSTEM=====\n{}\n=====USER=====\n{}\n=====END=====",
        ai::SYSTEM_PROMPT_CHECKIN,
        prompt
    );
    println!("\n用户消息字符数：{}", prompt.chars().count());

    // 请求体走生产同一构造器，便于核对实际发给服务端的参数（thinking / reasoning_effort 等）
    {
        let probe = ai::DeepSeekAIChatProvider::new(String::new());
        let body = probe.build_request_body(&prompt, Some(ai::SYSTEM_PROMPT_CHECKIN));
        println!(
            "\n=====REQUEST BODY（密钥不在其中）=====\n{}",
            serde_json::to_string_pretty(&body).expect("请求体序列化失败")
        );
    }

    let Ok(cred_path) = std::env::var("PREVIEW_CRED") else {
        println!("\n[跳过网络调用] 未设置 PREVIEW_CRED，仅渲染提示词");
        return;
    };

    let raw = std::fs::read_to_string(&cred_path).expect("读取凭据文件失败");
    let creds: serde_json::Value = serde_json::from_str(&raw).expect("凭据 JSON 解析失败");
    let key = creds["chat_api_key"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    assert!(!key.is_empty(), "凭据里 chat_api_key 为空，无法发起调用");

    let provider = ai::DeepSeekAIChatProvider::new(key);
    let runtime = tokio::runtime::Runtime::new().expect("创建 tokio 运行时失败");
    let started = std::time::Instant::now();
    let result = runtime.block_on(provider.call_api(&prompt, Some(ai::SYSTEM_PROMPT_CHECKIN)));
    let elapsed = started.elapsed().as_secs_f64();

    match result {
        Ok((answer, reasoning)) => {
            println!(
                "\n=====REPLY（{:.1}s，{} 字）=====\n{}\n=====END=====",
                elapsed,
                answer.chars().count(),
                answer
            );
            let thought: String = reasoning.chars().take(500).collect();
            println!("\n=====REASONING（截断 500 字）=====\n{}\n=====END=====", thought);
        }
        Err(err) => panic!("调用失败（{:.1}s）：{}", elapsed, err),
    }
}
