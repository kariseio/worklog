import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

// Tauri 가 `beforeDevCommand` 로 이 서버를 띄우고 devUrl(1420) 에 접속한다.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [solid()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    watch: {
      // Rust 소스 변경은 tauri CLI 가 처리하므로 vite 는 무시한다.
      ignored: ["**/src-tauri/**"],
    },
  },
  envPrefix: ["VITE_", "TAURI_ENV_*"],
  build: {
    // WebView2(Edge) 만 대상이라 최신 문법을 그대로 낸다.
    target: "chrome120",
    minify: "esbuild",
    sourcemap: false,
  },
});
