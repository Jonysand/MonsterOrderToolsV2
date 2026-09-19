import { useEffect, useRef, useState } from "react";

interface MarqueeTextProps {
  text: string;
  className?: string;
}

/**
 * 长文本往返滚动（D6）：仅当文本宽度超出容器时启动，鼠标悬停暂停。
 * 对齐原工程 OrderedMonsterWindow.OnScrollTextLoaded / OnScrollTextMouseEnter / OnScrollTextMouseLeave
 * （时长按「超出像素 / 25」估算，最少 2 秒）。
 */
export const MarqueeText: React.FC<MarqueeTextProps> = ({ text, className }) => {
  const boxRef = useRef<HTMLDivElement>(null);
  const textRef = useRef<HTMLSpanElement>(null);
  const [shift, setShift] = useState(0);

  useEffect(() => {
    const box = boxRef.current;
    const span = textRef.current;
    if (!box || !span) return;

    const measure = () => {
      const overflow = span.scrollWidth - box.clientWidth;
      setShift(overflow > 2 ? overflow + 12 : 0);
    };
    measure();

    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(box);
    return () => observer.disconnect();
  }, [text]);

  const duration = Math.max(2, shift / 25);
  const style =
    shift > 0
      ? ({
          animation: `marquee-x ${duration}s ease-in-out infinite alternate`,
          "--marquee-shift": `-${shift}px`,
        } as React.CSSProperties)
      : undefined;

  return (
    <div ref={boxRef} className={`overflow-hidden marquee-pause ${className ?? ""}`} title={text}>
      <span ref={textRef} className="inline-block whitespace-nowrap" style={style}>
        {text}
      </span>
    </div>
  );
};
