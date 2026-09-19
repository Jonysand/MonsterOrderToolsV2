import { useState, useEffect } from "react";
import { MainWindow } from "./views/MainWindow";
import { OverlayWindow } from "./views/OverlayWindow";
import "./App.css";

export default function App() {
  const [route, setRoute] = useState(window.location.hash);

  useEffect(() => {
    const handleHashChange = () => setRoute(window.location.hash);
    window.addEventListener("hashchange", handleHashChange);
    return () => window.removeEventListener("hashchange", handleHashChange);
  }, []);

  if (route === "#/overlay") {
    return <OverlayWindow />;
  }

  return <MainWindow />;
}