import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  QueueItem,
  QueueSnapshot,
  AppConfig,
  OrderPlacedPayload,
  OrderBlockedPayload,
  CheckinReplyPayload,
  CheckinUnavailablePayload,
  LikeRewardFailedPayload,
  RetroactivePayload,
  LikeRewardPayload,
  GiftReceivedPayload,
  SuperChatReceivedPayload,
  GuardReceivedPayload,
  ConnectionStatusPayload,
} from "../types";
import { VirtualList } from "../components/VirtualList";
import { MarqueeText } from "../components/MarqueeText";
import { Shield, X, GripVertical, Volume2, Lock, Bell } from "lucide-react";

const FALLBACK_MARQUEE = "发送'点怪 xxx'进行点怪";
/** 单条跑马灯滚动时长（原工程固定 10s） */
const MARQUEE_SECS = 10;
const MAX_BUBBLES = 5;
const BUBBLE_TTL_MS = 15000;
/** 虚拟列表行高：条目 56 + 间距 4（对齐原工程 ListViewItem Height=60） */
const QUEUE_ROW_HEIGHT = 60;
/** 完成动效总时长：钤印 560ms 与离场 420ms（延迟 560ms）重叠 */
const COMPLETE_ANIM_MS = 980;
/** 撤销垫保留时长 */
const UNDO_TTL_MS = 5200;

type BubbleTone = "checkin" | "retro" | "like" | "gift" | "system";

/** 大航海等级名（与原工程一致：1=总督, 2=提督, 3=舰长） */
const GUARD_NAMES: Record<number, string> = { 1: "总督", 2: "提督", 3: "舰长" };

/** 顶栏连接状态点配色 */
const CONN_DOT_COLORS: Record<string, string> = {
  Connected: "bg-emerald-400 shadow-[0_0_6px_#59d98a]",
  Connecting: "bg-amber-400 shadow-[0_0_6px_#fbbf24]",
  Reconnecting: "bg-amber-400 shadow-[0_0_6px_#fbbf24]",
  ReconnectFailed: "bg-red-500 shadow-[0_0_6px_#ef4444]",
  Disconnected: "bg-gray-500",
};

/** 渲染行：ghost = 已完成但仍在播钤印动画的暂留行 */
interface RenderRow {
  item: QueueItem;
  ghost: boolean;
}

interface GhostRow {
  item: QueueItem;
  /** 完成时在渲染列表中的下标，合并时按下标插回原位 */
  index: number;
  /** 完成时行的面板坐标：钤印特效层挂在面板根（虚拟列表滚动容器会裁切行内上溢），按此定位 */
  rect: { left: number; top: number; width: number; height: number };
}

interface UndoRecord {
  item: QueueItem;
  index: number;
}

/**
 * 手柄拖拽排序状态：被拖条目脱离列表流浮起跟随指针，列表里让出空位。
 * 只放「形状」——跟手坐标等逐帧变化的东西放在 ref 里直接写 DOM，
 * 否则每个指针事件都会 setState，整列跟着重渲染，手感发涩。
 * 顺序用 user_id 表达（不用下标）：完成动画期间序号会漂移，只有 id 稳定。
 */
interface DragState {
  uid: string;
  /** 拖拽开始时的顺序，拖到可落地区域之外松手时原样收回 */
  origin: string[];
  /** 预览顺序：被拖条目已就位，其他条目按落点让位 */
  order: string[];
}

interface OverlayBubble {
  id: number;
  title: string;
  username: string;
  content: string;
  tone: BubbleTone;
}

const BUBBLE_TONES: Record<BubbleTone, string> = {
  checkin: "bg-emerald-950/95 border-emerald-400/60 text-emerald-300",
  retro: "bg-purple-950/95 border-purple-400/60 text-purple-300",
  like: "bg-amber-950/95 border-amber-400/70 text-amber-300",
  gift: "bg-rose-950/95 border-rose-400/60 text-rose-300",
  system: "bg-gray-900/95 border-gray-500/60 text-gray-300",
};

export const OverlayWindow: React.FC = () => {
  const [queue, setQueue] = useState<QueueItem[]>([]);
  const [ghosts, setGhosts] = useState<GhostRow[]>([]);
  const [enteringIds, setEnteringIds] = useState<Set<string>>(new Set());
  const [undoStack, setUndoStack] = useState<UndoRecord[]>([]);
  const [theme, setTheme] = useState<"wilds" | "asc">("wilds");
  const [decor, setDecor] = useState<boolean>(false);
  const [conn, setConn] = useState<ConnectionStatusPayload | null>(null);
  const [defaultMarquee, setDefaultMarquee] = useState<string>(FALLBACK_MARQUEE);
  const [marqueeText, setMarqueeText] = useState<string>(FALLBACK_MARQUEE);
  const [marqueeQueue, setMarqueeQueue] = useState<{ id: number; text: string }[]>([]);
  const [opacity, setOpacity] = useState<number>(95);
  const [penetratingOpacity, setPenetratingOpacity] = useState<number>(50);
  const [locked, setLocked] = useState<boolean>(false);
  const [bubbles, setBubbles] = useState<OverlayBubble[]>([]);
  const [drag, setDrag] = useState<DragState | null>(null);
  const marqueeSeqRef = useRef(0);
  /** 权威队列快照（后端版本 + 落盘状态），拖拽/撤销的基准 */
  const authoritativeRef = useRef<QueueSnapshot>({ items: [], revision: 0, persistence: "Saved" });
  /** 行级锁：正在播完成动画的 user_id，期间拒绝再次点击 */
  const completingRef = useRef<Set<string>>(new Set());
  /** 已知条目 id，用于识别新入队行以播入场动画 */
  const knownIdsRef = useRef<Set<string>>(new Set());
  /** 列表容器：行元素查询范围限定在此（浮起的条目渲染在列表之外） */
  const listRef = useRef<HTMLDivElement>(null);
  /** 根节点：拖拽期间承接指针捕获（虚拟列表可能摘掉被拖行，不能用行内元素当宿主） */
  const rootRef = useRef<HTMLDivElement>(null);
  /** 浮起层元素：跟手位置直接写 style，不经 state */
  const floatRef = useRef<HTMLDivElement>(null);
  /** 拖拽几何：抓取偏移、浮起层初始位置、指针是否在可落地区域内（逐帧变化，故不入 state） */
  const dragGeomRef = useRef<{
    grabOffset: number;
    left: number;
    width: number;
    y: number;
    valid: boolean;
  } | null>(null);
  /** 上一帧各行布局顶边与行序指纹，供让位补间（FLIP）判定位移 */
  const rowLayoutRef = useRef<{ key: string; firstTop: number; tops: Map<string, number> }>({
    key: "",
    firstTop: Number.NaN,
    tops: new Map(),
  });
  /** 队列写操作串行链：不依赖 Tauri 同步命令跑主线程这一实现细节 */
  const opChainRef = useRef<Promise<unknown>>(Promise.resolve());
  /** 完成动画定时器（按 user_id），卸载时统一清理 */
  const completeTimersRef = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map());
  const enterTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** 跑马灯是否正在播放业务消息（false = 显示默认文本循环滚动） */
  const marqueeBusyRef = useRef(false);
  /** 待播消息队列（与 marqueeQueue 同步的 ref，供定时器回调读取） */
  const marqueeQueueRef = useRef<{ id: number; text: string }[]>([]);
  /** 播放令牌：递增即作废上一条消息的兜底定时器，避免重复推进 */
  const marqueeTokenRef = useRef(0);
  const defaultMarqueeRef = useRef(FALLBACK_MARQUEE);
  const bubbleSeqRef = useRef(0);
  const bubbleTimersRef = useRef<Map<number, ReturnType<typeof setTimeout>>>(new Map());

  /** 队列写操作串行化：无论命令同步还是异步，顺序都不依赖 Tauri 线程模型 */
  const serial = (fn: () => Promise<unknown>) => {
    opChainRef.current = opChainRef.current.then(fn).catch(console.error);
    return opChainRef.current;
  };

  /**
   * 队列写入唯一入口：fetchQueue、queue-updated 与命令成功回调都走这里。
   *
   * 版本检查必须在**所有**路径生效：旧 revision 的快照直接丢弃，
   * 同 revision 只允许 `PendingRetry → Saved` 推进。
   */
  const applyQueue = (snap: QueueSnapshot) => {
    const prev = authoritativeRef.current;
    if (snap.revision < prev.revision) return;
    if (snap.revision === prev.revision && prev.persistence === "Saved" && snap.persistence === "PendingRetry") {
      return;
    }
    authoritativeRef.current = snap;
    const items = snap.items;

    const ids = new Set(items.map((i) => i.id));
    // 后端仍在队中说明该单并未删除成功 → 撤回对应幽灵行，避免行残留
    setGhosts((prev) =>
      prev.some((g) => ids.has(g.item.id)) ? prev.filter((g) => !ids.has(g.item.id)) : prev,
    );

    const fresh = items.filter((i) => !knownIdsRef.current.has(i.id)).map((i) => i.id);
    knownIdsRef.current = new Set(items.map((i) => i.id));
    if (fresh.length > 0) {
      setEnteringIds(new Set(fresh));
      if (enterTimerRef.current) clearTimeout(enterTimerRef.current);
      enterTimerRef.current = setTimeout(() => setEnteringIds(new Set()), 500);
    }

    setQueue(items);
  };

  const fetchQueue = async () => {
    try {
      applyQueue(await invoke<QueueSnapshot>("get_queue"));
    } catch (e) {
      console.error(e);
    }
  };

  const fetchConfig = async () => {
    try {
      const cfg = await invoke<AppConfig>("get_app_config");
      if (cfg.default_marquee_text) {
        setDefaultMarquee(cfg.default_marquee_text);
        defaultMarqueeRef.current = cfg.default_marquee_text;
        if (!marqueeBusyRef.current) {
          setMarqueeText(cfg.default_marquee_text);
        }
      }
      // 用 ?? 而非 ||：透明度 0（全透明背景）是合法配置，不得被 falsy 判断吞成默认值
      setOpacity(cfg.opacity ?? 100);
      setPenetratingOpacity(cfg.penetrating_mode_opacity ?? 50);
      setTheme(cfg.overlay_theme === "asc" ? "asc" : "wilds");
      setDecor(!!cfg.enable_overlay_decor);
    } catch (e) {
      console.error(e);
    }
  };

  /** 开始播放一条业务消息：切换为非默认文本（单次滚动），并设置兜底定时器 */
  const startMarqueeMessage = (text: string) => {
    setMarqueeText(text);
    const token = ++marqueeTokenRef.current;
    window.setTimeout(() => {
      if (marqueeTokenRef.current === token) {
        onMarqueeFinished();
      }
    }, (MARQUEE_SECS + 1) * 1000);
  };

  /** 一条业务消息播放结束：取队首继续，队列空则回到默认文本 */
  const onMarqueeFinished = () => {
    marqueeTokenRef.current += 1;
    const next = marqueeQueueRef.current.shift();
    if (next) {
      marqueeQueueRef.current = [...marqueeQueueRef.current];
      setMarqueeQueue(marqueeQueueRef.current);
      startMarqueeMessage(next.text);
    } else {
      marqueeBusyRef.current = false;
      setMarqueeQueue([]);
      setMarqueeText(defaultMarqueeRef.current);
    }
  };

  /** 业务消息入队：空闲则立即播放，否则排队（原工程 AddRollingInfo 语义） */
  const pushMarquee = (text: string) => {
    if (!marqueeBusyRef.current) {
      marqueeBusyRef.current = true;
      startMarqueeMessage(text);
      return;
    }
    marqueeQueueRef.current = [
      ...marqueeQueueRef.current,
      { id: ++marqueeSeqRef.current, text },
    ];
    setMarqueeQueue(marqueeQueueRef.current);
  };

  // 气泡入栈：上限 5 条（超出移除最旧），15s 后自动退场（对齐原工程 AIBubbleControl 语义）
  const pushBubble = (bubble: Omit<OverlayBubble, "id">) => {
    const id = ++bubbleSeqRef.current;
    setBubbles((prev) => {
      const next = [...prev, { ...bubble, id }];
      const removed = next.length > MAX_BUBBLES ? next.splice(0, next.length - MAX_BUBBLES) : [];
      removed.forEach((b) => {
        const timer = bubbleTimersRef.current.get(b.id);
        if (timer) {
          clearTimeout(timer);
          bubbleTimersRef.current.delete(b.id);
        }
      });
      return next;
    });
    const timer = setTimeout(() => {
      bubbleTimersRef.current.delete(id);
      setBubbles((prev) => prev.filter((b) => b.id !== id));
    }, BUBBLE_TTL_MS);
    bubbleTimersRef.current.set(id, timer);
  };

  useEffect(() => {
    fetchQueue();
    fetchConfig();
    invoke<boolean>("get_overlay_locked")
      .then(setLocked)
      .catch(() => {});

    const interval = setInterval(fetchQueue, 1500);

    // 队列实时更新（权威快照，带版本与落盘状态）
    const unlistenQueue = listen<QueueSnapshot>("queue-updated", (event) => {
      applyQueue(event.payload);
    });

    // 顶栏连接状态点（复用主窗口同一事件源）
    invoke<ConnectionStatusPayload>("get_bili_connection_state")
      .then(setConn)
      .catch(() => {});
    const unlistenConn = listen<ConnectionStatusPayload>("connection-state-changed", (event) => {
      setConn(event.payload);
    });

    // D3 跑马灯：点怪成功提示入队（文案对齐原工程 DanmuManager.OnDanmuProcessed）
    const unlistenOrder = listen<OrderPlacedPayload>("order-placed", (event) => {
      const { user_name, monster_name, is_priority } = event.payload;
      const text = is_priority
        ? monster_name
          ? `${user_name} 优先 ${monster_name} 成功，已置前！`
          : `${user_name} 优先插队成功，已置前！`
        : `${user_name} 点怪 ${monster_name} 成功！`;
      pushMarquee(text);
    });

    // 禁点名单拦截：命中字典但该怪已在禁点名单内 —— 不入队，跑马灯就地提示原因
    const unlistenBlocked = listen<OrderBlockedPayload>("order-blocked", (event) => {
      const { user_name, monster_name } = event.payload;
      pushMarquee(`${user_name} 点怪 ${monster_name} 未生效（已在禁点名单）`);
    });

    // D4 停用模块气泡（打卡 / 补签 / 点赞）：Lite 构建下无此功能，监听整体剔除（noop 占位保持清理逻辑统一）
    const liteNoop = Promise.resolve(() => {});

    // 打卡数据不可持久化（Lite 形态停用，或完整版数据库故障）：
    // 必须让主播看到，而不是让指令静默消失
    const unlistenCheckinDown = __IS_LITE__
      ? liteNoop
      : listen<CheckinUnavailablePayload>("checkin-unavailable", (event) => {
          pushBubble({
            title: "打卡不可用",
            username: "系统",
            content: event.payload.message,
            tone: "system",
          });
        });

    // D3 点赞结算失败：整事件已回滚，必须让主播看见，且说明它是可重试的
    const unlistenLikeFailed = __IS_LITE__
      ? liteNoop
      : listen<LikeRewardFailedPayload>("like-reward-failed", (event) => {
          pushBubble({
            title: "点赞结算未完成",
            username: "系统",
            content: event.payload.message,
            tone: "system",
          });
        });

    // D4 舰长打卡回复气泡（原工程 CheckinTTSPlay 回调）
    const unlistenCheckin = __IS_LITE__
      ? liteNoop
      : listen<CheckinReplyPayload>("checkin-reply", (event) => {
          const { user_name, reply, is_ai } = event.payload;
          pushBubble({
            title: is_ai ? "舰长打卡 · AI 回复" : "舰长打卡",
            username: user_name,
            content: reply,
            tone: "checkin",
          });
        });

    // D4 补签结果气泡
    const unlistenRetro = __IS_LITE__
      ? liteNoop
      : listen<RetroactivePayload>("retroactive-checkin-recorded", (event) => {
          const { user_name, reply } = event.payload;
          pushBubble({ title: "补签结果", username: user_name, content: reply, tone: "retro" });
        });

    // D4 补签查询气泡（仅气泡不朗读）
    const unlistenQuery = __IS_LITE__
      ? liteNoop
      : listen<RetroactivePayload>("retroactive-query", (event) => {
          const { user_name, reply } = event.payload;
          pushBubble({ title: "补签查询", username: user_name, content: reply, tone: "retro" });
        });

    // D4 点赞奖卡气泡
    const unlistenLike = __IS_LITE__
      ? liteNoop
      : listen<LikeRewardPayload>("like-reward-granted", (event) => {
          const { user_name, replies } = event.payload;
          pushBubble({
            title: "点赞奖卡",
            username: user_name,
            content: replies.join("\n"),
            tone: "like",
          });
        });

    // D4 礼物 / SC / 上舰气泡：Lite 构建下无此功能，监听整体剔除
    const unlistenGift = __IS_LITE__
      ? liteNoop
      : listen<GiftReceivedPayload>("gift-received", (event) => {
          const { uname, gift_name, gift_num } = event.payload;
          pushBubble({
            title: "礼物",
            username: uname,
            content: `赠送 ${gift_name} ×${gift_num}`,
            tone: "gift",
          });
        });

    // D4 SC 气泡
    const unlistenSc = __IS_LITE__
      ? liteNoop
      : listen<SuperChatReceivedPayload>("super-chat-received", (event) => {
          const sc = event.payload;
          if (!sc || sc.kind !== "SuperChat") return;
          pushBubble({
            title: `醒目留言 ¥${sc.rmb}`,
            username: sc.uname,
            content: sc.message,
            tone: "like",
          });
        });

    // D4 上舰气泡
    const unlistenGuard = __IS_LITE__
      ? liteNoop
      : listen<GuardReceivedPayload>("guard-received", (event) => {
          const g = event.payload;
          if (!g || g.kind !== "Guard") return;
          pushBubble({
            title: "上舰",
            username: g.uname,
            content: `开通 ${g.guard_unit} ×${g.guard_num}（等级 ${g.guard_level}）`,
            tone: "gift",
          });
        });

    // 主窗口保存配置后即时刷新跑马灯 / 透明度，无需重启
    const unlistenConfig = listen("config-changed", () => {
      fetchConfig();
    });

    // D2 锁定状态（命令 / Alt+, 热键均会广播）
    const unlistenLock = listen<boolean>("overlay-lock-changed", (event) => {
      setLocked(event.payload);
    });

    // D2 窗口位置记忆：拖动结束后防抖落盘 top_pos_x/y
    const appWindow = getCurrentWindow();
    const unlistenMoved = appWindow.onMoved(async ({ payload }) => {
      try {
        const scale = await appWindow.scaleFactor();
        await invoke("save_overlay_position", {
          x: payload.x / scale,
          y: payload.y / scale,
        });
      } catch (err) {
        console.error("记忆悬浮窗位置失败:", err);
      }
    });

    return () => {
      clearInterval(interval);
      unlistenQueue.then((f) => f());
      unlistenCheckinDown.then((f) => f());
      unlistenLikeFailed.then((f) => f());
      unlistenOrder.then((f) => f());
      unlistenBlocked.then((f) => f());
      unlistenCheckin.then((f) => f());
      unlistenRetro.then((f) => f());
      unlistenQuery.then((f) => f());
      unlistenLike.then((f) => f());
      unlistenGift.then((f) => f());
      unlistenSc.then((f) => f());
      unlistenGuard.then((f) => f());
      unlistenConfig.then((f) => f());
      unlistenLock.then((f) => f());
      unlistenConn.then((f) => f());
      unlistenMoved.then((f) => f());
      bubbleTimersRef.current.forEach((timer) => clearTimeout(timer));
      bubbleTimersRef.current.clear();
      completeTimersRef.current.forEach((timer) => clearTimeout(timer));
      completeTimersRef.current.clear();
      if (enterTimerRef.current) clearTimeout(enterTimerRef.current);
    };
  }, []);

  // 撤销垫 TTL：每次完成压栈后重新计时，超时自动收起
  useEffect(() => {
    if (undoStack.length === 0) return;
    const timer = setTimeout(() => setUndoStack([]), UNDO_TTL_MS);
    return () => clearTimeout(timer);
  }, [undoStack]);

  /** 是否正在显示默认文本（决定循环滚动样式与单次滚动样式） */
  const isDefaultMarquee = marqueeText === defaultMarquee;

  // 默认文本：循环滚动（维持现有 animate-marquee）；业务消息：单次滚动后回默认
  const marqueeStyle = isDefaultMarquee
    ? undefined
    : ({ animation: `marquee ${MARQUEE_SECS}s linear 1` } as React.CSSProperties);

  // 拖拽预览顺序：按预览顺序排rank，队列里新增的条目（拖拽期间入队）保持末位
  const orderedQueue = useMemo<QueueItem[]>(() => {
    if (!drag) return queue;
    const rank = new Map(drag.order.map((uid, i) => [uid, i]));
    return [...queue].sort(
      (a, b) =>
        (rank.get(a.user_id) ?? Number.MAX_SAFE_INTEGER) - (rank.get(b.user_id) ?? Number.MAX_SAFE_INTEGER),
    );
  }, [queue, drag]);

  // 渲染列表：存活条目 + 完成动画中的暂留行（按下标插回原位）
  const renderRows = useMemo<RenderRow[]>(() => {
    const out: RenderRow[] = orderedQueue.map((item) => ({ item, ghost: false }));
    if (ghosts.length > 0) {
      for (const g of [...ghosts].sort((a, b) => a.index - b.index)) {
        out.splice(Math.min(Math.max(g.index, 0), out.length), 0, { item: g.item, ghost: true });
      }
    }
    return out;
  }, [orderedQueue, ghosts]);

  /** 被拖条目在渲染列表中的下标（浮起条目沿用同一序号，列表内序号因此连续不断档） */
  const draggedIndex = useMemo(
    () => (drag ? renderRows.findIndex((r) => r.item.user_id === drag.uid) : -1),
    [renderRows, drag?.uid],
  );

  // 窗口全局拖拽处理：按住悬浮窗非按钮非条目手柄区域自由拖动窗口
  const handleWindowMouseDown = async (e: React.MouseEvent) => {
    if (locked) return;
    if (e.button !== 0) return; // 仅限左键
    const target = e.target as HTMLElement;
    if (
      target.closest("button") ||
      target.closest("input") ||
      target.closest("[data-no-window-drag]") ||
      target.closest("[draggable='true']")
    ) {
      return;
    }
    try {
      const appWindow = getCurrentWindow();
      await appWindow.startDragging();
    } catch (err) {
      console.error("Window drag error:", err);
    }
  };

  /** 提交拖拽结果：把预览顺序落到后端（顺序没变就不发命令） */
  const commitDragOrder = (order: string[]) => {
    const current = authoritativeRef.current;
    const rank = new Map(order.map((uid, i) => [uid, i]));
    // 拖拽期间队列可能被弹幕改动：长度或成员对不上就放弃，交由 queue-updated 校正
    if (order.length !== current.items.length || current.items.some((it) => rank.get(it.user_id) === undefined)) return;

    const list = [...current.items].sort((a, b) => rank.get(a.user_id)! - rank.get(b.user_id)!);
    if (list.every((it, i) => it.user_id === current.items[i].user_id)) return;

    // 不做乐观整表提交：预览由 drag state 渲染，权威顺序只由后端快照决定
    serial(async () => {
      try {
        const snap = await invoke<QueueSnapshot>("reorder_queue", {
          orderedUserIds: order,
          expectedRevision: current.revision,
        });
        applyQueue(snap);
      } catch (err) {
        pushMarquee(`排序未生效：${err}`);
        await fetchQueue();
      }
    });
  };

  /**
   * 拖拽排序：按下行首手柄后由指针事件驱动（等价于原工程 ListView 的 DoDragDrop）。
   * 不用 HTML5 drag/drop —— Windows 上 WebView 的拖拽循环会与页面争抢落点，
   * 松手时常常收不到 drop，表现为「拖得动、松手没反应」。
   * setPointerCapture 把后续事件绑在手柄上：指针移出窗口也不会丢 pointerup。
   */
  const handleGripPointerDown = (e: React.PointerEvent<HTMLElement>, userId: string) => {
    if (locked || e.button !== 0) return;
    e.preventDefault(); // 阻止拖动时选中文本、触发原生拖放
    e.stopPropagation(); // 不冒泡到行（点击完成）与窗口拖动
    const rowEl = e.currentTarget.closest<HTMLElement>(".queue-row");
    const origin = authoritativeRef.current.items.map((i) => i.user_id);
    if (!rowEl || !origin.includes(userId)) return;

    const rect = rowEl.getBoundingClientRect();
    // 捕获挂在外层根节点：拖拽中列表会重排、被拖行可能被虚拟列表摘掉，手柄不是稳定宿主
    rootRef.current?.setPointerCapture(e.pointerId);
    dragGeomRef.current = {
      grabOffset: e.clientY - rect.top,
      left: rect.left,
      width: rect.width,
      y: rect.top,
      valid: true,
    };
    setDrag({ uid: userId, origin, order: origin });
  };

  /**
   * 落点跟随：被拖条目浮起跟手，列表里按「锚点行的上半区插到其前、下半区插到其后」让位。
   * 用锚点行的 user_id 而不是下标表达落点：虚拟列表会滚动、完成动画会插入暂留行，下标不可靠。
   * 位置一律取布局值：让位补间期间 rect 含在途 transform，按它判定会让落点随动画来回跳。
   */
  const handleGripPointerMove = (e: React.PointerEvent<HTMLElement>) => {
    const geom = dragGeomRef.current;
    if (!drag || !geom) return;
    const rows = listRef.current?.querySelectorAll<HTMLElement>(".queue-row:not(.queue-row-completing)");
    if (!rows || rows.length === 0) return;

    const y = e.clientY;
    // 跟手：直接写浮起层，免得每个指针事件都 setState 让整列重渲染
    geom.y = y - geom.grabOffset;
    if (floatRef.current) floatRef.current.style.transform = `translate3d(0, ${geom.y}px, 0)`;

    const first = layoutTop(rows[0]);
    const last = layoutTop(rows[rows.length - 1]) + rows[rows.length - 1].offsetHeight;
    // 首尾各放宽半行：行间 4px 缝隙与轻微过冲仍算可落地
    geom.valid = y >= first - QUEUE_ROW_HEIGHT / 2 && y <= last + QUEUE_ROW_HEIGHT / 2;
    if (!geom.valid) return; // 区域外不改预览，松手按 valid 决定提交还是收回

    let anchorUid: string | null = null;
    let after = false;
    for (const el of rows) {
      const top = layoutTop(el);
      const bottom = top + el.offsetHeight;
      if (y < bottom) {
        anchorUid = el.dataset.uid ?? null;
        after = y > top + el.offsetHeight / 2;
        break;
      }
    }
    if (!anchorUid) {
      anchorUid = rows[rows.length - 1].dataset.uid ?? null;
      after = true;
    }
    if (!anchorUid) return;

    const rest = drag.order.filter((u) => u !== drag.uid);
    const at = rest.indexOf(anchorUid);
    // at === -1 表示锚点就是被拖条目自己：指针还在原地，无需让位
    if (at === -1) return;

    const insert = at + (after ? 1 : 0);
    const next = [...rest.slice(0, insert), drag.uid, ...rest.slice(insert)];
    if (next.some((u, i) => u !== drag.order[i])) setDrag({ ...drag, order: next });
  };

  const handleGripPointerUp = () => {
    const geom = dragGeomRef.current;
    const d = drag;
    dragGeomRef.current = null;
    setDrag(null);
    if (!geom || !d) return;
    if (!locked && geom.valid && d.order.some((u, i) => u !== d.origin[i])) commitDragOrder(d.order);
  };

  /** 取消拖拽（指针捕获丢失 / 被系统取消）：原样收回，不提交 */
  const cancelGripDrag = () => {
    dragGeomRef.current = null;
    setDrag(null);
  };

  /** 元素当前布局顶边：rect 含在途过渡的 transform，减掉才是真实布局位置 */
  const layoutTop = (el: HTMLElement) => {
    const rect = el.getBoundingClientRect().top;
    const t = getComputedStyle(el).transform;
    if (!t || t === "none") return rect;
    const m = /matrix\(([^)]+)\)/.exec(t);
    if (!m) return rect;
    const parts = m[1].split(",").map(Number);
    const ty = parts.length >= 6 ? parts[5] : 0;
    return rect - (Number.isFinite(ty) ? ty : 0);
  };

  /**
   * 让位补间（FLIP）：行序变化时给位置变动的行补一段过渡，让其他条目「自然贴合」。
   * 两道保险，缺一个就会抖：
   * - 位置取布局值（layoutTop）：rect 里含在途 transform，直接当基准会逐帧叠加；
   * - 只在行序或滚动基准变化时才测量并推进：拖动时每帧都在重渲染，无条件推进必然打架。
   */
  useLayoutEffect(() => {
    const list = listRef.current ? [...listRef.current.querySelectorAll<HTMLElement>(".queue-row")] : [];
    if (list.length === 0) return;

    const key = list.map((el) => el.dataset.uid ?? "").join(",");
    const firstTop = layoutTop(list[0]);
    const prev = rowLayoutRef.current;
    if (prev.key === key && Math.abs(prev.firstTop - firstTop) < 0.5) return;

    const tops = new Map<string, number>();
    for (const el of list) {
      const uid = el.dataset.uid;
      if (uid) tops.set(uid, layoutTop(el));
    }

    // 行序没变、只是整体位移（滚动）：更新基准即可，补间会把滚动量再走一遍
    if (prev.key === key) {
      rowLayoutRef.current = { key, firstTop, tops };
      return;
    }

    for (const el of list) {
      const uid = el.dataset.uid;
      if (!uid || drag?.uid === uid) continue;
      const before = prev.tops.get(uid);
      const top = tops.get(uid);
      if (before === undefined || top === undefined || Math.abs(before - top) < 0.5) continue;
      el.style.transition = "none";
      el.style.transform = `translateY(${(before - top).toFixed(1)}px)`;
      // 读一次布局：把反向位移确立为起始态，否则两次样式会被合并、过渡不触发
      void el.getBoundingClientRect();
      requestAnimationFrame(() => {
        el.style.transition =
          "transform 0.18s cubic-bezier(0.2, 0.9, 0.25, 1), border-color 0.16s, background 0.16s";
        el.style.transform = "";
      });
    }
    rowLayoutRef.current = { key, firstTop, tops };
  });

  // 浮起层落位：挂载后与每次重渲染后都要贴一次（拖动中的跟手由指针事件直接写）
  useLayoutEffect(() => {
    const el = floatRef.current;
    const geom = dragGeomRef.current;
    if (el && geom) el.style.transform = `translate3d(0, ${geom.y}px, 0)`;
  });

  /** 完成一单：整行点击 → 钤印 → 离场，可撤销。
   *  钤印特效不渲染在行内：虚拟列表滚动容器 overflow-y:auto 必裁行内上溢内容，
   *  第一行的 2× 印面会被列表顶边裁掉 —— 特效层挂面板根（见渲染处 queue-clear-fx），
   *  此处只捕获行完成瞬间的面板坐标 */
  const handleComplete = (item: QueueItem, index: number, rect: GhostRow["rect"]) => {
    if (locked) return;
    if (completingRef.current.has(item.user_id)) return; // 行级锁：连点同一行只删一次

    completingRef.current.add(item.user_id);
    // 乐观移除 + 幽灵行插回原位：同一 key 就地复用 DOM，动画不会被打断。
    // 此处只改本地渲染 state，authoritativeRef 保持后端权威快照，等命令回执再更新
    setGhosts((prev) => [...prev, { item, index, rect }]);
    setQueue((prev) => prev.filter((i) => i.user_id !== item.user_id));

    // 串行链保证回执按序到达；版本检查会丢弃任何迟到的旧快照
    serial(async () => {
      try {
        applyQueue(await invoke<QueueSnapshot>("dequeue_by_user_id", { userId: item.user_id }));
      } catch (e) {
        console.error(e);
      }
    });

    const timer = setTimeout(() => {
      completeTimersRef.current.delete(item.user_id);
      completingRef.current.delete(item.user_id);
      setGhosts((prev) => prev.filter((g) => g.item.user_id !== item.user_id));
      setUndoStack((prev) => [...prev, { item, index }]);
    }, COMPLETE_ANIM_MS);
    completeTimersRef.current.set(item.user_id, timer);
  };

  /** 撤销完成：原样插回原下标（保留 id 与 timestamp，不重置插入语义） */
  const handleUndo = () => {
    if (locked) return;
    const record = undoStack[undoStack.length - 1];
    if (!record) return;

    const at = Math.min(record.index, authoritativeRef.current.items.length);
    // 后端确认成功后才消耗撤销入口；同一用户已重新入队时必须保留条目并提示失败
    serial(async () => {
      try {
        const snap = await invoke<QueueSnapshot>("restore_order", {
          item: record.item,
          index: at,
        });
        applyQueue(snap);
        setUndoStack((prev) => prev.filter((r) => r !== record));
      } catch (err) {
        pushMarquee(`撤销失败：${err}`);
        await fetchQueue();
      }
    });
  };

  const handleClose = async () => {
    try {
      await invoke("hide_window", { label: "overlay" });
    } catch {
      try {
        const appWindow = getCurrentWindow();
        await appWindow.hide();
      } catch (e) {
        console.error("Failed to hide window:", e);
      }
    }
  };

  // D8：透明度仅作用于背景层（原工程仅插值 MainGrid 背景 alpha），文字与条目保持不透明；
  // 锁定时使用穿透模式透明度（原 RefreshWindow 语义）
  const backgroundAlpha = (locked ? penetratingOpacity : opacity) / 100;

  /** 大航海等级的双斜杠标记（数量对齐设计稿：直接取 guard_level） */
  const guardChevron = (level: number) => (
    <span className="queue-chev">
      {Array.from({ length: level }, (_, i) => (
        <i key={i} />
      ))}
    </span>
  );

  /** 行内容：列表行与浮起条目共用（浮起层 pointer-events: none，事件不会触发）。
   *  钤印特效（环/十字光/印面）不在行内渲染 —— 列表滚动容器裁切行内上溢，
   *  统一由面板根的 .queue-clear-fx 特效层承载 */
  const renderRowBody = (item: QueueItem, seq: number) => (
    <>
      <span className="queue-spine" />
      <span className="queue-corner tl" />
      <span className="queue-corner br" />

      {/* 行首手柄热区：手柄图标与顺位号一起可按下（到昵称之前为止）；stopPropagation 挡住冒泡成点击完成 */}
      <span
        className="queue-handle"
        title="拖拽调整顺序"
        onClick={(e) => e.stopPropagation()}
        onPointerDown={(e) => handleGripPointerDown(e, item.user_id)}
      >
        <span className="queue-grip">
          <GripVertical className="w-3 h-3" />
        </span>
        <span className="queue-seq">{String(seq).padStart(2, "0")}</span>
      </span>

      <MarqueeText text={item.user_name} className="queue-nick" />

      <span className="queue-iconcard">
        {item.icon_url ? (
          <img
            src={`/monster_icons/${item.icon_url}`}
            alt={item.monster_name}
            onError={(e) => {
              (e.target as HTMLElement).style.display = "none";
            }}
          />
        ) : (
          <Shield className="icon-fallback w-5 h-5" />
        )}
      </span>

      <MarqueeText text={item.monster_name} className="queue-mon" textClassName="queue-mon-text" />

      {/* 稀有度已由图鉴卡描边与脊线表达，徽章组只放舰长等级与优先，最多两枚 */}
      <span className="queue-badges">
        {item.guard_level >= 1 && item.guard_level <= 3 && (
          <span className={`queue-chip g${item.guard_level}`}>
            {guardChevron(item.guard_level)}
            {GUARD_NAMES[item.guard_level]}
          </span>
        )}
        {item.is_priority && <span className="queue-chip prio">优先</span>}
      </span>

      <span className="queue-hint">点击完成</span>
      <span className="queue-sheen" />
    </>
  );

  /** 行内容的面板坐标（面板与根容器同尺寸，差值即相对面板原点的偏移） */
  const rowRectInPanel = (el: HTMLElement) => {
    const panel = rootRef.current?.getBoundingClientRect();
    const r = el.getBoundingClientRect();
    return {
      left: r.left - (panel?.left ?? 0),
      top: r.top - (panel?.top ?? 0),
      width: r.width,
      height: r.height,
    };
  };

  const renderQueueRow = (row: RenderRow, idx: number) => {
    const { item, ghost } = row;
    // 被拖条目在列表里只留空位（内容已在浮起层），其余条目按预览顺序让位
    const placeholder = !ghost && drag?.uid === item.user_id;
    const classNames = ["queue-row"];
    if (ghost) classNames.push("queue-row-completing");
    else if (placeholder) classNames.push("queue-row-placeholder");
    if (enteringIds.has(item.id)) classNames.push("queue-row-enter");

    return (
      <div
        key={item.id}
        className={classNames.join(" ")}
        data-rarity={item.tempered_level}
        data-priority={item.is_priority ? "1" : "0"}
        data-uid={item.user_id}
        data-no-window-drag
        onClick={placeholder ? undefined : (e) => handleComplete(item, idx, rowRectInPanel(e.currentTarget))}
      >
        {placeholder ? null : renderRowBody(item, idx + 1)}
      </div>
    );
  };

  /** 被抓起条目（列表里对应位置已空出，内容在浮起层渲染） */
  const draggedRow = drag && draggedIndex >= 0 ? renderRows[draggedIndex] : null;

  return (
    <div
      ref={rootRef}
      data-theme={theme}
      data-decor={decor ? "on" : "off"}
      onMouseDown={handleWindowMouseDown}
      onPointerMove={handleGripPointerMove}
      onPointerUp={handleGripPointerUp}
      onPointerCancel={cancelGripDrag}
      onLostPointerCapture={cancelGripDrag}
      className="h-screen w-screen bg-transparent select-none overflow-hidden font-sans cursor-default"
    >
      <div
        data-tauri-drag-region
        onMouseDown={handleWindowMouseDown}
        style={{ "--panel-alpha": String(backgroundAlpha) } as React.CSSProperties}
        className="overlay-panel h-full w-full flex flex-col overflow-hidden relative cursor-move"
      >
        {/* 环境装饰层（荒野：沙霾浮尘 / 凌越：蓝雾赤雷，默认关闭；凌越主题第三团为左下红雾） */}
        <span className="overlay-sky">
          <i />
          <i />
          <i />
        </span>
        <div className="overlay-drift" />
        <div className="overlay-flash" />

        {/* 凌越红轮水印：取自 ASCENDANCE logo 下部红轮法阵，仅凌越主题显示（样式见 App.css .overlay-ringwm） */}
        <svg className="overlay-ringwm" viewBox="0 0 100 100" aria-hidden="true">
          <g fill="none" stroke="currentColor">
            <circle cx="50" cy="50" r="47" strokeWidth="1.2" />
            <circle cx="50" cy="50" r="36" strokeWidth="0.8" strokeDasharray="3 4" />
            <circle cx="50" cy="50" r="26" strokeWidth="1.6" />
            <circle cx="50" cy="50" r="17" strokeWidth="0.8" strokeDasharray="6 3" />
            <circle cx="50" cy="50" r="6" strokeWidth="2" />
            <path d="M50 3v10M50 87v10M3 50h10M87 50h10" strokeWidth="1.4" />
            <path d="M17 17l6 6M77 77l6 6M83 17l-6 6M23 77l-6 6" strokeWidth="1" />
          </g>
        </svg>

        {/* 顶栏与跑马灯合并为一行，腾出行高（对齐原工程 36px 跑马灯区） */}
        <div
          data-tauri-drag-region
          onMouseDown={handleWindowMouseDown}
          className="relative z-[2] flex items-center gap-2 px-2.5 pt-1.5 pb-1 shrink-0 cursor-move"
        >
          <span
            className={`w-1.5 h-1.5 rounded-full shrink-0 ${
              CONN_DOT_COLORS[conn?.state ?? "Disconnected"]
            }`}
          />
          <span className="overlay-title pointer-events-none">狩猎点单队列</span>
          <span className="overlay-count pointer-events-none">
            {String(queue.length).padStart(2, "0")}
          </span>
          {locked && (
            <span
              className="pointer-events-none flex items-center gap-0.5 text-[10.5px] border px-1 py-0.5 rounded-sm shrink-0"
              style={{ color: "var(--hl)", borderColor: "var(--line-soft)" }}
            >
              <Lock className="w-2.5 h-2.5" /> 已锁定
            </span>
          )}

          <div className="flex-1 min-w-0 overflow-hidden whitespace-nowrap pointer-events-none flex items-center gap-1.5">
            <Volume2 className="w-3 h-3 shrink-0" style={{ color: "var(--hl)" }} />
            <span className="overflow-hidden flex-1">
              <span
                key={marqueeText}
                onAnimationEnd={isDefaultMarquee ? undefined : onMarqueeFinished}
                style={
                  isDefaultMarquee
                    ? { color: "var(--ink-dim)" }
                    : { ...marqueeStyle, color: "var(--hl2)", fontWeight: 700 }
                }
                className={isDefaultMarquee ? "animate-marquee text-[12.5px] font-medium" : "inline-block whitespace-nowrap text-[12.5px]"}
              >
                {marqueeText}
              </span>
            </span>
            {marqueeQueue.length > 0 && (
              <span className="text-[10.5px] shrink-0 font-mono" style={{ color: "var(--ink-faint)" }}>
                +{marqueeQueue.length}
              </span>
            )}
          </div>

          <button
            onClick={handleClose}
            className="p-1 rounded transition cursor-pointer shrink-0 hover:bg-red-500/30"
            style={{ color: "var(--ink-faint)" }}
            title="隐藏悬浮窗"
          >
            <X className="w-3.5 h-3.5" />
          </button>
        </div>

        {/* 几何回纹饰带：MH 系列品牌纹样（凌越主题在其上叠红色回纹，mask 只留右段，见 .overlay-band-red） */}
        <div className="relative z-[2] shrink-0">
          <svg
            className="overlay-band"
            viewBox="0 0 440 9"
            preserveAspectRatio="none"
            aria-hidden="true"
          >
            <defs>
              <pattern id="overlay-meander" width="14" height="9" patternUnits="userSpaceOnUse">
                <path d="M1 8V1h12v6H4V3h7" fill="none" stroke="currentColor" strokeWidth="1" />
              </pattern>
            </defs>
            <rect width="440" height="9" fill="url(#overlay-meander)" />
          </svg>
          <svg
            className="overlay-band-red"
            viewBox="0 0 440 9"
            preserveAspectRatio="none"
            aria-hidden="true"
          >
            <defs>
              <pattern id="overlay-meander-red" width="14" height="9" patternUnits="userSpaceOnUse">
                <path d="M1 8V1h12v6H4V3h7" fill="none" stroke="currentColor" strokeWidth="1" />
              </pattern>
            </defs>
            <rect width="440" height="9" fill="url(#overlay-meander-red)" />
          </svg>
        </div>

        {/* D4 业务气泡（多条堆叠，最多 5 条，15s 自动退场） */}
        {bubbles.length > 0 && (
          <div className="absolute top-[46px] left-1.5 right-1.5 bottom-2 z-50 space-y-1.5 pointer-events-none overflow-hidden">
            {bubbles
              .slice()
              .reverse()
              .map((bubble) => (
                <div
                  key={bubble.id}
                  className={`rounded-lg p-2.5 shadow-2xl backdrop-blur-md animate-in fade-in zoom-in duration-200 cursor-default border ${BUBBLE_TONES[bubble.tone]}`}
                >
                  <div className="flex items-center gap-1.5 text-xs font-bold mb-1">
                    <Bell className="w-3.5 h-3.5" />
                    <span>{bubble.title}</span>
                    <span className="text-[10px] text-gray-400 font-normal">
                      @{bubble.username}
                    </span>
                  </div>

                  <div className="text-xs text-white font-medium break-words leading-relaxed whitespace-pre-line">
                    {bubble.content}
                  </div>
                </div>
              ))}
          </div>
        )}

        {/* 核心排队条目列表（虚拟化：仅渲染可视区行） */}
        <div
          ref={listRef}
          onMouseDown={handleWindowMouseDown}
          className="relative z-[2] flex-1 min-h-0 px-1"
        >
          {renderRows.length === 0 ? (
            <div
              data-tauri-drag-region
              onMouseDown={handleWindowMouseDown}
              className="queue-empty cursor-move"
            >
              <div className="flex flex-col items-center gap-1.5">
                <Shield className="w-7 h-7 mb-1 pointer-events-none" style={{ opacity: 0.5 }} />
                <span className="pointer-events-none" style={{ color: "var(--ink-dim)" }}>
                  当前排队为空
                </span>
                <span className="text-[11.5px] pointer-events-none">
                  发送弹幕【点怪 怪物名】即可上榜
                </span>
              </div>
            </div>
          ) : (
            <VirtualList
              items={renderRows}
              rowHeight={QUEUE_ROW_HEIGHT}
              className="h-full overflow-y-auto scrollbar-thin scrollbar-thumb-gray-800"
              renderItem={renderQueueRow}
            />
          )}
        </div>

        {/* 撤销垫：连点几单就压几层，点一次回退一单 */}
        {undoStack.length > 0 && (
          <div className="queue-undo show">
            <span>
              「{undoStack[undoStack.length - 1].item.monster_name}」讨伐完毕
              {undoStack.length > 1 ? ` · 可撤销 ${undoStack.length} 单` : ""}
            </span>
            <button onClick={handleUndo}>撤销</button>
          </div>
        )}

        {/* 钤印特效层：面板根直挂，矩形对齐完成行。虚拟列表滚动容器 overflow-y:auto
            必裁行内上溢内容（第一行的 2× 印面曾被列表顶边裁掉），列表外渲染才能
            盖过顶栏；absolute 定位随窗口整体移动。z 压过顶栏/气泡/拖拽浮起，
            播放期间（980ms）为悬浮窗最上层 */}
        {ghosts.map((g) => (
          <div
            key={g.item.user_id}
            className="queue-clear-fx"
            style={{ left: g.rect.left, top: g.rect.top, width: g.rect.width, height: g.rect.height }}
          >
            <span className="queue-ring" />
            <span className="queue-flare">
              <i />
              <i />
            </span>
            <span className="queue-stamp">QUEST CLEAR</span>
          </div>
        ))}
      </div>

      {/* 被抓起条目：脱离列表流浮起、跟手移动；列表内对应位置已空出（fixed 定位，不随列表滚动） */}
      {drag && draggedRow && (
        <div
          ref={floatRef}
          className="queue-float"
          style={{ left: dragGeomRef.current?.left ?? 0, width: dragGeomRef.current?.width ?? 0 }}
          aria-hidden="true"
        >
          <div
            className="queue-row queue-row-lifted"
            data-rarity={draggedRow.item.tempered_level}
            data-priority={draggedRow.item.is_priority ? "1" : "0"}
          >
            {renderRowBody(draggedRow.item, draggedIndex + 1)}
          </div>
        </div>
      )}
    </div>
  );
};
