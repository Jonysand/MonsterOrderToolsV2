use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use md5::{Digest as Md5Digest, Md5};
use rand::Rng;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

type HmacSha256 = Hmac<Sha256>;

/// 特殊管理员用户 OpenID（对应原项目 SpecialUserHelper.h，永远自动赋权总督）
pub const SPECIAL_OPEN_ID: &str = "6ed4fb45ecd94f938a2cf747c5487707";

/// B站开放平台凭据
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BiliCredentials {
    pub app_id: String,
    pub access_key_id: String,
    pub access_key_secret: String,
    pub id_code: String,
}

impl BiliCredentials {
    /// 计算开放平台 API 签名 Header
    pub fn generate_signed_headers(&self, body: &str) -> BTreeMap<String, String> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();

        let mut rng = rand::thread_rng();
        let nonce: String = format!("{}{}", rng.gen_range(1..100000), ts);

        // 1. MD5 of JSON body
        let mut hasher = Md5::new();
        hasher.update(body.as_bytes());
        let md5_bytes = hasher.finalize();
        let content_md5 = hex::encode(md5_bytes);

        // 2. 构造待签名 header 映射
        let mut headers = BTreeMap::new();
        headers.insert("x-bili-accesskeyid".to_string(), self.access_key_id.clone());
        headers.insert("x-bili-content-md5".to_string(), content_md5);
        headers.insert("x-bili-signature-method".to_string(), "HMAC-SHA256".to_string());
        headers.insert("x-bili-signature-nonce".to_string(), nonce);
        headers.insert("x-bili-signature-version".to_string(), "1.0".to_string());
        headers.insert("x-bili-timestamp".to_string(), ts);

        // 3. 字典序拼接待签名字符串
        let mut header_str = String::new();
        for (k, v) in &headers {
            header_str.push_str(&format!("{}:{}\n", k, v));
        }
        if header_str.ends_with('\n') {
            header_str.pop();
        }

        // 4. HMAC-SHA256 计算签名
        let mut mac = HmacSha256::new_from_slice(self.access_key_secret.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(header_str.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());

        // 5. 组装最终请求头
        headers.insert("Authorization".to_string(), signature);
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        // 开放平台强制校验 Accept，缺失时 start/heartbeat/end 一律返回 code 4013
        headers.insert("Accept".to_string(), "application/json".to_string());

        headers
    }

    /// 校验必要字段是否完整
    pub fn is_valid(&self) -> bool {
        !self.app_id.trim().is_empty()
            && !self.access_key_id.trim().is_empty()
            && !self.access_key_secret.trim().is_empty()
            && !self.id_code.trim().is_empty()
    }

    /// 调用 B 站直播开放平台 start 接口开启互动应用
    /// 返回 (game_id, wss_links, auth_body)
    pub async fn start_app(&self) -> Result<(String, Vec<String>, String), StartAppError> {
        if !self.is_valid() {
            return Err(StartAppError::AuthFailed(
                "B站开放平台凭证不完整（app_id, access_key, secret, id_code 均不可为空）".into(),
            ));
        }

        let app_id_num: serde_json::Value = self
            .app_id
            .trim()
            .parse::<i64>()
            .map(serde_json::Value::from)
            .unwrap_or_else(|_| serde_json::Value::String(self.app_id.clone()));

        let body = serde_json::json!({
            "code": self.id_code.trim(),
            "app_id": app_id_num
        })
        .to_string();

        let headers_map = self.generate_signed_headers(&body);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| StartAppError::Other(e.to_string()))?;

        let mut req = client.post("https://live-open.biliapi.com/v2/app/start");
        for (k, v) in headers_map {
            req = req.header(k, v);
        }

        let resp = req
            .body(body)
            .send()
            .await
            .map_err(|e| StartAppError::Other(format!("B站开放平台 start 接口请求失败: {}", e)))?;

        let http_status = resp.status().as_u16();
        let json_val: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| StartAppError::Other(format!("解析 start 响应 JSON 失败: {}", e)))?;

        let code = json_val.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = json_val
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("未知接口错误");
            let text = format!("B站开播失败 (code {}): {}", code, msg);
            return Err(classify_start_error(http_status, code, text));
        }

        let game_id = json_val
            .pointer("/data/game_info/game_id")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string();

        let wss_links: Vec<String> = json_val
            .pointer("/data/websocket_info/wss_link")
            .and_then(|arr| arr.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let auth_body = json_val
            .pointer("/data/websocket_info/auth_body")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string();

        if wss_links.is_empty() || auth_body.is_empty() {
            return Err(StartAppError::Other(
                "B站开放平台返回的 wss_link 或 auth_body 为空".into(),
            ));
        }

        Ok((game_id, wss_links, auth_body))
    }

    /// 解析心跳/接口响应中的业务 code。
    ///
    /// 缺 `code`、`code` 非整数、或超出 `i32` 范围都视为**无效响应**而不是成功：
    /// 早期实现用 `unwrap_or(0)`，于是 HTTP 200 的 `{}`、`{"code":"bad"}` 或越界值
    /// 都会被当成"心跳正常"，把已经失效的会话一直藏起来。
    pub fn parse_business_code(val: &serde_json::Value) -> Result<i32, String> {
        let raw = val
            .get("code")
            .ok_or_else(|| "响应缺少 code 字段".to_string())?;
        let n = raw
            .as_i64()
            .ok_or_else(|| "响应 code 不是整数".to_string())?;
        i32::try_from(n).map_err(|_| format!("响应 code 超出 i32 范围: {}", n))
    }

    /// 发送应用心跳 POST /v2/app/heartbeat
    /// 返回响应体的业务 code（对齐原工程 BliveManager::OnReceiveHeartbeatResponse 的判定：
    /// 0 / 4004 视为正常，其它 code 说明会话已失效需重连）
    pub async fn heartbeat_app(&self, game_id: &str) -> Result<i32, String> {
        let body = serde_json::json!({ "game_id": game_id }).to_string();
        let headers_map = self.generate_signed_headers(&body);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| e.to_string())?;

        let mut req = client.post("https://live-open.biliapi.com/v2/app/heartbeat");
        for (k, v) in headers_map {
            req = req.header(k, v);
        }

        let resp = req
            .body(body)
            .send()
            .await
            .map_err(|e| format!("心跳请求失败: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("心跳 HTTP 状态异常: {}", resp.status()));
        }

        let val: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("心跳响应解析失败: {}", e))?;
        Self::parse_business_code(&val)
    }

    /// 停止并关闭互动应用 POST /v2/app/end
    ///
    /// 只有**明确证实**关闭成功才返回 `Ok`：网络错误、非 2xx、响应体无有效成功码
    /// 一律返回 `Err`，由调用方保留可重试的 game_id 并标记"关闭状态未知"。
    /// 早期实现丢弃 `.send()` 结果永远返回成功，会让日志假记"已关闭上一场"。
    pub async fn end_app(&self, game_id: &str) -> Result<(), String> {
        let app_id_num: serde_json::Value = self
            .app_id
            .trim()
            .parse::<i64>()
            .map(serde_json::Value::from)
            .unwrap_or_else(|_| serde_json::Value::String(self.app_id.clone()));

        let body = serde_json::json!({
            "game_id": game_id,
            "app_id": app_id_num
        })
        .to_string();

        let headers_map = self.generate_signed_headers(&body);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| e.to_string())?;

        let mut req = client.post("https://live-open.biliapi.com/v2/app/end");
        for (k, v) in headers_map {
            req = req.header(k, v);
        }

        let resp = req
            .body(body)
            .send()
            .await
            .map_err(|e| format!("下播请求失败: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("下播 HTTP 状态异常: {}", resp.status()));
        }

        let val: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("下播响应解析失败: {}", e))?;
        // 协议成功码为 0；缺 code 或非整数都不得当作成功
        let code = Self::parse_business_code(&val)?;
        if code != 0 {
            return Err(format!("下播被服务端拒绝（code {}）", code));
        }
        Ok(())
    }
}

/// ProtoUtils WebSocket 封包结构体
#[derive(Debug, Clone, PartialEq)]
pub struct Packet {
    pub packet_len: i32,
    pub header_len: i16,
    pub ver: i16,
    pub op: i32,
    pub seq: i32,
    pub body: Vec<u8>,
}

impl Packet {
    pub const OP_HEARTBEAT: i32 = 2;
    pub const OP_HEARTBEAT_REPLY: i32 = 3;
    pub const OP_MESSAGE: i32 = 5;
    pub const OP_AUTH: i32 = 7;
    pub const OP_AUTH_REPLY: i32 = 8;

    /// 服务端下发的正常控制包：鉴权回复(8)，以及自身心跳(2)/鉴权(7) 的回显。
    /// 原工程 `BliveManager.cpp:510-532` 对它们只记日志/排心跳，仅 switch 落 default 才重连；
    /// 若当作未知包处理，start 成功后会在第一个鉴权回复上无限重连，真实弹幕永远进不来。
    pub fn is_ignorable_control(op: i32) -> bool {
        matches!(op, Self::OP_AUTH_REPLY | Self::OP_AUTH | Self::OP_HEARTBEAT)
    }

    /// 构造数据包
    pub fn new(op: i32, body: Vec<u8>) -> Self {
        let header_len: i16 = 16;
        let packet_len = header_len as i32 + body.len() as i32;
        Self {
            packet_len,
            header_len,
            ver: 0,
            op,
            // 原工程 ProtoUtils::Packet 的出站 seq 恒为 0（鉴权包/心跳包均如此）
            seq: 0,
            body,
        }
    }

    /// 编码为网络字节流（大端）
    pub fn pack(&self) -> Vec<u8> {
        let total_len = 16 + self.body.len();
        let mut buf = Vec::with_capacity(total_len);

        buf.extend_from_slice(&(total_len as i32).to_be_bytes());
        buf.extend_from_slice(&self.header_len.to_be_bytes());
        buf.extend_from_slice(&self.ver.to_be_bytes());
        buf.extend_from_slice(&self.op.to_be_bytes());
        buf.extend_from_slice(&self.seq.to_be_bytes());
        buf.extend_from_slice(&self.body);

        buf
    }

    /// 从字节流中解包头部与数据体
    pub fn unpack(buf: &[u8]) -> Result<Self, String> {
        if buf.len() < 16 {
            return Err("Buffer too short for packet header".into());
        }

        let packet_len = i32::from_be_bytes(buf[0..4].try_into().unwrap());
        let header_len = i16::from_be_bytes(buf[4..6].try_into().unwrap());
        let ver = i16::from_be_bytes(buf[6..8].try_into().unwrap());
        let op = i32::from_be_bytes(buf[8..12].try_into().unwrap());
        let seq = i32::from_be_bytes(buf[12..16].try_into().unwrap());

        if header_len != 16 {
            return Err(format!("Invalid header length: {}", header_len));
        }

        if (buf.len() as i32) < packet_len {
            return Err(format!("Incomplete packet: expected {}, got {}", packet_len, buf.len()));
        }

        let body = buf[16..packet_len as usize].to_vec();

        Ok(Self {
            packet_len,
            header_len,
            ver,
            op,
            seq,
            body,
        })
    }

    /// 从字节流中解包所有粘包数据包
    pub fn unpack_all(mut buf: &[u8]) -> Vec<Self> {
        let mut packets = Vec::new();
        while buf.len() >= 16 {
            let packet_len = i32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
            if packet_len < 16 || buf.len() < packet_len {
                break;
            }
            if let Ok(pkt) = Self::unpack(&buf[..packet_len]) {
                packets.push(pkt);
            }
            buf = &buf[packet_len..];
        }
        packets
    }
}

/// 弹幕原始数据
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DanmuData {
    pub user_id: String,
    pub user_name: String,
    pub message: String,
    pub timestamp: i64,
    pub has_medal: bool,
    pub medal_level: i32,
    pub guard_level: i32, // 1=总督, 2=提督, 3=舰长, 0=普通
    pub msg_id: String,
    /// 是否付费礼物（仅礼物通道解析 paid 后置位；DM 通道恒 false）
    pub is_paid_gift: bool,
    /// 原始包是否带齐 History 留档所需的必需字段（粉丝牌佩戴状态 / 舰长等级 / 昵称 / 消息）。
    ///
    /// 旧完整版只在字段齐全时才把原弹幕写入独立 History 队列，缺字段的畸形包不写。
    /// 解析路径按同一判定置位，避免把空值弹幕留档成「 说：」。
    pub has_history_required_fields: bool,
}

/// 直播间事件（SC / 上舰 / 进场），对齐原工程 `HandleSpeekSC/Guard/Enter`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum LiveEvent {
    SuperChat {
        user_id: String,
        uname: String,
        rmb: i32,
        message: String,
    },
    Guard {
        user_id: String,
        uname: String,
        guard_level: i32,
        guard_num: i32,
        guard_unit: String,
    },
    RoomEnter {
        user_id: String,
        uname: String,
    },
}

impl LiveEvent {
    /// 生成播报文案（进场事件不播报，与原工程 HandleSpeekEnter 仅记录历史一致）
    pub fn tts_text(&self) -> Option<String> {
        match self {
            LiveEvent::SuperChat {
                uname,
                rmb,
                message,
                ..
            } => Some(format!(
                "感谢 {} 赠送的{}元SC：{}",
                uname, rmb, message
            )),
            LiveEvent::Guard {
                uname,
                guard_level,
                guard_num,
                guard_unit,
                ..
            } => {
                let guard_name = match guard_level {
                    1 => "总督",
                    2 => "提督",
                    3 => "舰长",
                    _ => return None,
                };
                Some(format!(
                    "感谢 {} 上船{}{}的{}",
                    uname, guard_num, guard_unit, guard_name
                ))
            }
            LiveEvent::RoomEnter { .. } => None,
        }
    }

    /// 事件类型标识（前端事件名后缀）
    pub fn event_name(&self) -> &'static str {
        match self {
            LiveEvent::SuperChat { .. } => "super-chat-received",
            LiveEvent::Guard { .. } => "guard-received",
            LiveEvent::RoomEnter { .. } => "room-enter-received",
        }
    }

    pub fn user_id(&self) -> &str {
        match self {
            LiveEvent::SuperChat { user_id, .. }
            | LiveEvent::Guard { user_id, .. }
            | LiveEvent::RoomEnter { user_id, .. } => user_id,
        }
    }
}

/// 从事件 data 中提取用户标识（open_id 优先，回退 uid）
fn pick_user_id(data: &serde_json::Value) -> String {
    data.get("open_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| data.get("uid").and_then(|v| v.as_i64()).map(|u| u.to_string()))
        .unwrap_or_default()
}

/// 点赞事件（对齐原工程 LikeEvent：uid / uname / msg_id / like_count / timestamp）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LikeEvent {
    pub uid: String,
    pub username: String,
    pub msg_id: String,
    pub like_count: i32,
    /// 服务器时间戳（秒），用于推导点赞日期（sendDate 口径）
    #[serde(default)]
    pub timestamp: i64,
}

/// 解析点赞事件（对齐原工程 DanmuProcessor::ParseLikeJson）
pub fn parse_like_event(data: &serde_json::Value) -> Option<LikeEvent> {
    // 单条点赞风控上限（与原工程 MAX_LIKE_COUNT 一致）
    const MAX_LIKE_COUNT: i32 = 10000;

    let uid = pick_user_id(data);
    if uid.is_empty() {
        // 缺 uid（open_id/uid 皆无）时记录告警后丢弃（对齐 like-event-tracking spec 的可观测要求）。
        // 只记"缺哪个字段"，不回显原始 JSON —— 包里带用户标识与昵称，属业务内容
        crate::log_warn!("[BiliLive] 点赞事件缺少 uid/open_id，已丢弃");
        return None;
    }

    let mut like_count = data
        .get("like_count")
        .and_then(|v| v.as_i64())
        .or_else(|| data.get("click_count").and_then(|v| v.as_i64()))
        .unwrap_or(0) as i32;
    if like_count <= 0 {
        // uid 不写入普通日志（业务标识）；需要定位时按事件计数与时间窗口排查
        crate::log_warn!("[BiliLive] 点赞事件 like_count<=0，已丢弃");
        return None;
    }
    if like_count > MAX_LIKE_COUNT {
        like_count = MAX_LIKE_COUNT;
    }

    Some(LikeEvent {
        uid,
        username: data
            .get("uname")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        msg_id: data
            .get("msg_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        like_count,
        timestamp: data
            .get("timestamp")
            .and_then(|v| v.as_i64())
            .or_else(|| data.get("send_time").and_then(|v| v.as_i64()))
            .unwrap_or(0),
    })
}

/// 服务器时间戳（秒）→ 本地日期（对齐原工程 sendDate 口径：localtime(serverTimestamp)）
pub fn server_date(timestamp_secs: i64) -> Option<chrono::NaiveDate> {
    if timestamp_secs <= 0 {
        return None;
    }
    chrono::DateTime::from_timestamp(timestamp_secs, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).date_naive())
}

/// 解析 SC 事件
pub fn parse_super_chat(data: &serde_json::Value) -> Option<LiveEvent> {
    let uname = data.get("uname")?.as_str()?.to_string();
    let rmb = data
        .get("rmb")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(0) as i32;
    let message = data
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Some(LiveEvent::SuperChat {
        user_id: pick_user_id(data),
        uname,
        rmb,
        message,
    })
}

/// 解析上舰事件
pub fn parse_guard(data: &serde_json::Value) -> Option<LiveEvent> {
    let uname = data
        .get("user_info")
        .and_then(|u| u.get("uname"))
        .and_then(|v| v.as_str())?
        .to_string();
    let guard_level = data.get("guard_level").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    let guard_num = data.get("guard_num").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    let guard_unit = data
        .get("guard_unit")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Some(LiveEvent::Guard {
        user_id: pick_user_id(data),
        uname,
        guard_level,
        guard_num,
        guard_unit,
    })
}

/// 解析进场事件
pub fn parse_room_enter(data: &serde_json::Value) -> Option<LiveEvent> {
    let uname = data.get("uname")?.as_str()?.to_string();
    Some(LiveEvent::RoomEnter {
        user_id: pick_user_id(data),
        uname,
    })
}

/// 解析礼物事件（含 paid / gift_id / 官方 combo_info）
pub fn parse_gift_event(data: &serde_json::Value) -> Option<crate::tts::GiftEvent> {
    let uname = data.get("uname")?.as_str()?.to_string();
    let gift_name = data
        .get("gift_name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if gift_name.is_empty() {
        return None;
    }
    let gift_num = data.get("gift_num").and_then(|v| v.as_i64()).unwrap_or(1) as i32;
    let paid = data.get("paid").and_then(|v| v.as_bool()).unwrap_or(false);
    let gift_id = data
        .get("gift_id")
        .map(|v| {
            v.as_i64()
                .map(|i| i.to_string())
                .or_else(|| v.as_str().map(|s| s.to_string()))
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let combo = data.get("combo_info").and_then(|c| {
        if c.is_null() {
            return None;
        }
        Some(crate::tts::ComboInfo {
            base_num: c.get("combo_base_num").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            count: c.get("combo_count").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            timeout_secs: c.get("combo_timeout").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32,
        })
    });
    Some(crate::tts::GiftEvent {
        open_id: pick_user_id(data),
        gift_id,
        uname,
        gift_name,
        gift_num,
        paid,
        combo,
    })
}

/// 弹幕处理结果（点怪匹配与入队信息）。
/// 注：原工程 `DanmuProcessResult.shouldSpeak/speakText` 为死字段（全工程无消费方），
/// 点怪弹幕的语音播报统一走普通朗读链（见 lib.rs 第 4 节），故此处不保留对应字段。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DanmuProcessResult {
    pub matched: bool,
    pub added_to_queue: bool,
    pub priority_updated: bool,
    pub user_id: String,
    pub user_name: String,
    pub monster_name: String,
    pub tempered_level: i32,
    /// 禁点名单拦截：命中字典但该怪已被禁点（此时不入队，由调用方就地提示）
    pub blocked_by_roster: bool,
}

/// 消息 ID 预留结果 —— 三态去重的载体。
///
/// 弹幕（DM）与点赞（LIKE）共用同一份缓存，但语义不同：
/// - DM：预留后**永久保留**（一条弹幕只处理一次）；
/// - LIKE：处理中保留、提交成功确认保留、**失败则释放** ——
///   否则一次数据库失败会把该事件的重投永久挡在缓存外，合法点赞就真的丢了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgIdReservation {
    /// 空 msg_id：不参与去重，允许合法的多次处理（不得伪造唯一键）
    NoKey,
    /// 首次出现，已预留
    Reserved,
    /// 已存在：重复投递，应丢弃
    Duplicate,
}

/// 10 万条 LRU 消息 ID 去重缓存
pub struct MsgIdCache {
    max_size: usize,
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl MsgIdCache {
    pub fn new(max_size: usize) -> Self {
        Self {
            max_size,
            set: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    /// 尝试预留一个 msg_id。
    ///
    /// 空 ID 返回 [`MsgIdReservation::NoKey`]（调用方照常处理，不做去重）；
    /// 已存在返回 [`MsgIdReservation::Duplicate`]；否则登记并返回 `Reserved`。
    /// 整个过程在一把锁内完成，因此并发同 ID 只可能有一条拿到 `Reserved`。
    pub fn try_reserve(&mut self, msg_id: &str) -> MsgIdReservation {
        if msg_id.is_empty() {
            return MsgIdReservation::NoKey;
        }

        if self.set.contains(msg_id) {
            return MsgIdReservation::Duplicate;
        }

        self.order.push_back(msg_id.to_string());
        self.set.insert(msg_id.to_string());

        while self.order.len() > self.max_size {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }

        MsgIdReservation::Reserved
    }

    /// 撤销预留（处理失败时调用），使同一事件的重投可以重试。
    /// 返回是否确实移除了一个预留。
    pub fn release(&mut self, msg_id: &str) -> bool {
        if msg_id.is_empty() || !self.set.remove(msg_id) {
            return false;
        }
        // order 允许保留墓碑（LRU 淘汰时 set.remove 会返回 false，无副作用）
        true
    }

    pub fn contains(&self, msg_id: &str) -> bool {
        !msg_id.is_empty() && self.set.contains(msg_id)
    }

    /// 检查并添加，若已存在则返回 true（表示重复）。
    /// 供**弹幕**使用：预留即长期保留（对齐原工程"先成功占用 ID 再处理一次"）。
    pub fn check_and_add(&mut self, msg_id: &str) -> bool {
        self.try_reserve(msg_id) == MsgIdReservation::Duplicate
    }
}

/// DanmuProcessor 核心处理器
/// 过滤开关使用原子类型，支持配置保存后运行期热更新
pub struct DanmuProcessor {
    order_patterns: Vec<(Regex, &'static str)>,
    priority_patterns: Vec<Regex>,
    msg_id_cache: Mutex<MsgIdCache>,
    pub only_medal_order: AtomicBool,
}

impl Default for DanmuProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl DanmuProcessor {
    pub fn new() -> Self {
        let order_patterns = vec![
            (Regex::new(r"^点怪").unwrap(), "点怪"),
            (Regex::new(r"^点个").unwrap(), "点个"),
            (Regex::new(r"^点只").unwrap(), "点只"),
            (Regex::new(r"^點怪").unwrap(), "點怪"),
            (Regex::new(r"^點個").unwrap(), "點個"),
            (Regex::new(r"^點隻").unwrap(), "點隻"),
        ];

        let priority_patterns = vec![
            Regex::new(r"优先").unwrap(),
            Regex::new(r"插队").unwrap(),
            Regex::new(r"優先").unwrap(),
            Regex::new(r"插隊").unwrap(),
        ];

        Self {
            order_patterns,
            priority_patterns,
            msg_id_cache: Mutex::new(MsgIdCache::new(100_000)),
            only_medal_order: AtomicBool::new(false),
        }
    }

    /// 运行期热更新过滤开关（配置保存后即时生效，无需重启）。
    /// 播报过滤（仅粉丝牌/舰长等级）不在此列：实际判定直接读 config 锁快照。
    pub fn update_filters(&self, only_medal_order: bool) {
        self.only_medal_order.store(only_medal_order, Ordering::Relaxed);
    }

    /// 非弹幕事件（点赞等）复用同一 msg_id 去重缓存（对齐原工程 DanmuProcessor::IsDuplicateMsgId）
    /// 返回 true 表示重复消息，应丢弃
    pub fn is_duplicate_msg_id(&self, msg_id: &str) -> bool {
        if msg_id.is_empty() {
            return false;
        }
        self.msg_id_cache.lock().unwrap().check_and_add(msg_id)
    }

    /// 预留一个 msg_id（LIKE 通道使用三态语义：预留 → 成功确认 / 失败释放）
    pub fn reserve_msg_id(&self, msg_id: &str) -> MsgIdReservation {
        self.msg_id_cache.lock().unwrap().try_reserve(msg_id)
    }

    /// 释放预留：仅在处理失败时调用，使同一事件的重投能够重试
    pub fn release_msg_id(&self, msg_id: &str) -> bool {
        self.msg_id_cache.lock().unwrap().release(msg_id)
    }

    /// 查询某 msg_id 是否已被占用（仅用于测试与诊断）
    pub fn msg_id_reserved(&self, msg_id: &str) -> bool {
        self.msg_id_cache.lock().unwrap().contains(msg_id)
    }

    /// 文本预处理：过滤空格和逗号
    pub fn normalize_string(&self, input: &str) -> String {
        input.replace(' ', "").replace(',', "").replace('，', "")
    }

    /// 判定是否仅包含“优先”或“插队”关键词（两段式提权判定）
    pub fn is_priority_only_message(&self, text: &str) -> bool {
        let normalized = self.normalize_string(text);
        matches!(normalized.as_str(), "优先" | "插队" | "優先" | "插隊")
    }

    /// 判定文本中是否包含任意优先关键字
    pub fn has_priority_keyword(&self, text: &str) -> bool {
        self.priority_patterns.iter().any(|re| re.is_match(text))
    }

    /// 从 B 站弹幕 JSON 解析 DanmuData
    pub fn parse_danmu_json(&self, raw_json: &str) -> Option<DanmuData> {
        let val: serde_json::Value = serde_json::from_str(raw_json).ok()?;
        let data = if val.get("cmd").and_then(|c| c.as_str()) == Some("LIVE_OPEN_PLATFORM_DM") {
            val.get("data")?
        } else {
            &val
        };

        let user_id = data
            .get("open_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| data.get("uid").and_then(|v| v.as_i64()).map(|u| u.to_string()))
            .unwrap_or_default();

        let user_name = data.get("uname").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let message = data.get("msg").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        // 时间戳缺失时取 0（对齐原工程 DanmuData.timestamp 默认值 0：同秒入队时稳定排在队首，
        // 而不是被伪造成"当前时间"排到队尾）
        let timestamp = data
            .get("timestamp")
            .or_else(|| data.get("send_time"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        let has_medal = data.get("fans_medal_wearing_status").and_then(|v| v.as_bool()).unwrap_or(false);
        // 仅在佩戴粉丝牌时记录等级（对齐原工程 DanmuProcessor.cpp:274-279 的赋值条件）
        let medal_level = if has_medal {
            data.get("fans_medal_level").and_then(|v| v.as_i64()).unwrap_or(0) as i32
        } else {
            0
        };
        let mut guard_level = data.get("guard_level").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        // 特殊管理员 OpenID 永远判定为总督 (guard_level = 1)
        if user_id == SPECIAL_OPEN_ID {
            guard_level = 1;
        }
        let msg_id = data.get("msg_id").and_then(|v| v.as_str()).unwrap_or_default().to_string();

        // History 留档判定：昵称与消息必须非空，且佩戴状态/舰长等级字段必须真实存在。
        // 畸形空值包不得被留档成「 说：」。
        let has_history_required_fields = !user_name.trim().is_empty()
            && !message.trim().is_empty()
            && data.get("fans_medal_wearing_status").is_some()
            && data.get("guard_level").is_some();

        Some(DanmuData {
            user_id,
            user_name,
            message,
            timestamp,
            has_medal,
            medal_level,
            guard_level,
            msg_id,
            is_paid_gift: false,
            has_history_required_fields,
        })
    }

    /// 处理单条弹幕
    pub fn process_danmu(
        &self,
        danmu: &DanmuData,
        monster_matcher: &crate::monster::MonsterDataManager,
        roster: &crate::roster::MonsterRoster,
        queue_mgr: &mut crate::queue::QueueManager,
    ) -> DanmuProcessResult {
        let mut result = DanmuProcessResult {
            user_id: danmu.user_id.clone(),
            user_name: danmu.user_name.clone(),
            ..Default::default()
        };

        // 1. 消息 ID 去重校验
        if !danmu.msg_id.is_empty() {
            let mut cache = self.msg_id_cache.lock().unwrap();
            if cache.check_and_add(&danmu.msg_id) {
                return result;
            }
        }

        // 2. 检查两段式“优先”置前逻辑（已在队列中的舰长水友追加“优先”）
        if self.is_priority_only_message(&danmu.message) {
            if queue_mgr.contains(&danmu.user_id) {
                let updated = queue_mgr.update_priority(&danmu.user_id, danmu.guard_level);
                if updated {
                    result.priority_updated = true;
                    if let Some(item) = queue_mgr.items.iter().find(|i| i.user_id == danmu.user_id) {
                        result.monster_name = item.monster_name.clone();
                        result.tempered_level = item.tempered_level;
                    }
                    return result;
                }
            }
        }

        // 3. 粉丝牌门槛过滤
        if self.only_medal_order.load(Ordering::Relaxed) && !danmu.has_medal {
            return result;
        }

        let normalized = self.normalize_string(&danmu.message);

        // 4. 用户若已在队中：直接拦截重复点单（对齐原工程 DanmuProcessor.cpp:116-125）
        // 注：二次优先置前已在步骤 2 严格按整条精确匹配（is_priority_only_message）处理；
        // 句中包含“优先”等字样的常态聊天不再误提权（对齐原工程 v43 缺陷修复）
        if queue_mgr.contains(&danmu.user_id) {
            return result;
        }

        // 5. 正则前缀点怪匹配
        for (pattern, _keyword) in &self.order_patterns {
            if let Some(mat) = pattern.find(&normalized) {
                let mut substring = normalized[mat.end()..].to_string();

                // 移除优先/插队关键字。
                // 注：原工程此处带 guardLevel>0 条件，会导致非舰长带优先词时点怪匹配失败；
                // V2 有意无条件剥离：非舰长仍可正常点怪，只是不置优先。
                for priority_re in &self.priority_patterns {
                    substring = priority_re.replace_all(&substring, "").to_string();
                }

                if let Some(match_res) = monster_matcher.match_monster(&substring) {
                    result.matched = true;
                    result.monster_name = match_res.monster_name.clone();
                    result.tempered_level = match_res.tempered_level;

                    // 5.1 禁点名单拦截：该怪在禁点名单内时直接拒绝（不入队）
                    if roster.is_blocked(&match_res.monster_name) {
                        result.blocked_by_roster = true;
                        break;
                    }

                    let has_priority = self.has_priority_keyword(&normalized) && danmu.guard_level > 0;
                    let item_id = if !danmu.msg_id.is_empty() {
                        format!("bili-{}", danmu.msg_id)
                    } else {
                        format!("bili-{}-{}", danmu.timestamp, danmu.user_id)
                    };
                    let queue_item = crate::queue::QueueItem {
                        id: item_id,
                        user_id: danmu.user_id.clone(),
                        user_name: danmu.user_name.clone(),
                        monster_name: match_res.monster_name.clone(),
                        is_priority: has_priority,
                        guard_level: danmu.guard_level,
                        tempered_level: match_res.tempered_level,
                        timestamp: danmu.timestamp,
                        icon_url: match_res.icon_url,
                    };

                    let added = queue_mgr.add_or_update(queue_item);
                    result.added_to_queue = added;
                    break;
                }
            }
        }

        result
    }
}

/// 指数退避重连状态机
pub struct ExponentialBackoff {
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    pub attempt: u32,
}

/// B站开放平台 `start` 接口失败分类。
/// `AuthFailed` 表示凭证/身份码无效或权限不足，重试无意义（直接进入 ReconnectFailed）；
/// `Other` 为网络类错误，按指数退避持续重连。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartAppError {
    AuthFailed(String),
    Other(String),
}

impl StartAppError {
    pub fn message(&self) -> &str {
        match self {
            StartAppError::AuthFailed(m) | StartAppError::Other(m) => m,
        }
    }
}

impl std::fmt::Display for StartAppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

/// start 接口失败分类（见 [`StartAppError`]）：HTTP 401/403 或签名/鉴权/权限类错误码视为不可重试
fn classify_start_error(http_status: u16, code: i64, text: String) -> StartAppError {
    if http_status == 401
        || http_status == 403
        || code == -400
        || code == 100001
        || text.contains("签名")
        || text.contains("鉴权")
        || text.contains("权限")
    {
        StartAppError::AuthFailed(text)
    } else {
        StartAppError::Other(text)
    }
}

/// 长连五态（对齐原工程 `BliveManager.h` 的 `ConnectionState`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    ReconnectFailed,
}

impl ConnectionState {
    pub fn is_connected(self) -> bool {
        matches!(self, ConnectionState::Connected)
    }

    pub fn text_zh(self) -> &'static str {
        match self {
            ConnectionState::Disconnected => "未连接",
            ConnectionState::Connecting => "连接中...",
            ConnectionState::Connected => "已连接",
            ConnectionState::Reconnecting => "正在重连...",
            ConnectionState::ReconnectFailed => "重连失败",
        }
    }
}

/// 断连原因（对齐原工程 `DisconnectReason`，文案取自 `DisconnectReasonToString`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DisconnectReason {
    None,
    NetworkError,
    HeartbeatTimeout,
    ServerClose,
    AuthFailed,
}

impl DisconnectReason {
    pub fn text_zh(self) -> &'static str {
        match self {
            DisconnectReason::None => "无",
            DisconnectReason::NetworkError => "网络错误",
            DisconnectReason::HeartbeatTimeout => "心跳超时",
            DisconnectReason::ServerClose => "服务器断开",
            DisconnectReason::AuthFailed => "鉴权失败",
        }
    }
}

/// 连接状态快照（内部热状态，`Arc<Mutex<..>>` 持有）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionStatus {
    pub state: ConnectionState,
    pub reason: DisconnectReason,
    /// 已重连次数（`Connecting`/`Connected` 状态下为 0）
    pub attempt: u32,
}

impl Default for ConnectionStatus {
    fn default() -> Self {
        Self {
            state: ConnectionState::Disconnected,
            reason: DisconnectReason::None,
            attempt: 0,
        }
    }
}

/// 前端可读的连接状态载荷（事件 `connection-state-changed` 与命令 `get_bili_connection_state` 共用）
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConnectionStatusPayload {
    pub state: ConnectionState,
    pub reason: DisconnectReason,
    /// 断连原因中文（"无"/"网络错误"/"心跳超时"/"服务器断开"/"鉴权失败"）
    pub reason_text: String,
    /// 已重连次数
    pub attempt: u32,
    /// 界面展示文案（重连中带次数、重连失败带原因）
    pub display: String,
}

impl ConnectionStatus {
    pub fn new(state: ConnectionState, reason: DisconnectReason, attempt: u32) -> Self {
        Self {
            state,
            reason,
            attempt,
        }
    }

    /// 展示文案：重连中带次数、重连失败带原因（其余直接取状态中文）
    pub fn display_text(&self) -> String {
        match self.state {
            ConnectionState::Reconnecting if self.attempt > 0 => {
                format!("正在重连...(第{}次)", self.attempt)
            }
            ConnectionState::ReconnectFailed => {
                format!("重连失败，原因: {}", self.reason.text_zh())
            }
            other => other.text_zh().to_string(),
        }
    }

    /// 断连原因中文文案
    pub fn reason_text_zh(&self) -> &'static str {
        self.reason.text_zh()
    }

    pub fn payload(&self) -> ConnectionStatusPayload {
        ConnectionStatusPayload {
            state: self.state,
            reason: self.reason,
            reason_text: self.reason_text_zh().to_string(),
            attempt: self.attempt,
            display: self.display_text(),
        }
    }
}

impl Default for ExponentialBackoff {
    fn default() -> Self {
        Self::new(1000, 60000)
    }
}

impl ExponentialBackoff {
    pub fn new(base_delay_ms: u64, max_delay_ms: u64) -> Self {
        Self {
            base_delay_ms,
            max_delay_ms,
            attempt: 0,
        }
    }

    /// 获取本次重试延迟并步进
    pub fn next_delay(&mut self) -> u64 {
        // 对齐原工程 BliveManager.cpp:102-107：base × 2^min(attempt, 6)
        let delay = self.base_delay_ms.saturating_mul(1u64 << self.attempt.min(6));
        let capped = delay.min(self.max_delay_ms);
        self.attempt = self.attempt.saturating_add(1);
        capped
    }

    /// 重置退避状态
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// 会话结束状态：把"本地已停止"与"服务端已确认关闭"分开。
///
/// 只有 `Confirmed` 才允许在**同一进程内**直接开始新会话；
/// `Ending` 表示已有调用者取得该次 end 的唯一执行权（后台不得重复 end）；
/// `Unknown` 表示 end 失败/超时，必须保留 game_id 以便受控重试。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEndState {
    /// 有活动会话，尚未请求结束
    Active,
    /// 已有调用者正在执行 end
    Ending,
    /// 服务端已确认关闭
    Confirmed,
    /// end 失败或未执行：服务端关闭状态未知，保留可重试的 game_id
    Unknown,
}

/// B站直播长连服务状态与生命周期管理
pub struct BiliLiveService {
    running: Arc<AtomicBool>,
    game_id: Arc<Mutex<Option<String>>>,
    /// 当前会话的结束状态（与 game_id 同一把锁保护，避免两者读出不一致）
    end_state: Arc<Mutex<SessionEndState>>,
    /// 会话代号：每开一次会话递增。旧循环发现代号变了就自行退出，
    /// 因此不可能出现"旧会话被 running=true 复活"。
    session_generation: Arc<AtomicU64>,
    /// 连接循环是否仍在运行：断开方据此等待"真的停了"，而不是假设已停
    loop_alive: Arc<AtomicBool>,
}

/// 会话取消令牌：交给长连循环，用于判定自己是否已被取消，以及退出时回报。
///
/// 拆出独立结构是为了让"取消"与"退出确认"两件事都可单测，不必真的起网络连接。
#[derive(Clone)]
pub struct SessionCtl {
    generation: Arc<AtomicU64>,
    loop_alive: Arc<AtomicBool>,
    my_generation: u64,
}

impl SessionCtl {
    /// 是否已被取消：用户要求停止，或已有更新的一代会话开始
    pub fn is_cancelled(&self, running: &AtomicBool) -> bool {
        !running.load(Ordering::SeqCst)
            || self.generation.load(Ordering::SeqCst) != self.my_generation
    }

    pub fn my_generation(&self) -> u64 {
        self.my_generation
    }

    /// 可取消的等待：分片 sleep，任意时刻都能被取消打断。
    /// 原实现一次性 `sleep(最长 60s)`，导致断开后旧循环还要空等很久才退出。
    pub async fn cancellable_sleep(&self, running: &AtomicBool, total_ms: u64) {
        const SLICE_MS: u64 = 100;
        let mut left = total_ms;
        while left > 0 {
            if self.is_cancelled(running) {
                return;
            }
            let step = left.min(SLICE_MS);
            tokio::time::sleep(tokio::time::Duration::from_millis(step)).await;
            left -= step;
        }
    }

    /// 循环退出时回报。只有仍是"当前代"时才清标记，
    /// 否则会把刚启动的新会话的 alive 标记误清掉。
    pub fn mark_exited(&self) {
        if self.generation.load(Ordering::SeqCst) == self.my_generation {
            self.loop_alive.store(false, Ordering::SeqCst);
        }
    }
}

impl Default for BiliLiveService {
    fn default() -> Self {
        Self::new()
    }
}

impl BiliLiveService {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            game_id: Arc::new(Mutex::new(None)),
            end_state: Arc::new(Mutex::new(SessionEndState::Active)),
            session_generation: Arc::new(AtomicU64::new(0)),
            loop_alive: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn set_running(&self, val: bool) {
        self.running.store(val, Ordering::SeqCst);
    }

    pub fn get_running_flag(&self) -> Arc<AtomicBool> {
        self.running.clone()
    }

    pub fn set_game_id(&self, gid: Option<String>) {
        let mut g = self.game_id.lock().unwrap();
        *g = gid;
    }

    pub fn get_game_id(&self) -> Option<String> {
        self.game_id.lock().unwrap().clone()
    }

    pub fn get_game_id_ref(&self) -> Arc<Mutex<Option<String>>> {
        self.game_id.clone()
    }

    pub fn end_state(&self) -> SessionEndState {
        self.end_state.lock().unwrap().clone()
    }

    pub fn set_end_state(&self, state: SessionEndState) {
        *self.end_state.lock().unwrap() = state;
    }

    /// 开始一代新会话：标记循环存活并递增代号，返回本次代号。
    /// 旧循环会在下一次取消检查时发现代号已变而退出。
    pub fn begin_session(&self) -> u64 {
        self.loop_alive.store(true, Ordering::SeqCst);
        self.session_generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// 取当前代的取消令牌（必须在 `begin_session` 之后调用）
    pub fn session_ctl(&self) -> SessionCtl {
        SessionCtl {
            generation: self.session_generation.clone(),
            loop_alive: self.loop_alive.clone(),
            my_generation: self.session_generation.load(Ordering::SeqCst),
        }
    }

    /// 连接循环是否仍在运行
    pub fn is_loop_alive(&self) -> bool {
        self.loop_alive.load(Ordering::SeqCst)
    }

    /// 声明"本次 end 由我独占执行"。
    ///
    /// 返回 `Some(game_id)` 时调用者取得了唯一执行权，必须自己把状态推进到
    /// `Confirmed` 或 `Unknown`；已在 `Ending` 时返回 `None`（后台看到后不得重复 end）。
    /// **不 `take()` 掉 game_id**：即便 end 失败，也还留着可重试的 ID。
    pub fn begin_end(&self) -> Option<String> {
        let gid = self.get_game_id()?;
        let mut st = self.end_state.lock().unwrap();
        match *st {
            SessionEndState::Ending => None,
            _ => {
                *st = SessionEndState::Ending;
                Some(gid)
            }
        }
    }

    /// end 成功：清理 ID 并放行新会话
    pub fn confirm_end(&self) {
        self.set_game_id(None);
        self.set_end_state(SessionEndState::Confirmed);
    }

    /// end 失败/超时：保留 game_id 与"关闭状态未知"，禁止在未确认下立即开新会话
    pub fn mark_end_unknown(&self) {
        self.set_end_state(SessionEndState::Unknown);
    }

    /// 是否允许本进程立即开启新会话。
    /// 三个条件都必须满足：服务端关闭状态已确认、没有 end 在进行、**旧连接循环确已退出**。
    pub fn can_start_new_session(&self) -> bool {
        !matches!(self.end_state(), SessionEndState::Unknown | SessionEndState::Ending)
            && !self.is_loop_alive()
    }
}

/// 直播包类型统计的白名单（无法识别的 cmd 统一计入 `other`）。
/// 固定集合保证摘要长度有上限，不会被异常 cmd 撑爆日志。
pub const CMD_TYPE_WHITELIST: [&str; 7] = [
    "LIVE_OPEN_PLATFORM_DM",
    "LIVE_OPEN_PLATFORM_LIKE",
    "LIVE_OPEN_PLATFORM_SEND_GIFT",
    "LIVE_OPEN_PLATFORM_SUPER_CHAT",
    "LIVE_OPEN_PLATFORM_GUARD",
    "LIVE_OPEN_PLATFORM_LIVE_ROOM_ENTER",
    "LIVE_OPEN_PLATFORM_INTERACTION_END",
];

/// 包类型计数器：白名单内按类型计数，其余归 `other`
pub type CmdCounts = std::collections::BTreeMap<&'static str, u64>;

/// 记一次收包（不写日志，只累加）
pub fn record_cmd_type(counts: &mut CmdCounts, cmd: &str) {
    let key = CMD_TYPE_WHITELIST
        .iter()
        .find(|c| **c == cmd)
        .copied()
        .unwrap_or("other");
    *counts.entry(key).or_insert(0) += 1;
}

/// 生成有上限的收包摘要（按固定白名单顺序，未出现的类型也要以 0 呈现，
/// 这样"完全没收到 DM"与"收到 0 条 DM"是同一种可读事实）
pub fn format_cmd_summary(counts: &mut CmdCounts) -> String {
    let mut parts: Vec<String> = Vec::new();
    for t in CMD_TYPE_WHITELIST
        .iter()
        .copied()
        .chain(std::iter::once("other"))
    {
        let n = counts.get(t).copied().unwrap_or(0);
        let short = t
            .strip_prefix("LIVE_OPEN_PLATFORM_")
            .unwrap_or(t)
            .to_ascii_lowercase();
        parts.push(format!("{}={}", short, n));
    }
    counts.clear();
    parts.join(" ")
}

/// 运行 B 站直播开放平台 WebSocket 长连接后台主循环
pub async fn run_bili_live_loop<FDanmu, FLike, FGift, FEvent, FState>(
    creds: BiliCredentials,
    running: Arc<AtomicBool>,
    session: SessionCtl,
    current_game_id: Arc<Mutex<Option<String>>>,
    on_danmu: FDanmu,
    on_like: FLike,
    on_gift: FGift,
    on_event: FEvent,
    on_state_change: FState,
) where
    FDanmu: Fn(DanmuData) + Send + Sync + 'static,
    FLike: Fn(LikeEvent) + Send + Sync + 'static,
    FGift: Fn(crate::tts::GiftEvent) + Send + Sync + 'static,
    FEvent: Fn(LiveEvent) + Send + Sync + 'static,
    FState: Fn(ConnectionState, DisconnectReason, u32) + Send + Sync + 'static,
{
    // 对齐原工程 BliveManager.h:50-51：基数 1s、上限 60s
    let mut backoff = ExponentialBackoff::new(1000, 60000);
    let processor = DanmuProcessor::new();
    // 已重连次数（仅重连路径递增，成功后清零）
    let mut attempt: u32 = 0;

    // 进入即「连接中」（对齐原工程 Start() 的 SetConnectionState(Connecting, None)）
    on_state_change(ConnectionState::Connecting, DisconnectReason::None, 0);

    // L05：收包类型统计与 60 秒摘要节流（每个类型在完整窗口内至多一条统计）
    let mut cmd_counts: CmdCounts = CmdCounts::new();
    let mut last_cmd_summary = std::time::Instant::now();
    const CMD_SUMMARY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

    while !session.is_cancelled(&running) {
        // 0. 重连/重开前先关闭上一场互动应用（对齐原工程 BliveManager.cpp:146-148：
        //    若仍有 currentGameId 则先 End(gameId, restart=true)，否则会命中服务端 7001 请求冷却期）
        {
            let stale = current_game_id.lock().ok().and_then(|mut g| g.take());
            if let Some(old_gid) = stale {
                match creds.end_app(&old_gid).await {
                    Ok(()) => {
                        // 只有明确证实关闭成功才记 INFO，否则会假报"已关闭上一场"
                        crate::log_info!("[BiliLive] 重连前已确认关闭上一场互动应用");
                    }
                    Err(e) => {
                        crate::log_warn!(
                            "[BiliLive] 重连前关闭上一场互动应用未获确认（服务端关闭状态未知）: {}",
                            e
                        );
                    }
                }
            }
        }

        // 1. 调用 start_app 开启互动应用
        let start_res = creds.start_app().await;
        let (game_id, wss_links, auth_body) = match start_res {
            Ok(tuple) => {
                backoff.reset();
                tuple
            }
            Err(StartAppError::AuthFailed(e)) => {
                crate::log_error!("[BiliLive] 开播鉴权失败，停止重连: {}", e);
                running.store(false, Ordering::SeqCst);
                on_state_change(
                    ConnectionState::ReconnectFailed,
                    DisconnectReason::AuthFailed,
                    attempt,
                );
                break;
            }
            Err(StartAppError::Other(e)) => {
                crate::log_error!("[BiliLive] start_app 失败: {}", e);
                attempt = attempt.saturating_add(1);
                on_state_change(
                    ConnectionState::Reconnecting,
                    DisconnectReason::NetworkError,
                    attempt,
                );
                let delay = backoff.next_delay();
                // 可取消的退避等待：断开后旧循环不再空等到最长 60s
                session.cancellable_sleep(&running, delay).await;
                continue;
            }
        };

        // 记录 game_id
        {
            let mut gid_lock = current_game_id.lock().unwrap();
            *gid_lock = Some(game_id.clone());
        }

        // 尝试连接 WebSocket
        let mut _ws_connected = false;
        // 本轮断连原因（默认网络错误；由内层循环的具体失败点覆盖）
        let mut last_reason = DisconnectReason::NetworkError;
        for wss_url in &wss_links {
            if !running.load(Ordering::SeqCst) {
                break;
            }

            match connect_async(wss_url).await {
                Ok((ws_stream, _)) => {
                    _ws_connected = true;
                    attempt = 0;
                    on_state_change(ConnectionState::Connected, DisconnectReason::None, 0);

                    let (mut write, mut read) = ws_stream.split();

                    // 2. 发送 OP_AUTH 认证包
                    let auth_packet = Packet::new(Packet::OP_AUTH, auth_body.as_bytes().to_vec());
                    if let Err(e) = write.send(Message::Binary(auth_packet.pack())).await {
                        crate::log_error!("[BiliLive] 发送认证包失败: {}", e);
                        last_reason = DisconnectReason::NetworkError;
                        break;
                    }

                    // 3. 启动定时心跳与数据接收循环
                    //    WS 心跳间隔 20s（对齐原工程 HEARTBEAT_INTERVAL_MINISECONDS=20000）
                    let mut ws_heartbeat_interval = tokio::time::interval(tokio::time::Duration::from_secs(20));
                    let mut app_heartbeat_interval = tokio::time::interval(tokio::time::Duration::from_secs(20));
                    // 首个 WS 心跳延后 2s 发出（对齐原工程收到 OP_AUTH_REPLY 后排 2s 心跳）
                    ws_heartbeat_interval.reset();
                    // 取消轮询：保证断开后旧循环最多 500ms 就退出
                    let mut cancel_tick = tokio::time::interval(tokio::time::Duration::from_millis(500));

                    'ws_loop: loop {
                        if session.is_cancelled(&running) {
                            break;
                        }

                        // 收包摘要与心跳同频检查：到点就输出一次固定格式的统计并清零，
                        // 因此 60 秒窗口内每种类型至多出现一次
                        if last_cmd_summary.elapsed() >= CMD_SUMMARY_INTERVAL {
                            last_cmd_summary = std::time::Instant::now();
                            crate::log_info!("[BiliLive] 直播包统计（60s）: {}", format_cmd_summary(&mut cmd_counts));
                        }

                        tokio::select! {
                            // 取消检查（500ms）：没有这一支时，仅剩 20s 心跳分支，
                            // 断开后旧循环最长要空等 20s 才退出，新会话就被无谓地挡住
                            _ = cancel_tick.tick() => {
                                if session.is_cancelled(&running) {
                                    break 'ws_loop;
                                }
                            }
                            _ = ws_heartbeat_interval.tick() => {
                                let hb_pkt = Packet::new(Packet::OP_HEARTBEAT, vec![]);
                                if let Err(e) = write.send(Message::Binary(hb_pkt.pack())).await {
                                    crate::log_error!("[BiliLive] 发送 WS 心跳包失败: {}", e);
                                    last_reason = DisconnectReason::HeartbeatTimeout;
                                    break;
                                }
                            }
                            _ = app_heartbeat_interval.tick() => {
                                // 应用层心跳需判定响应 code（对齐原工程 OnReceiveHeartbeatResponse）：
                                // 0 / 4004 正常且复位重连计数；其它 code 视为会话失效，触发重连；
                                // HTTP/网络失败按网络错误重连。
                                match creds.heartbeat_app(&game_id).await {
                                    Ok(code) if code == 0 || code == 4004 => {
                                        attempt = 0;
                                        backoff.reset();
                                    }
                                    Ok(code) => {
                                        crate::log_warn!("[BiliLive] 应用心跳返回异常 code={}，触发重连", code);
                                        last_reason = DisconnectReason::HeartbeatTimeout;
                                        break;
                                    }
                                    Err(e) => {
                                        crate::log_warn!("[BiliLive] 应用心跳失败，触发重连: {}", e);
                                        last_reason = DisconnectReason::NetworkError;
                                        break;
                                    }
                                }
                            }
                            msg_opt = read.next() => {
                                match msg_opt {
                                    Some(Ok(Message::Binary(bytes))) => {
                                        let packets = Packet::unpack_all(&bytes);
                                        for pkt in packets {
                                            if pkt.op == Packet::OP_HEARTBEAT_REPLY {
                                                // 收到心跳回复即证明链路存活：复位重连计数（对齐原工程
                                                // BliveManager.cpp:513-518 的 reconnectAttemptCount.store(0)）
                                                attempt = 0;
                                                backoff.reset();
                                                continue;
                                            }
                                            if Packet::is_ignorable_control(pkt.op) {
                                                continue;
                                            }
                                            if pkt.op != Packet::OP_MESSAGE {
                                                // 未知操作码：原工程按网络错误触发重连（BliveManager.cpp:533-537）
                                                crate::log_warn!("[BiliLive] 收到未知操作码 {}，触发重连", pkt.op);
                                                last_reason = DisconnectReason::NetworkError;
                                                break 'ws_loop;
                                            }
                                            let text = match std::str::from_utf8(&pkt.body) {
                                                Ok(t) => t,
                                                Err(e) => {
                                                    crate::log_warn!("[BiliLive] 弹幕包体非 UTF-8，已跳过: {}", e);
                                                    continue;
                                                }
                                            };
                                            let val = match serde_json::from_str::<serde_json::Value>(text) {
                                                Ok(v) => v,
                                                Err(e) => {
                                                    crate::log_warn!("[BiliLive] 弹幕 JSON 解析失败，已跳过: {}", e);
                                                    continue;
                                                }
                                            };
                                            let cmd = val.get("cmd").and_then(|c| c.as_str()).unwrap_or_default();
                                            // L05：按固定白名单统计直播包类型，周期性输出有上限的 INFO 摘要。
                                            // 这样才能在 Release 日志里区分"没收到包"与"收到了但被过滤"，
                                            // 又不必为每条包写文件（也无法识别的类型统一计为 other）
                                            record_cmd_type(&mut cmd_counts, cmd);
                                            if cmd == "LIVE_OPEN_PLATFORM_DM" {
                                                if let Some(danmu) = processor.parse_danmu_json(text) {
                                                    on_danmu(danmu);
                                                }
                                            } else if cmd == "LIVE_OPEN_PLATFORM_LIKE" {
                                                if let Some(data) = val.get("data") {
                                                    if let Some(ev) = parse_like_event(data) {
                                                        if ev.like_count > 0 {
                                                            on_like(ev);
                                                        }
                                                    }
                                                }
                                            } else if cmd == "LIVE_OPEN_PLATFORM_SEND_GIFT" {
                                                if let Some(data) = val.get("data") {
                                                    if let Some(ev) = parse_gift_event(data) {
                                                        if ev.gift_num > 0 {
                                                            on_gift(ev);
                                                        }
                                                    }
                                                }
                                            } else if cmd == "LIVE_OPEN_PLATFORM_SUPER_CHAT" {
                                                if let Some(data) = val.get("data") {
                                                    if let Some(ev) = parse_super_chat(data) {
                                                        on_event(ev);
                                                    }
                                                }
                                            } else if cmd == "LIVE_OPEN_PLATFORM_GUARD" {
                                                if let Some(data) = val.get("data") {
                                                    if let Some(ev) = parse_guard(data) {
                                                        on_event(ev);
                                                    }
                                                }
                                            } else if cmd == "LIVE_OPEN_PLATFORM_LIVE_ROOM_ENTER" {
                                                if let Some(data) = val.get("data") {
                                                    if let Some(ev) = parse_room_enter(data) {
                                                        on_event(ev);
                                                    }
                                                }
                                            } else if cmd == "LIVE_OPEN_PLATFORM_INTERACTION_END" {
                                                // 仅当终止的 game_id 与本场一致时才断线重连（对齐原工程
                                                // BliveManager.cpp:579-595 的 gameId 比对与清理）
                                                let ended_gid = val
                                                    .get("data")
                                                    .and_then(|d| d.get("game_id"))
                                                    .and_then(|g| g.as_str())
                                                    .unwrap_or_default();
                                                if !ended_gid.is_empty() && ended_gid != game_id {
                                                    // 不回显 game_id（会话标识不进普通日志）
                                                    crate::log_warn!(
                                                        "[BiliLive] 收到其它会话的终止包，已忽略"
                                                    );
                                                    continue;
                                                }
                                                crate::log_warn!("[BiliLive] 接收到服务端开播终止包");
                                                if let Ok(mut g) = current_game_id.lock() {
                                                    g.take();
                                                }
                                                last_reason = DisconnectReason::ServerClose;
                                                break 'ws_loop;
                                            }
                                        }
                                    }
                                    Some(Ok(Message::Close(_))) => {
                                        crate::log_warn!("[BiliLive] WebSocket 收到服务端关闭帧");
                                        last_reason = DisconnectReason::ServerClose;
                                        break;
                                    }
                                    Some(Err(e)) => {
                                        crate::log_error!("[BiliLive] WebSocket 接收错误: {}", e);
                                        last_reason = DisconnectReason::NetworkError;
                                        break;
                                    }
                                    None => {
                                        last_reason = DisconnectReason::ServerClose;
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    break;
                }
                Err(e) => {
                    // 不回显 WSS 地址（含鉴权信息）与原始错误全文，只记错误类别
                    crate::log_warn!("[BiliLive] WebSocket 连接失败，将按退避重试: {}", e);
                }
            }
        }

        // 如果用户要求停止，发送 end_app 并回到「未连接」（鉴权失败路径在上方已单独上报，不会走到这里）
        if !running.load(Ordering::SeqCst) {
            let _ = creds.end_app(&game_id).await;
            let mut gid_lock = current_game_id.lock().unwrap();
            *gid_lock = None;
            on_state_change(ConnectionState::Disconnected, DisconnectReason::None, 0);
            break;
        }

        // 否则等待退避时间后重试重连（重连中带次数与原因）
        attempt = attempt.saturating_add(1);
        on_state_change(ConnectionState::Reconnecting, last_reason, attempt);
        let delay = backoff.next_delay();
        session.cancellable_sleep(&running, delay).await;
    }

    // 循环真正结束：回报退出，让断开方可以确认"旧任务已经不在了"
    session.mark_exited();
}

#[cfg(test)]
mod heartbeat_and_session_tests {
    use super::*;

    /// E3：心跳业务码解析 —— 缺 code / 非整数 / 越界都必须是无效响应，不能默认为成功
    #[test]
    fn test_parse_business_code_rejects_invalid_shapes() {
        // 正常
        assert_eq!(
            BiliCredentials::parse_business_code(&serde_json::json!({"code": 0})).unwrap(),
            0
        );
        assert_eq!(
            BiliCredentials::parse_business_code(&serde_json::json!({"code": 4004})).unwrap(),
            4004
        );
        assert_eq!(
            BiliCredentials::parse_business_code(&serde_json::json!({"code": 7001})).unwrap(),
            7001
        );

        // HTTP 200 但响应体是空对象：早期 unwrap_or(0) 会把它当成"心跳正常"
        let err = BiliCredentials::parse_business_code(&serde_json::json!({})).unwrap_err();
        assert!(err.contains("缺少 code"), "{}", err);

        // 字符串 code
        let err =
            BiliCredentials::parse_business_code(&serde_json::json!({"code": "bad"})).unwrap_err();
        assert!(err.contains("不是整数"), "{}", err);

        // 超出 i32 范围：不得截断成看似正常的值
        let err = BiliCredentials::parse_business_code(&serde_json::json!({"code": 4294967296i64}))
            .unwrap_err();
        assert!(err.contains("超出 i32 范围"), "{}", err);

        // null / 布尔 / 数组同样无效
        for bad in [
            serde_json::json!({"code": null}),
            serde_json::json!({"code": true}),
            serde_json::json!({"code": [0]}),
        ] {
            assert!(
                BiliCredentials::parse_business_code(&bad).is_err(),
                "非整数 code 必须判为无效响应: {}",
                bad
            );
        }
        println!("[PASS] test_parse_business_code_rejects_invalid_shapes passed");
    }

    /// E3：会话结束状态机 —— 只有确认关闭才放行新会话；end 失败保留可重试 ID
    #[test]
    fn test_session_end_state_machine() {
        let svc = BiliLiveService::new();
        assert!(svc.can_start_new_session(), "初始状态允许开播");

        svc.set_game_id(Some("gid_1".into()));
        svc.set_end_state(SessionEndState::Active);

        // 第一个调用者取得唯一执行权
        assert_eq!(svc.begin_end().as_deref(), Some("gid_1"));
        assert_eq!(svc.end_state(), SessionEndState::Ending);
        // 后台看到 Ending 时不得重复 end
        assert_eq!(svc.begin_end(), None, "同一会话只允许一次 end");
        assert!(
            !svc.can_start_new_session(),
            "end 进行中不得开新会话"
        );
        // 关键：end 进行中也没有丢掉 game_id
        assert_eq!(svc.get_game_id().as_deref(), Some("gid_1"));

        // end 失败：转入 Unknown，仍保留可重试 ID
        svc.mark_end_unknown();
        assert_eq!(svc.end_state(), SessionEndState::Unknown);
        assert_eq!(svc.get_game_id().as_deref(), Some("gid_1"), "失败后必须保留可重试的 ID");
        assert!(
            !svc.can_start_new_session(),
            "服务端关闭状态未知时不得立即开新会话"
        );

        // 重试成功后才清 ID 并放行
        assert_eq!(svc.begin_end().as_deref(), Some("gid_1"), "Unknown 允许受控重试");
        svc.confirm_end();
        assert_eq!(svc.end_state(), SessionEndState::Confirmed);
        assert_eq!(svc.get_game_id(), None);
        assert!(svc.can_start_new_session());
        println!("[PASS] test_session_end_state_machine passed");
    }

    /// E3：没有会话时 begin_end 不得凭空标记 Ending
    #[test]
    fn test_begin_end_without_session_is_none() {
        let svc = BiliLiveService::new();
        assert_eq!(svc.begin_end(), None);
        assert_eq!(svc.end_state(), SessionEndState::Active);
        println!("[PASS] test_begin_end_without_session_is_none passed");
    }

    // ---------------- E2：会话取消与退出确认 ----------------

    /// 只把 running=false 不算"已停"：循环未回报退出前不得开新会话
    #[test]
    fn test_loop_alive_blocks_new_session_until_exit_reported() {
        let svc = BiliLiveService::new();
        assert!(svc.can_start_new_session(), "初始可开播");
        assert!(!svc.is_loop_alive());

        let gen = svc.begin_session();
        assert_eq!(gen, 1);
        assert!(svc.is_loop_alive(), "开播后循环标记应为存活");

        // 停止：running=false，但循环还没回报退出
        svc.set_running(false);
        svc.set_end_state(SessionEndState::Confirmed);
        assert!(
            !svc.can_start_new_session(),
            "循环尚未退出时不得允许新会话（否则新旧循环会并行）"
        );

        // 循环回报退出后才放行
        svc.session_ctl().mark_exited();
        assert!(!svc.is_loop_alive());
        assert!(svc.can_start_new_session(), "退出确认后应放行");
        println!("[PASS] test_loop_alive_blocks_new_session_until_exit_reported passed");
    }

    /// 代号递增后旧会话立即被判为取消；旧循环退出不得清掉新会话的存活标记
    #[test]
    fn test_generation_cancels_old_session_and_mark_exited_is_guarded() {
        let svc = BiliLiveService::new();
        let running = std::sync::Arc::new(AtomicBool::new(true));

        let gen1 = svc.begin_session();
        let old_ctl = svc.session_ctl();
        assert_eq!(old_ctl.my_generation(), gen1);
        assert!(!old_ctl.is_cancelled(&running), "当前会话未被取消");

        // 开新一代：旧令牌立刻视为已取消（即便 running 还是 true）
        let gen2 = svc.begin_session();
        assert_ne!(gen1, gen2);
        assert!(
            old_ctl.is_cancelled(&running),
            "代号变化必须让旧会话立即被判为取消"
        );
        let new_ctl = svc.session_ctl();
        assert!(!new_ctl.is_cancelled(&running));

        // 旧循环延迟退出：不得把新会话的存活标记清掉
        old_ctl.mark_exited();
        assert!(
            svc.is_loop_alive(),
            "旧一代退出回报不得清除新一代的存活标记"
        );
        new_ctl.mark_exited();
        assert!(!svc.is_loop_alive());
        println!("[PASS] test_generation_cancels_old_session_and_mark_exited_is_guarded passed");
    }

    /// 可取消的退避等待：取消后必须立刻返回，而不是睡满整个退避时长
    #[test]
    fn test_cancellable_sleep_returns_promptly_on_cancel() {
        let svc = BiliLiveService::new();
        let running = std::sync::Arc::new(AtomicBool::new(true));
        svc.begin_session();
        let ctl = svc.session_ctl();

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // 已取消：立即返回
            running.store(false, Ordering::SeqCst);
            let t0 = std::time::Instant::now();
            ctl.cancellable_sleep(&running, 60_000).await;
            assert!(
                t0.elapsed() < std::time::Duration::from_millis(200),
                "取消后不得继续等待，实际 {:?}",
                t0.elapsed()
            );

            // 未取消：等待约等于请求时长（分片 sleep 不应显著跑偏）
            running.store(true, Ordering::SeqCst);
            let t1 = std::time::Instant::now();
            ctl.cancellable_sleep(&running, 300).await;
            let elapsed = t1.elapsed();
            assert!(
                elapsed >= std::time::Duration::from_millis(250),
                "未取消时应等满，实际 {:?}",
                elapsed
            );
            assert!(
                elapsed < std::time::Duration::from_millis(900),
                "不应显著超时，实际 {:?}",
                elapsed
            );
        });
        println!("[PASS] test_cancellable_sleep_returns_promptly_on_cancel passed");
    }

    /// L05：收包类型统计按固定白名单归类，未知 cmd 计入 other，摘要长度有上限
    #[test]
    fn test_cmd_type_counting_and_bounded_summary() {
        let mut counts: CmdCounts = CmdCounts::new();
        for _ in 0..3 {
            record_cmd_type(&mut counts, "LIVE_OPEN_PLATFORM_DM");
        }
        record_cmd_type(&mut counts, "LIVE_OPEN_PLATFORM_LIKE");
        // 未知类型统一归 other，不得为每个异常 cmd 生成一个新键
        for weird in ["SOME_NEW_CMD", "ANOTHER_CMD", ""] {
            record_cmd_type(&mut counts, weird);
        }

        let summary = format_cmd_summary(&mut counts);
        assert!(summary.contains("dm=3"), "{}", summary);
        assert!(summary.contains("like=1"), "{}", summary);
        assert!(summary.contains("other=3"), "{}", summary);
        // 未出现的类型也以 0 呈现：这样才能区分"没收到"与"收到了但被过滤"
        assert!(summary.contains("send_gift=0"), "{}", summary);
        assert_eq!(
            summary.split_whitespace().count(),
            CMD_TYPE_WHITELIST.len() + 1,
            "摘要条目数固定，不会被异常 cmd 撑爆: {}",
            summary
        );
        // 生成摘要即清零，保证 60 秒窗口内每种类型至多一条统计
        assert!(counts.is_empty(), "摘要生成后应清零");
        println!("[PASS] test_cmd_type_counting_and_bounded_summary passed");
    }

    // ---------------- D3：msg_id 三态去重 ----------------

    /// 三态语义：预留 → 重复被拒 → 释放后可重试
    #[test]
    fn test_msg_id_three_state_reserve_release_retry() {
        let mut cache = MsgIdCache::new(100);

        // 首次预留成功
        assert_eq!(cache.try_reserve("m1"), MsgIdReservation::Reserved);
        assert!(cache.contains("m1"));
        // 处理中：并发/重投的同一 ID 被拒
        assert_eq!(cache.try_reserve("m1"), MsgIdReservation::Duplicate);
        // 失败释放后可重试
        assert!(cache.release("m1"), "释放应成功");
        assert!(!cache.contains("m1"));
        assert_eq!(cache.try_reserve("m1"), MsgIdReservation::Reserved);

        // 重复释放无副作用
        assert!(!cache.release("never_reserved"));

        // 空 msg_id：不参与去重，允许合法多次处理
        assert_eq!(cache.try_reserve(""), MsgIdReservation::NoKey);
        assert_eq!(cache.try_reserve(""), MsgIdReservation::NoKey);
        assert!(!cache.release(""), "空 ID 不做释放");
        assert!(!cache.contains(""));
        println!("[PASS] test_msg_id_three_state_reserve_release_retry passed");
    }

    /// 释放后 LRU 淘汰仍安全：墓碑不会让计数错乱或误删新预留
    #[test]
    fn test_msg_id_release_interacts_safely_with_lru_eviction() {
        let mut cache = MsgIdCache::new(2);

        assert_eq!(cache.try_reserve("a"), MsgIdReservation::Reserved);
        assert_eq!(cache.try_reserve("b"), MsgIdReservation::Reserved);
        // 释放 a（order 里留下墓碑）
        assert!(cache.release("a"));
        // 再放 c：容量为 2，b 与 c 应保留，a 的墓碑被淘汰时不得影响 b
        assert_eq!(cache.try_reserve("c"), MsgIdReservation::Reserved);
        assert!(cache.contains("b"), "淘汰墓碑不得误删有效预留");
        assert!(cache.contains("c"));
        assert!(!cache.contains("a"));

        // 已释放的 ID 可以重新预留（这就是"失败可重试"）
        assert_eq!(cache.try_reserve("a"), MsgIdReservation::Reserved);
        println!("[PASS] test_msg_id_release_interacts_safely_with_lru_eviction passed");
    }

    /// 并发同 ID 只能有一条拿到 Reserved（其余全部 Duplicate）
    #[test]
    fn test_concurrent_same_msg_id_only_one_reserved() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        use std::sync::Arc;

        let cache = Arc::new(std::sync::Mutex::new(MsgIdCache::new(1000)));
        let reserved = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for _ in 0..16 {
            let c = cache.clone();
            let r = reserved.clone();
            handles.push(std::thread::spawn(move || {
                if c.lock().unwrap().try_reserve("same_id") == MsgIdReservation::Reserved {
                    r.fetch_add(1, AtomicOrdering::SeqCst);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            reserved.load(AtomicOrdering::SeqCst),
            1,
            "并发同一 msg_id 只允许一条进入处理"
        );
        println!("[PASS] test_concurrent_same_msg_id_only_one_reserved passed");
    }

    /// DM 路径沿用"预留即长期保留"：check_and_add 不释放，一条弹幕只处理一次
    #[test]
    fn test_dm_path_keeps_permanent_reservation() {
        let mut cache = MsgIdCache::new(100);
        assert!(!cache.check_and_add("dm_1"), "首次 DM 不应判重");
        assert!(cache.check_and_add("dm_1"), "同 ID 重投应判重");
        assert!(cache.check_and_add("dm_1"), "DM 预留不会被自动释放");
        // 空 ID 不判重（否则会挡掉合法的多条弹幕）
        assert!(!cache.check_and_add(""));
        assert!(!cache.check_and_add(""));
        println!("[PASS] test_dm_path_keeps_permanent_reservation passed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用名单：默认空名单（不限制任何点怪），路径指向临时目录以免触碰真实配置
    fn test_roster() -> crate::roster::MonsterRoster {
        crate::roster::MonsterRoster::load(Some(
            &std::env::temp_dir()
                .join("mh_test_bilibili_roster")
                .join(crate::roster::ROSTER_FILE_NAME),
        ))
    }

    #[test]
    fn test_connection_status_five_states_and_reasons() {
        // 五态展示文案（对齐原工程 C# SetStatus 与「重连中(N)」诉求）
        assert_eq!(
            ConnectionStatus::new(ConnectionState::Disconnected, DisconnectReason::None, 0).display_text(),
            "未连接"
        );
        assert_eq!(
            ConnectionStatus::new(ConnectionState::Connecting, DisconnectReason::None, 0).display_text(),
            "连接中..."
        );
        let connected =
            ConnectionStatus::new(ConnectionState::Connected, DisconnectReason::None, 0);
        assert_eq!(connected.display_text(), "已连接");
        assert!(connected.state.is_connected());

        let reconnecting =
            ConnectionStatus::new(ConnectionState::Reconnecting, DisconnectReason::HeartbeatTimeout, 3);
        assert_eq!(reconnecting.display_text(), "正在重连...(第3次)");
        assert_eq!(reconnecting.reason_text_zh(), "心跳超时");
        assert!(!reconnecting.state.is_connected());

        let failed =
            ConnectionStatus::new(ConnectionState::ReconnectFailed, DisconnectReason::AuthFailed, 1);
        assert_eq!(failed.display_text(), "重连失败，原因: 鉴权失败");
        assert_eq!(failed.payload().display, "重连失败，原因: 鉴权失败");
        assert_eq!(failed.payload().reason_text, "鉴权失败");

        // 断连原因中文映射（对齐 DisconnectReasonToString）
        assert_eq!(DisconnectReason::None.text_zh(), "无");
        assert_eq!(DisconnectReason::NetworkError.text_zh(), "网络错误");
        assert_eq!(DisconnectReason::ServerClose.text_zh(), "服务器断开");

        // 默认值为未连接
        let def = ConnectionStatus::default();
        assert_eq!(def.state, ConnectionState::Disconnected);
        assert_eq!(def.attempt, 0);
        println!("[PASS] test_connection_status_five_states_and_reasons passed");
    }

    #[test]
    fn test_start_error_classification_stops_retry_on_auth() {
        // 鉴权/权限类：HTTP 401/403、错误码 -400/100001、关键字命中 → AuthFailed（不再重试）
        assert!(matches!(
            classify_start_error(401, 0, "unauthorized".into()),
            StartAppError::AuthFailed(_)
        ));
        assert!(matches!(
            classify_start_error(200, -400, "B站开播失败 (code -400): 请求参数错误".into()),
            StartAppError::AuthFailed(_)
        ));
        assert!(matches!(
            classify_start_error(200, 100001, "B站开播失败 (code 100001): 签名校验失败".into()),
            StartAppError::AuthFailed(_)
        ));
        assert!(matches!(
            classify_start_error(200, 99, "B站开播失败 (code 99): 无开播权限".into()),
            StartAppError::AuthFailed(_)
        ));
        // 网络/未知类：可重试
        let other = classify_start_error(200, 12345, "B站开播失败 (code 12345): 未知接口错误".into());
        assert!(matches!(other, StartAppError::Other(_)));
        assert!(matches!(
            classify_start_error(500, -1, "服务器内部错误".into()),
            StartAppError::Other(_)
        ));

        // 本地凭证不完整同样归为 AuthFailed（重试无意义）
        let creds = BiliCredentials::default();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(creds.start_app()).unwrap_err();
        assert!(matches!(err, StartAppError::AuthFailed(_)), "{:?}", err);
        println!("[PASS] test_start_error_classification_stops_retry_on_auth passed");
    }

    #[test]
    fn test_bili_api_signature() {
        let creds = BiliCredentials {
            app_id: "123456".into(),
            access_key_id: "test_key_id".into(),
            access_key_secret: "test_key_secret".into(),
            id_code: "ABCDEF".into(),
        };

        let body = r#"{"code":"ABCDEF","app_id":123456}"#;
        let headers = creds.generate_signed_headers(body);

        assert!(headers.contains_key("Authorization"));
        assert_eq!(headers.get("x-bili-accesskeyid").unwrap(), "test_key_id");
        assert_eq!(headers.get("x-bili-signature-version").unwrap(), "1.0");
        assert_eq!(headers.get("x-bili-signature-method").unwrap(), "HMAC-SHA256");
        assert!(headers.get("x-bili-content-md5").unwrap().len() == 32);
        assert!(headers.get("Authorization").unwrap().len() == 64);
        // 开放平台缺 Accept 头会直接 code 4013 拒绝，三接口共用同一签名头来源
        assert_eq!(headers.get("Accept").unwrap(), "application/json");
        assert_eq!(headers.get("Content-Type").unwrap(), "application/json");
        println!("[PASS] test_bili_api_signature passed");
    }

    #[test]
    fn test_packet_pack_unpack() {
        let body = b"{\"cmd\":\"LIVE_OPEN_PLATFORM_DM\"}".to_vec();
        let p = Packet::new(Packet::OP_MESSAGE, body.clone());
        let packed = p.pack();

        assert_eq!(packed.len(), 16 + body.len());
        let unpacked = Packet::unpack(&packed).unwrap();
        assert_eq!(unpacked.op, Packet::OP_MESSAGE);
        assert_eq!(unpacked.header_len, 16);
        assert_eq!(unpacked.body, body);
        println!("[PASS] test_packet_pack_unpack passed");
    }

    #[test]
    fn test_control_op_classification() {
        // 真实开播后服务端下发的第一个包就是鉴权回复(op=8)，必须忽略而非触发重连
        assert!(Packet::is_ignorable_control(Packet::OP_AUTH_REPLY));
        assert!(Packet::is_ignorable_control(Packet::OP_AUTH));
        assert!(Packet::is_ignorable_control(Packet::OP_HEARTBEAT));
        // 弹幕(5) 走业务解析、心跳回复(3) 走重连计数复位，都不属于本分支
        assert!(!Packet::is_ignorable_control(Packet::OP_MESSAGE));
        assert!(!Packet::is_ignorable_control(Packet::OP_HEARTBEAT_REPLY));
        // 未知 op 仍按原工程 switch default 触发重连
        assert!(!Packet::is_ignorable_control(9));
        println!("[PASS] test_control_op_classification passed");
    }

    #[test]
    fn test_lru_msg_id_dedup() {
        let mut cache = MsgIdCache::new(3);

        assert!(!cache.check_and_add("m1"));
        assert!(!cache.check_and_add("m2"));
        assert!(!cache.check_and_add("m3"));

        // 重复添加 m2
        assert!(cache.check_and_add("m2"));

        // 加入 m4 导致最老 m1 逐出
        assert!(!cache.check_and_add("m4"));

        // 此时 m1 应已被逐出
        assert!(!cache.check_and_add("m1"));
        println!("[PASS] test_lru_msg_id_dedup passed");
    }

    #[test]
    fn test_danmu_processor_flow() {
        let mut matcher = crate::monster::MonsterDataManager::new();
        let _ = matcher.load_from_file(None);
        let processor = DanmuProcessor::new();
        let mut queue_mgr = crate::queue::QueueManager::new();
        let roster = test_roster();

        // 1. 点单弹幕
        let dm1 = DanmuData {
            user_id: "user_001".into(),
            user_name: "猎人甲".into(),
            message: "点怪霸主太太".into(),
            timestamp: 1000,
            has_medal: true,
            medal_level: 10,
            guard_level: 3, // 舰长
            msg_id: "msg_1".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };

        let res1 = processor.process_danmu(&dm1, &matcher, &roster, &mut queue_mgr);
        assert!(res1.matched);
        assert_eq!(res1.monster_name, "霸主雌火龙");
        assert_eq!(queue_mgr.items.len(), 1);
        assert!(!queue_mgr.items[0].is_priority);

        // 2. 在队舰长发送包含“优先”字样的常态聊天（句中含词）：断言不提权（v43 缺陷对齐）
        let dm_chat = DanmuData {
            user_id: "user_001".into(),
            user_name: "猎人甲".into(),
            message: "主播优先打哪个怪呀".into(),
            timestamp: 1002,
            has_medal: true,
            medal_level: 10,
            guard_level: 3,
            msg_id: "msg_chat".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };
        let res_chat = processor.process_danmu(&dm_chat, &matcher, &roster, &mut queue_mgr);
        assert!(!res_chat.priority_updated, "句中含优先词不应触发提权");
        assert!(!queue_mgr.items[0].is_priority, "排队项仍应保持非优先状态");

        // 3. 两段式提权弹幕（整条精确等于“优先”）：提权成功
        let dm2 = DanmuData {
            user_id: "user_001".into(),
            user_name: "猎人甲".into(),
            message: "优先".into(),
            timestamp: 1005,
            has_medal: true,
            medal_level: 10,
            guard_level: 3,
            msg_id: "msg_2".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };

        let res2 = processor.process_danmu(&dm2, &matcher, &roster, &mut queue_mgr);
        assert!(res2.priority_updated);
        assert!(queue_mgr.items[0].is_priority);
        println!("[PASS] test_danmu_processor_flow passed");
    }

    #[test]
    fn test_update_filters_takes_effect_immediately() {
        let mut matcher = crate::monster::MonsterDataManager::new();
        let _ = matcher.load_from_file(None);
        let processor = DanmuProcessor::new();
        let mut queue_mgr = crate::queue::QueueManager::new();
        let roster = test_roster();

        let make_no_medal_danmu = |msg_id: &str| DanmuData {
            user_id: "user_nm".into(),
            user_name: "无牌水友".into(),
            message: "点怪霸主太太".into(),
            timestamp: 3000,
            has_medal: false,
            medal_level: 0,
            guard_level: 0,
            msg_id: msg_id.into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };

        // 运行期开启"仅粉丝牌可点怪"：无粉丝牌弹幕不入队
        processor.update_filters(true);
        let res = processor.process_danmu(&make_no_medal_danmu("msg_nm_1"), &matcher, &roster, &mut queue_mgr);
        assert!(!res.matched);
        assert!(!res.added_to_queue);
        assert_eq!(queue_mgr.items.len(), 0);

        // 运行期关闭过滤（无需重启）：随后的新弹幕立即可入队
        processor.update_filters(false);
        let res2 = processor.process_danmu(&make_no_medal_danmu("msg_nm_2"), &matcher, &roster, &mut queue_mgr);
        assert!(res2.matched);
        assert!(res2.added_to_queue);
        assert_eq!(queue_mgr.items.len(), 1);
        assert_eq!(queue_mgr.items[0].monster_name, "霸主雌火龙");

        println!("[PASS] test_update_filters_takes_effect_immediately passed");
    }

    #[test]
    fn test_special_open_id_guard_override() {
        let processor = DanmuProcessor::new();
        let raw_json = format!(
            r#"{{"cmd":"LIVE_OPEN_PLATFORM_DM","data":{{"open_id":"{}","uname":"神秘总督","msg":"点怪优先黑蚀龙","guard_level":0,"fans_medal_wearing_status":true,"fans_medal_level":20,"msg_id":"special_1","timestamp":2000}}}}"#,
            SPECIAL_OPEN_ID
        );

        let dm = processor.parse_danmu_json(&raw_json).expect("Parse danmu failed");
        // 关键断言：特定管理员 open_id 必须被自动提升为总督 (guard_level = 1)
        assert_eq!(dm.guard_level, 1);
        assert_eq!(dm.user_id, SPECIAL_OPEN_ID);

        let mut matcher = crate::monster::MonsterDataManager::new();
        let _ = matcher.load_from_file(None);
        let mut queue_mgr = crate::queue::QueueManager::new();
        let roster = test_roster();

        let res = processor.process_danmu(&dm, &matcher, &roster, &mut queue_mgr);
        assert!(res.matched);
        assert_eq!(queue_mgr.items.len(), 1);
        // 总督水友的优先请求应被成功批准
        assert!(queue_mgr.items[0].is_priority);
        assert_eq!(queue_mgr.items[0].guard_level, 1);
        println!("[PASS] test_special_open_id_guard_override passed");
    }

    #[test]
    fn test_non_guard_cannot_claim_priority() {
        let processor = DanmuProcessor::new();
        let mut matcher = crate::monster::MonsterDataManager::new();
        let _ = matcher.load_from_file(None);
        let mut queue_mgr = crate::queue::QueueManager::new();
        let roster = test_roster();

        // 普通水友（guard_level = 0）发送点怪并要求优先
        let dm = DanmuData {
            user_id: "normal_user".into(),
            user_name: "路人水友".into(),
            message: "点怪优先灭尽龙".into(),
            timestamp: 3000,
            has_medal: true,
            medal_level: 5,
            guard_level: 0,
            msg_id: "msg_norm_1".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };

        let res = processor.process_danmu(&dm, &matcher, &roster, &mut queue_mgr);
        assert!(res.matched);
        assert_eq!(queue_mgr.items.len(), 1);
        // 关键断言：非舰长水友的优先请求必须被驳回！
        assert!(!queue_mgr.items[0].is_priority);

        // 该水友后续试图追加“优先”插队
        let dm_prio = DanmuData {
            user_id: "normal_user".into(),
            user_name: "路人水友".into(),
            message: "优先".into(),
            timestamp: 3005,
            has_medal: true,
            medal_level: 5,
            guard_level: 0,
            msg_id: "msg_norm_2".into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };

        let res_prio = processor.process_danmu(&dm_prio, &matcher, &roster, &mut queue_mgr);
        // 关键断言：两段式提权同样必须驳回！
        assert!(!res_prio.priority_updated);
        assert!(!queue_mgr.items[0].is_priority);
        println!("[PASS] test_non_guard_cannot_claim_priority passed");
    }

    /// 禁点名单：空名单不限制任何点怪；名单内的怪被拒绝入队（含繁体前缀与优先词组合）
    #[test]
    fn test_roster_blacklist_blocks_listed_monster() {
        use crate::roster::RosterData;

        let mut matcher = crate::monster::MonsterDataManager::new();
        let _ = matcher.load_from_file(None);
        let processor = DanmuProcessor::new();
        let mut queue_mgr = crate::queue::QueueManager::new();

        let dir = std::env::temp_dir().join("mh_test_bilibili_roster_blacklist");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let roster =
            crate::roster::MonsterRoster::load(Some(&dir.join(crate::roster::ROSTER_FILE_NAME)));

        let make = |uid: &str, msg_id: &str, message: &str, guard: i32| DanmuData {
            user_id: uid.into(),
            user_name: format!("水友{}", uid),
            message: message.into(),
            timestamp: 4000,
            has_medal: true,
            medal_level: 10,
            guard_level: guard,
            msg_id: msg_id.into(),
            is_paid_gift: false,
            has_history_required_fields: true,
        };

        // 1. 空名单：不限制任何点怪
        let res = processor.process_danmu(
            &make("u_free", "msg_free", "点怪金狮子", 0),
            &matcher,
            &roster,
            &mut queue_mgr,
        );
        assert!(res.matched);
        assert!(!res.blocked_by_roster);
        assert!(res.added_to_queue);
        assert_eq!(queue_mgr.items.len(), 1);

        // 2. 把「金狮子」加入禁点名单：该怪被拒绝，队列不变
        roster
            .replace(RosterData {
                items: vec!["金狮子".into()],
            })
            .unwrap();
        let res = processor.process_danmu(
            &make("u_blk", "msg_blk", "点怪金狮子", 0),
            &matcher,
            &roster,
            &mut queue_mgr,
        );
        assert!(res.matched, "字典命中信息需保留，供前端就地提示");
        assert!(res.blocked_by_roster);
        assert!(!res.added_to_queue);
        assert_eq!(res.monster_name, "金狮子");
        assert_eq!(queue_mgr.items.len(), 1, "被拦截的点怪不得进入队列");

        // 3. 名单外的怪物照常入队
        let res = processor.process_danmu(
            &make("u_ok", "msg_ok", "点怪黑龙", 0),
            &matcher,
            &roster,
            &mut queue_mgr,
        );
        assert!(res.matched);
        assert!(!res.blocked_by_roster);
        assert!(res.added_to_queue);
        assert_eq!(queue_mgr.items.len(), 2);

        // 4. 繁体前缀 + 优先词 + 名单外：剥离优先词后命中并入队，优先词仍生效
        let res = processor.process_danmu(
            &make("u_tw", "msg_tw", "點怪優先黑龙", 3),
            &matcher,
            &roster,
            &mut queue_mgr,
        );
        assert!(!res.blocked_by_roster);
        assert!(res.added_to_queue);
        let item = queue_mgr
            .items
            .iter()
            .find(|i| i.user_id == "u_tw")
            .expect("繁体点怪应入队");
        assert!(item.is_priority, "舰长的优先词应生效");

        // 5. 繁体前缀 + 历战修饰词 + 名单内：修饰词剥离后仍按金狮子拦截
        let res = processor.process_danmu(
            &make("u_tw2", "msg_tw2", "點隻歷戰金狮子", 3),
            &matcher,
            &roster,
            &mut queue_mgr,
        );
        assert!(res.matched);
        assert!(res.blocked_by_roster);
        assert_eq!(res.tempered_level, 1, "历战修饰词仍应解析出等级");
        assert!(!queue_mgr.items.iter().any(|i| i.user_id == "u_tw2"));

        let _ = std::fs::remove_dir_all(&dir);
        println!("[PASS] test_roster_blacklist_blocks_listed_monster passed");
    }

    #[test]
    fn test_bili_service_lifecycle() {
        let service = BiliLiveService::new();
        assert!(!service.is_running());
        assert_eq!(service.get_game_id(), None);

        service.set_running(true);
        service.set_game_id(Some("game_abc_123".into()));
        assert!(service.is_running());
        assert_eq!(service.get_game_id(), Some("game_abc_123".into()));

        service.set_running(false);
        service.set_game_id(None);
        assert!(!service.is_running());
        assert_eq!(service.get_game_id(), None);
        println!("[PASS] test_bili_service_lifecycle passed");
    }

    #[test]
    fn test_exponential_backoff() {
        let mut eb = ExponentialBackoff::new(1000, 10000);
        assert_eq!(eb.next_delay(), 1000);
        assert_eq!(eb.next_delay(), 2000);
        assert_eq!(eb.next_delay(), 4000);
        assert_eq!(eb.next_delay(), 8000);
        assert_eq!(eb.next_delay(), 10000); // 封顶 10000
        eb.reset();
        assert_eq!(eb.next_delay(), 1000);
        println!("[PASS] test_exponential_backoff passed");
    }

    #[test]
    fn test_parse_live_events_and_tts_text() {
        // SC 事件：文案与用户标识（open_id 优先）
        let sc = serde_json::json!({
            "open_id": "open_sc_1",
            "uname": "土豪水友",
            "rmb": 30,
            "message": "加油！"
        });
        let ev = parse_super_chat(&sc).expect("SC 应解析成功");
        assert_eq!(ev.user_id(), "open_sc_1");
        assert_eq!(ev.event_name(), "super-chat-received");
        assert_eq!(
            ev.tts_text().unwrap(),
            "感谢 土豪水友 赠送的30元SC：加油！"
        );

        // 上舰事件：guard_level 映射名称（1总督/2提督/3舰长）
        let guard = serde_json::json!({
            "uid": 12345,
            "user_info": { "uname": "新晋舰长" },
            "guard_level": 3,
            "guard_num": 1,
            "guard_unit": "月"
        });
        let ev = parse_guard(&guard).expect("上舰应解析成功");
        assert_eq!(ev.user_id(), "12345"); // open_id 缺失时回退 uid
        assert_eq!(ev.event_name(), "guard-received");
        assert_eq!(ev.tts_text().unwrap(), "感谢 新晋舰长 上船1月的舰长");

        // 未知 guard_level 不播报
        let bad_guard = serde_json::json!({
            "user_info": { "uname": "神秘人" },
            "guard_level": 9,
            "guard_num": 1,
            "guard_unit": "月"
        });
        assert!(parse_guard(&bad_guard).unwrap().tts_text().is_none());

        // 进场事件：仅前端提示，不播报（与原工程一致）
        let enter = serde_json::json!({ "open_id": "open_enter_1", "uname": "路过水友" });
        let ev = parse_room_enter(&enter).expect("进场应解析成功");
        assert_eq!(ev.event_name(), "room-enter-received");
        assert!(ev.tts_text().is_none());

        // 字段缺失时解析失败不 panic
        assert!(parse_super_chat(&serde_json::json!({})).is_none());
        assert!(parse_guard(&serde_json::json!({ "guard_level": 3 })).is_none());
        assert!(parse_room_enter(&serde_json::json!({ "uid": 1 })).is_none());
        println!("[PASS] test_parse_live_events_and_tts_text passed");
    }

    #[test]
    fn test_parse_gift_event_with_combo_and_paid() {
        // 完整官方连击数据
        let full = serde_json::json!({
            "open_id": "open_gift_1",
            "gift_id": 1001,
            "uname": "送礼水友",
            "gift_name": "小心心",
            "gift_num": 1,
            "paid": true,
            "combo_info": {
                "combo_base_num": 5,
                "combo_count": 3,
                "combo_timeout": 2.5
            }
        });
        let ev = parse_gift_event(&full).expect("礼物应解析成功");
        assert_eq!(ev.open_id, "open_gift_1");
        assert_eq!(ev.gift_id, "1001");
        assert!(ev.paid);
        let combo = ev.combo.expect("combo_info 应解析");
        assert_eq!(combo.base_num, 5);
        assert_eq!(combo.count, 3);
        assert!((combo.timeout_secs - 2.5).abs() < 0.01);

        // 缺少 paid / combo_info：按免费礼物、无连击处理
        let minimal = serde_json::json!({
            "open_id": "open_gift_2",
            "gift_id": "2002",
            "uname": "普通水友",
            "gift_name": "辣条",
            "gift_num": 2
        });
        let ev = parse_gift_event(&minimal).expect("礼物应解析成功");
        assert!(!ev.paid);
        assert!(ev.combo.is_none());
        assert_eq!(ev.gift_id, "2002");

        // gift_name 缺失视为无效事件
        assert!(parse_gift_event(&serde_json::json!({ "uname": "x" })).is_none());
        println!("[PASS] test_parse_gift_event_with_combo_and_paid passed");
    }

    #[test]
    fn test_parse_like_event() {
        // 完整字段
        let full = serde_json::json!({
            "open_id": "open_like_1",
            "uname": "点赞水友",
            "like_count": 25,
            "msg_id": "like_msg_1",
            "timestamp": 1758240000
        });
        let ev = parse_like_event(&full).expect("点赞事件应解析成功");
        assert_eq!(ev.uid, "open_like_1");
        assert_eq!(ev.username, "点赞水友");
        assert_eq!(ev.like_count, 25);
        assert_eq!(ev.msg_id, "like_msg_1");
        assert_eq!(ev.timestamp, 1758240000);

        // uid 回退 + click_count + send_time
        let minimal = serde_json::json!({
            "uid": 123456,
            "click_count": 3,
            "send_time": 1758240001
        });
        let ev = parse_like_event(&minimal).expect("点赞事件应解析成功");
        assert_eq!(ev.uid, "123456");
        assert_eq!(ev.like_count, 3);
        assert_eq!(ev.timestamp, 1758240001);

        // 超过风控上限时截断
        let huge = serde_json::json!({ "open_id": "u", "like_count": 999999 });
        assert_eq!(parse_like_event(&huge).unwrap().like_count, 10000);

        // 无用户标识视为无效
        assert!(parse_like_event(&serde_json::json!({ "like_count": 5 })).is_none());

        // 服务器时间戳 → 本地日期
        assert!(server_date(1758240000).is_some());
        assert!(server_date(0).is_none());
        println!("[PASS] test_parse_like_event passed");
    }
}

