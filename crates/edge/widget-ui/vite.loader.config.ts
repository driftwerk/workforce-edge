import { defineConfig } from "vite";

// The loader: a tiny dependency-free IIFE served by the edge at /embed/{agent}
// and included by the host page. It mounts the sandboxed iframe and bridges
// token + theme (+ later, host-page actions) over postMessage. Built separately
// in library mode so it emits one plain script with no module wrapper.
export default defineConfig({
  build: {
    outDir: "dist",
    emptyOutDir: false, // keep the app build already in dist/
    lib: {
      entry: "src/loader/loader.ts",
      formats: ["iife"],
      name: "WorkforceEdgeWidget",
      fileName: () => "loader.js",
    },
    rollupOptions: {
      output: { entryFileNames: "loader.js" },
    },
    minify: "esbuild",
  },
});
