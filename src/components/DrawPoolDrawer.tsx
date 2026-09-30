import { useMemo, useState } from "react";
import { Search, X } from "lucide-react";
import { DrawSettings } from "../types";
import { GAME_LABEL, GAME_ORDER, MonsterEntry, iconSrc, matchKeyword } from "../lib/monsterList";

interface Props {
  open: boolean;
  /** 全量怪物候选（按作品分组前） */
  entries: MonsterEntry[];
  settings: DrawSettings;
  /** 当前实际可抽的怪物数（已扣掉作品筛选与排除） */
  poolCount: number;
  onToggleGame: (game: string) => void;
  onToggleMonster: (name: string) => void;
  /** 批量设置某批怪物的排除状态 */
  onBulkToggle: (names: string[], exclude: boolean) => void;
  onClearAll: () => void;
  onSyncRoster: () => void;
  onClose: () => void;
}

const LV_NAME = ["普通", "历战", "历战王"];

/**
 * 池子配置抽屉：作品多选 + 手选排除名单。
 *
 * 只负责筛选与展示，状态变更全部回抛给 `RandomDrawTab` —— 排除名单与设置文件
 * 是同一份权威状态，抽屉再存一份副本会立刻产生两份真相。
 */
export const DrawPoolDrawer: React.FC<Props> = ({
  open,
  entries,
  settings,
  poolCount,
  onToggleGame,
  onToggleMonster,
  onBulkToggle,
  onClearAll,
  onSyncRoster,
  onClose,
}) => {
  const [keyword, setKeyword] = useState("");

  const excluded = useMemo(() => new Set(settings.excluded_monsters), [settings.excluded_monsters]);
  const games = useMemo(() => new Set(settings.games), [settings.games]);

  /** 抽屉内列表只展示被选中的作品，避免在一个"已关掉的池子"里做排除 */
  const visible = useMemo(
    () => entries.filter((e) => games.has(e.game) && matchKeyword(e, keyword)),
    [entries, games, keyword]
  );

  const grouped = useMemo(() => {
    const map = new Map<string, MonsterEntry[]>();
    for (const e of visible) {
      const list = map.get(e.game);
      if (list) list.push(e);
      else map.set(e.game, [e]);
    }
    return GAME_ORDER.filter((g) => map.has(g)).map((g) => ({
      game: g,
      items: map.get(g) as MonsterEntry[],
    }));
  }, [visible]);

  const excludedInPool = entries.filter((e) => excluded.has(e.name)).length;

  return (
    <>
      <div className={`rd-mask${open ? " on" : ""}`} onClick={onClose} />
      <aside className={`rd-drawer${open ? " on" : ""}`} aria-hidden={!open}>
        <div className="rd-drawer-hd">
          <div>
            <h2>排除名单 · 池子配置</h2>
            <p>
              点怪物图标切换「排除 / 放回」。排除只影响本页抽选，不动「怪物名单」页签的禁点名单。
            </p>
          </div>
          <button className="rd-x" onClick={onClose} title="关闭">
            <X className="w-3.5 h-3.5" />
          </button>
        </div>

        <div className="rd-drawer-bd">
          <div className="rd-search">
            <Search className="w-3.5 h-3.5" />
            <input
              value={keyword}
              onChange={(e) => setKeyword(e.target.value)}
              placeholder="搜怪物名或黑话别称（如「太太」「棺材」）"
              spellCheck={false}
            />
          </div>

          <div className="rd-chips">
            {GAME_ORDER.map((g) => {
              const total = entries.filter((e) => e.game === g).length;
              const on = games.has(g);
              return (
                <button
                  key={g}
                  className={`rd-chip${on ? " on" : ""}`}
                  onClick={() => onToggleGame(g)}
                  title={on ? `取消勾选「${GAME_LABEL[g]}」` : `勾选「${GAME_LABEL[g]}」`}
                >
                  {GAME_LABEL[g]}
                  <span className="c">{total}</span>
                </button>
              );
            })}
          </div>

          {grouped.length === 0 ? (
            <div className="rd-empty">
              {keyword.trim()
                ? `没有匹配「${keyword.trim()}」的怪物`
                : "当前一个作品都没勾选，先去上面把作品勾回来"}
            </div>
          ) : (
            grouped.map(({ game, items }) => {
              const offN = items.filter((e) => excluded.has(e.name)).length;
              const allOff = offN === items.length;
              return (
                <div key={game}>
                  <div className="rd-grp">
                    <span>{GAME_LABEL[game]}</span>
                    <span className="c">
                      {items.length - offN}/{items.length}
                    </span>
                    <span className="line" />
                    <button className="mini" onClick={() => onBulkToggle(items.map((e) => e.name), !allOff)}>
                      {allOff ? "全部放回" : "全部排除"}
                    </button>
                  </div>
                  <div className="rd-mgrid">
                    {items.map((e) => {
                      const off = excluded.has(e.name);
                      return (
                        <div
                          key={e.name}
                          className={`rd-mcell${off ? " off" : ""}`}
                          onClick={() => onToggleMonster(e.name)}
                          title={`${e.name}${e.aliases.length ? " · " + e.aliases.join(" / ") : ""}${
                            off ? "（已排除）" : ""
                          }`}
                        >
                          <img src={iconSrc(e.icon)} alt="" loading="lazy" draggable={false} />
                          {e.level > 0 && <span className={`lv t${e.level}`}>{LV_NAME[e.level]}</span>}
                          <span className="nm">{e.name}</span>
                        </div>
                      );
                    })}
                  </div>
                </div>
              );
            })
          )}
        </div>

        <div className="rd-drawer-ft">
          <span className="stat">
            已排除 <b>{excludedInPool}</b> 只 · 当前可抽 <span>{poolCount}</span> 只
          </span>
          <span className="sp" />
          <button className="rd-btn sec" onClick={onSyncRoster} title="把「怪物名单」页签的禁点名单并入排除名单">
            同步自禁点名单
          </button>
          <button className="rd-btn sec" onClick={onClearAll}>
            全部恢复
          </button>
          <button className="rd-btn pri" onClick={onClose}>
            完成
          </button>
        </div>
      </aside>
    </>
  );
};
