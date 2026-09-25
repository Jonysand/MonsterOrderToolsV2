import { useEffect, useMemo, useState } from "react";
import { Lock, Search, Shield } from "lucide-react";
import { MonsterDict, RosterData } from "../types";
import { GAME_LABEL, GAME_ORDER, dictToEntries, iconSrc, matchKeyword } from "../lib/monsterList";

export interface PickerOrderPayload {
  userName: string;
  monsterName: string;
  guardLevel: number;
  /** null = 跟随字典默认历战等级；0/1/2 = 强制该等级 */
  temperedLevel: number | null;
  isPriority: boolean;
}

interface Props {
  dict: MonsterDict;
  roster: RosterData;
  submitting: boolean;
  onSubmit: (payload: PickerOrderPayload) => void;
}

const LV_NAME = ["普通", "历战", "历战王"];

/**
 * 选怪面板：点图标即点怪（取代原「快速手动点怪」文字表单）。
 * 候选为全量字典；禁点名单内的怪物置灰加锁，点击只提示拦截原因（与弹幕拦截同一份名单）。
 */
export const MonsterPickerPanel: React.FC<Props> = ({ dict, roster, submitting, onSubmit }) => {
  const [keyword, setKeyword] = useState("");
  const [game, setGame] = useState("all");
  const [selected, setSelected] = useState<string | null>(null);
  const [userName, setUserName] = useState("");
  const [guardLevel, setGuardLevel] = useState(0);
  /** "default" = 跟随字典；否则为强制等级 */
  const [tempered, setTempered] = useState<"default" | 0 | 1 | 2>("default");
  const [isPriority, setIsPriority] = useState(false);
  const [footMsg, setFootMsg] = useState<string | null>(null);

  const entries = useMemo(() => dictToEntries(dict), [dict]);
  const blockedSet = useMemo(() => new Set(roster.items), [roster.items]);
  const entryByName = useMemo(() => new Map(entries.map((e) => [e.name, e])), [entries]);

  /** 全量字典按筛选展示（禁点名单内的怪置灰但仍在列表内，顺序即字典顺序） */
  const hits = useMemo(
    () =>
      entries.filter((e) => (game === "all" || e.game === game) && matchKeyword(e, keyword)),
    [entries, game, keyword]
  );

  /**
   * 选中项必须始终来自**当前可见且未禁点**的候选。
   *
   * 早期实现在失效时静默回落到"第一个可点的怪"，导致实际提交目标与主播当下看到的选择不一致
   * （筛选掉「黑龙」后确认卡仍提交黑龙，或被禁点后自动换成另一只）。现在一律清空选中，
   * 由主播重新点选。
   */
  useEffect(() => {
    if (!selected) return;
    const stillSelectable = hits.some((e) => e.name === selected) && !blockedSet.has(selected);
    if (!stillSelectable) {
      setSelected(null);
      setFootMsg(`已取消选中「${selected}」（不在当前筛选结果中或已被禁点），请重新点选`);
    }
  }, [blockedSet, hits, selected]);

  /** 底部状态文案：禁点名单基数决定基线，临时提示由点击行为覆盖 */
  const statusText = roster.items.length
    ? `禁点名单内的怪物已置灰不可点（共 ${roster.items.length} 种，弹幕点单同样被拦截）`
    : "禁点名单为空 · 所有怪物均可点";

  /**
   * 提交前的权威选择：再验一次当前可见性与禁点状态，
   * 防止 `useEffect` 尚未运行就点了「加入排队」。
   */
  const currentSelection = (): typeof entries[number] | null => {
    if (!selected) return null;
    if (blockedSet.has(selected)) return null;
    if (!hits.some((e) => e.name === selected)) return null;
    return entryByName.get(selected) ?? null;
  };

  const selectedEntry = selected ? entryByName.get(selected) : undefined;
  const selAliases = selectedEntry
    ? selectedEntry.aliases.filter((a) => a !== selectedEntry.name)
    : [];

  const handleGo = () => {
    const picked = currentSelection();
    if (!picked) {
      setFootMsg("当前选择已失效（被筛选掉或已禁点），请在列表中点选一个怪物");
      setSelected(null);
      return;
    }
    onSubmit({
      userName: userName.trim(),
      monsterName: picked.name,
      guardLevel,
      temperedLevel: tempered === "default" ? null : tempered,
      isPriority,
    });
  };

  return (
    <>
      <header>
        <span className="ttl">选怪面板</span>
        <span className="cnt">{hits.length}</span>
        <span style={{ flex: 1 }} />
        <span style={{ fontSize: 9.5, color: "var(--w-ink-4)" }}>点图标 → 确认 → 入队</span>
      </header>

      <div className="op-tools">
        <label className="search">
          <Search />
          <input
            value={keyword}
            onChange={(e) => setKeyword(e.target.value)}
            placeholder="搜索怪物 / 别称，点图标即点怪…"
          />
        </label>
        <div className="op-segs">
          {["all", ...GAME_ORDER].map((g) => (
            <button
              key={g}
              className={game === g ? "active" : ""}
              onClick={() => setGame(g)}
            >
              {GAME_LABEL[g]}
            </button>
          ))}
        </div>
      </div>

      <div className="op-grid">
        {hits.map((e) => {
          const blocked = blockedSet.has(e.name);
          return (
            <div
              key={e.name}
              className={`oc${blocked ? " locked" : ""}${selected === e.name ? " sel" : ""}`}
              title={blocked ? `${e.name}（已在禁点名单：到「怪物名单」移出后可点）` : e.name}
              onClick={() => {
                if (blocked) {
                  setFootMsg(`「${e.name}」已在禁点名单，不可点 · 请到「怪物名单」tab 移出`);
                  return;
                }
                setSelected(e.name);
                setFootMsg(null);
              }}
            >
              <span className={`lv l${e.level}`}>{LV_NAME[e.level] || "普通"}</span>
              {blocked && <Lock className="lock-ic" />}
              {e.icon ? (
                <img
                  src={iconSrc(e.icon)}
                  loading="lazy"
                  alt=""
                  onError={(ev) => {
                    (ev.target as HTMLElement).style.visibility = "hidden";
                  }}
                />
              ) : (
                <span className="icon-ph" title="未设置图标">
                  <Shield />
                </span>
              )}
              <div className="nm">{e.name}</div>
            </div>
          );
        })}
      </div>

      <div className="op-confirm">
        {selectedEntry ? (
          <div className="sel-line">
            {selectedEntry.icon ? (
              <img src={iconSrc(selectedEntry.icon)} alt="" />
            ) : (
              <span className="icon-ph" title="未设置图标">
                <Shield />
              </span>
            )}
            <div style={{ minWidth: 0 }}>
              <div className="m">{selectedEntry.name}</div>
              <div className="an">
                别称：{selAliases.slice(0, 4).join(" · ") || "（无）"}
              </div>
            </div>
          </div>
        ) : (
          <div className="sel-line">
            <div className="an">尚未选择怪物 —— 请先在上方点选（禁点名单内的怪不可选）</div>
          </div>
        )}

        <div className="crow">
          <input
            value={userName}
            onChange={(e) => setUserName(e.target.value)}
            placeholder="水友昵称（留空 → 以「房管」身份入队）"
          />
        </div>
        <div className="crow">
          <select
            value={guardLevel}
            onChange={(e) => setGuardLevel(Number(e.target.value))}
          >
            <option value={0}>身份：普通水友</option>
            <option value={3}>身份：舰长（三等）</option>
            <option value={2}>身份：提督（二等）</option>
            <option value={1}>身份：总督（一等）</option>
          </select>
          <select
            value={String(tempered)}
            onChange={(e) =>
              setTempered(
                e.target.value === "default" ? "default" : (Number(e.target.value) as 0 | 1 | 2)
              )
            }
          >
            <option value="default">难度：默认（按怪物配置）</option>
            <option value={0}>难度：普通</option>
            <option value={1}>难度：历战</option>
            <option value={2}>难度：历战王</option>
          </select>
        </div>
        <div className="cfoot">
          <label className="prio">
            <input
              type="checkbox"
              checked={isPriority}
              onChange={(e) => setIsPriority(e.target.checked)}
            />
            优先插队
          </label>
          <button className="go" disabled={submitting} onClick={handleGo}>
            {submitting ? "入队中…" : "加入排队"}
          </button>
        </div>
      </div>

      <div className="op-foot">
        <Lock />
        <span>{footMsg ?? statusText}</span>
      </div>
    </>
  );
};
