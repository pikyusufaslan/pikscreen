import { defineConfig } from "vite";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(async () => ({

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    // 3. transform main.ts before the webview asks for anything.
    //    main.ts turns the wallpapers into asset modules, and the webview
    //    requests those modules before main.ts itself.  Vite only resolves
    //    `x.jpg?import&url` once the importer is in its module graph; asked
    //    cold it falls through to the static handler and answers image/jpeg.
    //    The module graph then aborts with "not a valid JavaScript MIME type",
    //    main.ts never runs, and the bare HTML is left on screen.
    warmup: { clientFiles: ["./src/main.ts"] },
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 4. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
