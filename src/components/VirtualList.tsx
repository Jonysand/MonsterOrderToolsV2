import { useEffect, useRef, useState } from "react";

interface VirtualListProps<T> {
  items: T[];
  /** 固定行高（px），用于计算可视窗口 */
  rowHeight: number;
  /** 滚动容器样式类（需含 overflow-y-auto 与高度约束） */
  className?: string;
  /** 可视区上下额外渲染的行数，缓解快速滚动白屏 */
  overscan?: number;
  renderItem: (item: T, index: number) => React.ReactNode;
}

/**
 * 固定行高虚拟列表（D6）：大队列时仅渲染可视区行，避免上千 DOM 节点导致卡顿。
 * 对齐原工程 OrderedMonsterWindow 的 ObservableCollection + ListView 虚拟化效果。
 */
export function VirtualList<T>({
  items,
  rowHeight,
  className,
  overscan = 4,
  renderItem,
}: VirtualListProps<T>) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [range, setRange] = useState({ start: 0, end: 0 });
  /**
   * 最新输入快照：ResizeObserver 只挂载一次，其回调永远闭包着首帧的 items，
   * 若直接读闭包内的 items.length 会把可视区回滚成旧长度、导致新入队条目不再渲染
   * —— 故一律经此 ref 读实时值。
   */
  const inputRef = useRef({ items, rowHeight, overscan });

  useEffect(() => {
    inputRef.current = { items, rowHeight, overscan };
  });

  const recompute = () => {
    const el = containerRef.current;
    if (!el) return;
    const { items: list, rowHeight: rh, overscan: os } = inputRef.current;
    const visibleCount = Math.ceil(el.clientHeight / rh) + os * 2;
    const start = Math.max(0, Math.floor(el.scrollTop / rh) - os);
    const end = Math.min(list.length, start + visibleCount);
    setRange((prev) => (prev.start === start && prev.end === end ? prev : { start, end }));
  };

  useEffect(() => {
    recompute();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [items.length, rowHeight, className]);

  useEffect(() => {
    const el = containerRef.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => recompute());
    observer.observe(el);
    return () => observer.disconnect();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const padTop = range.start * rowHeight;
  const padBottom = Math.max(0, (items.length - range.end) * rowHeight);

  return (
    <div ref={containerRef} onScroll={recompute} className={className}>
      <div style={{ paddingTop: padTop, paddingBottom: padBottom }}>
        {items.slice(range.start, range.end).map((item, idx) => renderItem(item, range.start + idx))}
      </div>
    </div>
  );
}
