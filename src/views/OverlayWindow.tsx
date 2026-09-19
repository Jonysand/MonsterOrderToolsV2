import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  QueueItem,
  AppConfig,
  AIBubblePayload,
  OrderPlacedPayload,
  CheckinReplyPayload,
  RetroactivePayload,
  LikeRewardPayload,
  GiftReceivedPayload,
  SuperChatReceivedPayload,
  GuardReceivedPayload,
} from "../types";
import { VirtualList } from "../components/VirtualList";
import { MarqueeText } from "../components/MarqueeText";
import { Shield, X, GripHorizontal, GripVertical, Sparkles, Flame, Volume2, Bot, Lock, Bell } from "lucide-react";

const FALLBACK_MARQUEE = "发送'点怪 xxx'进行点怪";
/** 单条跑马灯滚动时长（原工程固定 10s） */
const MARQUEE_SECS = 10;
const MAX_BUBBLES = 5;
const BUBBLE_TTL_MS = 15000;
/** 虚拟列表行高（条目卡 44px + 下间距 6px） */
const QUEUE_ROW_HEIGHT = 50;

type BubbleTone = "ai" | "checkin" | "retro" | "like" | "gift" | "system";

interface OverlayBubble {
  id: number;
  title: string;
  username: string;
  content: string;
  tone: BubbleTone;
  reasoning?: string;
}

const BUBBLE_TONES: Record<BubbleTone, string> = {
  ai: "bg-indigo-950/95 border-cyan-400/60 text-cyan-300",
  checkin: "bg-emerald-950/95 border-emerald-400/60 text-emerald-300",
  retro: "bg-purple-950/95 border-purple-400/60 text-purple-300",
  like: "bg-amber-950/95 border-amber-400/70 text-amber-300",
  gift: "bg-rose-950/95 border-rose-400/60 text-rose-300",
  system: "bg-gray-900/95 border-gray-500/60 text-gray-300",
};

export const OverlayWindow: React.FC = () => {
  const [queue, setQueue] = useState<QueueItem[]>([]);
  const [defaultMarquee, setDefaultMarquee] = useState<string>(FALLBACK_MARQUEE);
  const [marqueeText, setMarqueeText] = useState<string>(FALLBACK_MARQUEE);
  const [marqueeQueue, setMarqueeQueue] = useState<{ id: number; text: string }[]>([]);
  const [opacity, setOpacity] = useState<number>(95);
  const [penetratingOpacity, setPenetratingOpacity] = useState<number>(50);
  const [locked, setLocked] = useState<boolean>(false);
  const [bubbles, setBubbles] = useState<OverlayBubble[]>([]);
  const [draggedIndex, setDraggedIndex] = useState<number | null>(null);
  const marqueeSeqRef = useRef(0);
  /** 跑马灯是否正在播放业务消息（false = 显示默认文本循环滚动） */
  const marqueeBusyRef = useRef(false);
  /** 待播消息队列（与 marqueeQueue 同步的 ref，供定时器回调读取） */
  const marqueeQueueRef = useRef<{ id: number; text: string }[]>([]);
  /** 播放令牌：递增即作废上一条消息的兜底定时器，避免重复推进 */
  const marqueeTokenRef = useRef(0);
  const defaultMarqueeRef = useRef(FALLBACK_MARQUEE);
  const bubbleSeqRef = useRef(0);
  const bubbleTimersRef = useRef<Map<number, ReturnType<typeof setTimeout>>>(new Map());

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
      if (cfg.default_marquee_text) {
        setDefaultMarquee(cfg.default_marquee_text);
        defaultMarqueeRef.current = cfg.default_marquee_text;
        if (!marqueeBusyRef.current) {
          setMarqueeText(cfg.default_marquee_text);
        }
      }
      setOpacity(cfg.opacity || 95);
      setPenetratingOpacity(cfg.penetrating_mode_opacity ?? 50);
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

    // 队列实时更新
    const unlistenQueue = listen<QueueItem[]>("queue-updated", (event) => {
      setQueue(event.payload);
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

    // D4 AI 气泡（思考中 → 回答）
    const unlistenAi = listen<AIBubblePayload>("ai-bubble", (event) => {
      const payload = event.payload;
      if (payload.is_thinking) {
        pushBubble({
          title: "随从猫 AI 正在沉思中...",
          username: payload.username,
          content: "思考中...",
          tone: "ai",
        });
        return;
      }
      pushBubble({
        title: "随从猫 AI 回答",
        username: payload.username,
        content: payload.answer,
        tone: "ai",
        reasoning: payload.reasoning,
      });
    });

    // D4 舰长打卡回复气泡（原工程 CheckinTTSPlay 回调）
    const unlistenCheckin = listen<CheckinReplyPayload>("checkin-reply", (event) => {
      const { user_name, reply, is_ai } = event.payload;
      pushBubble({
        title: is_ai ? "舰长打卡 · AI 回复" : "舰长打卡",
        username: user_name,
        content: reply,
        tone: "checkin",
      });
    });

    // D4 补签结果气泡
    const unlistenRetro = listen<RetroactivePayload>("retroactive-checkin-recorded", (event) => {
      const { user_name, reply } = event.payload;
      pushBubble({ title: "补签结果", username: user_name, content: reply, tone: "retro" });
    });

    // D4 补签查询气泡（仅气泡不朗读）
    const unlistenQuery = listen<RetroactivePayload>("retroactive-query", (event) => {
      const { user_name, reply } = event.payload;
      pushBubble({ title: "补签查询", username: user_name, content: reply, tone: "retro" });
    });

    // D4 点赞奖卡气泡
    const unlistenLike = listen<LikeRewardPayload>("like-reward-granted", (event) => {
      const { user_name, replies } = event.payload;
      pushBubble({
        title: "点赞奖卡",
        username: user_name,
        content: replies.join("\n"),
        tone: "like",
      });
    });

    // D4 礼物气泡
    const unlistenGift = listen<GiftReceivedPayload>("gift-received", (event) => {
      const { uname, gift_name, gift_num } = event.payload;
      pushBubble({
        title: "礼物",
        username: uname,
        content: `赠送 ${gift_name} ×${gift_num}`,
        tone: "gift",
      });
    });

    // D4 SC / 上舰气泡
    const unlistenSc = listen<SuperChatReceivedPayload>("super-chat-received", (event) => {
      const sc = event.payload.SuperChat;
      if (!sc) return;
      pushBubble({
        title: `醒目留言 ¥${sc.rmb}`,
        username: sc.uname,
        content: sc.message,
        tone: "like",
      });
    });
    const unlistenGuard = listen<GuardReceivedPayload>("guard-received", (event) => {
      const g = event.payload.Guard;
      if (!g) return;
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
      unlistenOrder.then((f) => f());
      unlistenAi.then((f) => f());
      unlistenCheckin.then((f) => f());
      unlistenRetro.then((f) => f());
      unlistenQuery.then((f) => f());
      unlistenLike.then((f) => f());
      unlistenGift.then((f) => f());
      unlistenSc.then((f) => f());
      unlistenGuard.then((f) => f());
      unlistenConfig.then((f) => f());
      unlistenLock.then((f) => f());
      unlistenMoved.then((f) => f());
      bubbleTimersRef.current.forEach((timer) => clearTimeout(timer));
      bubbleTimersRef.current.clear();
    };
  }, []);

  /** 是否正在显示默认文本（决定循环滚动样式与单次滚动样式） */
  const isDefaultMarquee = marqueeText === defaultMarquee;

  // 默认文本：循环滚动（维持现有 animate-marquee）；业务消息：单次滚动后回默认
  const marqueeStyle = isDefaultMarquee
    ? undefined
    : ({ animation: `marquee ${MARQUEE_SECS}s linear 1` } as React.CSSProperties);

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

  // 列表条目拖拽排序
  const handleItemDragStart = (e: React.DragEvent, index: number) => {
    if (locked) return;
    e.stopPropagation();
    setDraggedIndex(index);
    e.dataTransfer.effectAllowed = "move";
  };

  const handleItemDragOver = (e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
    e.dataTransfer.dropEffect = "move";
  };

  const handleItemDrop = async (e: React.DragEvent, targetIndex: number) => {
    e.preventDefault();
    e.stopPropagation();
    if (locked || draggedIndex === null || draggedIndex === targetIndex) {
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
    } catch (err) {
      console.error("Failed to reorder queue:", err);
    }
  };

  const handleDelete = async (userId: string) => {
    if (locked) return;
    try {
      const updated = await invoke<QueueItem[]>("dequeue_by_user_id", { userId });
      setQueue(updated);
    } catch (e) {
      console.error(e);
    }
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

  // 历战边框与背景样式
  const getItemCardStyle = (item: QueueItem) => {
    if (item.tempered_level === 2) {
      return "bg-gradient-to-r from-orange-950/80 via-red-950/60 to-black/80 border-orange-500/80 arch-tempered-glow text-orange-200";
    }
    if (item.tempered_level === 1) {
      return "bg-gradient-to-r from-purple-950/80 via-indigo-950/60 to-black/80 border-purple-500/70 tempered-glow text-purple-200";
    }
    if (item.is_priority) {
      return "bg-red-950/70 border-red-500/60 text-red-100 hover:bg-red-900/60";
    }
    return "bg-gray-900/85 border-gray-800/80 text-gray-200 hover:border-gray-600 hover:bg-gray-800/80";
  };

  // D8：透明度仅作用于背景层（原工程仅插值 MainGrid 背景 alpha），文字与条目保持不透明；
  // 锁定时使用穿透模式透明度（原 RefreshWindow 语义）
  const backgroundAlpha = (locked ? penetratingOpacity : opacity) / 100;

  const renderQueueItem = (item: QueueItem, idx: number) => (
    <div
      key={item.id}
      draggable
      onDragStart={(e) => handleItemDragStart(e, idx)}
      onDragOver={handleItemDragOver}
      onDrop={(e) => handleItemDrop(e, idx)}
      data-no-window-drag
      style={{ height: QUEUE_ROW_HEIGHT - 6 }}
      className={`group flex items-center justify-between p-2 rounded-lg border transition-all cursor-default select-none mb-1.5 ${
        draggedIndex === idx ? "opacity-40 scale-95 border-dashed border-amber-400" : ""
      } ${getItemCardStyle(item)}`}
    >
      {/* 拖拽排序手柄、序号与怪物图标 */}
      <div className="flex items-center gap-1.5 min-w-0 flex-1">
        <div
          title="上下拖拽调整排队顺序"
          className="p-0.5 text-gray-500 hover:text-amber-300 cursor-grab active:cursor-grabbing shrink-0"
        >
          <GripVertical className="w-3.5 h-3.5" />
        </div>

        <span className="w-4 text-center font-mono text-xs font-bold text-amber-400 shrink-0">
          {idx + 1}
        </span>

        {item.icon_url ? (
          <img
            src={`/monster_icons/${item.icon_url}`}
            alt={item.monster_name}
            onError={(e) => {
              (e.target as HTMLElement).style.display = "none";
            }}
            className="w-7 h-7 rounded border border-gray-700/60 object-contain shrink-0 bg-black/40 pointer-events-none"
          />
        ) : (
          <div className="w-7 h-7 rounded border border-gray-700/60 bg-black/40 flex items-center justify-center shrink-0 pointer-events-none">
            <Shield className="w-3.5 h-3.5 text-gray-500" />
          </div>
        )}

        {/* 怪物名称与用户名称（长文本往返滚动，悬停暂停） */}
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-1.5">
            <MarqueeText
              text={item.monster_name}
              className="text-xs font-bold max-w-[7rem] group-hover:max-w-[10rem] transition-all"
            />

            {/* 历战/历战王徽章 */}
            {item.tempered_level === 2 && (
              <span className="flex items-center gap-0.5 text-[9px] bg-orange-600/90 text-white font-bold px-1 py-0.2 rounded shrink-0 shadow">
                <Flame className="w-2.5 h-2.5" /> 历战王
              </span>
            )}
            {item.tempered_level === 1 && (
              <span className="flex items-center gap-0.5 text-[9px] bg-purple-600/90 text-white font-bold px-1 py-0.2 rounded shrink-0 shadow">
                <Sparkles className="w-2.5 h-2.5" /> 历战
              </span>
            )}

            {/* 舰长等级徽章 */}
            {item.guard_level === 1 && (
              <span className="text-[9px] bg-gradient-to-r from-red-600 to-amber-500 text-white font-bold px-1 py-0.2 rounded shrink-0 shadow">
                总督
              </span>
            )}
            {item.guard_level === 2 && (
              <span className="text-[9px] bg-gradient-to-r from-purple-600 to-pink-500 text-white font-bold px-1 py-0.2 rounded shrink-0 shadow">
                提督
              </span>
            )}
            {item.guard_level === 3 && (
              <span className="text-[9px] bg-gradient-to-r from-blue-600 to-cyan-500 text-white font-bold px-1 py-0.2 rounded shrink-0 shadow">
                舰长
              </span>
            )}

            {/* 优先置顶标识 */}
            {item.is_priority && (
              <span className="text-[9px] bg-red-600 text-white font-bold px-1 py-0.2 rounded shrink-0 animate-pulse">
                优先
              </span>
            )}
          </div>

          <MarqueeText text={item.user_name} className="text-[10px] text-gray-400" />
        </div>
      </div>

      {/* 点击完成删除按钮（D8 保留显式按钮，避免误删） */}
      <div className="shrink-0 pl-1.5">
        <button
          onClick={(e) => {
            e.stopPropagation();
            handleDelete(item.user_id);
          }}
          title="点击完成该单并删除"
          className="px-1.5 py-0.5 rounded text-[10px] text-gray-400 hover:text-white hover:bg-red-600/80 border border-gray-700/60 hover:border-red-500 transition font-medium cursor-pointer"
        >
          完成
        </button>
      </div>
    </div>
  );

  return (
    <div
      onMouseDown={handleWindowMouseDown}
      className="h-screen w-screen p-2 bg-transparent select-none overflow-hidden font-sans cursor-default"
    >
      <div
        data-tauri-drag-region
        onMouseDown={handleWindowMouseDown}
        style={{ backgroundColor: `rgba(3, 7, 18, ${backgroundAlpha})` }}
        className="h-full w-full flex flex-col rounded-xl backdrop-blur-lg border border-amber-500/40 shadow-2xl overflow-hidden relative cursor-move"
      >
        {/* 顶部可拖拽操作栏 */}
        <div
          data-tauri-drag-region
          onMouseDown={handleWindowMouseDown}
          className="flex items-center justify-between px-3 py-1.5 bg-gradient-to-r from-amber-950/80 via-neutral-900/80 to-gray-950/80 border-b border-amber-500/30 cursor-move shrink-0"
        >
          <div className="flex items-center gap-2 pointer-events-none">
            <GripHorizontal className="w-3.5 h-3.5 text-amber-400" />
            <span className="text-xs font-bold text-amber-200 tracking-wider">狩猎点单队列</span>
            <span className="text-[10px] bg-amber-500/25 text-amber-300 font-mono px-1.5 py-0.5 rounded-full border border-amber-500/40">
              {queue.length}
            </span>
            {locked && (
              <span className="flex items-center gap-0.5 text-[9px] bg-amber-500/20 text-amber-300 border border-amber-500/40 px-1.5 py-0.5 rounded-full">
                <Lock className="w-2.5 h-2.5" /> 已锁定
              </span>
            )}
          </div>

          <button
            onClick={handleClose}
            className="p-1 hover:bg-red-500/30 text-gray-400 hover:text-red-300 rounded transition cursor-pointer"
            title="隐藏悬浮窗"
          >
            <X className="w-3.5 h-3.5" />
          </button>
        </div>

        {/* 跑马灯通知栏：默认文本循环滚动，点怪消息逐条滚动后回默认 */}
        <div
          data-tauri-drag-region
          onMouseDown={handleWindowMouseDown}
          className="bg-amber-950/40 border-b border-amber-500/20 px-2 py-1 overflow-hidden shrink-0 flex items-center gap-1.5 cursor-move"
        >
          <Volume2 className="w-3 h-3 text-amber-400 shrink-0" />
          <div className="overflow-hidden whitespace-nowrap flex-1 pointer-events-none">
            <span
              key={marqueeText}
              onAnimationEnd={isDefaultMarquee ? undefined : onMarqueeFinished}
              style={marqueeStyle}
              className={
                isDefaultMarquee
                  ? "animate-marquee text-[11px] text-amber-100/90 font-medium"
                  : "inline-block whitespace-nowrap text-[11px] text-yellow-300 font-bold"
              }
            >
              {marqueeText}
            </span>
          </div>
          {marqueeQueue.length > 0 && (
            <span className="text-[9px] text-amber-400/80 font-mono shrink-0">
              +{marqueeQueue.length}
            </span>
          )}
        </div>

        {/* D4 业务气泡（多条堆叠，最多 5 条，15s 自动退场） */}
        {bubbles.length > 0 && (
          <div className="absolute top-14 left-2 right-2 z-50 space-y-1.5 pointer-events-none">
            {bubbles
              .slice()
              .reverse()
              .map((bubble) => (
                <div
                  key={bubble.id}
                  className={`rounded-lg p-2.5 shadow-2xl backdrop-blur-md animate-in fade-in zoom-in duration-200 cursor-default border ${BUBBLE_TONES[bubble.tone]}`}
                >
                  <div className="flex items-center gap-1.5 text-xs font-bold mb-1">
                    {bubble.tone === "ai" ? (
                      <Bot className="w-3.5 h-3.5 animate-bounce" />
                    ) : (
                      <Bell className="w-3.5 h-3.5" />
                    )}
                    <span>{bubble.title}</span>
                    <span className="text-[10px] text-gray-400 font-normal">
                      @{bubble.username}
                    </span>
                  </div>

                  {bubble.reasoning && (
                    <div className="text-[10px] text-cyan-200/70 bg-black/40 rounded p-1.5 mb-1 max-h-16 overflow-y-auto italic">
                      {bubble.reasoning}
                    </div>
                  )}

                  <div className="text-xs text-white font-medium break-words leading-relaxed whitespace-pre-line">
                    {bubble.content}
                  </div>
                </div>
              ))}
          </div>
        )}

        {/* 核心排队条目列表（虚拟化：仅渲染可视区行） */}
        <div onMouseDown={handleWindowMouseDown} className="flex-1 min-h-0 p-2">
          {queue.length === 0 ? (
            <div
              data-tauri-drag-region
              onMouseDown={handleWindowMouseDown}
              className="h-full flex flex-col items-center justify-center text-gray-400 text-xs gap-1.5 py-8 cursor-move"
            >
              <Shield className="w-7 h-7 text-gray-600 mb-1 pointer-events-none" />
              <span className="font-medium text-gray-300 pointer-events-none">当前排队为空</span>
              <span className="text-[10px] text-gray-500 pointer-events-none">
                发送弹幕【点怪 怪物名】即可上榜
              </span>
            </div>
          ) : (
            <VirtualList
              items={queue}
              rowHeight={QUEUE_ROW_HEIGHT}
              className="h-full overflow-y-auto scrollbar-thin scrollbar-thumb-gray-800"
              renderItem={renderQueueItem}
            />
          )}
        </div>
      </div>
    </div>
  );
};
