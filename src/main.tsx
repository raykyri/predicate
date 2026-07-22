import ReactDOM from "react-dom/client";
import App from "./App";
import { diagnosticsHeartbeat, diagnosticsRecordBatch } from "./lib/api";
import { startDiagnostics } from "./lib/diagnostics";
import { loadTerminalFont } from "./lib/terminalFont";
import "./styles.css";

// Freeze diagnostics: heartbeat the backend's stall watchdog, forward buffered
// frontend reports to the durable log, and detect main-thread stalls locally
// (see lib/diagnostics.ts / src-tauri/src/diagnostics.rs).
startDiagnostics({
  heartbeat: diagnosticsHeartbeat,
  recordBatch: diagnosticsRecordBatch,
});

// Start loading the bundled terminal font during app boot so pane sizing can use
// stable metrics before the first terminal is opened.
void loadTerminalFont().catch(() => {});

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(<App />);
