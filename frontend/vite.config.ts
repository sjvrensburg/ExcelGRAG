import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Dev runs the frontend on Vite's port and proxies the API to the Rust
// server, so HMR works without CORS. The production build lands in dist/,
// which the server embeds and serves itself.
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/api": "http://127.0.0.1:8765",
      "/ws": { target: "ws://127.0.0.1:8765", ws: true },
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
});
