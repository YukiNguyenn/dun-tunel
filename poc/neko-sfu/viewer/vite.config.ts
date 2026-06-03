import { defineConfig } from "vite";

export default defineConfig({
  server: {
    host: "0.0.0.0",
    port: 8090,
    strictPort: true,
  },
  preview: {
    host: "0.0.0.0",
    port: 8090,
  },
  build: {
    target: "es2022",
    sourcemap: true,
  },
});
