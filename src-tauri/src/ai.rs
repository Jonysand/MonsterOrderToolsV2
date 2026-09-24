use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// AI 交互请求参数
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AIChatRequest {
    pub prompt: String,
    pub username: String,
    #[serde(default)]
    pub system_prompt: Option<String>,
}

/// 打卡回复系统提示词：随从猫人设 + 播报硬约束（回复会进 TTS 队列，必须是纯口语短句）
pub const SYSTEM_PROMPT_CHECKIN: &str = concat!(
    "你是直播间里的「随从猫」：一只跟着主播混迹《怪物猎人：荒野》的猫，机灵、话痨、爱贫嘴，",
    "偶尔自嘲翻车，嘴上傲娇但真心捧场，说话口语化、轻松诙谐。\n",
    "现在的任务是给刚打卡的舰长喊一句捧场话：\n",
    "1. 必须喊出舰长名字，一句话讲完，不超过 20 字。\n",
    "2. 只夸不损：可以俏皮、可以拿猫的身份自嘲，但不调侃身体、外貌、隐私，不阴阳怪气。\n",
    "3. 纯口语，像直播里脱口而出的一句话；不要表情符号、颜文字、动作描写（例如「（摇尾巴）」）",
    "和生僻字，方便语音播报。",
);

/// DeepSeek 思考模式客户端（模型 deepseek-flash）
pub struct DeepSeekAIChatProvider {
    pub api_key: Mutex<String>,
    pub endpoint: String,
    pub model: String,
}

impl Default for DeepSeekAIChatProvider {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl DeepSeekAIChatProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key: Mutex::new(api_key),
            endpoint: "https://api.deepseek.com/chat/completions".to_string(),
            model: "deepseek-flash".to_string(),
        }
    }

    pub fn set_api_key(&self, key: String) {
        let mut k = self.api_key.lock().unwrap();
        *k = key;
    }

    /// 是否已配置 API Key（未配置时调用方应直接走兜底文案，不做无谓网络请求）
    pub fn is_configured(&self) -> bool {
        !self.api_key.lock().unwrap().trim().is_empty()
    }

    /// 构造依据 DeepSeek 官方规范《思考模式》的请求体：
    /// model: deepseek-flash（2026-09-10 随 V4.1-Flash 发布改为此名，旧名 deepseek-v4-flash 已下线）
    /// thinking: {"type": "enabled"}
    /// reasoning_effort: "high"（官方取值 low/high/max；思考模式默认即开启且默认 high）
    pub fn build_request_body(&self, prompt: &str, system_prompt: Option<&str>) -> serde_json::Value {
        let mut messages = Vec::new();
        if let Some(sys) = system_prompt {
            messages.push(serde_json::json!({
                "role": "system",
                "content": sys
            }));
        }
        messages.push(serde_json::json!({
            "role": "user",
            "content": prompt
        }));

        serde_json::json!({
            "model": self.model,
            "thinking": {
                "type": "enabled"
            },
            "reasoning_effort": "high",
            "messages": messages
        })
    }

    /// 解析 DeepSeek 响应，优先提取 content，次优提取 reasoning_content
    pub fn parse_response(&self, response_json: &serde_json::Value) -> Result<(String, String), String> {
        let choices = response_json
            .get("choices")
            .and_then(|c| c.as_array())
            .ok_or_else(|| "No choices array in DeepSeek response".to_string())?;

        if choices.is_empty() {
            return Err("Empty choices in response".to_string());
        }

        let message = choices[0]
            .get("message")
            .ok_or_else(|| "Missing message in choice".to_string())?;

        let reasoning = message
            .get("reasoning_content")
            .and_then(|r| r.as_str())
            .unwrap_or_default()
            .to_string();

        let content = message
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();

        let final_answer = if !content.trim().is_empty() {
            content
        } else if !reasoning.trim().is_empty() {
            reasoning.clone()
        } else {
            return Err("Both content and reasoning_content are empty".to_string());
        };

        Ok((final_answer, reasoning))
    }

    /// 同步/异步发起思考模式调用
    pub async fn call_api(&self, prompt: &str, system_prompt: Option<&str>) -> Result<(String, String), String> {
        let key = self.api_key.lock().unwrap().clone();
        if key.trim().is_empty() {
            return Err("DeepSeek API key is empty".to_string());
        }

        let body = self.build_request_body(prompt, system_prompt);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;

        let resp = client
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("HTTP request error: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("DeepSeek API error HTTP {}: {}", status, text));
        }

        let json_val: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {}", e))?;

        self.parse_response(&json_val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deepseek_request_body_thinking_mode() {
        let provider = DeepSeekAIChatProvider::new("test_key".into());
        let body = provider.build_request_body("怪猎荒野大剑怎么配装？", Some("你是怪猎荒野AI助手"));

        assert_eq!(body["model"], "deepseek-flash");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        println!("[PASS] test_deepseek_request_body_thinking_mode passed");
    }

    #[test]
    fn test_checkin_system_prompt_persona_and_constraints() {
        // 打卡回复：随从猫人设 + 播报硬约束
        assert!(SYSTEM_PROMPT_CHECKIN.contains("随从猫"), "{}", SYSTEM_PROMPT_CHECKIN);
        assert!(SYSTEM_PROMPT_CHECKIN.contains("舰长名字"));
        assert!(SYSTEM_PROMPT_CHECKIN.contains("20 字"));
        assert!(SYSTEM_PROMPT_CHECKIN.contains("只夸不损"));
        assert!(SYSTEM_PROMPT_CHECKIN.contains("语音播报"));
        assert!(SYSTEM_PROMPT_CHECKIN.contains("表情符号"));
        assert!(SYSTEM_PROMPT_CHECKIN.contains("动作描写"));

        // 系统提示词确实进入请求体，且排在 user 消息之前
        let provider = DeepSeekAIChatProvider::new("test_key".into());
        let body = provider.build_request_body("打卡啦", Some(SYSTEM_PROMPT_CHECKIN));
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], SYSTEM_PROMPT_CHECKIN);
        assert_eq!(body["messages"][1]["role"], "user");
        println!("[PASS] test_checkin_system_prompt_persona_and_constraints passed");
    }

    #[test]
    fn test_deepseek_response_parsing() {
        let provider = DeepSeekAIChatProvider::new("test_key".into());

        // 包含 content 和 reasoning_content
        let mock_resp = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "reasoning_content": "分析荒野大剑核心技能...",
                    "content": "推荐集中3、弱点特效3、超会心3。"
                }
            }]
        });

        let (answer, reasoning) = provider.parse_response(&mock_resp).unwrap();
        assert_eq!(answer, "推荐集中3、弱点特效3、超会心3。");
        assert_eq!(reasoning, "分析荒野大剑核心技能...");

        // 仅包含 reasoning_content 时兜底
        let mock_reasoning_only = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "reasoning_content": "思考中：大剑当前主流拔刀会心...",
                    "content": ""
                }
            }]
        });
        let (ans2, _) = provider.parse_response(&mock_reasoning_only).unwrap();
        assert_eq!(ans2, "思考中：大剑当前主流拔刀会心...");
        println!("[PASS] test_deepseek_response_parsing passed");
    }
}
