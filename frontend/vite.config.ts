import { defineConfig } from "vite";

export default defineConfig({
  build: {
    outDir: "dist",
    emptyOutDir: true
  },
  server: {
    port: 5173,
    proxy: {
      "/api": { target: "http://127.0.0.1:7654", changeOrigin: true }
    }
  }
});
