import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { PopupPlayer } from "./StreamPlayer";
import "./styles.css";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    {new URLSearchParams(location.search).has("player") ? <PopupPlayer /> : <App />}
  </React.StrictMode>,
);
