import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Copy, Dices, Send, Settings2 } from "lucide-react";
import { DrawMode, DrawPace, DrawSettings, MonsterDict, RosterData } from "../types";
import {
  GAME_LABEL,
  GAME_ORDER,
  MonsterEntry,
  dictToEntries,
  iconSrc,
} from "../lib/monsterList";
import { WEAPONS, WeaponEntry, warnOnCatalogDrift, weaponSrc } from "../lib/weaponList";
import { DrawPoolDrawer } from "../components/DrawPoolDrawer";
import { DrawReel, DrawReelHandle } from "../components/DrawReel";

interface Props {
  dict: MonsterDict;
  /** 点怪禁点名单：仅用于「同步自禁点名单」的一致性导入，不参与抽选判定 */
  roster: RosterData;
  /** 把抽到的怪物送去点单（切页签并预填选怪面板） */
  onSendToOrder: (monsterName: string) => void;
  toast: (msg: string) => void;
}

// ---------------------------------------------------------------------------
// 演出时间轴
// ---------------------------------------------------------------------------

/** 一段连续速度的滚动：`gap` 为标称帧间隔（毫秒），`dur` 为该段总时长。
 *  实际帧间隔由 `buildFrames` 按 `dur / n` 均分，`gap` 只用来定每段的帧数。 */
interface Segment {
  id: string;
  /** 段名，仅用于读代码时辨认这一拍在做什么，不进界面 */
  label: string;
  dur: number;
  gap: number;
}

/**
 * 悬念来自**速度的两次转折**，而不是把总时长拉长 —— 拉长只让人等，转折才让人揪心：
 * 减速段让人以为要停了，「伪停」把这一下拖住，紧接着「再冲」突然加速一次。
 * 「拖长」档拉的是每段间隔，转折形状保持不变。
 */
const SEGMENTS: Segment[] = [
  { id: "wind", label: "起手", dur: 340, gap: 90 },
  { id: "rush1", label: "高速", dur: 900, gap: 46 },
  { id: "rush2", label: "中速", dur: 900, gap: 62 },
  { id: "slow", label: "减速", dur: 500, gap: 110 },
  { id: "fake", label: "伪停", dur: 300, gap: 200 },
  { id: "burst", label: "再冲", dur: 200, gap: 70 },
  { id: "settle", label: "落位", dur: 200, gap: 160 },
];

const PACE_SCALE: Record<DrawPace, number> = { fast: 0.4, normal: 1, long: 1.6 };
const PACE_LABEL: Record<DrawPace, { label: string; sub: string }> = {
  fast: { label: "快", sub: "FAST" },
  normal: { label: "标准", sub: "NORMAL" },
  long: { label: "拖长", sub: "LONG" },
};

const MODE_LABEL: Record<DrawMode, { label: string; sub: string }> = {
  both: { label: "都要", sub: "武器＋怪物" },
  weapon: { label: "仅武器", sub: "WEAPON" },
  monster: { label: "仅怪物", sub: "MONSTER" },
};

const LV_NAME = ["普通", "历战", "历战王"];

const DEFAULT_SETTINGS: DrawSettings = {
  games: [...GAME_ORDER],
  excluded_monsters: [],
  excluded_weapons: [],
  mode: "both",
  pace: "normal",
};

interface Frame {
  gap: number;
  seg: Segment;
}

/**
 * 把段落表展开成逐帧计划；减少动态时只保留起手与落位各一帧。
 *
 * 帧间隔取 `dur / n` 而**不是**段落的标称 `gap`：按标称值走，每段都会因
 * `round(dur / gap)` 的取整而偏长（标准档标称 3.0s，实际滚出 3.53s），
 * 于是卡片上的时长读数和档位标签全是假的。按段内均分后每段总长恰好等于设计值。
 */
function buildFrames(pace: DrawPace, reduce: boolean): Frame[] {
  const scale = PACE_SCALE[pace];
  const segs: Segment[] = reduce
    ? [
        { ...SEGMENTS[0], dur: 120, gap: 120 },
        { ...SEGMENTS[SEGMENTS.length - 1], dur: 160, gap: 160 },
      ]
    : SEGMENTS.map((s) => ({ ...s, dur: s.dur * scale, gap: s.gap * scale }));

  const frames: Frame[] = [];
  for (const seg of segs) {
    const n = Math.max(1, Math.round(seg.dur / seg.gap));
    const gap = seg.dur / n;
    for (let i = 0; i < n; i++) frames.push({ gap, seg });
  }
  return frames;
}

/** 某档节奏的滚动段总长（毫秒）——档位标签与总时长读数都从真实帧表算，不另写一份常量 */
function rollMsOf(pace: DrawPace, reduce: boolean): number {
  return buildFrames(pace, reduce).reduce((a, f) => a + f.gap, 0);
}

interface DrawRecord {
  no: number;
  weapon: WeaponEntry | null;
  monster: MonsterEntry | null;
  at: number;
}

const HISTORY_MAX = 12;

const pick = <T,>(arr: T[]): T => arr[Math.floor(Math.random() * arr.length)];

const two = (n: number) => String(n).padStart(2, "0");
const clockOf = (ts: number) => {
  const d = new Date(ts);
  return `${two(d.getHours())}:${two(d.getMinutes())}`;
};

/**
 * 随机抽选武器与怪物。
 *
 * **结果先定后演**：真随机只摇一次（进入 `draw()` 立刻定下武器与怪物），
 * 之后每一帧滚动都只是演出，最后一帧落到中奖项上。绝不"边滚边随机"——
 * 那样观感上会像结果在被动画牵着走，也失去了"一次等概率"的可解释性。
 *
 * 抽选是**纯本地、零副作用**的：不写队列、不占弹幕额度、不发弹幕。
 * 想把结果变成真订单得主播自己点「送去点单」。
 *
 * Lite 形态同样可用（2026-09-30 决策）：不依赖 TTS / 打卡 / AI。
 */
export const RandomDrawTab: React.FC<Props> = ({ dict, roster, onSendToOrder, toast }) => {
  const [settings, setSettings] = useState<DrawSettings>(DEFAULT_SETTINGS);
  const [rolling, setRolling] = useState(false);
  const [phase, setPhase] = useState("待机");
  /** 盘子正在演出：此时把结果名收起来，别提前剧透 */
  const [reelBusy, setReelBusy] = useState(false);
  /** 本次抽选是否跳过逐帧滚动（减少动态，或页面不可见 —— 见 draw() 注释） */
  const [instantRoll, setInstantRoll] = useState(false);
  const [history, setHistory] = useState<DrawRecord[]>([]);
  const [last, setLast] = useState<DrawRecord | null>(null);
  const [drawerOpen, setDrawerOpen] = useState(false);

  const weaponReelRef = useRef<DrawReelHandle>(null);
  const monsterReelRef = useRef<DrawReelHandle>(null);
  const stageRef = useRef<HTMLDivElement>(null);
  const seqRef = useRef(0);
  const rollingRef = useRef(false);
  /** 卸载/重开时作废进行中的编排，避免在已销毁的组件上继续 setState */
  const runIdRef = useRef(0);

  const reduceMotion = useMemo(
    () =>
      typeof window !== "undefined" &&
      window.matchMedia("(prefers-reduced-motion: reduce)").matches,
    []
  );

  // -------------------------------------------------------------------------
  // 设置：单写者 + 合并最新意图
  // -------------------------------------------------------------------------

  /**
   * 同一时刻只允许一个 in-flight 提交，期间的新意图合并为最新一份。
   * 与 MainWindow 的 `commitRoster` 同款：并发的整表提交会乱序落地，
   * 而设置是"最新覆盖"，不需要版本 CAS，只需要顺序。
   */
  const saveRef = useRef<{ inFlight: boolean; pending: DrawSettings | null }>({
    inFlight: false,
    pending: null,
  });

  /**
   * `toast` 的稳定引用。
   *
   * MainWindow 的 `showToast` 是内联箭头函数 —— **父组件每渲染一次就是新引用**
   * （队列轮询、连接状态、实时队列快照都会触发）。一旦让它进依赖数组，
   * 「加载设置」这个本该一次性执行的 effect 就会反复重跑：每次都用后端反序列化出的
   * 新对象 `setSettings`，于是所有设置数组都换了引用 —— 而「铺一张初始图」的 effect
   * 正是挂在池子引用上，结果每次父渲染都把已经定格的抽选盘冲回池子第一项
   * （盘上是大剑、下面名字写着双剑，就是这个来的）。
   */
  const toastRef = useRef(toast);
  useEffect(() => {
    toastRef.current = toast;
  });

  /**
   * 权威设置的同步镜像。
   *
   * 直接读 `settings` 的闭包在**同一 tick 内连续变更**时会互相覆盖：
   * 每次点击都基于同一份旧 `settings` 算 `next`，前一次的结果被后一次抹掉
   * （实测连点 13 个武器格只生效最后 1 个）。真人逐次点击因中间有重渲染而不暴露，
   * 但快速连点是真实场景，所以变更一律经 `mutate()` 基于这份同步镜像前推。
   */
  const settingsRef = useRef(settings);
  useEffect(() => {
    settingsRef.current = settings;
  }, [settings]);

  const saveSettings = useCallback(
    (next: DrawSettings) => {
      settingsRef.current = next;
      setSettings(next);
      const st = saveRef.current;
      if (st.inFlight) {
        st.pending = next;
        return;
      }
      st.inFlight = true;
      void (async () => {
        let cur: DrawSettings | null = next;
        while (cur) {
          try {
            const applied = await invoke<DrawSettings>("set_draw_settings", { settings: cur });
            // 后端会纠正非法值（未知作品 id、越界枚举）：没有待发意图时用它对齐本地
            if (!saveRef.current.pending) setSettings(applied);
          } catch (e) {
            toastRef.current(`抽选设置保存失败：${e}`);
          }
          cur = saveRef.current.pending;
          saveRef.current.pending = null;
        }
        saveRef.current.inFlight = false;
      })();
    },
    []
  );

  /** 单一变更入口：基于最新设置算出目标态，再交给单写者落盘 */
  const mutate = useCallback(
    (fn: (prev: DrawSettings) => DrawSettings) => {
      saveSettings(fn(settingsRef.current));
    },
    [saveSettings]
  );

  /** 设置只在挂载时拉一次：依赖里放任何不稳定引用都会让它变成"每次父渲染都重拉" */
  useEffect(() => {
    warnOnCatalogDrift();
    let alive = true;
    void (async () => {
      try {
        const s = await invoke<DrawSettings>("get_draw_settings");
        if (alive) setSettings(s);
      } catch (e) {
        toastRef.current(`抽选设置读取失败，按默认值运行: ${e}`);
      }
    })();
    return () => {
      alive = false;
    };
  }, []);

  // -------------------------------------------------------------------------
  // 池子
  // -------------------------------------------------------------------------

  const entries = useMemo(() => dictToEntries(dict), [dict]);
  const games = useMemo(() => new Set(settings.games), [settings.games]);
  const excludedM = useMemo(
    () => new Set(settings.excluded_monsters),
    [settings.excluded_monsters]
  );
  const excludedW = useMemo(() => new Set(settings.excluded_weapons), [settings.excluded_weapons]);

  const poolW = useMemo(() => WEAPONS.filter((w) => !excludedW.has(w.id)), [excludedW]);
  const poolM = useMemo(
    () => entries.filter((e) => games.has(e.game) && !excludedM.has(e.name)),
    [entries, games, excludedM]
  );

  const needW = settings.mode !== "monster";
  const needM = settings.mode !== "weapon";

  const emptyReason = useMemo(() => {
    if (needW && !poolW.length) return "武器池空了 —— 14 把全部被排除，至少要放回 1 把";
    if (needM && !poolM.length)
      return "怪物池空了 —— 放宽作品筛选，或从排除名单里放回几只";
    return null;
  }, [needW, needM, poolW.length, poolM.length]);

  const hint = useMemo(() => {
    if (emptyReason) return null;
    if (settings.mode === "weapon") return `本次从 ${poolW.length} 把武器里等概率抽 1 把`;
    if (settings.mode === "monster") return `本次从 ${poolM.length} 只怪物里等概率抽 1 只`;
    return `本次从 ${poolW.length} 把武器 × ${poolM.length} 只怪物里等概率各抽 1 个`;
  }, [emptyReason, settings.mode, poolW.length, poolM.length]);

  // -------------------------------------------------------------------------
  // 演出节奏
  // -------------------------------------------------------------------------

  const frames = useMemo(
    () => buildFrames(settings.pace, reduceMotion),
    [settings.pace, reduceMotion]
  );

  /**
   * 滚动段之外的三拍总时长（毫秒）：起手抖动 + 逐项锁定 + 金光揭晓。
   * 起手那 260ms 必须算进来 —— 少了它读数会系统性偏小半拍（实测 4.93s 却报 4.6s）。
   */
  const tailMs = useMemo(() => {
    const n = (needW ? 1 : 0) + (needM ? 1 : 0);
    const wind = reduceMotion ? 90 : 260;
    const lock = reduceMotion ? 60 * n : 200 * n + 120;
    const reveal = reduceMotion ? 220 : 760;
    return wind + lock + reveal;
  }, [needW, needM, reduceMotion]);

  /** 三档的滚动段秒数：档位标签与总时长读数共用同一来源 */
  const rollSecs = useMemo(
    () =>
      Object.fromEntries(
        (Object.keys(PACE_LABEL) as DrawPace[]).map((p) => [p, rollMsOf(p, reduceMotion) / 1000])
      ) as Record<DrawPace, number>,
    [reduceMotion]
  );

  /**
   * 一次抽选的端到端时长读数（滚动 + 逐项锁定 + 揭晓）。
   * 只给主播一个量级感 —— 舞台上那条按段着色的节奏示意条已按 2026-09-30 决定从产物中移除。
   */
  const paceEta = (rollSecs[settings.pace] + tailMs / 1000).toFixed(1) + "s";

  // -------------------------------------------------------------------------
  // 抽选编排
  // -------------------------------------------------------------------------

  useEffect(() => () => {
    // 卸载时作废编排，避免切页签后还在空转
    runIdRef.current += 1;
  }, []);

  /** 无论正常结束还是异常，都必须解锁：中断在演出中途会让按钮永久禁用 */
  const settle = useCallback(() => {
    stageRef.current?.classList.remove("rolling");
    rollingRef.current = false;
    setRolling(false);
    setReelBusy(false);
  }, []);

  const draw = useCallback(async () => {
    if (rollingRef.current) return;
    if (needW && !poolW.length) {
      toastRef.current("武器池是空的 —— 至少要放回 1 把武器");
      return;
    }
    if (needM && !poolM.length) {
      toastRef.current("怪物池是空的 —— 放宽作品筛选或从排除名单里放回几只");
      return;
    }

    /**
     * 页面不可见时**必须**跳过逐帧滚动，而不是"照演但看不见"：
     * 不可见页面的 document.timeline 冻结，WAAPI 动画不会前进；此时唯一的推进手段是
     * setTimeout，而后台标签的定时器被节流到 ≥1s —— 50 帧会拖成近一分钟。
     * 主播此刻也确实看不到演出，定格跳过去是唯一合理的处理。
     */
    const instant = reduceMotion || document.hidden;
    const runId = ++runIdRef.current;
    rollingRef.current = true;
    setRolling(true);
    setReelBusy(true);
    setInstantRoll(instant);
    setLast(null);
    setPhase("蓄力");
    stageRef.current?.classList.add("rolling");

    // 真随机只摇一次；后面每一帧滚动都只是演出
    const wWin = needW ? pick(poolW) : null;
    const mWin = needM ? pick(poolM) : null;

    const asleep = (ms: number) => new Promise((r) => window.setTimeout(r, ms));

    try {
    // 起手：卡片抖动
    await asleep(instant ? 90 : 260);
    if (runId !== runIdRef.current) return;

    setPhase("滚动中");

    for (let i = 0; i < frames.length; i++) {
      if (runId !== runIdRef.current) return;
      const f = frames[i];
      const isLast = i === frames.length - 1;
      // 两个盘**并行**推进：串行 await 会让实际时长翻倍，与档位标签承诺的秒数对不上
      const jobs: Promise<void>[] = [];
      if (needW && wWin) {
        jobs.push(weaponReelRef.current?.step(isLast ? weaponSrc(wWin.icon) : weaponSrc(pick(poolW).icon), f.gap) ?? Promise.resolve());
      }
      if (needM && mWin) {
        jobs.push(monsterReelRef.current?.step(isLast ? iconSrc(mWin.icon) : iconSrc(pick(poolM).icon), f.gap) ?? Promise.resolve());
      }
      await Promise.all(jobs);
    }
    if (runId !== runIdRef.current) return;

    // 逐项锁定：先武器后怪物，错开一拍
    const stagger = instant ? 60 : 200;
    if (wWin) {
      setPhase("锁定 · 武器");
      weaponReelRef.current?.hit();
      await asleep(stagger);
      if (runId !== runIdRef.current) return;
    }
    if (mWin) {
      setPhase("锁定 · 怪物");
      monsterReelRef.current?.hit();
      await asleep(stagger);
      if (runId !== runIdRef.current) return;
    }

    // 揭晓
    const record: DrawRecord = { no: ++seqRef.current, weapon: wWin, monster: mWin, at: Date.now() };
    setLast(record);
    setHistory((h) => [record, ...h].slice(0, HISTORY_MAX));
    setPhase("揭晓");
    stageRef.current?.classList.add("revealed");
    window.setTimeout(
      () => stageRef.current?.classList.remove("revealed"),
      instant ? 400 : 950
    );

    await asleep(instant ? 120 : 520);
    if (runId !== runIdRef.current) return;

    setPhase("已出结果");
    settle();
    } catch (e) {
      // 演出链路上的任何异常（动画被打断、组件被卸载、意外 reject）都只该毁掉这一次演出，
      // 不该把界面留在「抽选中」—— 那样按钮永久禁用，只能靠切页签自救
      toastRef.current(`抽选演出出错：${e}`);
      settle();
    }
  }, [
    needW, needM, poolW, poolM, frames, reduceMotion, settle,
  ]);

  /**
   * 首屏给两个盘一张静止的开场图，避免空白框 —— **只铺一次**。
   *
   * 不能写成"池子一变就重铺"：池子数组的引用会因任何设置变更而重算
   * （编辑排除名单、父组件重渲染引发的设置重拉……），那样会把已经定格的盘面冲掉，
   * 出现"盘上是大剑、名字写着双剑"的错位。铺完第一张之后，显示权归抽选编排独占。
   */
  const seededRef = useRef({ weapon: false, monster: false });
  useEffect(() => {
    if (seededRef.current.weapon || !poolW.length) return;
    seededRef.current.weapon = true;
    weaponReelRef.current?.show(weaponSrc(poolW[0].icon));
  }, [poolW]);
  useEffect(() => {
    if (seededRef.current.monster || !poolM.length) return;
    seededRef.current.monster = true;
    monsterReelRef.current?.show(iconSrc(poolM[0].icon));
  }, [poolM]);

  // -------------------------------------------------------------------------
  // 池子编辑
  // -------------------------------------------------------------------------

  /** 清空某个池子会让抽选按钮永久置灰且前端无从恢复，故最后一项不允许被排除 */
  const toggleMonster = useCallback(
    (name: string) => {
      if (!excludedM.has(name) && poolM.length <= 1) {
        toastRef.current("至少要保留 1 只怪物可抽");
        return;
      }
      mutate((p) => {
        const off = new Set(p.excluded_monsters);
        if (off.has(name)) off.delete(name);
        else off.add(name);
        return { ...p, excluded_monsters: [...off] };
      });
    },
    [excludedM, poolM.length, mutate, toast]
  );

  const bulkToggleMonsters = useCallback(
    (names: string[], exclude: boolean) => {
      if (exclude) {
        const targeted = new Set(names);
        if (poolM.filter((e) => !targeted.has(e.name)).length === 0) {
          toastRef.current("至少要保留 1 只怪物可抽");
          return;
        }
      }
      mutate((p) => {
        const off = new Set(p.excluded_monsters);
        names.forEach((n) => (exclude ? off.add(n) : off.delete(n)));
        return { ...p, excluded_monsters: [...off] };
      });
    },
    [poolM, mutate, toast]
  );

  const toggleWeapon = useCallback(
    (w: WeaponEntry) => {
      if (!excludedW.has(w.id) && poolW.length <= 1) {
        toastRef.current("至少要保留 1 把武器");
        return;
      }
      mutate((p) => {
        const off = new Set(p.excluded_weapons);
        if (off.has(w.id)) off.delete(w.id);
        else off.add(w.id);
        return { ...p, excluded_weapons: [...off] };
      });
    },
    [excludedW, poolW.length, mutate, toast]
  );

  const toggleGame = useCallback(
    (game: string) => {
      if (games.has(game) && games.size <= 1) {
        toastRef.current("至少要保留一个作品");
        return;
      }
      mutate((p) => {
        const picked = new Set(p.games);
        if (picked.has(game)) picked.delete(game);
        else picked.add(game);
        // 按 GAME_ORDER 归一化顺序，与后端落盘口径一致
        return { ...p, games: GAME_ORDER.filter((g) => picked.has(g)) };
      });
    },
    [games, mutate, toast]
  );

  const syncFromRoster = useCallback(() => {
    const off = new Set(settings.excluded_monsters);
    let added = 0;
    for (const n of roster.items) {
      if (!off.has(n)) {
        off.add(n);
        added += 1;
      }
    }

    // 禁点名单可能把池子整片清空（名单很大时是常态）：并入后若池子见底就整体拒绝，
    // 而不是并入一半留个"看起来成功、实际抽不了"的状态
    const remaining = entries.filter(
      (e) => games.has(e.game) && !off.has(e.name)
    ).length;
    if (remaining === 0) {
      toastRef.current("禁点名单并进来会把怪物池清空 —— 先放宽作品筛选或恢复几只再同步");
      return;
    }
    mutate((p) => ({ ...p, excluded_monsters: [...off] }));
    toastRef.current(
      added
        ? `已把禁点名单里的 ${added} 只并入排除名单，当前可抽 ${remaining} 只`
        : "禁点名单里的怪物都已在排除名单中"
    );
  }, [settings.excluded_monsters, roster.items, entries, games, mutate]);

  const clearExcludedMonsters = useCallback(() => {
    mutate((p) => ({ ...p, excluded_monsters: [] }));
  }, [mutate]);

  const copyResult = useCallback(() => {
    if (!last) return;
    const txt = [last.weapon?.cn, last.monster?.name].filter(Boolean).join(" × ");
    const done = () => toastRef.current(`已复制「${txt}」`);
    if (navigator.clipboard) navigator.clipboard.writeText(txt).then(done, done);
    else done();
  }, [last]);

  // -------------------------------------------------------------------------
  // 渲染
  // -------------------------------------------------------------------------

  const monsterSub = last?.monster
    ? `${LV_NAME[last.monster.level] ?? "普通"}${
        last.monster.aliases.length ? " · " + last.monster.aliases[0] : ""
      }`
    : null;

  const drawerPoolM = useMemo(
    () => entries.filter((e) => games.has(e.game) && !excludedM.has(e.name)),
    [entries, games, excludedM]
  );

  return (
    <div className="rd-scope">
      <header className="rd-head">
        <div>
          <h1>随机抽选武器与怪物</h1>
          <p className="sub">
            从当前池子里等概率各抽 1 个 · 抽选<b>不写队列不占额度</b>，要入队请点「送去点单」·
            排除名单与「怪物名单」的禁点名单相互独立
          </p>
        </div>
      </header>

      <div className="rd-body">
        {/* ------------------------------ 左：池子与节奏 ------------------------------ */}
        <section className="rd-pane-cfg">
          <div className="rd-card">
            <div className="rd-card-hd">
              <span className="t">抽什么</span>
            </div>
            <div className="rd-seg3">
              {(Object.keys(MODE_LABEL) as DrawMode[]).map((m) => (
                <button
                  key={m}
                  className={settings.mode === m ? "on" : ""}
                  onClick={() => mutate((p) => ({ ...p, mode: m }))}
                  disabled={rolling}
                >
                  {MODE_LABEL[m].label}
                  <span className="sub">{MODE_LABEL[m].sub}</span>
                </button>
              ))}
            </div>
          </div>

          <div className="rd-card">
            <div className="rd-card-hd">
              <span className="t">节奏</span>
              <span className="n">{paceEta}</span>
            </div>
            <div className="rd-seg3">
              {(Object.keys(PACE_LABEL) as DrawPace[]).map((p) => (
                <button
                  key={p}
                  className={settings.pace === p ? "on" : ""}
                  onClick={() => mutate((prev) => ({ ...prev, pace: p }))}
                  disabled={rolling}
                >
                  {PACE_LABEL[p].label}
                  <span className="sub">{rollSecs[p].toFixed(1)}s 滚</span>
                </button>
              ))}
            </div>
          </div>

          <div className="rd-card">
            <div className="rd-card-hd">
              <span className="t">武器池</span>
              <span className="n">
                {poolW.length} / {WEAPONS.length}
              </span>
              <button
                className="mini"
                onClick={() => mutate((p) => ({ ...p, excluded_weapons: [] }))}
                disabled={rolling}
              >
                全选
              </button>
            </div>
            <div className="rd-wgrid">
              {WEAPONS.map((w) => {
                const off = excludedW.has(w.id);
                return (
                  <div
                    key={w.id}
                    className={`rd-wcell${off ? " off" : " on"}`}
                    onClick={() => !rolling && toggleWeapon(w)}
                    title={`${w.cn} ${w.en}${off ? "（已排除）" : ""}`}
                  >
                    <img src={weaponSrc(w.icon)} alt="" draggable={false} />
                  </div>
                );
              })}
            </div>
          </div>

          <div className="rd-card">
            <div className="rd-card-hd">
              <span className="t">怪物池</span>
              <span className="n">
                {poolM.length} / {entries.length}
              </span>
              <button className="mini" onClick={() => setDrawerOpen(true)}>
                管理名单…
              </button>
            </div>
            <div className="rd-chips">
              {GAME_ORDER.map((g) => {
                const total = entries.filter((e) => e.game === g).length;
                const on = games.has(g);
                return (
                  <button
                    key={g}
                    className={`rd-chip${on ? " on" : ""}`}
                    onClick={() => !rolling && toggleGame(g)}
                    disabled={rolling}
                  >
                    {GAME_LABEL[g] ?? g}
                    <span className="c">{total}</span>
                  </button>
                );
              })}
            </div>
            <div className="rd-poolstat">
              <span className="big">{poolM.length}</span>
              <span className="of">只可抽</span>
              <span className="txt">
                {excludedM.size ? `已排除 ${excludedM.size} 只` : "排除名单为空"}
              </span>
            </div>
            <button className="rd-wide" onClick={() => setDrawerOpen(true)} disabled={rolling}>
              <Settings2 className="w-3.5 h-3.5" />
              管理排除名单…
            </button>
          </div>
        </section>

        {/* ------------------------------ 中：抽选盘 ------------------------------ */}
        <section className="rd-pane-stage">
          <div className="rd-stage" ref={stageRef}>
            <div className="rd-compass">
              <i className="spin" />
              <i className="spin2" />
              <i />
            </div>
            <div className="rd-burst" />

            <div className="rd-brief">
              <span className="k">
                本次有效池　武器 <b>{poolW.length}</b> 把　·　怪物 <b>{poolM.length}</b> 只
              </span>
              <span className={`rd-phase${rolling ? " roll" : ""}`}>
                <span className="dot" />
                {phase}
              </span>
            </div>

            <div className="rd-reels">
              <DrawReel
                ref={weaponReelRef}
                kind="weapon"
                hidden={!needW}
                instant={instantRoll}
                label={reelBusy ? null : last?.weapon?.cn ?? null}
                sub={reelBusy ? null : last?.weapon?.en ?? null}
              />
              <div className={`rd-link${last ? " hot" : ""}`} style={{ display: needW && needM ? undefined : "none" }}>
                <Dices className="w-3.5 h-3.5" />
              </div>
              <DrawReel
                ref={monsterReelRef}
                kind="monster"
                hidden={!needM}
                instant={instantRoll}
                label={reelBusy ? null : last?.monster?.name ?? null}
                sub={reelBusy ? null : monsterSub}
              />
            </div>

            <div className="rd-actions">
              <button className="rd-draw" onClick={() => void draw()} disabled={rolling || !!emptyReason}>
                {rolling ? "抽选中" : "抽 选"}
              </button>
              {emptyReason ? (
                <div className="rd-hint warn">{emptyReason}</div>
              ) : (
                <div className="rd-hint">{hint}</div>
              )}
              {/* 结果区只放"把结果用起来"的动作：重新抽就是中间那颗大按钮，不在这里重复摆 */}
              <div className={`rd-result-actions${last && !rolling ? " on" : ""}`}>
                <button
                  className="rd-btn sec"
                  disabled={!last?.monster}
                  onClick={() => last?.monster && onSendToOrder(last.monster.name)}
                  title={
                    last?.monster
                      ? `切到「排队管理」并把「${last.monster.name}」预填进选怪面板`
                      : "仅抽武器时没有可送去的怪物"
                  }
                >
                  <Send className="w-3.5 h-3.5" />
                  送去点单
                </button>
                <button className="rd-btn sec" onClick={copyResult} disabled={!last}>
                  <Copy className="w-3.5 h-3.5" />
                  复制结果
                </button>
              </div>
            </div>
          </div>
        </section>
      </div>

      {/* ------------------------------ 下：抽选记录 ------------------------------ */}
      <footer className="rd-history">
        <div className="rd-history-hd">
          <span className="t">抽选记录</span>
          <span className="n">
            最近 {HISTORY_MAX} 次 · {history.length} 条
          </span>
          <button className="mini" onClick={() => setHistory([])} disabled={!history.length}>
            清空
          </button>
        </div>
        <div className="rd-history-row">
          {history.length === 0 ? (
            <div className="rd-history-empty">还没有抽选记录 —— 点上面的「抽选」开始</div>
          ) : (
            history.map((h) => (
              <div className="rd-hcard" key={h.no}>
                <div className="r1">
                  {h.weapon && <img src={weaponSrc(h.weapon.icon)} alt="" draggable={false} />}
                  {h.monster && <img src={iconSrc(h.monster.icon)} alt="" draggable={false} />}
                  <span className="no">#{h.no}</span>
                </div>
                <div className="r2">{h.monster?.name ?? "—"}</div>
                <div className="r3">
                  <span>{h.weapon?.cn ?? "—"}</span>
                  <span className="tm">{clockOf(h.at)}</span>
                </div>
              </div>
            ))
          )}
        </div>
      </footer>

      <DrawPoolDrawer
        open={drawerOpen}
        entries={entries}
        settings={settings}
        poolCount={drawerPoolM.length}
        onToggleGame={toggleGame}
        onToggleMonster={toggleMonster}
        onBulkToggle={bulkToggleMonsters}
        onClearAll={clearExcludedMonsters}
        onSyncRoster={syncFromRoster}
        onClose={() => setDrawerOpen(false)}
      />
    </div>
  );
};
