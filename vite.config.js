import { defineConfig } from 'vite';

export default defineConfig({
  server: {
    // Must match build.devUrl in src-tauri/tauri.conf.json. strictPort makes Vite fail loudly if the
    // port is taken, instead of moving to another port while Tauri still loads this one.
    port: 1430,
    strictPort: true,
    watch: {
      ignored: ['**/src-tauri/target/**'],
    },
  },
});
