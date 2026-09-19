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

/// AI 悬浮气泡状态负载
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AIBubblePayload {
    pub username: String,
    pub prompt: String,
    pub reasoning: String,
    pub answer: String,
    pub is_thinking: bool,
}

/// DeepSeek-v4-flash 思考模式客户端
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
            model: "deepseek-v4-flash".to_string(),
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
    /// model: deepseek-v4-flash
    /// thinking: {"type": "enabled"}
    /// reasoning_effort: "low"
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
            "reasoning_effort": "low",
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

        assert_eq!(body["model"], "deepseek-v4-flash");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        println!("[PASS] test_deepseek_request_body_thinking_mode passed");
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
