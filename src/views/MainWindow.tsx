import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import {
  QueueItem,
  AppConfig,
  UserSearchItem,
  UserProfile,
  BatchCheckinResult,
  AIBubblePayload,
  CredentialsStatus,
  ConnectionStatusPayload,
  DanmuReceivedPayload,
  LogsSnapshot,
  MonsterDict,
  RosterData,
} from "../types";
import { VirtualList } from "../components/VirtualList";
import { MarqueeText } from "../components/MarqueeText";
import { MonsterListTab } from "./MonsterListTab";
import { MonsterPickerPanel, PickerOrderPayload } from "../components/MonsterPickerPanel";
import {
  Shield,
  Sparkles,
  Radio,
  Sliders,
  Bot,
  Users,
  CalendarCheck,
  Download,
  Search,
  Gift,
  Flame,
  Power,
  MessageSquare,
  AlertCircle,
  GripVertical,
  ListPlus,
  Eye,
  Lock,
  Unlock,
  Key,
  Save,
  FileCheck,
  Upload,
  ScrollText,
  Volume2
} from "lucide-react";

const DEFAULT_CONNECTION: ConnectionStatusPayload = {
  state: "Disconnected",
  reason: "None",
  reason_text: "无",
  attempt: 0,
  display: "未连接",
};

/** 引擎名 → 展示文案（对齐原工程 UpdateCurrentTTSEngineLabel） */
const ENGINE_LABELS: Record<string, string> = {
  manbo: "Manbo",
  xiaomi: "小米MiMo",
  sapi: "Windows SAPI",
};

/** 主窗口队列行高（条目卡 46px + 间距 8px） */
const QUEUE_ROW_HEIGHT = 54;

export const MainWindow: React.FC = () => {
  const [activeTab, setActiveTab] = useState<
    "queue" | "monster" | "bili" | "gm" | "ai" | "settings" | "logs"
  >("queue");
  const [queue, setQueue] = useState<QueueItem[]>([]);
  const [isLite, setIsLite] = useState(false);
  const [config, setConfig] = useState<AppConfig | null>(null);
  const [draggedIndex, setDraggedIndex] = useState<number | null>(null);

  // 怪物字典与禁点名单（名单内的怪不可被点：弹幕点怪与选怪面板共享同一份约束）
  const [monsterDict, setMonsterDict] = useState<MonsterDict>({});
  const [roster, setRoster] = useState<RosterData>({ items: [] });
  const [orderSubmitting, setOrderSubmitting] = useState(false);
  const rosterRef = useRef<RosterData>({ items: [] });
  const rosterSaveTimerRef = useRef<number | null>(null);

  // B站长连五态状态机（D7）
  const [conn, setConn] = useState<ConnectionStatusPayload>(DEFAULT_CONNECTION);
  const [simDanmuMsg, setSimDanmuMsg] = useState("");
  const [simDanmuUser, setSimDanmuUser] = useState("测试水友");
  // 模拟身份（默认沿用旧硬编码值：舰长 + 佩戴 10 级粉丝牌）
  const [simGuardLevel, setSimGuardLevel] = useState(3);
  const [simHasMedal, setSimHasMedal] = useState(true);
  const [simMedalLevel, setSimMedalLevel] = useState(10);
  const [simLikeUser, setSimLikeUser] = useState("测试水友");
  const [simLikeCount, setSimLikeCount] = useState(30);
  // 直播间事件模拟（B8 礼物连击 / B2 SC·上舰）
  const [simGiftName, setSimGiftName] = useState("小心心");
  const [simGiftNum, setSimGiftNum] = useState(1);
  const [simGiftPaid, setSimGiftPaid] = useState(true);
  const [simScRmb, setSimScRmb] = useState(30);
  const [simScMessage, setSimScMessage] = useState("加油！");
  const [simGuardEventLevel, setSimGuardEventLevel] = useState(3);
  const [simGuardEventNum, setSimGuardEventNum] = useState(1);
  const [simGuardEventUnit, setSimGuardEventUnit] = useState("月");
  const [recentDanmu, setRecentDanmu] = useState<DanmuReceivedPayload[]>([]);
  const [recentCheckins, setRecentCheckins] = useState<UserProfile[]>([]);

  // 语音设置（D1）
  const [manboVoices, setManboVoices] = useState<string[]>([]);
  const [manboKeyInput, setManboKeyInput] = useState("");
  const [currentEngine, setCurrentEngine] = useState<string>("");

  // 悬浮窗锁定（D2）
  const [overlayLocked, setOverlayLocked] = useState(false);

  // 运行日志（D5）
  const [logSnapshot, setLogSnapshot] = useState<LogsSnapshot | null>(null);
  const [logLevel, setLogLevel] = useState<"DEBUG" | "INFO" | "WARNING" | "ERROR">("INFO");
  const [resourceWarnings, setResourceWarnings] = useState<string[]>([]);
  const [appVersion, setAppVersion] = useState("");
  const activeTabRef = useRef(activeTab);

  // GM 运维面板状态
  const [searchKeyword, setSearchKeyword] = useState("");
  const [searchedUsers, setSearchedUsers] = useState<UserSearchItem[]>([]);
  const [grantCardAmount, setGrantCardAmount] = useState(1);
  const [batchResult, setBatchResult] = useState<BatchCheckinResult | null>(null);

  // GM 打卡数据导出（对齐原工程 ExportOptionsDialog）
  const [exportFormat, setExportFormat] = useState<"csv" | "json">("csv");
  const [exportUsername, setExportUsername] = useState("");
  const [exportStartDate, setExportStartDate] = useState("");
  const [exportEndDate, setExportEndDate] = useState("");

  // AI 对话测试
  const [aiPrompt, setAiPrompt] = useState("");
  const [aiResult, setAiResult] = useState<AIBubblePayload | null>(null);
  const [aiLoading, setAiLoading] = useState(false);

  // 敏感凭证加密托管状态
  const [credStatus, setCredStatus] = useState<CredentialsStatus | null>(null);

  // 通知消息
  const [toastMsg, setToastMsg] = useState<string | null>(null);

  const showToast = (msg: string) => {
    setToastMsg(msg);
    setTimeout(() => setToastMsg(null), 3500);
  };

  // 原生二次确认（WebView2 下 window.confirm 静默放行，统一走 Rust 侧消息框）
  const askConfirm = async (message: string, title = "确认操作") => {
    try {
      return await invoke<boolean>("confirm_action", { title, message });
    } catch {
      return false;
    }
  };

  const fetchQueue = async () => {
    try {
      const items = await invoke<QueueItem[]>("get_queue");
      setQueue(items);
    } catch (e) {
      console.error(e);
    }
  };

  const fetchConfig = async () => {
    try {
      const cfg = await invoke<AppConfig>("get_app_config");
      setConfig(cfg);
      setIsLite(cfg.is_lite_mode);
    } catch (e) {
      console.error(e);
    }
  };

  const fetchBiliStatus = async () => {
    try {
      const status = await invoke<ConnectionStatusPayload>("get_bili_connection_state");
      setConn(status);
    } catch (e) {
      console.error(e);
    }
  };

  const fetchCurrentEngine = async () => {
    try {
      const name = await invoke<string>("get_current_tts_engine");
      setCurrentEngine(name);
    } catch (e) {
      console.error(e);
    }
  };

  const fetchLogs = async () => {
    try {
      const snapshot = await invoke<LogsSnapshot>("get_recent_logs", {
        limit: 300,
        minLevel: logLevel,
      });
      setLogSnapshot(snapshot);
    } catch (e) {
      console.error(e);
    }
  };

  const fetchOverlayLocked = async () => {
    try {
      setOverlayLocked(await invoke<boolean>("get_overlay_locked"));
    } catch (e) {
      console.error(e);
    }
  };

  const fetchCredentialsStatus = async () => {
    try {
      const status = await invoke<CredentialsStatus>("get_credentials_status");
      setCredStatus(status);
    } catch (e) {
      console.error("获取凭据状态异常:", e);
    }
  };

  useEffect(() => {
    fetchQueue();
    fetchConfig();
    fetchBiliStatus();
    fetchCredentialsStatus();
    fetchCurrentEngine();
    fetchOverlayLocked();
    fetchMonsterDict();
    fetchRoster();
    invoke<string[]>("get_manbo_voice_list")
      .then(setManboVoices)
      .catch((e) => console.error(e));

    const interval = setInterval(() => {
      fetchQueue();
      fetchBiliStatus();
      fetchCredentialsStatus();
      // 「当前引擎」实时刷新（对齐原工程 2s 定时器）
      fetchCurrentEngine();
      if (activeTabRef.current === "logs") {
        fetchLogs();
      }
    }, 2500);

    const unlistenQueue = listen<QueueItem[]>("queue-updated", (event) => {
      setQueue(event.payload);
    });

    // D7 五态连接状态
    const unlistenConn = listen<ConnectionStatusPayload>("connection-state-changed", (event) => {
      setConn(event.payload);
    });

    const unlistenAi = listen<AIBubblePayload>("ai-bubble", (event) => {
      setAiResult(event.payload);
    });

    // D4/D5 主播控制台动态：原始弹幕与打卡记录
    const unlistenDanmu = listen<DanmuReceivedPayload>("danmu-received", (event) => {
      setRecentDanmu((prev) => [event.payload, ...prev].slice(0, 20));
    });
    const unlistenCheckin = listen<UserProfile>("checkin-recorded", (event) => {
      setRecentCheckins((prev) => [event.payload, ...prev].slice(0, 10));
    });

    // D2 锁定状态同步（命令 / Alt+, 热键）
    const unlistenLock = listen<boolean>("overlay-lock-changed", (event) => {
      setOverlayLocked(event.payload);
    });

    // 配置在别处变更（如退出前落盘、其他窗口修改）时刷新设置面板，
    // 避免面板持有陈旧副本；top_pos 由后端独占维护，不受此影响
    const unlistenConfig = listen("config-changed", () => {
      fetchConfig();
    });

    // D5 资源缺失提示
    const unlistenMissing = listen<string>("resource-missing", (event) => {
      setResourceWarnings((prev) =>
        prev.includes(event.payload) ? prev : [...prev, event.payload]
      );
    });

    return () => {
      clearInterval(interval);
      if (rosterSaveTimerRef.current !== null) {
        window.clearTimeout(rosterSaveTimerRef.current);
      }
      unlistenQueue.then((f) => f());
      unlistenConn.then((f) => f());
      unlistenAi.then((f) => f());
      unlistenDanmu.then((f) => f());
      unlistenCheckin.then((f) => f());
      unlistenLock.then((f) => f());
      unlistenMissing.then((f) => f());
      unlistenConfig.then((f) => f());
    };
  }, []);

  // 版本号展示：与 tauri.conf.json 的 version 同源（原工程在标题栏显示 v44）
  useEffect(() => {
    getVersion()
      .then(setAppVersion)
      .catch(() => setAppVersion(""));
  }, []);

  useEffect(() => {
    activeTabRef.current = activeTab;
    if (activeTab === "logs") {
      fetchLogs();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeTab, logLevel]);

  /* ---------------- 怪物字典 / 禁点名单 ---------------- */
  const fetchMonsterDict = async () => {
    try {
      setMonsterDict(await invoke<MonsterDict>("get_monster_dict"));
    } catch (e) {
      console.error("获取怪物列表异常:", e);
    }
  };

  const fetchRoster = async () => {
    try {
      const data = await invoke<RosterData>("get_monster_roster");
      rosterRef.current = data;
      setRoster(data);
    } catch (e) {
      console.error("获取禁点名单异常:", e);
    }
  };

  /** 字典条目增删改后刷新：改名/删除会在后端联动禁点名单，故名单须一并重取 */
  const refreshMonsterData = async () => {
    await Promise.all([fetchMonsterDict(), fetchRoster()]);
  };

  /** 名单变更：本地乐观更新 + 300ms 防抖整表落盘；失败回滚并提示 */
  const commitRoster = (next: RosterData, immediate = false) => {
    const prev = rosterRef.current;
    rosterRef.current = next;
    setRoster(next);
    if (rosterSaveTimerRef.current !== null) {
      window.clearTimeout(rosterSaveTimerRef.current);
    }

    const persist = async () => {
      rosterSaveTimerRef.current = null;
      try {
        await invoke("set_monster_roster", { data: next });
      } catch (err) {
        rosterRef.current = prev;
        setRoster(prev);
        showToast(`名单保存失败，已回滚: ${err}`);
      }
    };

    if (immediate) {
      void persist();
    } else {
      rosterSaveTimerRef.current = window.setTimeout(persist, 300);
    }
  };

  const handleExportRoster = async () => {
    try {
      const path = await invoke<string | null>("export_monster_roster");
      if (path) showToast(`禁点名单已导出: ${path}`);
    } catch (err) {
      showToast(`导出失败: ${err}`);
    }
  };

  const handleImportRoster = async () => {
    try {
      const parsed = await invoke<RosterData | null>("import_monster_roster");
      if (!parsed) return;

      const merge = await askConfirm(
        `已选择名单文件：共 ${parsed.items.length} 个怪物将加入禁点名单。\n\n` +
          "「是」= 合并到当前名单（仅追加新怪物）\n" +
          `「否」= 覆盖当前名单（现有 ${roster.items.length} 项将被替换）`,
        "导入禁点名单"
      );

      let next: RosterData;
      if (merge) {
        const merged = [...roster.items];
        parsed.items.forEach((n) => {
          if (!merged.includes(n)) merged.push(n);
        });
        next = { items: merged };
      } else {
        const ok = await askConfirm(
          `确认覆盖当前名单？\n\n现有 ${roster.items.length} 项将被导入的 ${parsed.items.length} 项替换，此操作不可撤销。`,
          "确认覆盖名单"
        );
        if (!ok) return;
        next = parsed;
      }

      commitRoster(next, true);
      showToast(`名单导入完成（禁点 ${next.items.length} 项）`);
    } catch (err) {
      showToast(`导入失败: ${err}`);
    }
  };

  /** 选怪面板入队（与弹幕点怪写入同一条队列） */
  const handlePickerOrder = async (payload: PickerOrderPayload) => {
    setOrderSubmitting(true);
    try {
      const name = payload.userName || "房管";
      const updated = await invoke<QueueItem[]>("add_order", {
        userId: `manual-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
        userName: name,
        monsterName: payload.monsterName,
        isPriority: payload.isPriority,
        guardLevel: payload.guardLevel,
        temperedLevel: payload.temperedLevel,
      });
      setQueue(updated);
      showToast(`${name} 点怪 ${payload.monsterName} 已入队${payload.isPriority ? "（优先置前）" : ""}`);
    } catch (err) {
      showToast(`加入排队失败: ${err}`);
    } finally {
      setOrderSubmitting(false);
    }
  };

  const handleDelete = async (userId: string) => {
    try {
      const updated = await invoke<QueueItem[]>("dequeue_by_user_id", { userId });
      setQueue(updated);
      showToast("已完成条目并保序出队");
    } catch (e) {
      console.error(e);
    }
  };

  const handleClear = async () => {
    if (!(await askConfirm("确认清空当前点单排队队列吗？", "清空队列"))) return;
    try {
      await invoke("clear_queue");
      setQueue([]);
      showToast("队列已清空");
    } catch (e) {
      console.error(e);
    }
  };

  // 控制台条目拖拽排序
  const handleItemDragStart = (e: React.DragEvent, index: number) => {
    setDraggedIndex(index);
    e.dataTransfer.effectAllowed = "move";
  };

  const handleItemDragOver = (e: React.DragEvent) => {
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
  };

  const handleItemDrop = async (e: React.DragEvent, targetIndex: number) => {
    e.preventDefault();
    if (draggedIndex === null || draggedIndex === targetIndex) {
      setDraggedIndex(null);
      return;
    }

    const nextQueue = [...queue];
    const [moved] = nextQueue.splice(draggedIndex, 1);
    nextQueue.splice(targetIndex, 0, moved);

    setQueue(nextQueue);
    setDraggedIndex(null);

    try {
      await invoke("reorder_queue", { items: nextQueue });
      showToast("排队顺序已调整并同步保存！");
    } catch (err) {
      showToast(`排序更新失败: ${err}`);
    }
  };

  // 悬浮窗显隐切换
  const toggleOverlayWindow = async () => {
    try {
      const isVis = await invoke<boolean>("toggle_window", { label: "overlay" });
      showToast(isVis ? "已调出桌面点怪悬浮窗" : "已隐藏桌面点怪悬浮窗");
    } catch (err) {
      showToast(`悬浮窗操作异常: ${err}`);
    }
  };

  const toggleLite = async () => {
    try {
      const next = !isLite;
      await invoke("set_lite_mode", { enabled: next });
      setIsLite(next);
      if (config) {
        setConfig({ ...config, is_lite_mode: next });
      }
      showToast(next ? "已切换至 Lite 纯排队模式" : "已恢复完整全功能模式");
    } catch (e) {
      console.error(e);
    }
  };

  // 手动保存身份码到 Windows 注册表
  const handleSaveIdCode = async () => {
    if (!config) return;
    const code = (config.id_code || "").trim();
    if (!code) {
      showToast("身份码不能为空！");
      return;
    }
    try {
      await invoke("save_id_code", { idCode: code });
      showToast("开播身份码已持久化保存至 Windows 注册表！");
    } catch (e) {
      showToast(`保存身份码失败: ${e}`);
    }
  };

  // 切换 B 站连接（开播前自动校验并持久化身份码至注册表）
  const toggleBiliConnect = async () => {
    try {
      const isActive = conn.state === "Connected" || conn.state === "Connecting" || conn.state === "Reconnecting";
      const next = !isActive;
      if (next) {
        // id_code 不下发前端（skip_serializing），故留空时不拦截：交由 Rust 侧回退读注册表
        const currentCode = (config?.id_code || "").trim();
        if (currentCode) {
          // 自动将当前输入的身份码同步保存至注册表
          await invoke("save_id_code", { idCode: currentCode });
        }
      }
      await invoke("set_bili_connection", { connected: next });
      showToast(next ? "正在建立 B 站开放平台长连接..." : "已断开直播连接");
      fetchBiliStatus();
    } catch (e) {
      showToast(`连接操作失败: ${e}`);
      fetchBiliStatus();
    }
  };

  // 保存 Manbo API Key（仅写注册表，不回传明文）
  const handleSaveManboKey = async () => {
    if (!manboKeyInput.trim()) {
      showToast("请输入 Manbo API Key！");
      return;
    }
    try {
      await invoke("save_manbo_api_key", { key: manboKeyInput.trim() });
      setManboKeyInput("");
      showToast("Manbo API Key 已加密托管至注册表！");
      fetchCredentialsStatus();
    } catch (e) {
      showToast(`保存 Manbo Key 失败: ${e}`);
    }
  };

  // 悬浮窗锁定/解锁（等同 Alt+, 热键）
  const handleToggleOverlayLock = async () => {
    try {
      const next = await invoke<boolean>("set_overlay_locked", { locked: !overlayLocked });
      setOverlayLocked(next);
      showToast(next ? "悬浮窗已锁定：鼠标穿透 + 置顶（Alt+, 可解锁）" : "悬浮窗已解锁：可拖拽与点击");
    } catch (e) {
      showToast(`悬浮窗锁定操作失败: ${e}`);
    }
  };

  // 清空前端日志视图的内存环
  const handleClearLogs = async () => {
    try {
      await invoke("clear_recent_logs");
      fetchLogs();
      showToast("日志视图已清空（已落盘文件保留）");
    } catch (e) {
      showToast(`清空日志失败: ${e}`);
    }
  };

  // 模拟弹幕发送（通过后端统一业务管道）
  const handleSimDanmu = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!simDanmuMsg.trim()) return;

    try {
      const danmuPayload = {
        user_id: `sim-${simDanmuUser}`,
        user_name: simDanmuUser,
        message: simDanmuMsg.trim(),
        timestamp: Math.floor(Date.now() / 1000),
        has_medal: simHasMedal,
        medal_level: simHasMedal ? simMedalLevel : 0,
        guard_level: simGuardLevel,
        msg_id: `sim-${Date.now()}`,
        is_paid_gift: false,
      };
      await invoke("simulate_danmu", { danmu: danmuPayload });
      showToast(`已通过总线发射模拟弹幕：【${simDanmuMsg.trim()}】`);
      setSimDanmuMsg("");
    } catch (err) {
      showToast(`弹幕模拟处理失败: ${err}`);
    }
  };

  // GM 搜索水友
  const handleSearchUsers = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!searchKeyword.trim()) {
      showToast("请输入搜索关键词");
      return;
    }
    try {
      const list = await invoke<UserSearchItem[]>("gm_search_users", { keyword: searchKeyword.trim() });
      setSearchedUsers(list);
      showToast(list.length === 0 ? "未找到匹配的水友" : `找到 ${list.length} 个匹配用户`);
    } catch (err) {
      showToast(`搜索错误: ${err}`);
    }
  };

  // GM 一键批量补签
  const handleBatchCheckin = async () => {
    if (!(await askConfirm("确定对所有存在断签历史记录的水友执行一键批量补签吗？", "一键黑幕"))) return;
    try {
      const res = await invoke<BatchCheckinResult>("gm_batch_checkin");
      setBatchResult(res);
      showToast(res.message);
    } catch (err) {
      showToast(`批量补签失败: ${err}`);
    }
  };

  // GM 发卡（二次确认，防误触；文案对齐原工程 GMRetroactiveCardDialog）
  const handleGrantCard = async (uid: string, username: string) => {
    if (!(await askConfirm(`确认为 ${username} 发放 ${grantCardAmount} 张补签卡？`, "确认发放"))) return;
    try {
      const count = await invoke<number>("gm_grant_card", { uid, count: grantCardAmount });
      showToast(`已成功为「${username}」发放 ${grantCardAmount} 张补签卡，当前剩余 ${count} 张`);
      // 发卡后刷新搜索结果，保证列表展示的补签卡数不陈旧（对齐原工程 GMRetroactiveCardDialog 的重新搜索）
      if (searchKeyword.trim()) {
        const list = await invoke<UserSearchItem[]>("gm_search_users", { keyword: searchKeyword.trim() });
        setSearchedUsers(list);
      }
    } catch (err) {
      showToast(`发卡失败: ${err}`);
    }
  };

  // 凭据文件导入（安装包不随包分发 credentials.dat，需用户显式导入原工程的加密凭据文件）
  const handleImportCredentials = async () => {
    try {
      const status = await invoke<CredentialsStatus>("import_credentials_file");
      setCredStatus(status);
      showToast("凭据导入成功，已即时生效");
    } catch (err) {
      showToast(`凭据导入失败: ${err}`);
    }
  };

  // GM 导出打卡记录（系统保存对话框；CSV/JSON + 可选昵称/日期范围）
  const handleExportRecords = async () => {
    try {
      const savedPath = await invoke<string>("gm_export_checkin_records", {
        format: exportFormat,
        username: exportUsername.trim() || null,
        startDate: exportStartDate || null,
        endDate: exportEndDate || null,
      });
      showToast(`导出成功：${savedPath}`);
    } catch (err) {
      showToast(`导出失败: ${err}`);
    }
  };

  // 模拟点赞事件（msg_id 去重 + 奖卡播报）
  const handleSimLike = async (e: React.FormEvent) => {
    e.preventDefault();
    try {
      const replies = await invoke<string[]>("simulate_like", {
        event: {
          // 与弹幕模拟通道同口径（sim-{昵称}），保证跨链路档案连通
          uid: `sim-${simLikeUser}`,
          username: simLikeUser,
          msg_id: `sim_like_${Date.now()}`,
          like_count: simLikeCount,
          timestamp: Math.floor(Date.now() / 1000),
        },
      });
      showToast(
        replies.length > 0
          ? `已发放奖卡并播报：${replies.join(" / ")}`
          : `已记录 ${simLikeUser} 本次点赞 ${simLikeCount} 次（未触发奖卡）`
      );
    } catch (err) {
      showToast(`点赞模拟失败: ${err}`);
    }
  };

  // 模拟礼物事件（B8 连击合并 / B11 付费过滤；combo 置空走动态连击跟踪）
  const handleSimGift = async (e: React.FormEvent) => {
    e.preventDefault();
    try {
      await invoke("simulate_gift", {
        event: {
          open_id: `sim-${simDanmuUser}`,
          gift_id: `sim_gift_${Date.now()}`,
          uname: simDanmuUser,
          gift_name: simGiftName.trim() || "小心心",
          gift_num: simGiftNum,
          paid: simGiftPaid,
          combo: null,
        },
      });
      showToast(`已发射模拟礼物：${simDanmuUser} × ${simGiftName.trim() || "小心心"} ×${simGiftNum}`);
    } catch (err) {
      showToast(`礼物模拟失败: ${err}`);
    }
  };

  // 模拟 SC 事件（B2 高亮弹幕播报）
  const handleSimSuperChat = async (e: React.FormEvent) => {
    e.preventDefault();
    try {
      await invoke("simulate_live_event", {
        event: {
          kind: "SuperChat",
          user_id: `sim-${simDanmuUser}`,
          uname: simDanmuUser,
          rmb: simScRmb,
          message: simScMessage.trim() || "加油！",
        },
      });
      showToast(`已发射模拟 SC：${simDanmuUser} ¥${simScRmb}`);
    } catch (err) {
      showToast(`SC 模拟失败: ${err}`);
    }
  };

  // 模拟上舰事件（B2 舰长/提督/总督播报）
  const handleSimGuard = async (e: React.FormEvent) => {
    e.preventDefault();
    try {
      await invoke("simulate_live_event", {
        event: {
          kind: "Guard",
          user_id: `sim-${simDanmuUser}`,
          uname: simDanmuUser,
          guard_level: simGuardEventLevel,
          guard_num: simGuardEventNum,
          guard_unit: simGuardEventUnit.trim() || "月",
        },
      });
      showToast(`已发射模拟上舰：${simDanmuUser} 开通 ${simGuardEventNum} ${simGuardEventUnit.trim() || "月"}`);
    } catch (err) {
      showToast(`上舰模拟失败: ${err}`);
    }
  };

  // AI 提问
  const handleAskAi = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!aiPrompt.trim()) return;
    setAiLoading(true);
    try {
      const res = await invoke<AIBubblePayload>("ask_ai_thinking", {
        prompt: aiPrompt.trim(),
        username: "主播控制台",
      });
      setAiResult(res);
      setAiPrompt("");
      showToast("AI 回答已生成并推送到悬浮窗！");
    } catch (err) {
      showToast(`AI 请求失败: ${err}`);
    } finally {
      setAiLoading(false);
    }
  };

  // 保存设置
  const handleSaveConfig = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!config) return;
    try {
      await invoke("save_app_config", { newCfg: config });
      showToast("全局配置已保存并即时生效！");
    } catch (err) {
      showToast(`保存失败: ${err}`);
    }
  };

  // 滑杆类控件：改动静音期（800ms）后自动保存并广播 config-changed，
  // 使悬浮窗透明度/跑马灯等即时生效，且用户改完直接关窗也不会丢设置
  // （对齐原工程：每个控件 ConfigChanged → SaveConfig + RefreshWindow）
  const autoSaveTimerRef = useRef<number | null>(null);
  const applyConfigPatch = (patch: Partial<AppConfig>) => {
    setConfig((prev) => {
      if (!prev) return prev;
      const next = { ...prev, ...patch };
      if (autoSaveTimerRef.current !== null) window.clearTimeout(autoSaveTimerRef.current);
      autoSaveTimerRef.current = window.setTimeout(() => {
        invoke("save_app_config", { newCfg: next }).catch((err) =>
          showToast(`自动保存失败: ${err}`)
        );
      }, 800);
      return next;
    });
  };

  return (
    <div className="flex h-screen w-screen bg-neutral-950 text-neutral-100 font-sans select-none overflow-hidden">
      {/* 顶部/全局 Toast 通知 */}
      {toastMsg && (
        <div className="fixed top-4 left-1/2 -translate-x-1/2 z-50 bg-amber-500/90 text-black font-bold px-4 py-2 rounded-xl shadow-2xl backdrop-blur animate-in fade-in slide-in-from-top-4 duration-200">
          {toastMsg}
        </div>
      )}

      {/* 左侧功能导航栏 */}
      <aside className="w-56 bg-neutral-900/90 border-r border-neutral-800 flex flex-col justify-between p-3 shrink-0">
        <div className="space-y-4">
          {/* Logo 与标题 */}
          <div className="flex items-center gap-2.5 px-2 py-1">
            <div className="w-8 h-8 rounded-lg bg-gradient-to-tr from-amber-600 to-amber-400 flex items-center justify-center shadow-lg shadow-amber-600/30">
              <Shield className="w-5 h-5 text-neutral-950" />
            </div>
            <div>
              <div className="text-xs font-bold tracking-wider text-amber-300">MH 荒野弹幕</div>
              <div className="text-[10px] text-neutral-400 font-mono">
                Tools V2 旗舰版{appVersion ? ` (v ${appVersion})` : ""}
              </div>
            </div>
          </div>

          {/* 导航按钮 */}
          <nav className="space-y-1">
            <button
              onClick={() => setActiveTab("queue")}
              className={`w-full flex items-center gap-2.5 px-3 py-2 rounded-lg text-xs font-bold transition ${
                activeTab === "queue"
                  ? "bg-amber-500/20 text-amber-300 border border-amber-500/30"
                  : "text-neutral-400 hover:bg-neutral-800/60 hover:text-neutral-200"
              }`}
            >
              <Shield className="w-4 h-4" />
              <span>排队管理</span>
              <span className="ml-auto text-[10px] bg-neutral-800 px-1.5 py-0.5 rounded font-mono">
                {queue.length}
              </span>
            </button>

            <button
              onClick={() => setActiveTab("monster")}
              className={`w-full flex items-center gap-2.5 px-3 py-2 rounded-lg text-xs font-bold transition ${
                activeTab === "monster"
                  ? "bg-amber-500/20 text-amber-300 border border-amber-500/30"
                  : "text-neutral-400 hover:bg-neutral-800/60 hover:text-neutral-200"
              }`}
            >
              <ListPlus className="w-4 h-4" />
              <span>怪物名单</span>
              <span
                className={`ml-auto text-[10px] px-1.5 py-0.5 rounded font-mono ${
                  roster.items.length
                    ? "bg-red-500/20 text-red-300"
                    : "bg-neutral-800 text-neutral-400"
                }`}
                title={
                  roster.items.length
                    ? `禁点名单生效中：${roster.items.length} 个怪物不可被点`
                    : "禁点名单为空，不限制任何点怪"
                }
              >
                {roster.items.length}
              </span>
            </button>

            <button
              onClick={() => setActiveTab("bili")}
              className={`w-full flex items-center gap-2.5 px-3 py-2 rounded-lg text-xs font-bold transition ${
                activeTab === "bili"
                  ? "bg-amber-500/20 text-amber-300 border border-amber-500/30"
                  : "text-neutral-400 hover:bg-neutral-800/60 hover:text-neutral-200"
              }`}
            >
              <Radio className="w-4 h-4" />
              <span>直播长连</span>
              <span
                className={`ml-auto w-2 h-2 rounded-full ${
                  conn.state === "Connected"
                    ? "bg-emerald-400 shadow-emerald-500/50 shadow-sm"
                    : conn.state === "Connecting" || conn.state === "Reconnecting"
                    ? "bg-amber-400 animate-pulse"
                    : conn.state === "ReconnectFailed"
                    ? "bg-red-500"
                    : "bg-neutral-600"
                }`}
              />
            </button>

            <button
              onClick={() => setActiveTab("gm")}
              className={`w-full flex items-center gap-2.5 px-3 py-2 rounded-lg text-xs font-bold transition ${
                activeTab === "gm"
                  ? "bg-amber-500/20 text-amber-300 border border-amber-500/30"
                  : "text-neutral-400 hover:bg-neutral-800/60 hover:text-neutral-200"
              } ${isLite ? "opacity-40" : ""}`}
            >
              <CalendarCheck className="w-4 h-4" />
              <span>舰长打卡 & GM</span>
              {isLite && <span className="ml-auto text-[9px] text-amber-400">停用</span>}
            </button>

            <button
              onClick={() => setActiveTab("ai")}
              className={`w-full flex items-center gap-2.5 px-3 py-2 rounded-lg text-xs font-bold transition ${
                activeTab === "ai"
                  ? "bg-amber-500/20 text-amber-300 border border-amber-500/30"
                  : "text-neutral-400 hover:bg-neutral-800/60 hover:text-neutral-200"
              } ${isLite ? "opacity-40" : ""}`}
            >
              <Bot className="w-4 h-4" />
              <span>AI 思考互动</span>
              {isLite && <span className="ml-auto text-[9px] text-amber-400">停用</span>}
            </button>

            <button
              onClick={() => setActiveTab("logs")}
              className={`w-full flex items-center gap-2.5 px-3 py-2 rounded-lg text-xs font-bold transition ${
                activeTab === "logs"
                  ? "bg-amber-500/20 text-amber-300 border border-amber-500/30"
                  : "text-neutral-400 hover:bg-neutral-800/60 hover:text-neutral-200"
              }`}
            >
              <ScrollText className="w-4 h-4" />
              <span>运行日志</span>
              {resourceWarnings.length > 0 && (
                <span className="ml-auto text-[9px] bg-red-500/20 text-red-300 border border-red-500/40 px-1.5 py-0.5 rounded-full">
                  资源缺失
                </span>
              )}
            </button>

            <button
              onClick={() => setActiveTab("settings")}
              className={`w-full flex items-center gap-2.5 px-3 py-2 rounded-lg text-xs font-bold transition ${
                activeTab === "settings"
                  ? "bg-amber-500/20 text-amber-300 border border-amber-500/30"
                  : "text-neutral-400 hover:bg-neutral-800/60 hover:text-neutral-200"
              }`}
            >
              <Sliders className="w-4 h-4" />
              <span>设置中心</span>
            </button>
          </nav>
        </div>

        {/* 底部模式与快捷操作 */}
        <div className="space-y-2 border-t border-neutral-800 pt-3">
          <div className="bg-neutral-950/70 p-2.5 rounded-lg border border-neutral-800/80">
            <div className="flex items-center justify-between mb-1.5">
              <span className="text-xs font-bold text-neutral-300">Lite 纯排队模式</span>
              <button
                onClick={toggleLite}
                className={`relative inline-flex h-4 w-8 items-center rounded-full transition-colors ${
                  isLite ? "bg-amber-500" : "bg-neutral-700"
                }`}
              >
                <span
                  className={`inline-block h-3 w-3 transform rounded-full bg-white transition-transform ${
                    isLite ? "translate-x-4" : "translate-x-0.5"
                  }`}
                />
              </button>
            </div>
            <p className="text-[10px] text-neutral-500 leading-tight">
              {isLite ? "已停用 TTS、打卡和 AI 模块，超轻量运行" : "所有功能模块正常运行中"}
            </p>
          </div>

          <div className="bg-neutral-950/70 p-2.5 rounded-lg border border-neutral-800/80">
            <p className="text-[10px] text-neutral-500 leading-tight">
              OBS 推流：使用【窗口捕获】选择“桌面点怪悬浮窗”，无需额外服务。
            </p>
          </div>
        </div>
      </aside>

      {/* 右侧主视图区 */}
      <main className="flex-1 flex flex-col overflow-hidden bg-neutral-950">
        {/* 顶部状态提示栏 */}
        <header className="h-12 border-b border-neutral-800 px-6 flex items-center justify-between shrink-0 bg-neutral-900/40">
          <div className="flex items-center gap-3">
            <h1 className="text-sm font-bold text-neutral-200">
              {activeTab === "queue" && "点单排队管理"}
              {activeTab === "monster" && "怪物名单与禁点配置"}
              {activeTab === "bili" && "B 站开放平台直播间连接与监控"}
              {activeTab === "gm" && "舰长周打卡系统与 GM 运维管理"}
              {activeTab === "ai" && "DeepSeek-v4-flash 思考模式 AI 对话"}
              {activeTab === "settings" && "系统全局持久化参数设置"}
              {activeTab === "logs" && "运行日志与可观测诊断"}
            </h1>
            {isLite && (
              <span className="text-[10px] bg-amber-500/20 text-amber-300 border border-amber-500/40 font-bold px-2 py-0.5 rounded-full">
                Lite 纯排队模式已启用
              </span>
            )}
          </div>

          <div className="flex items-center gap-2">
            <button
              onClick={toggleOverlayWindow}
              className="flex items-center gap-1.5 text-[11px] bg-neutral-800 hover:bg-neutral-700 text-neutral-200 px-3 py-1.5 rounded-lg border border-neutral-700/60 transition cursor-pointer"
              title="切换桌面点怪悬浮窗显示/隐藏状态"
            >
              <Eye className="w-3.5 h-3.5 text-amber-400" />
              <span>桌面点怪悬浮窗</span>
            </button>
          </div>
        </header>

        {/* 标签页内容渲染 */}
        <div className="flex-1 overflow-y-auto p-6 scrollbar-thin scrollbar-thumb-neutral-800">
          {/* 1. 队列管理 TAB */}
          {activeTab === "queue" && (
            <div className="ml-scope">
              <header className="page-head">
                <div>
                  <h1>点单排队管理</h1>
                  <p className="sub">
                    点图标即点怪 · 受「<b>怪物名单</b>」约束（名单内的怪已禁点，弹幕与面板一致）· 与弹幕点单写入同一条队列
                  </p>
                </div>
              </header>

              <div className="editor-main">
                <section className="card op-panel">
                  <MonsterPickerPanel
                    dict={monsterDict}
                    roster={roster}
                    submitting={orderSubmitting}
                    onSubmit={handlePickerOrder}
                  />
                </section>

                <aside className="card queue-card">
                  <header>
                    <span className="ttl">当前排队列表</span>
                    <span className="cnt">{queue.length}</span>
                    <span style={{ flex: 1 }} />
                    <button
                      className="btn sm"
                      onClick={() => {
                        setActiveTab("monster");
                      }}
                    >
                      配置怪物名单
                    </button>
                    <button
                      className="btn sm danger"
                      disabled={queue.length === 0}
                      onClick={handleClear}
                    >
                      清空队列
                    </button>
                  </header>
                  <div className="flex-1 min-h-0 overflow-y-auto p-2.5">
                    {queue.length === 0 ? (
                      <div className="py-20 flex flex-col items-center justify-center text-neutral-500 gap-2">
                        <Shield className="w-10 h-10 text-neutral-700 mb-1" />
                        <span className="text-xs">暂无水友排队，等待弹幕发送【点怪 怪物名】</span>
                      </div>
                    ) : (
                      <VirtualList
                        items={queue}
                        rowHeight={QUEUE_ROW_HEIGHT}
                        className="h-full overflow-y-auto"
                        renderItem={(item, idx) => (
                          <div
                            key={item.id}
                            draggable
                            onDragStart={(e) => handleItemDragStart(e, idx)}
                            onDragOver={handleItemDragOver}
                            onDrop={(e) => handleItemDrop(e, idx)}
                            style={{ height: QUEUE_ROW_HEIGHT - 8, marginBottom: 8 }}
                            className={`flex items-center justify-between p-2.5 rounded-xl border transition-all select-none ${
                              draggedIndex === idx ? "opacity-40 scale-95 border-dashed border-amber-400" : ""
                            } ${
                              item.tempered_level === 2
                                ? "bg-gradient-to-r from-orange-950/50 via-red-950/30 to-black/60 border-orange-500/80 arch-tempered-glow text-orange-200"
                                : item.tempered_level === 1
                                ? "bg-gradient-to-r from-purple-950/50 via-indigo-950/30 to-black/60 border-purple-500/70 tempered-glow text-purple-200"
                                : item.is_priority
                                ? "bg-red-950/40 border-red-500/50 text-red-200"
                                : "bg-neutral-950/80 border-neutral-800 text-neutral-200"
                            }`}
                          >
                            <div className="flex items-center gap-2.5 min-w-0">
                              <div
                                title="按住上下拖拽调整排队顺序"
                                className="p-1 text-neutral-500 hover:text-amber-400 cursor-grab active:cursor-grabbing shrink-0"
                              >
                                <GripVertical className="w-4 h-4" />
                              </div>

                              <span className="font-mono text-sm font-bold text-amber-400 w-5 text-center shrink-0">
                                #{idx + 1}
                              </span>

                              {item.icon_url ? (
                                <img
                                  src={`/monster_icons/${item.icon_url}`}
                                  alt={item.monster_name}
                                  onError={(e) => {
                                    (e.target as HTMLElement).style.display = "none";
                                  }}
                                  className="w-8 h-8 rounded border border-neutral-700/60 object-contain bg-black/60"
                                />
                              ) : (
                                <div className="w-8 h-8 rounded border border-neutral-700/60 bg-black/60 flex items-center justify-center">
                                  <Shield className="w-4 h-4 text-neutral-500" />
                                </div>
                              )}

                              <div className="min-w-0">
                                <div className="flex items-center gap-2">
                                  <MarqueeText
                                    text={item.monster_name}
                                    className="text-xs font-bold text-white max-w-[14rem]"
                                  />
                                  {item.tempered_level === 2 && (
                                    <span className="flex items-center gap-0.5 text-[9px] bg-orange-600 text-white font-bold px-1.5 py-0.2 rounded">
                                      <Flame className="w-2.5 h-2.5" /> 历战王
                                    </span>
                                  )}
                                  {item.tempered_level === 1 && (
                                    <span className="flex items-center gap-0.5 text-[9px] bg-purple-600 text-white font-bold px-1.5 py-0.2 rounded">
                                      <Sparkles className="w-2.5 h-2.5" /> 历战
                                    </span>
                                  )}
                                  {item.guard_level === 1 && (
                                    <span className="text-[9px] bg-gradient-to-r from-red-600 to-amber-500 text-white font-bold px-1 rounded">
                                      总督
                                    </span>
                                  )}
                                  {item.guard_level === 2 && (
                                    <span className="text-[9px] bg-gradient-to-r from-purple-600 to-pink-500 text-white font-bold px-1 rounded">
                                      提督
                                    </span>
                                  )}
                                  {item.guard_level === 3 && (
                                    <span className="text-[9px] bg-gradient-to-r from-blue-600 to-cyan-500 text-white font-bold px-1 rounded">
                                      舰长
                                    </span>
                                  )}
                                  {item.is_priority && (
                                    <span className="text-[9px] bg-red-600 text-white font-bold px-1 rounded animate-pulse">
                                      优先
                                    </span>
                                  )}
                                </div>
                                <MarqueeText
                                  text={`水友: ${item.user_name}`}
                                  className="text-[11px] text-neutral-400 max-w-[14rem]"
                                />
                              </div>
                            </div>

                            <button
                              onClick={() => handleDelete(item.user_id)}
                              className="px-3 py-1 bg-neutral-800 hover:bg-red-600 hover:text-white text-neutral-400 text-xs font-bold rounded-lg transition shrink-0"
                            >
                              完成并出队
                            </button>
                          </div>
                        )}
                      />
                    )}
                  </div>
                </aside>
              </div>
            </div>
          )}

          {/* 2. 怪物名单 TAB */}
          {activeTab === "monster" && (
            <MonsterListTab
              dict={monsterDict}
              roster={roster}
              onRosterChange={commitRoster}
              onDictChanged={refreshMonsterData}
              onImport={handleImportRoster}
              onExport={handleExportRoster}
              toast={showToast}
              askConfirm={askConfirm}
            />
          )}


          {/* 3. 直播长连 TAB */}
          {activeTab === "bili" && (
            <div className="max-w-3xl space-y-6">
              <div className="bg-neutral-900/70 border border-neutral-800 rounded-xl p-5 space-y-4">
                <div className="flex items-center justify-between pb-3 border-b border-neutral-800">
                  <div>
                    <h2 className="text-sm font-bold text-neutral-200">B 站直播开放平台状态</h2>
                    <p className="text-xs text-neutral-400">实时长连接与 ProtoUtils 双向封包通信</p>
                  </div>

                  <div className="flex items-center gap-2">
                    <span
                      className={`w-2.5 h-2.5 rounded-full ${
                        conn.state === "Connected"
                          ? "bg-emerald-400 shadow-emerald-400/50 shadow-md animate-pulse"
                          : conn.state === "Connecting" || conn.state === "Reconnecting"
                          ? "bg-amber-400 animate-pulse"
                          : conn.state === "ReconnectFailed"
                          ? "bg-red-500"
                          : "bg-neutral-600"
                      }`}
                    />
                    <span
                      className={`text-xs font-bold ${
                        conn.state === "Connected"
                          ? "text-emerald-400"
                          : conn.state === "ReconnectFailed"
                          ? "text-red-400"
                          : conn.state === "Connecting" || conn.state === "Reconnecting"
                          ? "text-amber-300"
                          : "text-neutral-400"
                      }`}
                    >
                      {conn.display}
                    </span>
                  </div>
                </div>

                {/* 开播身份码与开启长连合一控制区 */}
                <div className="bg-neutral-950/80 border border-neutral-800 rounded-xl p-4 space-y-3">
                  <div className="flex items-center justify-between">
                    <label className="text-xs font-bold text-amber-300 flex items-center gap-1.5">
                      <Key className="w-3.5 h-3.5 text-amber-400" />
                      <span>开播身份码 (id_code) 与直播长连</span>
                    </label>
                    <span className="text-[10px] text-neutral-500 font-mono">
                      HKCU\Software\MonsterOrderWilds\IdCode
                    </span>
                  </div>

                  {/* 身份码输入框 + 保存 + 开启直播长连按钮紧密并排 */}
                  <div className="flex items-center gap-2.5">
                    <div className="relative flex-1">
                      <input
                        type="password"
                        value={config?.id_code || ""}
                        onChange={(e) => config && setConfig({ ...config, id_code: e.target.value })}
                        placeholder="在此输入或粘贴当次开播身份码 (id_code)..."
                        className="w-full bg-neutral-900 border border-neutral-700 rounded-lg px-3 py-2 text-xs text-neutral-100 placeholder-neutral-500 focus:border-amber-400 focus:outline-none"
                      />
                    </div>

                    <button
                      type="button"
                      onClick={handleSaveIdCode}
                      className="px-3 py-2 bg-neutral-800 hover:bg-neutral-700 text-neutral-200 text-xs font-bold rounded-lg border border-neutral-700 transition flex items-center gap-1.5 shrink-0"
                      title="单独将身份码保存至 Windows 注册表"
                    >
                      <Save className="w-3.5 h-3.5 text-amber-400" />
                      <span>保存</span>
                    </button>

                    <button
                      type="button"
                      onClick={toggleBiliConnect}
                      className={`px-4 py-2 rounded-lg text-xs font-bold flex items-center gap-2 transition shrink-0 shadow-lg ${
                        conn.state === "Connected"
                          ? "bg-red-500/20 text-red-300 border border-red-500/40 hover:bg-red-500/30"
                          : "bg-emerald-600 hover:bg-emerald-500 text-white shadow-emerald-600/30"
                      }`}
                    >
                      <Power className="w-4 h-4" />
                      <span>
                        {conn.state === "Connected"
                          ? "断开长连"
                          : conn.state === "Connecting" || conn.state === "Reconnecting"
                          ? "取消连接"
                          : "开启直播长连"}
                      </span>
                    </button>
                  </div>

                  <p className="text-[11px] text-neutral-400 leading-relaxed">
                    提示：填入身份码后点击【开启直播长连】会自动同步保存至 Windows 注册表；此处留空则直接沿用注册表
                    HKCU\Software\MonsterOrderWilds\IdCode 中已保存的身份码（出于安全不回显）。
                  </p>
                </div>
              </div>

              {/* 弹幕调试与模拟测试器 */}
              <div className="bg-neutral-900/70 border border-neutral-800 rounded-xl p-5 space-y-4">
                <h3 className="text-xs font-bold text-amber-300">本地弹幕模拟测试通道</h3>
                <form onSubmit={handleSimDanmu} className="space-y-3">
                  <div className="grid grid-cols-2 gap-3">
                    <div>
                      <label className="block text-[11px] font-bold text-neutral-400 mb-1">模拟昵称</label>
                      <input
                        type="text"
                        value={simDanmuUser}
                        onChange={(e) => setSimDanmuUser(e.target.value)}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-100"
                      />
                    </div>
                    <div>
                      <label className="block text-[11px] font-bold text-neutral-400 mb-1">弹幕内容</label>
                      <input
                        type="text"
                        value={simDanmuMsg}
                        onChange={(e) => setSimDanmuMsg(e.target.value)}
                        placeholder="例: 点怪 霸主太太 / 优先 / 打卡..."
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-100"
                      />
                    </div>
                  </div>

                  <div className="grid grid-cols-2 gap-3">
                    <div>
                      <label className="block text-[11px] font-bold text-neutral-400 mb-1">舰长等级</label>
                      <select
                        value={simGuardLevel}
                        onChange={(e) => setSimGuardLevel(Number(e.target.value))}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-2.5 py-1.5 text-xs text-neutral-200"
                      >
                        <option value={0}>无（普通水友）</option>
                        <option value={1}>总督</option>
                        <option value={2}>提督</option>
                        <option value={3}>舰长</option>
                      </select>
                    </div>
                    <div>
                      <label className="block text-[11px] font-bold text-neutral-400 mb-1">粉丝牌（佩戴 / 等级）</label>
                      <div className="flex items-center gap-2">
                        <label className="flex items-center gap-1.5 text-xs text-neutral-300 shrink-0">
                          <input
                            type="checkbox"
                            checked={simHasMedal}
                            onChange={(e) => setSimHasMedal(e.target.checked)}
                            className="accent-amber-500"
                          />
                          <span>佩戴</span>
                        </label>
                        <input
                          type="number"
                          min="0"
                          max="40"
                          value={simMedalLevel}
                          disabled={!simHasMedal}
                          onChange={(e) => setSimMedalLevel(Math.max(0, Number(e.target.value)))}
                          className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-2.5 py-1.5 text-xs text-neutral-100 disabled:opacity-40"
                        />
                      </div>
                    </div>
                  </div>

                  <button
                    type="submit"
                    className="bg-neutral-800 hover:bg-neutral-700 text-neutral-200 font-bold px-4 py-1.5 rounded-lg text-xs flex items-center gap-1.5"
                  >
                    <MessageSquare className="w-3.5 h-3.5" />
                    <span>发射模拟弹幕</span>
                  </button>
                </form>

                {/* 点赞奖卡模拟（走同一点赞管道：msg_id 去重 + 奖卡播报） */}
                <form onSubmit={handleSimLike} className="space-y-3 pt-3 border-t border-neutral-800">
                  <div className="grid grid-cols-2 gap-3">
                    <div>
                      <label className="block text-[11px] font-bold text-neutral-400 mb-1">点赞昵称</label>
                      <input
                        type="text"
                        value={simLikeUser}
                        onChange={(e) => setSimLikeUser(e.target.value)}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-100"
                      />
                    </div>
                    <div>
                      <label className="block text-[11px] font-bold text-neutral-400 mb-1">点赞次数</label>
                      <input
                        type="number"
                        min="1"
                        max="10000"
                        value={simLikeCount}
                        onChange={(e) => setSimLikeCount(Math.max(1, Number(e.target.value)))}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-100"
                      />
                    </div>
                  </div>
                  <button
                    type="submit"
                    disabled={isLite}
                    className="bg-neutral-800 hover:bg-neutral-700 text-neutral-200 font-bold px-4 py-1.5 rounded-lg text-xs flex items-center gap-1.5 disabled:opacity-40"
                  >
                    <Sparkles className="w-3.5 h-3.5" />
                    <span>发射模拟点赞（30 次触发奖卡）</span>
                  </button>
                </form>

                {/* 直播间事件模拟（B8 礼物连击 / B2 SC·上舰；复用「模拟昵称」作为 uname） */}
                <div className="space-y-2.5 pt-3 border-t border-neutral-800">
                  <div className="text-[11px] font-bold text-neutral-400">
                    直播间事件模拟（复用上方「模拟昵称」；Lite 下停用）
                  </div>

                  <form onSubmit={handleSimGift} className="flex flex-wrap items-end gap-2">
                    <div className="flex-1 min-w-24">
                      <label className="block text-[10px] text-neutral-500 mb-1">礼物名</label>
                      <input
                        type="text"
                        value={simGiftName}
                        onChange={(e) => setSimGiftName(e.target.value)}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-2.5 py-1.5 text-xs text-neutral-100"
                      />
                    </div>
                    <div className="w-20">
                      <label className="block text-[10px] text-neutral-500 mb-1">数量</label>
                      <input
                        type="number"
                        min="1"
                        value={simGiftNum}
                        onChange={(e) => setSimGiftNum(Math.max(1, Number(e.target.value)))}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-2.5 py-1.5 text-xs text-neutral-100"
                      />
                    </div>
                    <label className="flex items-center gap-1.5 text-xs text-neutral-300 py-1.5">
                      <input
                        type="checkbox"
                        checked={simGiftPaid}
                        onChange={(e) => setSimGiftPaid(e.target.checked)}
                        className="accent-amber-500"
                      />
                      <span>付费</span>
                    </label>
                    <button
                      type="submit"
                      disabled={isLite}
                      className="bg-neutral-800 hover:bg-neutral-700 text-neutral-200 font-bold px-3 py-1.5 rounded-lg text-xs flex items-center gap-1.5 disabled:opacity-40"
                    >
                      <Gift className="w-3.5 h-3.5" />
                      <span>模拟礼物</span>
                    </button>
                  </form>

                  <form onSubmit={handleSimSuperChat} className="flex flex-wrap items-end gap-2">
                    <div className="w-20">
                      <label className="block text-[10px] text-neutral-500 mb-1">金额 (¥)</label>
                      <input
                        type="number"
                        min="1"
                        value={simScRmb}
                        onChange={(e) => setSimScRmb(Math.max(1, Number(e.target.value)))}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-2.5 py-1.5 text-xs text-neutral-100"
                      />
                    </div>
                    <div className="flex-1 min-w-24">
                      <label className="block text-[10px] text-neutral-500 mb-1">内容</label>
                      <input
                        type="text"
                        value={simScMessage}
                        onChange={(e) => setSimScMessage(e.target.value)}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-2.5 py-1.5 text-xs text-neutral-100"
                      />
                    </div>
                    <button
                      type="submit"
                      disabled={isLite}
                      className="bg-neutral-800 hover:bg-neutral-700 text-neutral-200 font-bold px-3 py-1.5 rounded-lg text-xs flex items-center gap-1.5 disabled:opacity-40"
                    >
                      <Flame className="w-3.5 h-3.5" />
                      <span>模拟 SC</span>
                    </button>
                  </form>

                  <form onSubmit={handleSimGuard} className="flex flex-wrap items-end gap-2">
                    <div className="w-28">
                      <label className="block text-[10px] text-neutral-500 mb-1">舰长等级</label>
                      <select
                        value={simGuardEventLevel}
                        onChange={(e) => setSimGuardEventLevel(Number(e.target.value))}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-2.5 py-1.5 text-xs text-neutral-200"
                      >
                        <option value={1}>总督</option>
                        <option value={2}>提督</option>
                        <option value={3}>舰长</option>
                      </select>
                    </div>
                    <div className="w-20">
                      <label className="block text-[10px] text-neutral-500 mb-1">数量</label>
                      <input
                        type="number"
                        min="1"
                        value={simGuardEventNum}
                        onChange={(e) => setSimGuardEventNum(Math.max(1, Number(e.target.value)))}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-2.5 py-1.5 text-xs text-neutral-100"
                      />
                    </div>
                    <div className="w-20">
                      <label className="block text-[10px] text-neutral-500 mb-1">单位</label>
                      <input
                        type="text"
                        value={simGuardEventUnit}
                        onChange={(e) => setSimGuardEventUnit(e.target.value)}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-2.5 py-1.5 text-xs text-neutral-100"
                      />
                    </div>
                    <button
                      type="submit"
                      disabled={isLite}
                      className="bg-neutral-800 hover:bg-neutral-700 text-neutral-200 font-bold px-3 py-1.5 rounded-lg text-xs flex items-center gap-1.5 disabled:opacity-40"
                    >
                      <Shield className="w-3.5 h-3.5" />
                      <span>模拟上舰</span>
                    </button>
                  </form>
                </div>
              </div>

              {/* D4 主播控制台实时动态：原始弹幕 + 打卡记录 */}
              <div className="grid grid-cols-12 gap-6">
                <div className="col-span-7 bg-neutral-900/70 border border-neutral-800 rounded-xl p-5 space-y-3">
                  <div className="flex items-center justify-between pb-2 border-b border-neutral-800">
                    <h3 className="text-xs font-bold text-amber-300 flex items-center gap-1.5">
                      <MessageSquare className="w-4 h-4" />
                      <span>最近弹幕动态</span>
                    </h3>
                    <span className="text-[10px] text-neutral-500">最近 20 条</span>
                  </div>
                  <div className="max-h-56 overflow-y-auto space-y-1.5 scrollbar-thin scrollbar-thumb-neutral-800">
                    {recentDanmu.length === 0 ? (
                      <p className="text-[11px] text-neutral-500 py-4 text-center">
                        暂无弹幕，连接长连或使用上方模拟通道后此处实时刷新
                      </p>
                    ) : (
                      recentDanmu.map((d, idx) => (
                        <div
                          key={`${d.msg_id}-${idx}`}
                          className="flex items-center gap-2 text-[11px] bg-neutral-950/70 border border-neutral-800/80 rounded px-2 py-1.5"
                        >
                          <span className="text-amber-300 font-bold shrink-0">{d.user_name}</span>
                          {d.guard_level > 0 && (
                            <span className="text-[9px] bg-blue-600/70 text-white px-1 rounded shrink-0">
                              舰长{d.guard_level}
                            </span>
                          )}
                          {d.has_medal && d.guard_level === 0 && (
                            <span className="text-[9px] bg-emerald-700/60 text-white px-1 rounded shrink-0">
                              粉丝牌
                            </span>
                          )}
                          <span className="text-neutral-300 truncate">{d.message}</span>
                        </div>
                      ))
                    )}
                  </div>
                </div>

                <div className="col-span-5 bg-neutral-900/70 border border-neutral-800 rounded-xl p-5 space-y-3">
                  <div className="flex items-center justify-between pb-2 border-b border-neutral-800">
                    <h3 className="text-xs font-bold text-amber-300 flex items-center gap-1.5">
                      <CalendarCheck className="w-4 h-4" />
                      <span>打卡动态</span>
                    </h3>
                    <span className="text-[10px] text-neutral-500">最近 10 条</span>
                  </div>
                  <div className="max-h-56 overflow-y-auto space-y-1.5 scrollbar-thin scrollbar-thumb-neutral-800">
                    {recentCheckins.length === 0 ? (
                      <p className="text-[11px] text-neutral-500 py-4 text-center">
                        暂无打卡记录
                      </p>
                    ) : (
                      recentCheckins.map((p, idx) => (
                        <div
                          key={`${p.uid}-${p.last_checkin_date}-${idx}`}
                          className="flex items-center justify-between text-[11px] bg-neutral-950/70 border border-neutral-800/80 rounded px-2 py-1.5"
                        >
                          <span className="text-emerald-300 font-bold">{p.username}</span>
                          <span className="text-neutral-400 font-mono">
                            连续 {p.continuous_days} 天 / 累计 {p.cumulative_days} 天
                          </span>
                        </div>
                      ))
                    )}
                  </div>
                </div>
              </div>
            </div>
          )}

          {/* 4. 舰长打卡 & GM 运维 TAB */}
          {activeTab === "gm" && (
            <div className="space-y-6">
              {isLite && (
                <div className="bg-amber-950/40 border border-amber-500/50 rounded-xl p-4 flex items-center gap-3 text-amber-200 text-xs">
                  <AlertCircle className="w-5 h-5 text-amber-400 shrink-0" />
                  <span>当前处于 ONLY_ORDER_MONSTER (Lite 模式)，打卡与 GM 运维模块已冻结。如需使用请在左下角关闭 Lite 模式。</span>
                </div>
              )}

              <div className="grid grid-cols-12 gap-6">
                {/* 运维指令卡片 */}
                <div className="col-span-6 bg-neutral-900/70 border border-neutral-800 rounded-xl p-5 space-y-4">
                  <h3 className="text-xs font-bold text-amber-300 flex items-center gap-1.5">
                    <CalendarCheck className="w-4 h-4" />
                    <span>一键黑幕批量补签</span>
                  </h3>
                  <p className="text-xs text-neutral-400 leading-relaxed">
                    遍历所有累计打卡天数大于连续天数的水友，自动补齐历史断签日期，使其连续打卡拉平到今天。
                  </p>

                  <button
                    onClick={handleBatchCheckin}
                    disabled={isLite}
                    className="bg-purple-600 hover:bg-purple-500 text-white font-bold px-4 py-2 rounded-lg text-xs shadow-lg shadow-purple-600/30 transition disabled:opacity-40"
                  >
                    执行一键黑幕批量补签
                  </button>

                  {batchResult && (
                    <div className="p-3 bg-neutral-950 rounded-lg border border-purple-500/40 text-xs text-purple-200 space-y-1">
                      <div>覆盖总用户: {batchResult.total_users}</div>
                      <div>补签修复用户: {batchResult.patched_users}</div>
                      <div>跳过用户: {batchResult.skipped_users}（已连续到今天）</div>
                      <div>插入明细记录: {batchResult.total_inserted} 条</div>
                    </div>
                  )}
                </div>

                {/* 记录导出卡片 */}
                <div className="col-span-6 bg-neutral-900/70 border border-neutral-800 rounded-xl p-5 space-y-4">
                  <h3 className="text-xs font-bold text-amber-300 flex items-center gap-1.5">
                    <Download className="w-4 h-4" />
                    <span>打卡数据导出</span>
                  </h3>
                  <p className="text-xs text-neutral-400 leading-relaxed">
                    将打卡数据导出为 CSV / JSON 文件（UTF-8 BOM，Excel 直接打开不乱码）。
                    不填昵称与日期时导出全员打卡总览（连续与累计天数）；指定昵称或日期范围时导出明细流水记录，通过系统原生对话框选择保存路径。
                  </p>

                  <div className="grid grid-cols-2 gap-3">
                    <div>
                      <label className="block text-[11px] font-bold text-neutral-400 mb-1">导出格式</label>
                      <select
                        value={exportFormat}
                        onChange={(e) => setExportFormat(e.target.value as "csv" | "json")}
                        disabled={isLite}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-100 disabled:opacity-40"
                      >
                        <option value="csv">CSV</option>
                        <option value="json">JSON</option>
                      </select>
                    </div>
                    <div>
                      <label className="block text-[11px] font-bold text-neutral-400 mb-1">筛选昵称（可选）</label>
                      <input
                        type="text"
                        value={exportUsername}
                        onChange={(e) => setExportUsername(e.target.value)}
                        placeholder="支持部分匹配"
                        disabled={isLite}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-100 disabled:opacity-40"
                      />
                    </div>
                    <div>
                      <label className="block text-[11px] font-bold text-neutral-400 mb-1">开始日期（可选）</label>
                      <input
                        type="date"
                        value={exportStartDate}
                        onChange={(e) => setExportStartDate(e.target.value)}
                        disabled={isLite}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-100 disabled:opacity-40"
                      />
                    </div>
                    <div>
                      <label className="block text-[11px] font-bold text-neutral-400 mb-1">结束日期（可选）</label>
                      <input
                        type="date"
                        value={exportEndDate}
                        onChange={(e) => setExportEndDate(e.target.value)}
                        disabled={isLite}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-100 disabled:opacity-40"
                      />
                    </div>
                  </div>

                  <button
                    onClick={handleExportRecords}
                    disabled={isLite}
                    className="bg-emerald-600 hover:bg-emerald-500 text-white font-bold px-4 py-2 rounded-lg text-xs shadow-lg shadow-emerald-600/30 transition disabled:opacity-40 flex items-center gap-1.5"
                  >
                    <Download className="w-3.5 h-3.5" />
                    <span>导出并选择保存位置</span>
                  </button>
                </div>

                {/* 水友查询与手动发卡 */}
                <div className="col-span-12 bg-neutral-900/70 border border-neutral-800 rounded-xl p-5 space-y-4">
                  <h3 className="text-xs font-bold text-amber-300 flex items-center gap-1.5">
                    <Users className="w-4 h-4" />
                    <span>水友打卡档案模糊搜索 & 手动发卡</span>
                  </h3>

                  <form onSubmit={handleSearchUsers} className="flex gap-2">
                    <input
                      type="text"
                      value={searchKeyword}
                      onChange={(e) => setSearchKeyword(e.target.value)}
                      placeholder="输入水友昵称关键字或 UID 进行模糊搜索..."
                      disabled={isLite}
                      className="flex-1 bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-100 disabled:opacity-40"
                    />
                    <div className="flex items-center gap-1.5 bg-neutral-950 border border-neutral-800 rounded-lg px-2.5">
                      <span className="text-[11px] text-neutral-400">发卡数量:</span>
                      <input
                        type="number"
                        min="1"
                        max="20"
                        value={grantCardAmount}
                        onChange={(e) => setGrantCardAmount(Math.max(1, Number(e.target.value)))}
                        disabled={isLite}
                        className="w-12 bg-transparent text-xs text-amber-300 font-bold focus:outline-none"
                      />
                    </div>
                    <button
                      type="submit"
                      disabled={isLite}
                      className="bg-neutral-800 hover:bg-neutral-700 text-neutral-200 px-4 py-1.5 rounded-lg text-xs font-bold flex items-center gap-1 disabled:opacity-40"
                    >
                      <Search className="w-3.5 h-3.5" />
                      <span>搜索</span>
                    </button>
                  </form>

                  {searchedUsers.length > 0 && (
                    <div className="border border-neutral-800 rounded-lg overflow-hidden max-h-64 overflow-y-auto">
                      <table className="w-full text-left text-xs">
                        <thead className="bg-neutral-950 text-neutral-400 border-b border-neutral-800 font-bold">
                          <tr>
                            <th className="p-2.5">UID</th>
                            <th className="p-2.5">水友昵称</th>
                            <th className="p-2.5">连续天数</th>
                            <th className="p-2.5">累计天数</th>
                            <th className="p-2.5">补签卡</th>
                            <th className="p-2.5 text-right">GM 发放补签卡</th>
                          </tr>
                        </thead>
                        <tbody className="divide-y divide-neutral-800/60 text-neutral-200">
                          {searchedUsers.map((u) => (
                            <tr key={u.uid} className="hover:bg-neutral-800/40">
                              <td className="p-2.5 font-mono text-neutral-400">{u.uid}</td>
                              <td className="p-2.5 font-bold">{u.username}</td>
                              <td className="p-2.5 text-amber-400 font-bold">{u.continuous_days} 天</td>
                              <td className="p-2.5 text-neutral-400">{u.cumulative_days} 天</td>
                              <td className="p-2.5 text-emerald-400 font-bold">{u.card_count} 张</td>
                              <td className="p-2.5 text-right">
                                <button
                                  onClick={() => handleGrantCard(u.uid, u.username)}
                                  disabled={isLite}
                                  className="bg-amber-600/80 hover:bg-amber-500 text-black font-bold px-2 py-1 rounded text-[11px] inline-flex items-center gap-1 disabled:opacity-40"
                                >
                                  <Gift className="w-3 h-3" />
                                  <span>赠送 {grantCardAmount} 张补签卡</span>
                                </button>
                              </td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    </div>
                  )}
                </div>
              </div>
            </div>
          )}

          {/* 5. AI 思考互动 TAB */}
          {activeTab === "ai" && (
            <div className="max-w-3xl space-y-6">
              {isLite && (
                <div className="bg-amber-950/40 border border-amber-500/50 rounded-xl p-4 flex items-center gap-3 text-amber-200 text-xs">
                  <AlertCircle className="w-5 h-5 text-amber-400 shrink-0" />
                  <span>当前处于 ONLY_ORDER_MONSTER (Lite 模式)，AI 思考交互模块已停用。</span>
                </div>
              )}

              <div className="bg-neutral-900/70 border border-neutral-800 rounded-xl p-5 space-y-4">
                <div className="flex items-center gap-2 pb-2 border-b border-neutral-800">
                  <Bot className="w-5 h-5 text-cyan-400" />
                  <h3 className="text-xs font-bold text-neutral-200">DeepSeek-v4-flash 思考模式问答</h3>
                </div>

                <form onSubmit={handleAskAi} className="space-y-3">
                  <textarea
                    value={aiPrompt}
                    onChange={(e) => setAiPrompt(e.target.value)}
                    placeholder="向随从猫 AI 专家发起提问（例如：荒野片手剑怎么开荒？太刀见切时机？）..."
                    disabled={isLite || aiLoading}
                    rows={3}
                    className="w-full bg-neutral-950 border border-neutral-800 rounded-lg p-3 text-xs text-neutral-100 placeholder-neutral-600 focus:outline-none focus:border-cyan-500/50 disabled:opacity-40"
                  />

                  <button
                    type="submit"
                    disabled={isLite || aiLoading}
                    className="bg-cyan-600 hover:bg-cyan-500 text-black font-bold px-5 py-2 rounded-lg text-xs flex items-center gap-2 shadow-lg shadow-cyan-600/20 transition disabled:opacity-40"
                  >
                    <Sparkles className="w-4 h-4" />
                    <span>{aiLoading ? "正在沉思中..." : "启动思考并推送到悬浮窗"}</span>
                  </button>
                </form>

                {aiResult && (
                  <div className="mt-4 p-4 bg-neutral-950 rounded-xl border border-cyan-500/40 space-y-2">
                    {aiResult.reasoning && (
                      <div className="text-[11px] text-cyan-300/70 bg-neutral-900/80 p-2.5 rounded border border-neutral-800 italic">
                        <div className="font-bold mb-1">思考过程 (Reasoning):</div>
                        {aiResult.reasoning}
                      </div>
                    )}
                    <div className="text-xs text-neutral-100 font-medium leading-relaxed">
                      <div className="text-cyan-400 font-bold mb-1">最终回答:</div>
                      {aiResult.answer}
                    </div>
                  </div>
                )}
              </div>
            </div>
          )}

          {/* 6. 设置面板 TAB */}
          {activeTab === "settings" && config && (
            <form onSubmit={handleSaveConfig} className="max-w-4xl space-y-6 pb-12">
              {/* 敏感凭证安全托管状态卡片 (禁止手动修改) */}
              <div className="bg-gradient-to-r from-neutral-900/80 via-emerald-950/20 to-neutral-900/80 border border-emerald-500/30 rounded-xl p-5 space-y-3">
                <div className="flex items-center justify-between pb-2 border-b border-emerald-500/20">
                  <div className="flex items-center gap-2">
                    <Lock className="w-4 h-4 text-emerald-400" />
                    <h3 className="text-xs font-bold text-emerald-300">
                      敏感凭据加密托管 (credentials.dat)
                    </h3>
                  </div>
                  <span className="text-[10px] bg-emerald-500/15 text-emerald-300 border border-emerald-500/30 px-2 py-0.5 rounded-full font-mono flex items-center gap-1">
                    <FileCheck className="w-3 h-3 text-emerald-400" />
                    <span>{credStatus?.loaded ? "HMAC 签名校验通过" : "未检测到凭据文件"}</span>
                  </span>
                </div>

                <p className="text-[11px] text-neutral-400 leading-relaxed">
                  遵循原工程安全规范，APP ID、AccessKey、TTS Key 与 AI Key 均存储于本地加密配置文件 <span className="text-amber-200/80 font-mono">credentials.dat</span> 中，采用 Base64 + HMAC-SHA256 签名双重校验，<strong className="text-neutral-200">不允许手动设置或明文暴露</strong>。
                  安装包出于安全考虑<strong className="text-neutral-200">不随包分发该文件</strong>，请点击下方按钮导入由原工程生成（或随原始发行包提供）的凭据文件。
                </p>

                <div className="flex items-center gap-2 flex-wrap">
                  <button
                    type="button"
                    onClick={handleImportCredentials}
                    className="flex items-center gap-1.5 bg-emerald-600 hover:bg-emerald-500 text-white font-bold px-3 py-1.5 rounded-lg text-[11px] shadow-lg shadow-emerald-600/30 transition"
                  >
                    <Upload className="w-3.5 h-3.5" />
                    <span>导入凭据文件 (credentials.dat)</span>
                  </button>
                  <span className="text-[10px] font-mono text-neutral-500 break-all">
                    目标路径：{credStatus?.file_path || "—"}
                  </span>
                </div>

                <div className="grid grid-cols-2 sm:grid-cols-4 gap-3 pt-1">
                  <div className="bg-neutral-950/70 p-2.5 rounded-lg border border-neutral-800">
                    <span className="text-[10px] text-neutral-500 block">应用 APP ID</span>
                    <span className="text-xs font-mono font-bold text-neutral-200">
                      {credStatus?.app_id || "未加载"}
                    </span>
                  </div>

                  <div className="bg-neutral-950/70 p-2.5 rounded-lg border border-neutral-800">
                    <span className="text-[10px] text-neutral-500 flex items-center gap-1">
                      <Key className="w-2.5 h-2.5 text-amber-400" />
                      <span>AccessKey ID</span>
                    </span>
                    <span className="text-xs font-mono font-bold text-neutral-200">
                      {credStatus?.access_key_masked || "未加载"}
                    </span>
                  </div>

                  <div className="bg-neutral-950/70 p-2.5 rounded-lg border border-neutral-800">
                    <span className="text-[10px] text-neutral-500 block">AI 模型凭据</span>
                    <span className={`text-xs font-bold ${credStatus?.has_chat_key ? "text-emerald-300" : "text-neutral-500"}`}>
                      {credStatus?.has_chat_key ? `已绑定 (${credStatus.chat_provider})` : "未绑定"}
                    </span>
                  </div>

                  <div className="bg-neutral-950/70 p-2.5 rounded-lg border border-neutral-800">
                    <span className="text-[10px] text-neutral-500 block">多引擎语音</span>
                    <span className={`text-xs font-bold ${credStatus?.has_mimo_key || credStatus?.has_vip_tts_key ? "text-emerald-300" : "text-neutral-500"}`}>
                      {credStatus?.has_mimo_key ? "MiMo 已绑定" : "本地 SAPI"}
                    </span>
                  </div>
                </div>
              </div>

              {/* D1 多引擎 TTS 语音与音效 */}
              <div className={`bg-neutral-900/70 border border-neutral-800 rounded-xl p-5 space-y-4 ${isLite ? "opacity-40" : ""}`}>
                <div className="flex items-center justify-between pb-2 border-b border-neutral-800">
                  <h3 className="text-xs font-bold text-amber-300 flex items-center gap-1.5">
                    <Volume2 className="w-4 h-4" />
                    <span>多引擎 TTS 语音与音效</span>
                  </h3>
                  {isLite && <span className="text-[10px] text-amber-400">Lite 模式下已停用</span>}
                </div>

                {/* 总开关与播报过滤 */}
                <div className="grid grid-cols-2 md:grid-cols-4 gap-3">
                  <label className="flex items-center gap-2 text-xs text-neutral-300 font-bold cursor-pointer">
                    <input
                      type="checkbox"
                      checked={config.enable_voice}
                      disabled={isLite}
                      onChange={(e) => setConfig({ ...config, enable_voice: e.target.checked })}
                      className="rounded bg-neutral-950 border-neutral-800 text-amber-500 focus:ring-0"
                    />
                    <span>开启语音播报（总开关）</span>
                  </label>

                  <label className="flex items-center gap-2 text-xs text-neutral-300 font-bold cursor-pointer">
                    <input
                      type="checkbox"
                      checked={config.only_speek_wearing_medal}
                      disabled={isLite}
                      onChange={(e) => setConfig({ ...config, only_speek_wearing_medal: e.target.checked })}
                      className="rounded bg-neutral-950 border-neutral-800 text-amber-500 focus:ring-0"
                    />
                    <span>仅播报佩戴粉丝牌的弹幕</span>
                  </label>

                  <label className="flex items-center gap-2 text-xs text-neutral-300 font-bold cursor-pointer">
                    <input
                      type="checkbox"
                      checked={config.only_speek_paid_gift}
                      disabled={isLite}
                      onChange={(e) => setConfig({ ...config, only_speek_paid_gift: e.target.checked })}
                      className="rounded bg-neutral-950 border-neutral-800 text-amber-500 focus:ring-0"
                    />
                    <span>仅播报付费礼物</span>
                  </label>

                  <div>
                    <label className="block text-[11px] text-neutral-400 mb-1">播报至少等级（大航海）</label>
                    <select
                      value={config.only_speek_guard_level}
                      disabled={isLite}
                      onChange={(e) => setConfig({ ...config, only_speek_guard_level: Number(e.target.value) })}
                      className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-200 disabled:opacity-40"
                    >
                      <option value={0}>所有人</option>
                      <option value={3}>舰长</option>
                      <option value={2}>提督</option>
                      <option value={1}>总督</option>
                    </select>
                  </div>
                </div>

                {/* 引擎与当前实际引擎 */}
                <div className="grid grid-cols-2 md:grid-cols-4 gap-3 pt-3 border-t border-neutral-800">
                  <div>
                    <label className="block text-[11px] text-neutral-400 mb-1">TTS 引擎</label>
                    <select
                      value={config.tts_engine || "auto"}
                      disabled={isLite}
                      onChange={(e) => setConfig({ ...config, tts_engine: e.target.value })}
                      className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-200 disabled:opacity-40"
                    >
                      <option value="auto">自动（Manbo → MiMo → SAPI）</option>
                      <option value="manbo">Manbo</option>
                      <option value="mimo">小米 MiMo</option>
                      <option value="sapi">Windows SAPI</option>
                    </select>
                  </div>
                  <div className="flex items-end">
                    <span className="text-[11px] font-bold text-cyan-300 bg-cyan-950/40 border border-cyan-500/30 rounded-lg px-3 py-1.5">
                      当前引擎: {ENGINE_LABELS[currentEngine] || "未知"}
                    </span>
                  </div>
                  <div>
                    <label className="block text-[11px] text-neutral-400 mb-1">缓存保留天数</label>
                    <input
                      type="number"
                      min={1}
                      max={365}
                      value={config.tts_cache_days_to_keep}
                      disabled={isLite}
                      onChange={(e) =>
                        setConfig({
                          ...config,
                          tts_cache_days_to_keep: Math.min(365, Math.max(1, Number(e.target.value))),
                        })
                      }
                      className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-200 disabled:opacity-40"
                    />
                  </div>
                  <div>
                    <label className="block text-[11px] text-neutral-400 mb-1">
                      音色选择（{manboVoices.length || 184} 种）
                    </label>
                    <select
                      value={config.manbo_voice}
                      disabled={isLite}
                      onChange={(e) => setConfig({ ...config, manbo_voice: e.target.value })}
                      className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-200 disabled:opacity-40"
                    >
                      {(manboVoices.length > 0 ? manboVoices : [config.manbo_voice || "曼波"]).map((voice) => (
                        <option key={voice} value={voice}>
                          {voice === "曼波" ? "曼波（付费）" : voice}
                        </option>
                      ))}
                      {manboVoices.length > 0 && !manboVoices.includes(config.manbo_voice) && (
                        <option value={config.manbo_voice}>{config.manbo_voice}</option>
                      )}
                    </select>
                  </div>
                </div>

                {/* Manbo API Key：仅写注册表，永不回传明文 */}
                <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
                  <div>
                    <label className="block text-[11px] text-neutral-400 mb-1">
                      Manbo API Key（仅写入注册表，不落 JSON / 不回显）
                    </label>
                    <div className="flex items-center gap-2">
                      <input
                        type="password"
                        value={manboKeyInput}
                        disabled={isLite}
                        onChange={(e) => setManboKeyInput(e.target.value)}
                        placeholder={credStatus?.has_vip_tts_key ? "已绑定（留空则保持不变）" : "输入 Manbo API Key"}
                        className="flex-1 bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-100 placeholder-neutral-600 disabled:opacity-40"
                      />
                      <button
                        type="button"
                        onClick={handleSaveManboKey}
                        disabled={isLite}
                        className="px-3 py-1.5 bg-neutral-800 hover:bg-neutral-700 text-neutral-200 text-xs font-bold rounded-lg border border-neutral-700 transition shrink-0 disabled:opacity-40"
                      >
                        保存 Key
                      </button>
                    </div>
                  </div>
                  <div className="grid grid-cols-2 gap-3">
                    <div>
                      <label className="block text-[11px] text-neutral-400 mb-1">MiMo 语音角色</label>
                      <select
                        value={config.mimo_voice}
                        disabled={isLite}
                        onChange={(e) => setConfig({ ...config, mimo_voice: e.target.value })}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-200 disabled:opacity-40"
                      >
                        <option value="mimo_default">默认语音</option>
                        <option value="default_zh">中文语音</option>
                        <option value="default_en">英文语音</option>
                      </select>
                    </div>
                    <div>
                      <label className="block text-[11px] text-neutral-400 mb-1">MiMo 语音风格</label>
                      <select
                        value={config.mimo_style}
                        disabled={isLite}
                        onChange={(e) => setConfig({ ...config, mimo_style: e.target.value })}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-200 disabled:opacity-40"
                      >
                        <option value="">默认</option>
                        <option value="温柔轻声">温柔轻声</option>
                        <option value="激昂慷慨">激昂慷慨</option>
                        <option value="新闻播报">新闻播报</option>
                        <option value="欢乐活泼">欢乐活泼</option>
                        <option value="沉稳严肃">沉稳严肃</option>
                      </select>
                    </div>
                  </div>
                </div>

                {/* 语速 / 音量 / 音高 */}
                <div className="grid grid-cols-1 md:grid-cols-3 gap-3">
                  <div>
                    <label className="block text-[11px] text-neutral-400 mb-1">语音速率 ({config.speech_rate})</label>
                    <input
                      type="range"
                      min="-10"
                      max="10"
                      step="1"
                      value={config.speech_rate}
                      disabled={isLite}
                      onChange={(e) => applyConfigPatch({ speech_rate: Number(e.target.value) })}
                      className="w-full accent-amber-500 disabled:opacity-40"
                    />
                  </div>
                  <div>
                    <label className="block text-[11px] text-neutral-400 mb-1">
                      语音音量 ({config.speech_volume}，SAPI 按 1/2 生效；其余引擎按 200 为满量程)
                    </label>
                    <input
                      type="range"
                      min="0"
                      max="200"
                      step="1"
                      value={config.speech_volume}
                      disabled={isLite}
                      onChange={(e) => applyConfigPatch({ speech_volume: Number(e.target.value) })}
                      className="w-full accent-amber-500 disabled:opacity-40"
                    />
                  </div>
                  <div>
                    <label className="block text-[11px] text-neutral-400 mb-1">
                      语音音调 (SAPI) ({config.speech_pitch})
                    </label>
                    <input
                      type="range"
                      min="-10"
                      max="10"
                      step="1"
                      value={config.speech_pitch}
                      disabled={isLite}
                      onChange={(e) => applyConfigPatch({ speech_pitch: Number(e.target.value) })}
                      className="w-full accent-amber-500 disabled:opacity-40"
                    />
                  </div>
                </div>
              </div>

              {/* D1/D2 点怪门槛、舰长打卡 AI 与跑马灯文本 */}
              <div className={`bg-neutral-900/70 border border-neutral-800 rounded-xl p-5 space-y-4 ${isLite ? "opacity-40" : ""}`}>
                <div className="flex items-center justify-between pb-2 border-b border-neutral-800">
                  <h3 className="text-xs font-bold text-amber-300 flex items-center gap-1.5">
                    <CalendarCheck className="w-4 h-4" />
                    <span>点怪门槛与舰长打卡 AI</span>
                  </h3>
                  {isLite && <span className="text-[10px] text-amber-400">Lite 模式下已停用</span>}
                </div>

                <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
                  <label className="flex items-center gap-2 text-xs text-neutral-300 font-bold cursor-pointer">
                    <input
                      type="checkbox"
                      checked={config.only_medal_order}
                      onChange={(e) => setConfig({ ...config, only_medal_order: e.target.checked })}
                      className="rounded bg-neutral-950 border-neutral-800 text-amber-500 focus:ring-0"
                    />
                    <span>仅粉丝牌（含舰长）可点怪</span>
                  </label>

                  <label className="flex items-center gap-2 text-xs text-neutral-300 font-bold cursor-pointer">
                    <input
                      type="checkbox"
                      checked={config.enable_captain_checkin_ai}
                      disabled={isLite}
                      onChange={(e) => setConfig({ ...config, enable_captain_checkin_ai: e.target.checked })}
                      className="rounded bg-neutral-950 border-neutral-800 text-amber-500 focus:ring-0"
                    />
                    <span>开启舰长打卡 AI 功能</span>
                  </label>

                  <div>
                    <label className="block text-[11px] text-neutral-400 mb-1">打卡触发词（逗号分隔）</label>
                    <input
                      type="text"
                      value={config.checkin_trigger_words}
                      disabled={isLite}
                      onChange={(e) => setConfig({ ...config, checkin_trigger_words: e.target.value })}
                      className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-200 disabled:opacity-40"
                    />
                    <span className="text-[10px] text-neutral-500">默认：打卡,签到（清空则完全停用打卡指令）</span>
                  </div>

                  <div>
                    <label className="block text-[11px] text-neutral-400 mb-1">默认跑马灯循环通告内容</label>
                    <input
                      type="text"
                      value={config.default_marquee_text}
                      onChange={(e) => applyConfigPatch({ default_marquee_text: e.target.value })}
                      placeholder="发送'点怪 xxx'进行点怪"
                      className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-200"
                    />
                    <span className="text-[10px] text-neutral-500">留空时悬浮窗显示默认提示文案</span>
                  </div>
                </div>
              </div>

              {/* D2 悬浮窗窗口控制 */}
              <div className="bg-neutral-900/70 border border-neutral-800 rounded-xl p-5 space-y-4">
                <h3 className="text-xs font-bold text-amber-300 pb-2 border-b border-neutral-800">
                  悬浮窗窗口控制
                </h3>

                <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
                  <div>
                    <label className="block text-[11px] text-neutral-400 mb-1">
                      点怪窗口透明度 ({config.opacity}%)（仅作用于背景）
                    </label>
                    <input
                      type="range"
                      min="0"
                      max="100"
                      step="1"
                      value={config.opacity}
                      onChange={(e) => applyConfigPatch({ opacity: Number(e.target.value) })}
                      className="w-full accent-amber-500"
                    />
                  </div>
                  <div>
                    <label className="block text-[11px] text-neutral-400 mb-1">
                      穿透模式透明度 ({config.penetrating_mode_opacity}%)
                    </label>
                    <input
                      type="range"
                      min="0"
                      max="100"
                      step="1"
                      value={config.penetrating_mode_opacity}
                      onChange={(e) => applyConfigPatch({ penetrating_mode_opacity: Number(e.target.value) })}
                      className="w-full accent-amber-500"
                    />
                  </div>
                  <div>
                    <label className="block text-[11px] text-neutral-400 mb-1">点怪列表主题</label>
                    <select
                      value={config.overlay_theme}
                      onChange={(e) => applyConfigPatch({ overlay_theme: e.target.value })}
                      className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-200"
                    >
                      <option value="wilds">调查队 · 荒野（沙骨米白 / 橄榄 / 夕阳橙）</option>
                      <option value="asc">云居遗迹 · 凌越（深夜蓝 / 天青 / 蓝紫）</option>
                    </select>
                  </div>
                </div>

                <label className="flex items-center gap-2 text-xs text-neutral-300 font-bold cursor-pointer">
                  <input
                    type="checkbox"
                    checked={config.enable_overlay_decor}
                    onChange={(e) => applyConfigPatch({ enable_overlay_decor: e.target.checked })}
                    className="rounded bg-neutral-950 border-neutral-800 text-amber-500 focus:ring-0"
                  />
                  <span>环境装饰动效（优先扫光 / 脊线呼吸 / 云雾雷光）</span>
                </label>

                <div className="bg-neutral-950/80 border border-neutral-800 rounded-xl p-4 space-y-2">
                  <div className="flex items-center gap-3 flex-wrap">
                    <button
                      type="button"
                      onClick={handleToggleOverlayLock}
                      className={`flex items-center gap-1.5 text-xs font-bold px-4 py-2 rounded-lg border transition ${
                        overlayLocked
                          ? "bg-amber-500/20 text-amber-300 border-amber-500/50 hover:bg-amber-500/30"
                          : "bg-neutral-800 text-neutral-200 border-neutral-700 hover:bg-neutral-700"
                      }`}
                    >
                      {overlayLocked ? <Unlock className="w-3.5 h-3.5" /> : <Lock className="w-3.5 h-3.5" />}
                      <span>{overlayLocked ? "解锁窗口" : "锁定窗口"}</span>
                    </button>
                    <span className="text-[11px] text-neutral-500">
                      全局热键
                      <span className="mx-1 px-1.5 py-0.5 bg-neutral-800 border border-neutral-700 rounded font-mono text-neutral-300">
                        Alt + ,
                      </span>
                      锁定/解锁点怪窗口
                    </span>
                  </div>
                  <p className="text-[10px] text-neutral-500 leading-relaxed">
                    锁定后窗口透明且鼠标穿透（使用穿透模式透明度），方便查看游戏画面；窗口位置会自动记忆
                    （配置项 top_pos_x / top_pos_y），下次启动按记忆位置显示。
                  </p>
                </div>
              </div>

              <div className="flex justify-end pt-2">
                <button
                  type="submit"
                  className="bg-amber-500 hover:bg-amber-400 text-black font-bold px-6 py-2.5 rounded-xl text-xs shadow-xl shadow-amber-500/20 transition"
                >
                  保存常规配置
                </button>
              </div>
            </form>
          )}

          {/* 6. 运行日志 TAB（D5，全模式保留） */}
          {activeTab === "logs" && (
            <div className="max-w-5xl space-y-4">
              {resourceWarnings.length > 0 && (
                <div className="bg-red-950/40 border border-red-500/50 rounded-xl p-4 space-y-1">
                  <div className="flex items-center gap-2 text-red-200 text-xs font-bold">
                    <AlertCircle className="w-4 h-4 text-red-400" />
                    <span>资源缺失，相关功能将降级运行：</span>
                  </div>
                  {resourceWarnings.map((w) => (
                    <div key={w} className="text-[11px] text-red-300 font-mono pl-6">
                      {w}
                    </div>
                  ))}
                </div>
              )}

              <div className="bg-neutral-900/70 border border-neutral-800 rounded-xl p-5 space-y-4">
                <div className="flex items-center justify-between pb-3 border-b border-neutral-800">
                  <div>
                    <h3 className="text-xs font-bold text-amber-300 flex items-center gap-1.5">
                      <ScrollText className="w-4 h-4" />
                      <span>运行日志（内存环最近 500 条）</span>
                    </h3>
                    <p className="text-[10px] text-neutral-500 mt-1 font-mono break-all">
                      文件日志: {logSnapshot?.dir || "解析中..."}/YYYY-MM-DD.txt（UTF-8 BOM，按日期分文件）
                    </p>
                  </div>

                  <div className="flex items-center gap-2">
                    <select
                      value={logLevel}
                      onChange={(e) =>
                        setLogLevel(e.target.value as "DEBUG" | "INFO" | "WARNING" | "ERROR")
                      }
                      className="bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-200"
                    >
                      <option value="ERROR">仅 ERROR</option>
                      <option value="WARNING">WARNING 及以上</option>
                      <option value="INFO">INFO 及以上</option>
                      <option value="DEBUG">全部（含 DEBUG）</option>
                    </select>
                    <button
                      onClick={fetchLogs}
                      className="px-3 py-1.5 bg-neutral-800 hover:bg-neutral-700 text-neutral-200 text-xs font-bold rounded-lg border border-neutral-700 transition"
                    >
                      刷新
                    </button>
                    <button
                      onClick={handleClearLogs}
                      className="px-3 py-1.5 bg-neutral-800 hover:bg-red-600 hover:text-white text-neutral-400 text-xs font-bold rounded-lg border border-neutral-700 transition"
                    >
                      清空视图
                    </button>
                  </div>
                </div>

                <div className="max-h-[34rem] overflow-y-auto bg-neutral-950 border border-neutral-800 rounded-lg p-3 font-mono text-[11px] leading-relaxed scrollbar-thin scrollbar-thumb-neutral-800">
                  {!logSnapshot || logSnapshot.entries.length === 0 ? (
                    <div className="text-neutral-500 py-8 text-center">
                      暂无日志（Debug 级别在 Release 构建下不输出）
                    </div>
                  ) : (
                    logSnapshot.entries.map((entry, idx) => (
                      <div key={`${entry.time}-${idx}`} className="flex gap-2">
                        <span className="text-neutral-500 shrink-0">{entry.time}</span>
                        <span
                          className={`shrink-0 font-bold ${
                            entry.level === "ERROR"
                              ? "text-red-400"
                              : entry.level === "WARNING"
                              ? "text-amber-400"
                              : entry.level === "INFO"
                              ? "text-emerald-400"
                              : "text-neutral-400"
                          }`}
                        >
                          [{entry.level}]
                        </span>
                        <span className="text-neutral-200 break-all">{entry.message}</span>
                      </div>
                    ))
                  )}
                </div>
              </div>
            </div>
          )}
        </div>
      </main>
    </div>
  );
};