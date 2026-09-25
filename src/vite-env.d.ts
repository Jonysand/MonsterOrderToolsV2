/// <reference types="vite/client" />

/**
 * 编译期形态常量：对应后端 Cargo feature `lite`（原工程 ONLY_ORDER_MONSTER 编译期宏）。
 * 由 vite.config.ts 按构建模式注入——`vite build --mode lite` 为 true（Lite 纯排队版），
 * 其余为 false（完整版）。运行期不可切换，release 构建下死分支会被整体消除。
 */
declare const __IS_LITE__: boolean;
