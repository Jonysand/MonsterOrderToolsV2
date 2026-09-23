import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";

// 窗口已关闭 Tauri 原生拖放（dragDropEnabled: false，Windows 上不关就没法用 HTML5 拖拽排序），
// 此时外部文件拖入会交回 WebView2 自行处理，默认行为是导航到该文件、整个界面被替换掉。
// 只在拖拽内容含 Files 时拦截；内部排序拖拽带的是 text/plain，不受影响。
const isExternalFileDrag = (e: DragEvent) =>
  !!e.dataTransfer && Array.from(e.dataTransfer.types).includes("Files");

window.addEventListener("dragover", (e) => {
  if (isExternalFileDrag(e)) e.preventDefault();
});
window.addEventListener("drop", (e) => {
  if (isExternalFileDrag(e)) e.preventDefault();
});

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
