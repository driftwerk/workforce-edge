import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The chat SPA that runs INSIDE the sandboxed iframe. Emitted to dist/ as
// index.html + hashed assets; served by the edge under /widget/ via rust-embed.
export default defineConfig({
  plugins: [react()],
  // Vitest stubs CSS imports to "" by default; the lane palette is declared in
  // styles.css and duplicated as literal hex for the standalone diagram export,
  // and a `?raw` import of the stylesheet is what lets a test hold the two to
  // each other.
  test: { css: true },
  base: "/widget/",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // Single JS + CSS chunk keeps the embed asset list small and predictable.
    rollupOptions: {
      output: {
        entryFileNames: "assets/app-[hash].js",
        chunkFileNames: "assets/[name]-[hash].js",
        assetFileNames: "assets/[name]-[hash][extname]",
      },
    },
  },
});
