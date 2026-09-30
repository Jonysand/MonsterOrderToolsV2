import { forwardRef, useEffect, useImperativeHandle, useRef } from "react";

/**
 * 抽选盘：一个「老虎机滚轮」。
 *
 * 滚动用 Web Animations API 直接驱动两层 `<img>`（旧图上移淡出、新图自下进入），
 * 而**不走 React state** —— 高速段每 46ms 一帧，经 state 重渲染既跟不上，
 * 也会把时间轴/记录一起拖着刷新。图标 src 由本组件直接写 DOM，React 不参与。
 *
 * 关键约束：`step()` 返回的 Promise 必须等到动画真的结束才 resolve，
 * 编排方（RandomDrawTab）靠它精确控制总时长；两个盘必须**并行** await，
 * 串行会让实际时长翻倍、时间轴播放头对不上（设计稿初版踩过）。
 */
export interface DrawReelHandle {
  /** 立即定格到某一项（不播动画） */
  show: (icon: string) => void;
  /** 续滚一帧：`dur` 毫秒后落到 `icon` */
  step: (icon: string, dur: number) => Promise<void>;
  /** 锁定：回弹 + 光环扩散 + 白光一闪 */
  hit: () => void;
}

interface Props {
  kind: "weapon" | "monster";
  /** 结果名（未出结果/滚动中传 null） */
  label: string | null;
  /** 副标题（拉丁名 / 历战等级等） */
  sub?: string | null;
  /** 本盘不参与本次抽取时整体隐藏 */
  hidden: boolean;
  /** 跳过逐帧滚动（减少动态 / 页面不可见）：只保留定格与锁定 */
  instant: boolean;
}

const CAPTION: Record<Props["kind"], string> = {
  weapon: "武 器",
  monster: "怪 物",
};

/** 锁定演出时长（与 App.css 的 .rd-reel.locked 动画一致） */
const LOCK_MS = 720;

export const DrawReel = forwardRef<DrawReelHandle, Props>(function DrawReel(
  { kind, label, sub, hidden, instant },
  ref
) {
  const rootRef = useRef<HTMLDivElement>(null);
  const curRef = useRef<HTMLImageElement>(null);
  const nextRef = useRef<HTMLImageElement>(null);
  /** 当前定格图标：动画结束后用它回写下层，保证两层始终同图 */
  const currentRef = useRef<string>("");

  useImperativeHandle(
    ref,
    (): DrawReelHandle => ({
      show(icon: string) {
        currentRef.current = icon;
        if (curRef.current) curRef.current.src = icon;
        if (nextRef.current) nextRef.current.src = icon;
      },

      async step(icon: string, dur: number) {
        const cur = curRef.current;
        const next = nextRef.current;
        if (!cur || !next) return;

        if (instant) {
          this.show(icon);
          return;
        }

        next.src = icon;
        const out = cur.animate(
          [
            { transform: "translateY(0) scale(1)", opacity: 1, filter: "blur(0px)" },
            { transform: "translateY(-58%) scale(.82)", opacity: 0, filter: "blur(4px)" },
          ],
          { duration: dur, easing: "linear", fill: "forwards" }
        );
        const into = next.animate(
          [
            { transform: "translateY(58%) scale(.82)", opacity: 0, filter: "blur(4px)" },
            { transform: "translateY(0) scale(1)", opacity: 1, filter: "blur(0px)" },
          ],
          { duration: dur, easing: "linear", fill: "forwards" }
        );
        // 必须与超时竞速：页面一旦不可见（切到别的标签、窗口被最小化/遮挡），
        // document.timeline 会整个冻结，WAAPI 的 finished **永不 resolve**，
        // 于是 await 挂死、抽选按钮永久禁用 —— 只能切页签才能恢复。
        // 超时兜底让"动画没跑完"退化成"这一帧少播一次"，而不是把整个流程卡住。
        await Promise.race([
          Promise.all([out.finished, into.finished]).catch(() => undefined),
          new Promise<void>((r) => window.setTimeout(r, dur + 60)),
        ]);

        // 先把新图落到下层再取消动画：两层同图，取消后回到各自静态位置也不跳变
        currentRef.current = icon;
        cur.src = icon;
        out.cancel();
        into.cancel();
      },

      hit() {
        const root = rootRef.current;
        if (!root) return;
        root.classList.add("locked");
        window.setTimeout(() => root.classList.remove("locked"), LOCK_MS);
      },
    }),
    [instant]
  );

  /** 组件挂载时清掉可能残留的锁定态（停止/重开抽选时被复用） */
  useEffect(() => {
    rootRef.current?.classList.remove("locked");
  }, []);

  return (
    <div
      ref={rootRef}
      className={`rd-reel rd-reel-${kind}${hidden ? " off" : ""}`}
      data-kind={kind}
    >
      <div className="rd-frame">
        <div className="rd-cur">
          <img ref={curRef} alt="" draggable={false} />
        </div>
        <div className="rd-next">
          <img ref={nextRef} alt="" draggable={false} />
        </div>
        <div className="rd-ring" />
        <div className="rd-flash" />
      </div>
      <div className="rd-cap">{CAPTION[kind]}</div>
      {/* 名字泛金 = 已揭晓。不用独立的 hit 状态位：label 非空本身就等价于"出结果了" */}
      <div className={`rd-name${label ? " hit" : ""}`}>
        <div className="cn">{label ?? "—"}</div>
        <div className="en">{sub || "\u00a0"}</div>
      </div>
    </div>
  );
});
