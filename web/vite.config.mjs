import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

function demoApiFallbackPlugin() {
  return {
    name: "sommelier-demo-api-fallback",
    configurePreviewServer(server) {
      server.middlewares.use((req, res, next) => {
        if (req.url === "/api/state" || req.url?.startsWith("/api/state?")) {
          res.statusCode = 200;
          res.setHeader("Content-Type", "application/json");
          res.setHeader("Cache-Control", "no-store");
          res.end(JSON.stringify({ mode: "demo" }));
          return;
        }
        next();
      });
    },
  };
}

export default defineConfig({
  build: {
    outDir: "../bus/ui",
    emptyOutDir: true,
  },
  optimizeDeps: {
    include: ["react", "react-dom/client"],
  },
  server: {
    host: "127.0.0.1",
    port: 5173,
    proxy: {
      "/api": "http://127.0.0.1:7300",
    },
  },
  preview: {
    host: "127.0.0.1",
    port: 4174,
  },
  plugins: [react(), demoApiFallbackPlugin()],
});
