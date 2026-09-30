import { WEAPON_ICON_LIST } from "../generated/weaponIconManifest";

/**
 * 14 把武器的口径（随机抽选的武器池）。
 *
 * 图标来源：monsterhunterwiki 的 `MHWA-<Weapon> Icon Base.webp`（100×100 纯灰阶线稿），
 * 已转 PNG 落在 `public/weapon_icons/MHWilds/`，打包副本见 `MonsterOrderWilds_configs/weapon_icons.zip`。
 *
 * 这里**只维护 slug → 名称**的映射，图标路径由 slug 拼出，因此不存在
 * "改了文件名忘了改引用" 的空档；文件是否真的存在由后端
 * `draw.rs::test_weapon_icon_files_exist` 与下方的清单核对共同兜底。
 */

export interface WeaponEntry {
  /** 稳定 id（= 相对 /weapon_icons/ 的路径）。排除名单按它存，改中文名不会让排除失效 */
  id: string;
  /** 官方简中名 */
  cn: string;
  /** 拉丁名（抽选盘上作副标题） */
  en: string;
  /** 相对 /weapon_icons/ 的路径 */
  icon: string;
}

const GAME_DIR = "MHWilds";
const FILE_PREFIX = "MHWilds-";
const FILE_SUFFIX = "_Icon_Base.png";

/** 顺序即游戏内武器栏顺序（大剑 → 弓） */
const CATALOG: ReadonlyArray<{ slug: string; cn: string; en: string }> = [
  { slug: "Great_Sword", cn: "大剑", en: "GREAT SWORD" },
  { slug: "Long_Sword", cn: "太刀", en: "LONG SWORD" },
  { slug: "Sword_and_Shield", cn: "单手剑", en: "SWORD & SHIELD" },
  { slug: "Dual_Blades", cn: "双剑", en: "DUAL BLADES" },
  { slug: "Hammer", cn: "大锤", en: "HAMMER" },
  { slug: "Hunting_Horn", cn: "狩猎笛", en: "HUNTING HORN" },
  { slug: "Lance", cn: "长枪", en: "LANCE" },
  { slug: "Gunlance", cn: "铳枪", en: "GUNLANCE" },
  { slug: "Switch_Axe", cn: "斩击斧", en: "SWITCH AXE" },
  { slug: "Charge_Blade", cn: "盾斧", en: "CHARGE BLADE" },
  { slug: "Insect_Glaive", cn: "操虫棍", en: "INSECT GLAIVE" },
  { slug: "Light_Bowgun", cn: "轻弩炮", en: "LIGHT BOWGUN" },
  { slug: "Heavy_Bowgun", cn: "重弩炮", en: "HEAVY BOWGUN" },
  { slug: "Bow", cn: "弓", en: "BOW" },
];

export const WEAPONS: WeaponEntry[] = CATALOG.map(({ slug, cn, en }) => {
  const icon = `${GAME_DIR}/${FILE_PREFIX}${slug}${FILE_SUFFIX}`;
  return { id: icon, cn, en, icon };
});

/** 武器图标静态资源地址（随包目录 public/weapon_icons） */
export const weaponSrc = (icon: string) => `/weapon_icons/${icon}`;

/**
 * 口径核对：目录、名称表与构建期清单三者必须一致。
 *
 * 只返回缺失项而不抛异常 —— 抽选盘上少一张图标是个空白格，在暗色舞台上极不显眼，
 * 适合"记一笔日志"而不是"整页崩掉"。开发态由 `warnOnCatalogDrift()` 输出到控制台，
 * 发布态另由后端 `draw.rs::test_weapon_icon_files_exist` 在 `cargo test` 阶段拦下。
 */
export function missingWeaponIcons(): string[] {
  const inManifest = new Set(WEAPON_ICON_LIST);
  const missing = WEAPONS.filter((w) => !inManifest.has(w.icon)).map((w) => w.icon);
  if (WEAPON_ICON_LIST.length !== WEAPONS.length) {
    missing.push(
      `清单共 ${WEAPON_ICON_LIST.length} 张，而武器表为 ${WEAPONS.length} 把（多出来的图标没有对应名称）`
    );
  }
  return missing;
}

/** 开发态自检（发布构建下整段被消除） */
export function warnOnCatalogDrift(): void {
  if (!import.meta.env.DEV) return;
  const missing = missingWeaponIcons();
  if (missing.length) {
    console.warn("[Draw] 武器图标口径不一致，请检查 public/weapon_icons 与武器表:", missing);
  }
}
