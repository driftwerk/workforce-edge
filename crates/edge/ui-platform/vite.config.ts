import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The platform console, embedded into the edge binary and served at /platform
// on the platform host. Same build shape as ui-admin: a hashed bundle under
// assets/, the shell at index.html.
export default defineConfig({
  plugins: [react()],
  base: "/platform/",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    rollupOptions: {
      output: {
        entryFileNames: "assets/app-[hash].js",
        chunkFileNames: "assets/[name]-[hash].js",
        assetFileNames: "assets/[name]-[hash][extname]",
      },
    },
  },
});
