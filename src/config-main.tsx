import React from "react";
import ReactDOM from "react-dom/client";
import ConfigApp from "./ConfigApp";
import { saveAppSettings } from "./app-settings";
import "./app.css";

void fetch("/api/settings", { cache: "no-store" })
  .then(async (response) => {
    if (response.ok) saveAppSettings(window.localStorage, await response.json());
  })
  .catch(() => undefined)
  .finally(() => {
    ReactDOM.createRoot(document.getElementById("root")!).render(
      <React.StrictMode>
        <ConfigApp />
      </React.StrictMode>,
    );
  });
