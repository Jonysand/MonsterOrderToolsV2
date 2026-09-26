import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import {
  QueueItem,
  QueueSnapshot,
  QueuePersistence,
  AppConfig,
  UserSearchItem,
  UserProfile,
  BatchCheckinResult,
  CredentialsStatus,
  CheckinStatus,
  CheckinUnavailablePayload,
  ConnectionStatusPayload,
  MonsterDict,
  RosterData,
  RosterSnapshot,
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
  Users,
  CalendarCheck,
  Download,
  Search,
  Gift,
  Flame,
  Power,
  GripVertical,
  ListPlus,
  Eye,
  Lock,
  Unlock,
  Key,
  Save,
  FileCheck,
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
  sapi: "Windows 本地语音",
};

/** 主窗口队列行高（条目卡 46px + 间距 8px） */
const QUEUE_ROW_HEIGHT = 54;

export const MainWindow: React.FC = () => {
  const [activeTab, setActiveTab] = useState<
    "queue" | "monster" | "bili" | "gm" | "settings"
  >("queue");
  const [queue, setQueue] = useState<QueueItem[]>([]);
  // 编译期形态常量（vite --mode lite 注入）：完整版 false / Lite 版 true，运行期不可切换
  const isLite = __IS_LITE__;
  const [config, setConfig] = useState<AppConfig | null>(null);
  /** 拖拽起点：记 user_id 而不是下标 —— 拖动期间队列可能被弹幕改动，下标会漂移 */
  const [draggedUserId, setDraggedUserId] = useState<string | null>(null);
  /** 最近应用的权威快照（版本 + 落盘状态），所有队列写入都必须经由 applyQueueSnapshot */
  const queueSnapRef = useRef<QueueSnapshot>({ items: [], revision: 0, persistence: "Saved" });
  /** 磁盘落盘异常提示（内存已更新、磁盘待重试），成功后自动收起 */
  const [queueSaveWarning, setQueueSaveWarning] = useState(false);

  // 怪物字典与禁点名单（名单内的怪不可被点：弹幕点怪与选怪面板共享同一份约束）
  const [monsterDict, setMonsterDict] = useState<MonsterDict>({});
  const [roster, setRoster] = useState<RosterData>({ items: [] });
  const [orderSubmitting, setOrderSubmitting] = useState(false);
  const rosterRef = useRef<RosterData>({ items: [] });
  /** 后端名单版本（CAS 用）：提交时带上，旧版本会被后端拒绝 */
  const rosterRevRef = useRef<number>(0);
  /** 只允许一个 in-flight 整表提交；期间的新意图记在 latestIntent */
  const rosterInFlightRef = useRef<boolean>(false);
  const rosterLatestIntentRef = useRef<RosterData | null>(null);
  const rosterSaveTimerRef = useRef<number | null>(null);

  // B站直播连接五态状态机（D7）
  const [conn, setConn] = useState<ConnectionStatusPayload>(DEFAULT_CONNECTION);
  const [recentCheckins, setRecentCheckins] = useState<UserProfile[]>([]);

  // 语音设置（D1）
  const [manboVoices, setManboVoices] = useState<string[]>([]);
  const [currentEngine, setCurrentEngine] = useState<string>("");

  // 悬浮窗锁定（D2）
  const [overlayLocked, setOverlayLocked] = useState(false);

  const [appVersion, setAppVersion] = useState("");

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

  // 敏感凭证加密托管状态
  const [credStatus, setCredStatus] = useState<CredentialsStatus | null>(null);

  /** 打卡子系统可用性：冷启动读取快照，不依赖 setup 阶段可能丢失的单次事件 */
  const [checkinStatus, setCheckinStatus] = useState<CheckinStatus | null>(null);
  /** 运行期打卡不可用提示（数据库故障或 Lite 停用），由 checkin-unavailable 事件驱动 */
  const [checkinDownMsg, setCheckinDownMsg] = useState<string | null>(null);

  /** 缺失资源：事件与冷启动快照按名合并去重（F2） */
  const [missingResources, setMissingResources] = useState<string[]>([]);
  const missingRef = useRef<Set<string>>(new Set());

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

  /**
   * 队列写入唯一入口（轮询、事件、IPC 成功回调都走这里）。
   *
   * 版本检查必须在**所有**路径生效，否则一个迟到的旧命令返回值就能把 UI 拉回旧队列：
   * - 旧 revision 的快照直接丢弃（弹幕新单不会从界面上消失）；
   * - 同 revision 只允许 `PendingRetry → Saved` 推进，迟到的失败状态不得盖回成功。
   */
  const applyQueueSnapshot = (snap: QueueSnapshot) => {
    const prev = queueSnapRef.current;
    if (snap.revision < prev.revision) return;
    if (snap.revision === prev.revision && prev.persistence === "Saved" && snap.persistence === "PendingRetry") {
      return;
    }
    queueSnapRef.current = snap;
    setQueue(snap.items);
    setQueueSaveWarning(snap.persistence === "PendingRetry");
  };

  const fetchQueue = async () => {
    try {
      applyQueueSnapshot(await invoke<QueueSnapshot>("get_queue"));
    } catch (e) {
      console.error(e);
    }
  };

  const fetchConfig = async () => {
    try {
      const cfg = await invoke<AppConfig>("get_app_config");
      // 已保存的身份码单独读取（配置序列化排除敏感字段），供输入框以密码形态回显
      const savedCode = await invoke<string>("get_id_code").catch(() => "");
      setConfig({ ...cfg, id_code: savedCode || cfg.id_code || "" });
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

  const fetchOverlayLocked = async () => {
    try {
      setOverlayLocked(await invoke<boolean>("get_overlay_locked"));
    } catch (e) {
      console.error(e);
    }
  };

  const fetchCredentialsStatus = async () => {
    // 凭据由后端启动时从随包/数据目录的 credentials.dat 自动校验加载，
    // 前端仅轮询展示脱敏状态，不提供任何凭据输入入口（唯一用户输入为开播身份码）
    try {
      const status = await invoke<CredentialsStatus>("get_credentials_status");
      setCredStatus(status);
    } catch (e) {
      console.error("获取凭据状态异常:", e);
    }
  };

  /** 打卡可用性快照：Lite 显示「按构建形态已停用」，完整版显示可用或数据库故障 */
  const fetchCheckinStatus = async () => {
    try {
      setCheckinStatus(await invoke<CheckinStatus>("get_checkin_status"));
    } catch (e) {
      console.error("获取打卡状态异常:", e);
    }
  };

  useEffect(() => {
    fetchQueue();
    fetchConfig();
    fetchBiliStatus();
    fetchCredentialsStatus();
    fetchCheckinStatus();
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
    }, 2500);

    const unlistenQueue = listen<QueueSnapshot>("queue-updated", (event) => {
      applyQueueSnapshot(event.payload);
    });

    // 落盘状态边沿事件：同 revision 的 PendingRetry/Saved 切换也要即时反映
    const unlistenPersist = listen<{ persistence: QueuePersistence }>(
      "queue-persistence-changed",
      (event) => {
        setQueueSaveWarning(event.payload.persistence === "PendingRetry");
      },
    );

    // D7 五态连接状态
    const unlistenConn = listen<ConnectionStatusPayload>("connection-state-changed", (event) => {
      setConn(event.payload);
    });

    // D4/D5 主播控制台动态：打卡记录（Lite 构建下无此功能，监听与提示一并剔除）
    const unlistenCheckin = __IS_LITE__
      ? Promise.resolve(() => {})
      : listen<UserProfile>("checkin-recorded", (event) => {
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

    // 打卡数据不可持久化：运行期数据库故障时也必须可见
    const unlistenCheckinDown = __IS_LITE__
      ? Promise.resolve(() => {})
      : listen<CheckinUnavailablePayload>("checkin-unavailable", (event) => {
          setCheckinDownMsg(event.payload.message);
        });

    // D5/F2 资源缺失提示：事件按名去重合并，且必须与冷启动快照合并 ——
    // `resource-missing` 只在 setup 阶段 emit 一次，挂载晚于 setup 时会永久丢失
    const reportMissing = (name: string) => {
      if (!name || missingRef.current.has(name)) return;
      missingRef.current.add(name);
      setMissingResources([...missingRef.current]);
    };
    const unlistenMissing = listen<string>("resource-missing", (event) => {
      reportMissing(event.payload);
    });
    invoke<string[]>("get_missing_resources")
      .then((list) => list.forEach(reportMissing))
      .catch((e) => console.error("获取资源缺失快照异常:", e));

    return () => {
      clearInterval(interval);
      if (rosterSaveTimerRef.current !== null) {
        window.clearTimeout(rosterSaveTimerRef.current);
      }
      unlistenQueue.then((f) => f());
      unlistenPersist.then((f) => f());
      unlistenCheckinDown.then((f) => f());
      unlistenConn.then((f) => f());
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
      const snap = await invoke<RosterSnapshot>("get_monster_roster");
      rosterRef.current = snap.data;
      rosterRevRef.current = snap.revision;
      setRoster(snap.data);
    } catch (e) {
      console.error("获取禁点名单异常:", e);
    }
  };

  /** 字典条目增删改后刷新：改名/删除会在后端联动禁点名单，故名单须一并重取 */
  const refreshMonsterData = async () => {
    await Promise.all([fetchMonsterDict(), fetchRoster()]);
  };

  /**
   * 名单变更：本地乐观更新 + 300ms 防抖整表落盘。
   *
   * - 已发出的 IPC 不能用 clearTimeout 撤回，因此只允许**一个 in-flight 提交**；
   *   期间的新意图记为 latestIntent，提交成功后用返回版本继续提交。
   * - 提交带 expectedRevision（CAS）：旧请求被后端拒绝时**不无条件回滚旧画面**，
   *   而是取回后端权威快照（可能已被字典改名/删除联动过）。
   */
  const commitRoster = (next: RosterData, immediate = false) => {
    rosterRef.current = next;
    setRoster(next);
    if (rosterSaveTimerRef.current !== null) {
      window.clearTimeout(rosterSaveTimerRef.current);
    }

    const persist = async () => {
      rosterSaveTimerRef.current = null;
      if (rosterInFlightRef.current) {
        // 只保留最新意图，避免并发整表提交互相覆盖
        rosterLatestIntentRef.current = next;
        return;
      }
      rosterInFlightRef.current = true;
      try {
        let intent: RosterData | null = next;
        while (intent) {
          rosterLatestIntentRef.current = null;
          const snap = await invoke<RosterSnapshot>("set_monster_roster", {
            data: intent,
            expectedRevision: rosterRevRef.current,
          });
          rosterRevRef.current = snap.revision;
          intent = rosterLatestIntentRef.current;
        }
      } catch (err) {
        showToast(`名单保存失败：${err}`);
        // 失败后以后端权威快照为准，不用可能已过期的本地快照覆盖
        await fetchRoster();
      } finally {
        rosterInFlightRef.current = false;
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

  /** 选怪面板入队（与弹幕点怪写入同一条队列；按原名精确取键 + 后端禁点校验） */
  const handlePickerOrder = async (payload: PickerOrderPayload) => {
    setOrderSubmitting(true);
    try {
      const name = payload.userName || "房管";
      const snap = await invoke<QueueSnapshot>("add_picked_order", {
        userId: `manual-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
        userName: name,
        monsterName: payload.monsterName,
        isPriority: payload.isPriority,
        guardLevel: payload.guardLevel,
        temperedLevel: payload.temperedLevel,
      });
      applyQueueSnapshot(snap);
      // Toast 只报后端实际确认的条目，不复述未校验的请求文案
      const confirmed = snap.items.find((i) => i.user_name === name)?.monster_name ?? payload.monsterName;
      showToast(`${name} 点怪 ${confirmed} 已入队${payload.isPriority ? "（优先置前）" : ""}`);
    } catch (err) {
      // 禁点/未知怪物等都在这里被后端拒绝
      showToast(`加入排队失败: ${err}`);
      await fetchRoster();
    } finally {
      setOrderSubmitting(false);
    }
  };

  const handleDelete = async (userId: string) => {
    try {
      applyQueueSnapshot(await invoke<QueueSnapshot>("dequeue_by_user_id", { userId }));
      showToast("已完成条目并保序出队");
    } catch (e) {
      console.error(e);
    }
  };

  const handleClear = async () => {
    if (!(await askConfirm("确认清空当前点单排队队列吗？", "清空队列"))) return;
    try {
      applyQueueSnapshot(await invoke<QueueSnapshot>("clear_queue"));
      showToast("队列已清空");
    } catch (e) {
      console.error(e);
    }
  };

  // 控制台条目拖拽排序：按下标取元素只用于渲染，提交时一律换算成 user_id 顺序
  const handleItemDragStart = (e: React.DragEvent, userId: string) => {
    setDraggedUserId(userId);
    e.dataTransfer.effectAllowed = "move";
  };

  const handleItemDragOver = (e: React.DragEvent) => {
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
  };

  const handleItemDrop = async (e: React.DragEvent, targetIndex: number) => {
    e.preventDefault();
    const draggedId = draggedUserId;
    setDraggedUserId(null);
    if (draggedId === null) return;

    // 按 user_id 定位当前下标：拖动期间弹幕可能已插入新单，拖动起点的下标不再可信
    const fromIndex = queue.findIndex((i) => i.user_id === draggedId);
    if (fromIndex < 0 || fromIndex === targetIndex) return;

    const nextQueue = [...queue];
    const [moved] = nextQueue.splice(fromIndex, 1);
    nextQueue.splice(targetIndex, 0, moved);

    try {
      // 只提交「预期版本 + 用户 ID 顺序」：后端按 ID 从当前条目重组，
      // 一次拖拽不可能删掉另一路新订单、复活完成单或撤回提权
      const snap = await invoke<QueueSnapshot>("reorder_queue", {
        orderedUserIds: nextQueue.map((i) => i.user_id),
        expectedRevision: queueSnapRef.current.revision,
      });
      applyQueueSnapshot(snap);
      showToast("排队顺序已调整并同步保存！");
    } catch (err) {
      showToast(`排序更新失败: ${err}`);
      // 冲突时取最新权威快照，提示主播重新拖动
      await fetchQueue();
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
      showToast("开播身份码已保存到本机！");
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
        // 回显值未改动时重复写入同值无副作用；留空则交由 Rust 侧回退读注册表
        const currentCode = (config?.id_code || "").trim();
        if (currentCode) {
          await invoke("save_id_code", { idCode: currentCode });
        }
      }
      await invoke("set_bili_connection", { connected: next });
      showToast(next ? "正在建立 B 站直播连接..." : "已断开直播连接");
      fetchBiliStatus();
    } catch (e) {
      showToast(`连接操作失败: ${e}`);
      fetchBiliStatus();
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

  // 试听语音音量：显式传滑块即时值（绕开 800ms 自动保存防抖），
  // 后端按当前所选 TTS 引擎播报（Manbo 失败自动降级 SAPI），即时反映当前参数效果
  const handleTestVolume = async () => {
    if (!config) return;
    try {
      await invoke("test_speech_volume", {
        volume: config.speech_volume,
        rate: config.speech_rate,
        pitch: config.speech_pitch,
      });
    } catch (err) {
      showToast(`试听失败: ${err}`);
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

      {/* 队列磁盘落盘异常：内存已更新但尚未确认落盘，退出一律提示而不是谎报成功 */}
      {queueSaveWarning && (
        <div className="fixed top-16 left-1/2 -translate-x-1/2 z-50 bg-red-600/95 text-white text-xs font-bold px-4 py-2 rounded-xl shadow-2xl">
          队列已更新，但写入磁盘失败，正在自动重试（请检查磁盘空间与目录权限）
        </div>
      )}

      {/* 打卡不可用常驻告警：Lite 形态为预期行为，完整版为数据库故障（核心点怪不受影响） */}
      {checkinStatus && !checkinStatus.available && !isLite && (
        <div className="fixed top-28 left-1/2 -translate-x-1/2 z-50 max-w-2xl bg-red-700/95 text-white text-xs font-bold px-4 py-2 rounded-xl shadow-2xl">
          打卡不可持久化：{checkinDownMsg ?? checkinStatus.message}
        </div>
      )}

      {/* 双库冲突常驻告警：另一份打卡库未展示、未删除 */}
      {checkinStatus?.available && checkinStatus.has_shadow_db && (
        <div className="fixed top-28 left-1/2 -translate-x-1/2 z-50 max-w-2xl bg-amber-600/95 text-black text-xs font-bold px-4 py-2 rounded-xl shadow-2xl">
          检测到两份打卡库：本次使用「{checkinStatus.active_db_file}」，另一份既未展示也未删除。
          请勿删除任何文件，退出应用后备份整个数据目录再离线合并。
        </div>
      )}

      {/* 资源缺失常驻告警：合并事件与冷启动快照，按名去重 */}
      {missingResources.length > 0 && (
        <div className="fixed top-40 left-1/2 -translate-x-1/2 z-50 max-w-2xl bg-amber-700/95 text-white text-xs font-bold px-4 py-2 rounded-xl shadow-2xl">
          资源缺失：{missingResources.join("、")}（相关功能将降级运行）
        </div>
      )}

      {/* 左侧功能导航栏 */}
      <aside className="w-56 bg-neutral-900/90 border-r border-neutral-800 flex flex-col justify-between p-3 shrink-0">
        <div className="space-y-4">
          {/* 标题 */}
          <div className="px-2 py-1">
            <div className="text-xs font-bold tracking-wider text-amber-300">MonsterOrderWilds</div>
            <div className="text-[10px] text-neutral-400 font-mono">
              Ascendance{appVersion ? ` (v ${appVersion})` : ""}
            </div>
          </div>

          {/* 导航按钮 */}
          <nav className="space-y-1">
            <button
              onClick={() => setActiveTab("bili")}
              className={`w-full flex items-center gap-2.5 px-3 py-2 rounded-lg text-xs font-bold transition ${
                activeTab === "bili"
                  ? "bg-amber-500/20 text-amber-300 border border-amber-500/30"
                  : "text-neutral-400 hover:bg-neutral-800/60 hover:text-neutral-200"
              }`}
            >
              <Radio className="w-4 h-4" />
              <span>直播连接</span>
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

            {/* 舰长打卡 & GM：Lite 构建下无此功能，页签入口整体隐藏（无占位、无提示） */}
            {!isLite && (
              <button
                onClick={() => setActiveTab("gm")}
                className={`w-full flex items-center gap-2.5 px-3 py-2 rounded-lg text-xs font-bold transition ${
                  activeTab === "gm"
                    ? "bg-amber-500/20 text-amber-300 border border-amber-500/30"
                    : "text-neutral-400 hover:bg-neutral-800/60 hover:text-neutral-200"
                }`}
              >
                <CalendarCheck className="w-4 h-4" />
                <span>舰长打卡 & GM</span>
              </button>
            )}

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

        {/* 底部快捷操作 */}
        <div className="space-y-2 border-t border-neutral-800 pt-3">
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
              {activeTab === "gm" && !isLite && "舰长周打卡系统与 GM 运维管理"}
              {activeTab === "settings" && "系统全局持久化参数设置"}
            </h1>
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
                            onDragStart={(e) => handleItemDragStart(e, item.user_id)}
                            onDragOver={handleItemDragOver}
                            onDrop={(e) => handleItemDrop(e, idx)}
                            style={{ height: QUEUE_ROW_HEIGHT - 8, marginBottom: 8 }}
                            className={`flex items-center justify-between p-2.5 rounded-xl border transition-all select-none ${
                              draggedUserId === item.user_id ? "opacity-40 scale-95 border-dashed border-amber-400" : ""
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


          {/* 3. 直播连接 TAB */}
          {activeTab === "bili" && (
            <div className="max-w-3xl space-y-6">
              <div className="bg-neutral-900/70 border border-neutral-800 rounded-xl p-5 space-y-4">
                <div className="flex items-center justify-between pb-3 border-b border-neutral-800">
                  <div>
                    <h2 className="text-sm font-bold text-neutral-200">B 站直播开放平台状态</h2>
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

                {/* 开播身份码与开启连接合一控制区 */}
                <div className="bg-neutral-950/80 border border-neutral-800 rounded-xl p-4 space-y-3">
                  <div className="flex items-center justify-between">
                    <label className="text-xs font-bold text-amber-300 flex items-center gap-1.5">
                      <Key className="w-3.5 h-3.5 text-amber-400" />
                      <span>开播身份码与直播连接</span>
                    </label>
                    <span className="text-[10px] text-neutral-500">本机安全保存</span>
                  </div>

                  {/* 身份码输入框 + 保存 + 开启直播连接按钮紧密并排 */}
                  <div className="flex items-center gap-2.5">
                    <div className="relative flex-1 min-w-0">
                      <input
                        type="password"
                        value={config?.id_code || ""}
                        onChange={(e) => config && setConfig({ ...config, id_code: e.target.value })}
                        placeholder="在此输入或粘贴当次开播身份码..."
                        className="w-full bg-neutral-900 border border-neutral-700 rounded-lg px-3 py-2 text-xs text-neutral-100 placeholder-neutral-500 focus:border-amber-400 focus:outline-none"
                      />
                    </div>

                    <button
                      type="button"
                      onClick={handleSaveIdCode}
                      className="px-3 py-2 bg-neutral-800 hover:bg-neutral-700 text-neutral-200 text-xs font-bold rounded-lg border border-neutral-700 transition flex items-center gap-1.5 shrink-0"
                      title="单独将身份码保存到本机"
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
                          ? "断开连接"
                          : conn.state === "Connecting" || conn.state === "Reconnecting"
                          ? "取消连接"
                          : "开启直播连接"}
                      </span>
                    </button>
                  </div>

                  <p className="text-[11px] text-neutral-400 leading-relaxed">
                    提示：点击【开启直播连接】会自动保存到本机；已保存的身份码以密码形式回显，可直接修改或重新粘贴覆盖（留空则沿用本机已保存值）。
                  </p>
                </div>
              </div>
            </div>
          )}

          {/* 4. 舰长打卡 & GM 运维 TAB：Lite 构建下无此功能，整页隐藏（导航入口一并隐藏） */}
          {activeTab === "gm" && !isLite && (
            <div className="space-y-6">
              <div className="grid grid-cols-12 gap-6">
                {/* D4 主播控制台实时动态：打卡记录 */}
                <div className="col-span-12 bg-neutral-900/70 border border-neutral-800 rounded-xl p-5 space-y-3">
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
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-100 disabled:opacity-40"
                      />
                    </div>
                    <div>
                      <label className="block text-[11px] font-bold text-neutral-400 mb-1">开始日期（可选）</label>
                      <input
                        type="date"
                        value={exportStartDate}
                        onChange={(e) => setExportStartDate(e.target.value)}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-100 disabled:opacity-40"
                      />
                    </div>
                    <div>
                      <label className="block text-[11px] font-bold text-neutral-400 mb-1">结束日期（可选）</label>
                      <input
                        type="date"
                        value={exportEndDate}
                        onChange={(e) => setExportEndDate(e.target.value)}
                        className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-100 disabled:opacity-40"
                      />
                    </div>
                  </div>

                  <button
                    onClick={handleExportRecords}
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
                      className="flex-1 min-w-0 bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-100 disabled:opacity-40"
                    />
                    <div className="flex items-center gap-1.5 bg-neutral-950 border border-neutral-800 rounded-lg px-2.5">
                      <span className="text-[11px] text-neutral-400">发卡数量:</span>
                      <input
                        type="number"
                        min="1"
                        max="20"
                        value={grantCardAmount}
                        onChange={(e) => setGrantCardAmount(Math.max(1, Number(e.target.value)))}
                        className="w-12 bg-transparent text-xs text-amber-300 font-bold focus:outline-none"
                      />
                    </div>
                    <button
                      type="submit"
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

          {/* 6. 设置面板 TAB */}
          {activeTab === "settings" && config && (
            <form onSubmit={handleSaveConfig} className="max-w-4xl space-y-6 pb-12">
              {/* 敏感凭证安全托管状态卡片 (禁止手动修改) */}
              <div className="bg-gradient-to-r from-neutral-900/80 via-emerald-950/20 to-neutral-900/80 border border-emerald-500/30 rounded-xl p-5 space-y-3">
                <div className="flex items-center justify-between pb-2 border-b border-emerald-500/20">
                  <div className="flex items-center gap-2">
                    <Lock className="w-4 h-4 text-emerald-400" />
                    <h3 className="text-xs font-bold text-emerald-300">
                      敏感凭据加密托管
                    </h3>
                  </div>
                  <span className="text-[10px] bg-emerald-500/15 text-emerald-300 border border-emerald-500/30 px-2 py-0.5 rounded-full font-mono flex items-center gap-1">
                    <FileCheck className="w-3 h-3 text-emerald-400" />
                    <span>{credStatus?.loaded ? "校验通过" : "未检测到凭据文件"}</span>
                  </span>
                </div>

                <p className="text-[11px] text-neutral-400 leading-relaxed">
                  遵循原工程安全规范，应用凭据（APP ID、访问密钥{isLite ? "" : "、语音与 AI 密钥"}）均由发行方加密托管于凭据文件并随安装包内置，启动时自动校验加载，<strong className="text-neutral-200">不允许手动设置或明文暴露</strong>。
                  如需更换凭据请获取新的安装包，或使用发行方提供的凭据文件替换数据目录下的 credentials.dat（对下一次连接生效）。
                </p>

                <div className="flex items-center gap-2 flex-wrap">
                  <span className="text-[10px] font-mono text-neutral-500 break-all">
                    凭据文件：{credStatus?.file_path || "—"}
                  </span>
                </div>

                {/* AI 模型凭据与多引擎语音芯片：Lite 构建下无对应功能，一并隐藏 */}
                <div className={`grid grid-cols-2 gap-3 pt-1 ${isLite ? "" : "sm:grid-cols-4"}`}>
                  <div className="bg-neutral-950/70 p-2.5 rounded-lg border border-neutral-800">
                    <span className="text-[10px] text-neutral-500 block">应用 APP ID</span>
                    <span className="text-xs font-mono font-bold text-neutral-200">
                      {credStatus?.app_id || "未加载"}
                    </span>
                  </div>

                  <div className="bg-neutral-950/70 p-2.5 rounded-lg border border-neutral-800">
                    <span className="text-[10px] text-neutral-500 flex items-center gap-1">
                      <Key className="w-2.5 h-2.5 text-amber-400" />
                      <span>访问密钥 ID</span>
                    </span>
                    <span className="text-xs font-mono font-bold text-neutral-200">
                      {credStatus?.access_key_masked || "未加载"}
                    </span>
                  </div>

                  {!isLite && (
                    <>
                      <div className="bg-neutral-950/70 p-2.5 rounded-lg border border-neutral-800">
                        <span className="text-[10px] text-neutral-500 block">AI 模型凭据</span>
                        <span className={`text-xs font-bold ${credStatus?.has_chat_key ? "text-emerald-300" : "text-neutral-500"}`}>
                          {credStatus?.has_chat_key ? `已绑定 (${credStatus.chat_provider})` : "未绑定"}
                        </span>
                      </div>

                      <div className="bg-neutral-950/70 p-2.5 rounded-lg border border-neutral-800">
                        <span className="text-[10px] text-neutral-500 block">多引擎语音</span>
                        <span className={`text-xs font-bold ${credStatus?.has_manbo_key ? "text-emerald-300" : "text-neutral-500"}`}>
                          {credStatus?.has_manbo_key ? "Manbo 已绑定" : "本地语音"}
                        </span>
                      </div>
                    </>
                  )}
                </div>
              </div>

              {/* D1 多引擎 TTS 语音与音效：Lite 构建下无此功能，整卡隐藏（无占位、无提示） */}
              {!isLite && (
              <div className="bg-neutral-900/70 border border-neutral-800 rounded-xl p-5 space-y-4">
                <div className="flex items-center justify-between pb-2 border-b border-neutral-800">
                  <h3 className="text-xs font-bold text-amber-300 flex items-center gap-1.5">
                    <Volume2 className="w-4 h-4" />
                    <span>语音播报与音效</span>
                  </h3>
                </div>

                {/* 总开关与播报过滤 */}
                <div className="grid grid-cols-2 md:grid-cols-4 gap-3">
                  <label className="flex items-center gap-2 text-xs text-neutral-300 font-bold cursor-pointer">
                    <input
                      type="checkbox"
                      checked={config.enable_voice}
                      onChange={(e) => setConfig({ ...config, enable_voice: e.target.checked })}
                      className="rounded bg-neutral-950 border-neutral-800 text-amber-500 focus:ring-0"
                    />
                    <span>开启语音播报（总开关）</span>
                  </label>

                  <label className="flex items-center gap-2 text-xs text-neutral-300 font-bold cursor-pointer">
                    <input
                      type="checkbox"
                      checked={config.only_speek_wearing_medal}
                      onChange={(e) => setConfig({ ...config, only_speek_wearing_medal: e.target.checked })}
                      className="rounded bg-neutral-950 border-neutral-800 text-amber-500 focus:ring-0"
                    />
                    <span>仅播报佩戴粉丝牌的弹幕</span>
                  </label>

                  <label className="flex items-center gap-2 text-xs text-neutral-300 font-bold cursor-pointer">
                    <input
                      type="checkbox"
                      checked={config.only_speek_paid_gift}
                      onChange={(e) => setConfig({ ...config, only_speek_paid_gift: e.target.checked })}
                      className="rounded bg-neutral-950 border-neutral-800 text-amber-500 focus:ring-0"
                    />
                    <span>仅播报付费礼物</span>
                  </label>

                  <div>
                    <label className="block text-[11px] text-neutral-400 mb-1">播报至少等级（大航海）</label>
                    <select
                      value={config.only_speek_guard_level}
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
                    <label className="block text-[11px] text-neutral-400 mb-1">播报引擎</label>
                    <select
                      value={config.tts_engine || "auto"}
                      onChange={(e) => setConfig({ ...config, tts_engine: e.target.value })}
                      className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-200 disabled:opacity-40"
                    >
                      <option value="auto">自动（按优先级依次尝试）</option>
                      <option value="manbo">Manbo</option>
                      <option value="sapi">Windows 本地语音</option>
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
                      onChange={(e) => applyConfigPatch({ speech_rate: Number(e.target.value) })}
                      className="w-full accent-amber-500 disabled:opacity-40"
                    />
                  </div>
                  <div>
                    <div className="flex items-center justify-between mb-1">
                      <label className="text-[11px] text-neutral-400">
                        语音音量 ({config.speech_volume})
                      </label>
                      <button
                        type="button"
                        onClick={handleTestVolume}
                        className="px-2 py-0.5 bg-neutral-800 hover:bg-neutral-700 text-neutral-200 text-[11px] font-bold rounded-md border border-neutral-700 transition"
                        title="按当前所选引擎与音量/语速/音调播报一句试听"
                      >
                        试听
                      </button>
                    </div>
                    <input
                      type="range"
                      min="0"
                      max="200"
                      step="1"
                      value={config.speech_volume}
                      onChange={(e) => applyConfigPatch({ speech_volume: Number(e.target.value) })}
                      className="w-full accent-amber-500 disabled:opacity-40"
                    />
                  </div>
                  <div>
                    <label className="block text-[11px] text-neutral-400 mb-1">
                      语音音调 ({config.speech_pitch})
                    </label>
                    <input
                      type="range"
                      min="-10"
                      max="10"
                      step="1"
                      value={config.speech_pitch}
                      onChange={(e) => applyConfigPatch({ speech_pitch: Number(e.target.value) })}
                      className="w-full accent-amber-500 disabled:opacity-40"
                    />
                  </div>
                </div>
              </div>
              )}

              {/* D1/D2 点怪门槛、舰长打卡 AI 与跑马灯文本（Lite 构建下仅保留点怪门槛与跑马灯） */}
              <div className="bg-neutral-900/70 border border-neutral-800 rounded-xl p-5 space-y-4">
                <div className="flex items-center justify-between pb-2 border-b border-neutral-800">
                  <h3 className="text-xs font-bold text-amber-300 flex items-center gap-1.5">
                    <CalendarCheck className="w-4 h-4" />
                    <span>{isLite ? "点怪门槛与跑马灯" : "点怪门槛与舰长打卡 AI"}</span>
                  </h3>
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

                  {/* 打卡 AI 与触发词：Lite 构建下无此功能，控件隐藏 */}
                  {!isLite && (
                    <>
                      <label className="flex items-center gap-2 text-xs text-neutral-300 font-bold cursor-pointer">
                        <input
                          type="checkbox"
                          checked={config.enable_captain_checkin_ai}
                          onChange={(e) => setConfig({ ...config, enable_captain_checkin_ai: e.target.checked })}
                          className="rounded bg-neutral-950 border-neutral-800 text-amber-500 focus:ring-0"
                        />
                        <span>开启舰长打卡 AI 功能</span>
                      </label>

                      <div>
                        <label className="block text-[11px] text-neutral-400 mb-1">打卡触发词（中英文逗号分隔）</label>
                        <input
                          type="text"
                          value={config.checkin_trigger_words}
                          onChange={(e) => setConfig({ ...config, checkin_trigger_words: e.target.value })}
                          className="w-full bg-neutral-950 border border-neutral-800 rounded-lg px-3 py-1.5 text-xs text-neutral-200 disabled:opacity-40"
                        />
                        <span className="text-[10px] text-neutral-500">默认：打卡,签到（支持中文逗号，清空则完全停用打卡指令）</span>
                      </div>
                    </>
                  )}

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
                    锁定后窗口透明且鼠标穿透（使用穿透模式透明度），方便查看游戏画面；窗口位置会自动记忆，下次启动按记忆位置显示。
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
        </div>
      </main>
    </div>
  );
};