import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Porta 1420 = devUrl do app/tauri.conf.json. O build gera web/dist,
// que é o frontendDist da casca Tauri (ignorado pelo git).
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
});
