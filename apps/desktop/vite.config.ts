import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Tauri v2 expects a fixed dev server port (see src-tauri/tauri.conf.json `devUrl`).
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: "localhost",
  },
  envPrefix: ["VITE_", "TAURI_ENV_"],
});
