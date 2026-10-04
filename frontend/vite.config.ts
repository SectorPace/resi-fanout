import { defineConfig } from "vite";

export default defineConfig({
  // relative asset URLs so the UI also works under a random base_path
  base: "./",
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
