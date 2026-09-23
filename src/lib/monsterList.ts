import { MonsterDict } from "../types";
import { iconGroup } from "../generated/iconManifest";

/** 作品分段（图鉴库 / 选怪面板共用；`all` 为不筛选） */
export const GAME_ORDER = ["MHWilds", "MHWorld", "MHWI", "MHRise", "MHRS"] as const;

export const GAME_LABEL: Record<string, string> = {
  all: "全部",
  MHWilds: "荒野",
  MHWorld: "世界",
  MHWI: "冰原",
  MHRise: "崛起",
  MHRS: "曙光",
};

/** 怪物条目在界面中的派生视图（字典 + 图标路径推导） */
export interface MonsterEntry {
  name: string;
  icon: string;
  level: number;
  aliases: string[];
  game: string;
}

/** 字典 → 按作品分组的稳定列表（作品顺序固定，组内按原名排序） */
export function dictToEntries(dict: MonsterDict): MonsterEntry[] {
  const entries: MonsterEntry[] = Object.entries(dict).map(([name, cfg]) => ({
    name,
    icon: cfg.图标地址,
    level: cfg.默认历战等级,
    aliases: (cfg.别称 || []).map((a) => a.trim()).filter(Boolean),
    game: iconGroup(cfg.图标地址),
  }));
  return entries.sort((a, b) => {
    const ga = GAME_ORDER.indexOf(a.game as (typeof GAME_ORDER)[number]);
    const gb = GAME_ORDER.indexOf(b.game as (typeof GAME_ORDER)[number]);
    if (ga !== gb) return (ga < 0 ? 99 : ga) - (gb < 0 ? 99 : gb);
    return a.name.localeCompare(b.name, "zh-Hans-CN");
  });
}

/** 图标静态资源地址（随包目录 public/monster_icons） */
export const iconSrc = (path: string) => `/monster_icons/${path}`;

/** 全字包含匹配：命中原名或任一别称（与后端 ^…$ 匹配的直觉一致，便于用黑话定位） */
export function matchKeyword(entry: MonsterEntry, keyword: string): boolean {
  const kw = keyword.trim().toLowerCase();
  if (!kw) return true;
  if (entry.name.toLowerCase().includes(kw)) return true;
  return entry.aliases.some((a) => a.toLowerCase().includes(kw));
}

/** 别称归属表：{ 别称: 归属条目名[] } */
export function buildAliasOwners(
  entries: MonsterEntry[]
): Map<string, string[]> {
  const owners = new Map<string, string[]>();
  for (const e of entries) {
    const words = new Set<string>([...e.aliases, e.name]);
    for (const w of words) {
      const list = owners.get(w);
      if (list) {
        if (!list.includes(e.name)) list.push(e.name);
      } else {
        owners.set(w, [e.name]);
      }
    }
  }
  return owners;
}

/** 冲突别称集合（同一别称被多个条目占用；运行时按字典序命中） */
export function conflictWords(owners: Map<string, string[]>): Set<string> {
  const set = new Set<string>();
  owners.forEach((list, word) => {
    if (list.length > 1) set.add(word);
  });
  return set;
}

/**
 * 冲突命中结论：与运行时一致 —— 字典序最小的归属条目胜出。
 * 返回 null 表示该别称无冲突。
 */
export function conflictWinner(
  owners: Map<string, string[]>,
  word: string
): string | null {
  const list = owners.get(word);
  if (!list || list.length < 2) return null;
  return [...list].sort()[0];
}
