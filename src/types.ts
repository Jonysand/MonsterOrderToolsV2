export interface QueueItem {
  id: string;
  user_id: string;
  user_name: string;
  monster_name: string;
  is_priority: boolean;
  guard_level: number; // 1=总督, 2=提督, 3=舰长, 0=普通
  tempered_level: number; // 0=普通, 1=历战, 2=历战王
  timestamp: number;
  icon_url: string;
}

export interface UserProfile {
  uid: string;
  username: string;
  last_checkin_date: number;
  continuous_days: number;
  cumulative_days: number;
  last_danmu_timestamp: number;
  created_at: number;
  updated_at: number;
}

// GM 搜索结果：档案 + 当前补签卡数
export interface UserSearchItem extends UserProfile {
  card_count: number;
}

export interface RetroactiveCardData {
  uid: string;
  card_count: number;
  total_earned: number;
  weekly_first_claimed: number;
  last_earned_date: number;
}

// 敏感字段（app_id / access_key_* / manbo_api_key / mimo_api_key / deepseek_api_key）
// 运行时恒为空串：权威来源为 credentials.dat（应用凭据状态走 get_credentials_status）；
// 保存配置时回传空值不会清空已有凭据。id_code 权威来源为 Windows 注册表。
export interface AppConfig {
  id_code: string;
  app_id: string;
  access_key_id: string;
  access_key_secret: string;

  tts_engine: string;
  enable_voice: boolean;
  speech_rate: number;
  speech_volume: number;
  speech_pitch: number;
  manbo_api_key: string;
  manbo_voice: string;
  mimo_api_key: string;
  mimo_voice: string;
  mimo_style: string;
  mimo_audio_format: string;
  tts_cache_days_to_keep: number;

  only_medal_order: boolean;
  only_speek_wearing_medal: boolean;
  only_speek_paid_gift: boolean;
  only_speek_guard_level: number;

  opacity: number;
  penetrating_mode_opacity: number;
  top_pos_x: number;
  top_pos_y: number;
  default_marquee_text: string;
  /** 悬浮窗主题：wilds（荒野）/ asc（凌越） */
  overlay_theme: string;
  /** 环境装饰动效开关（云雾/雷光/扫光/呼吸），默认关闭 */
  enable_overlay_decor: boolean;

  enable_captain_checkin_ai: boolean;
  checkin_trigger_words: string;
  deepseek_api_key: string;

  is_lite_mode: boolean;
}

export interface AIBubblePayload {
  username: string;
  prompt: string;
  reasoning: string;
  answer: string;
  is_thinking: boolean;
}

export interface BatchCheckinResult {
  success: boolean;
  total_users: number;
  patched_users: number;
  skipped_users: number;
  total_inserted: number;
  message: string;
}

export interface CredentialsStatus {
  loaded: boolean;
  file_path: string;
  app_id: string;
  access_key_masked: string;
  chat_provider: string;
  has_chat_key: boolean;
  has_mimo_key: boolean;
  has_vip_tts_key: boolean;
}

// ---------- D7 连接状态机（五态 + 断连原因） ----------
export type ConnectionStateKind =
  | "Disconnected"
  | "Connecting"
  | "Connected"
  | "Reconnecting"
  | "ReconnectFailed";

export type DisconnectReasonKind =
  | "None"
  | "NetworkError"
  | "HeartbeatTimeout"
  | "ServerClose"
  | "AuthFailed";

export interface ConnectionStatusPayload {
  state: ConnectionStateKind;
  reason: DisconnectReasonKind;
  reason_text: string;
  attempt: number;
  /** 后端拼好的展示文案（重连中带次数、重连失败带原因） */
  display: string;
}

// ---------- D3 跑马灯 ----------
export interface OrderPlacedPayload {
  user_id: string;
  user_name: string;
  monster_name: string;
  is_priority: boolean;
}

// ---------- D4 业务事件气泡 / 动态 ----------
export interface CheckinReplyPayload {
  user_id: string;
  user_name: string;
  reply: string;
  is_ai: boolean;
}

export interface RetroactivePayload {
  user_id: string;
  user_name: string;
  reply: string;
  success?: boolean;
  remaining_cards?: number;
  card_count?: number;
  date?: number;
}

export interface LikeRewardPayload {
  uid: string;
  user_name: string;
  likes: number;
  daily_total: number;
  replies: string[];
}

export interface GiftReceivedPayload {
  open_id: string;
  gift_id: string;
  uname: string;
  gift_name: string;
  gift_num: number;
  paid: boolean;
}

/**
 * LiveEvent 为**内部标签**枚举（Rust `#[serde(tag = "kind")]`）：
 * `{ kind: "SuperChat", ... }` / `{ kind: "Guard", ... }` / `{ kind: "RoomEnter", ... }`
 */
export interface SuperChatReceivedPayload {
  kind: "SuperChat";
  user_id: string;
  uname: string;
  rmb: number;
  message: string;
}

export interface GuardReceivedPayload {
  kind: "Guard";
  user_id: string;
  uname: string;
  guard_level: number;
  guard_num: number;
  guard_unit: string;
}

// ---------- D5 运行日志 ----------
export interface LogEntry {
  time: string;
  level: string;
  message: string;
}

export interface LogsSnapshot {
  dir: string;
  entries: LogEntry[];
}

/** danmu-received 事件载荷（原始弹幕，供主播控制台实时查看） */
export interface DanmuReceivedPayload {
  user_id: string;
  user_name: string;
  message: string;
  timestamp: number;
  has_medal: boolean;
  medal_level: number;
  guard_level: number;
  msg_id: string;
  is_paid_gift: boolean;
}