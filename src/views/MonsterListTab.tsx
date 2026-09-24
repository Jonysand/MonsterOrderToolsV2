import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  AlertTriangle,
  Check,
  Download,
  Pencil,
  Plus,
  Search,
  Shield,
  Upload,
  X,
} from "lucide-react";
import { MonsterConfig, MonsterDict, RosterData } from "../types";
import { ICON_LIST, iconGroup } from "../generated/iconManifest";
import {
  GAME_LABEL,
  GAME_ORDER,
  buildAliasOwners,
  conflictWinner,
  conflictWords,
  dictToEntries,
  iconSrc,
  matchKeyword,
} from "../lib/monsterList";

/** 条目编辑草稿（抽屉内为草稿态，仅点「保存」才落盘） */
interface EntryDraft {
  /** 改名前旧名；新建条目时为 null */
  original: string | null;
  name: string;
  icon: string;
  level: number;
  aliases: string[];
}

interface Props {
  dict: MonsterDict;
  roster: RosterData;
  /** 名单变更（乐观更新 + 防抖落盘由主窗口统一负责） */
  onRosterChange: (next: RosterData) => void;
  /** 字典增删条目后重新拉取 */
  onDictChanged: () => Promise<void> | void;
  onImport: () => void;
  onExport: () => void;
  toast: (msg: string) => void;
  askConfirm: (message: string, title?: string) => Promise<boolean>;
}

const LV_NAME = ["普通", "历战", "历战王"];

export const MonsterListTab: React.FC<Props> = ({
  dict,
  roster,
  onRosterChange,
  onDictChanged,
  onImport,
  onExport,
  toast,
  askConfirm,
}) => {
  // 图鉴库筛选
  const [libKeyword, setLibKeyword] = useState("");
  const [libGame, setLibGame] = useState("all");
  const [libState, setLibState] = useState<"all" | "in" | "out">("all");

  // 条目编辑抽屉
  const [draft, setDraft] = useState<EntryDraft | null>(null);
  const [aliasInput, setAliasInput] = useState("");
  const [saving, setSaving] = useState(false);

  // 别称冲突明细（只读清单，点条目可直接跳到编辑抽屉）
  const [conflictOpen, setConflictOpen] = useState(false);

  // 图标选择器（二级弹层）
  const [pickerOpen, setPickerOpen] = useState(false);
  const [pkKeyword, setPkKeyword] = useState("");
  const [pkGame, setPkGame] = useState("all");

  const entries = useMemo(() => dictToEntries(dict), [dict]);
  const owners = useMemo(() => buildAliasOwners(entries), [entries]);
  const conflicts = useMemo(() => conflictWords(owners), [owners]);
  const rosterSet = useMemo(() => new Set(roster.items), [roster.items]);
  const entryByName = useMemo(
    () => new Map(entries.map((e) => [e.name, e])),
    [entries]
  );

  /** 冲突明细：别称 + 全部归属条目 + 运行时实际生效者（字典序最小者） */
  const conflictList = useMemo(() => {
    const list: { word: string; owners: string[]; winner: string }[] = [];
    conflicts.forEach((word) => {
      const holders = owners.get(word) || [];
      const winner = conflictWinner(owners, word);
      if (holders.length > 1 && winner) list.push({ word, owners: holders, winner });
    });
    return list.sort((a, b) => a.word.localeCompare(b.word, "zh-Hans-CN"));
  }, [conflicts, owners]);

  const libList = useMemo(
    () =>
      entries.filter((e) => {
        if (libGame !== "all" && e.game !== libGame) return false;
        if (libState === "in" && !rosterSet.has(e.name)) return false;
        if (libState === "out" && rosterSet.has(e.name)) return false;
        return matchKeyword(e, libKeyword);
      }),
    [entries, libGame, libState, libKeyword, rosterSet]
  );

  const pkList = useMemo(
    () =>
      ICON_LIST.filter((p) => {
        if (pkGame !== "all" && iconGroup(p) !== pkGame) return false;
        if (pkKeyword.trim() && !p.toLowerCase().includes(pkKeyword.trim().toLowerCase()))
          return false;
        return true;
      }),
    [pkGame, pkKeyword]
  );

  /* ---------------- 名单操作 ---------------- */
  const togglePick = (name: string) => {
    const items = rosterSet.has(name)
      ? roster.items.filter((n) => n !== name)
      : [...roster.items, name];
    onRosterChange({ ...roster, items });
  };

  /** 追加去重（保持已有顺位不变） */
  const appendNames = (names: string[], tipLabel: string) => {
    const add = names.filter((n) => !rosterSet.has(n));
    if (!add.length) {
      toast(`${tipLabel}：没有新增怪物（均已在禁点名单中）`);
      return;
    }
    onRosterChange({ ...roster, items: [...roster.items, ...add] });
    toast(`${tipLabel}：新增 ${add.length} 个怪物`);
  };

  /* ---------------- 条目编辑 ---------------- */
  const openDrawer = (name: string) => {
    const e = entryByName.get(name);
    if (!e) return;
    setDraft({
      original: name,
      name,
      icon: e.icon,
      level: e.level,
      aliases: e.aliases.filter((a) => a !== name),
    });
    setAliasInput("");
    setPickerOpen(false);
  };

  const openNewDrawer = () => {
    setDraft({
      original: null,
      name: "",
      // 图标可留空：自定义怪物允许不配图，展示位用占位兜底
      icon: "",
      level: 0,
      aliases: [],
    });
    setAliasInput("");
    setPickerOpen(false);
  };

  const saveDraft = async () => {
    if (!draft) return;
    const name = draft.name.trim();
    if (!name) {
      toast("怪物原名不能为空");
      return;
    }
    const config: MonsterConfig = {
      默认历战等级: draft.level,
      图标地址: draft.icon,
      别称: draft.aliases,
    };
    setSaving(true);
    try {
      await invoke("save_monster_entry", {
        name,
        config,
        original: draft.original,
      });
      await onDictChanged();
      setDraft(null);
      toast(`条目「${name}」已保存并生效`);
    } catch (err) {
      toast(`保存失败: ${err}`);
    } finally {
      setSaving(false);
    }
  };

  const deleteDraft = async () => {
    if (!draft?.original) return;
    const name = draft.original;
    const ok = await askConfirm(
      `删除条目「${name}」？\n\n删除后弹幕不再能点该怪，运行中的队列不受影响。此操作不可撤销。`,
      "删除怪物条目"
    );
    if (!ok) return;
    setSaving(true);
    try {
      await invoke("delete_monster_entry", { name });
      await onDictChanged();
      setDraft(null);
      toast(`条目「${name}」已删除`);
    } catch (err) {
      toast(`删除失败: ${err}`);
    } finally {
      setSaving(false);
    }
  };

  const addAlias = () => {
    const v = aliasInput.trim();
    if (!draft || !v) return;
    if (v === draft.name.trim() || draft.aliases.includes(v)) {
      setAliasInput("");
      return;
    }
    setDraft({ ...draft, aliases: [...draft.aliases, v] });
    setAliasInput("");
  };

  /** 草稿别称冲突：与运行时一致地给出「字典序最小者命中」的确定性结论 */
  const draftConflicts = useMemo(() => {
    if (!draft) return [] as { word: string; others: string[]; winner: string }[];
    const self = draft.original || draft.name.trim();
    const words = new Set([...draft.aliases, draft.name.trim()].filter(Boolean));
    const hits: { word: string; others: string[]; winner: string }[] = [];
    words.forEach((w) => {
      if (!conflicts.has(w)) return;
      const others = (owners.get(w) || []).filter((n) => n !== self);
      if (!others.length) return;
      hits.push({ word: w, others, winner: conflictWinner(owners, w) || self });
    });
    return hits;
  }, [draft, conflicts, owners]);

  // ESC 关闭抽屉 / 图标选择器 / 冲突清单
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (pickerOpen) setPickerOpen(false);
      else if (draft) setDraft(null);
      else if (conflictOpen) setConflictOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [pickerOpen, draft, conflictOpen]);

  const conflictCount = conflictList.length;

  return (
    <div className="ml-scope">
      <header className="page-head">
        <div>
          <h1>怪物名单</h1>
          <p className="sub">
            禁点名单：名单内的怪物不可被点单（弹幕与选怪面板同时生效）· 空名单不限制任何点怪 · 自动保存到本机
          </p>
        </div>
        <div className="head-actions">
          <button className="btn" onClick={onImport}>
            <Upload />
            导入
          </button>
          <button className="btn" onClick={onExport}>
            <Download />
            导出
          </button>
        </div>
      </header>

      <div className="stat-grid">
        <div className="stat">
          <span className="num">{entries.length}</span>
          <span className="cap">怪物总数 · 覆盖 5 部作品</span>
        </div>
        <div className="stat">
          <span className="num">
            {roster.items.length}
            <small> / {entries.length}</small>
          </span>
          <span className="cap">禁点名单 · 弹幕与面板均不可点</span>
        </div>
        <button
          type="button"
          className={`stat${conflictCount ? " warn clickable" : ""}`}
          title={
            conflictCount
              ? "查看冲突别称明细（哪些别称重复、实际命中哪个条目）"
              : "当前没有别称冲突"
          }
          onClick={() => {
            if (conflictCount) setConflictOpen(true);
          }}
        >
          <span className="num">{conflictCount}</span>
          <span className="cap">
            别称冲突 · 按名称排序靠前者优先命中
            {conflictCount > 0 && <em>查看明细</em>}
          </span>
        </button>
      </div>

      <div className="editor-main">
        {/* 左：全量字典图鉴库 */}
        <section className="card lib">
          <div className="toolbar">
            <div className="tb-row">
              <label className="search">
                <Search />
                <input
                  value={libKeyword}
                  onChange={(e) => setLibKeyword(e.target.value)}
                  placeholder="搜索怪物名 / 别称（试试：大胖虎、历战钢龙、霸主）…"
                />
              </label>
              <span className="tb-count">{libList.length} 项</span>
              <button
                className="btn sm"
                title="把当前筛选（作品 / 状态 / 关键词）命中的怪物批量加入禁点名单"
                onClick={() => appendNames(libList.map((e) => e.name), "全部禁点")}
              >
                <Plus />
                全部禁点
              </button>
            </div>
            <div className="tb-row">
              <div className="seg">
                {["all", ...GAME_ORDER].map((g) => (
                  <button
                    key={g}
                    className={libGame === g ? "active" : ""}
                    onClick={() => setLibGame(g)}
                  >
                    {GAME_LABEL[g]}
                  </button>
                ))}
              </div>
              <div className="seg">
                {(
                  [
                    ["all", "全部"],
                    ["in", "已禁点"],
                    ["out", "未禁点"],
                  ] as const
                ).map(([s, label]) => (
                  <button
                    key={s}
                    className={libState === s ? "active" : ""}
                    onClick={() => setLibState(s)}
                  >
                    {label}
                  </button>
                ))}
              </div>
            </div>
          </div>

          <div className="grid-wrap">
            {libList.length === 0 ? (
              <div className="grid-empty">
                <Search />
                <div>没有匹配的怪物 · 换个关键词或别称试试</div>
              </div>
            ) : (
              <div className="monster-grid">
                {libList.map((e) => {
                  const picked = rosterSet.has(e.name);
                  const dupWords = e.aliases.filter((a) => conflicts.has(a));
                  return (
                    <div
                      key={e.name}
                      className={`mcard${picked ? " picked" : ""}${e.game === "MHWilds" ? " wild" : ""}`}
                      title={
                        dupWords.length
                          ? `${e.name} · 冲突别称：${dupWords.join("、")}`
                          : e.name
                      }
                      onClick={() => togglePick(e.name)}
                    >
                      <button
                        className="edit-btn"
                        title="编辑条目"
                        onClick={(ev) => {
                          ev.stopPropagation();
                          openDrawer(e.name);
                        }}
                      >
                        <Pencil />
                      </button>
                      <span className="pick-dot">
                        <Check />
                      </span>
                      {e.icon ? (
                        <img
                          className="ic"
                          src={iconSrc(e.icon)}
                          alt={e.name}
                          loading="lazy"
                          onError={(ev) => {
                            (ev.target as HTMLElement).style.visibility = "hidden";
                          }}
                        />
                      ) : (
                        <span className="ic icon-ph" title="未设置图标">
                          <Shield />
                        </span>
                      )}
                      <div className="nm">{e.name}</div>
                      <div className="meta">
                        <span className={`lv l${e.level}`}>{LV_NAME[e.level] || "普通"}</span>
                        <span>{e.aliases.length} 别称</span>
                        {dupWords.length > 0 && <span style={{ color: "#fca5a5" }}>◆</span>}
                      </div>
                    </div>
                  );
                })}
              </div>
            )}
          </div>
        </section>

        {/* 右：禁点名单（只读展示，增删一律走左库卡片点击） */}
        <aside className="card roster">
          <header>
            <span className="ttl">禁点名单</span>
            <span className="cnt">{roster.items.length}</span>
          </header>

          <div className="roster-list">
            {roster.items.length === 0 ? (
              <div className="grid-empty" style={{ padding: "32px 0" }}>
                <Shield />
                <div>
                  禁点名单为空
                  <br />
                  不限制任何点怪 · 从左库点击卡片可加入禁点
                </div>
              </div>
            ) : (
              roster.items.map((name) => {
                const e = entryByName.get(name);
                return (
                  <div key={name} className="rrow">
                    {e ? (
                      e.icon ? (
                        <img
                          className="ric"
                          src={iconSrc(e.icon)}
                          alt=""
                          onError={(ev) => {
                            (ev.target as HTMLElement).style.visibility = "hidden";
                          }}
                        />
                      ) : (
                        <span className="ric icon-ph" title="未设置图标">
                          <Shield />
                        </span>
                      )
                    ) : (
                      <span className="ric missing" title="条目已不存在">
                        ?
                      </span>
                    )}
                    <span className="rn" title={e ? name : `${name}（条目已不存在）`}>
                      {name}
                    </span>
                    <span className="ra">{e ? `${e.aliases.length} 别称` : "条目缺失"}</span>
                  </div>
                );
              })
            )}
          </div>

          <div className="roster-note">
            名单内的怪弹幕与选怪面板都点不了 · 从左库点击卡片可加入或移出
          </div>
          <footer>
            <button className="btn" onClick={openNewDrawer}>
              <Plus />
              新增自定义怪物
            </button>
          </footer>
        </aside>
      </div>

      {/* 条目编辑抽屉 + 图标选择器 */}
      {draft && (
        <div className="drawer-layer">
          <div className="drawer-mask" onClick={() => setDraft(null)} />

          {pickerOpen && (
            <div className="icon-picker">
              <header>
                <span className="ttl">选择图标</span>
                <span className="tb-count">{pkList.length} 张</span>
                <button className="x" onClick={() => setPickerOpen(false)}>
                  <X />
                </button>
              </header>
              <div className="pk-tools">
                <div className="seg">
                  {["all", ...GAME_ORDER].map((g) => (
                    <button
                      key={g}
                      className={pkGame === g ? "active" : ""}
                      onClick={() => setPkGame(g)}
                    >
                      {GAME_LABEL[g]}
                    </button>
                  ))}
                </div>
                <label className="search" style={{ flex: 1 }}>
                  <Search />
                  <input
                    value={pkKeyword}
                    onChange={(e) => setPkKeyword(e.target.value)}
                    placeholder="图标文件名…"
                  />
                </label>
              </div>
              <div className="pk-grid">
                {pkList.map((p) => (
                  <div
                    key={p}
                    className={`ipick${draft.icon === p ? " sel" : ""}`}
                    title={p}
                    onClick={() => setDraft({ ...draft, icon: p })}
                  >
                    <img src={iconSrc(p)} loading="lazy" alt="" />
                  </div>
                ))}
              </div>
            </div>
          )}

          <aside className="drawer">
            <header>
              <span className="ttl">{draft.original ? "编辑怪物条目" : "新增怪物条目"}</span>
              <span className="src">本机列表文件</span>
              <button className="x" onClick={() => setDraft(null)}>
                <X />
              </button>
            </header>

            <div className="drawer-body">
              <div className="field">
                <label>图鉴图标</label>
                <div className="icon-edit">
                  {draft.icon ? (
                    <img className="big" src={iconSrc(draft.icon)} alt="" />
                  ) : (
                    <span className="big" title="未设置图标">
                      <Shield />
                    </span>
                  )}
                  <div className="info">
                    <div className="ipath">{draft.icon || "（未选择图标）"}</div>
                    <button className="btn sm" onClick={() => setPickerOpen(true)}>
                      <Upload />
                      更换图标
                    </button>
                    {draft.icon && (
                      <button
                        className="btn sm"
                        title="清除图标，保存后生效"
                        onClick={() => setDraft({ ...draft, icon: "" })}
                      >
                        清除图标
                      </button>
                    )}
                  </div>
                </div>
                <div className="hint">图标可留空：列表、选怪面板与队列会显示占位图标</div>
              </div>

              <div className="field">
                <label>怪物原名</label>
                <input
                  className="input"
                  value={draft.name}
                  onChange={(e) => setDraft({ ...draft, name: e.target.value })}
                  placeholder="例：历战王锁刃龙"
                />
                <div className="hint">
                  修改原名会以新名重建条目并同步禁点名单中的引用 —— 已排队条目保留旧名，不影响直播中的队列
                </div>
              </div>

              <div className="field">
                <label>默认历战等级</label>
                <div className="lv-seg">
                  {[0, 1, 2].map((lv) => (
                    <button
                      key={lv}
                      className={`a${lv}${draft.level === lv ? " active" : ""}`}
                      onClick={() => setDraft({ ...draft, level: lv })}
                    >
                      {LV_NAME[lv]}
                      <small>
                        {lv === 0 ? "等级 0" : lv === 1 ? "等级 1 · 紫色" : "等级 2 · 橙红"}
                      </small>
                    </button>
                  ))}
                </div>
                <div className="hint">弹幕未写「历战 / 历战王 / AT」修饰词时，采用此默认值</div>
              </div>

              <div className="field">
                <label>别称（回车添加）</label>
                <div className="alias-box">
                  <span className="chip orig">
                    {draft.name.trim() || "（原名）"}
                    <span style={{ fontSize: 9, opacity: 0.6, marginLeft: 2 }}>原名</span>
                  </span>
                  {draft.aliases.map((a) => {
                    const dup = conflicts.has(a);
                    const others = (owners.get(a) || []).filter(
                      (n) => n !== (draft.original || draft.name.trim())
                    );
                    return (
                      <span
                        key={a}
                        className={`chip${dup ? " dup" : ""}`}
                        title={
                          dup ? `与「${others.join("、")}」冲突：按名称排序靠前者优先命中` : ""
                        }
                      >
                        {a}
                        <button
                          className="x"
                          title="删除别称"
                          onClick={() =>
                            setDraft({ ...draft, aliases: draft.aliases.filter((x) => x !== a) })
                          }
                        >
                          <X />
                        </button>
                      </span>
                    );
                  })}
                  <input
                    className="alias-input"
                    value={aliasInput}
                    onChange={(e) => setAliasInput(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") {
                        e.preventDefault();
                        addAlias();
                      }
                    }}
                    placeholder="+ 输入别称后回车…"
                  />
                </div>
                <div className="hint">
                  别称与弹幕文本做全字匹配（^…$）；原名自动可作为别称参与匹配
                </div>
              </div>

              {draftConflicts.length > 0 && (
                <div className="alert warn">
                  <AlertTriangle />
                  <span>
                    别称「<b>{draftConflicts[0].word}</b>」与条目「
                    <b>{draftConflicts[0].others.join("、")}</b>」重复：运行时按名称排序取靠前者，
                    {draftConflicts[0].winner === (draft.original || draft.name.trim()) ? (
                      <>
                        本条目（<b>{draft.name.trim()}</b>）生效，对方不会命中该别称
                      </>
                    ) : (
                      <>
                        条目「<b>{draftConflicts[0].winner}</b>」生效，本条目该别称不会命中
                      </>
                    )}
                    。建议更换或删除冲突项。
                  </span>
                </div>
              )}
            </div>

            <footer>
              <button
                className="btn danger"
                disabled={!draft.original || saving}
                onClick={deleteDraft}
              >
                删除条目
              </button>
              <span className="spacer" />
              <button className="btn" onClick={() => setDraft(null)}>
                取消
              </button>
              <button className="btn primary" disabled={saving} onClick={saveDraft}>
                {saving ? "保存中…" : "保存"}
              </button>
            </footer>
          </aside>
        </div>
      )}

      {/* 别称冲突明细（独立浮层：点条目直接跳到编辑抽屉） */}
      {conflictOpen && !draft && (
        <div className="drawer-layer">
          <div className="drawer-mask" onClick={() => setConflictOpen(false)} />
          <aside className="drawer conflict-drawer">
            <header>
              <span className="ttl">别称冲突明细</span>
              <span className="src">{conflictCount} 组</span>
              <button className="x" onClick={() => setConflictOpen(false)}>
                <X />
              </button>
            </header>
            <div className="drawer-body">
              <div className="hint">
                同一别称被多个条目占用时，只有一个会生效：运行时按名称排序取靠前者（下表标注生效 / 不生效）。点条目可直接打开编辑抽屉修改。
              </div>
              {conflictList.length === 0 ? (
                <div className="grid-empty" style={{ padding: "24px 0" }}>
                  <Shield />
                  <div>当前没有别称冲突</div>
                </div>
              ) : (
                conflictList.map((c) => (
                  <div className="cf-item" key={c.word}>
                    <div className="cf-word" title={`冲突别称「${c.word}」被 ${c.owners.length} 个条目占用`}>
                      {c.word}
                    </div>
                    <div className="cf-owners">
                      {c.owners.map((n) => (
                        <button
                          key={n}
                          className={`cf-owner${n === c.winner ? " win" : ""}`}
                          title={
                            n === c.winner
                              ? `「${c.word}」实际命中此条目`
                              : `「${c.word}」不会命中此条目（被 ${c.winner} 抢先）`
                          }
                          onClick={() => {
                            setConflictOpen(false);
                            openDrawer(n);
                          }}
                        >
                          <span className="n">{n}</span>
                          <span className="s">{n === c.winner ? "生效" : "不生效"}</span>
                        </button>
                      ))}
                    </div>
                  </div>
                ))
              )}
            </div>
          </aside>
        </div>
      )}
    </div>
  );
};
