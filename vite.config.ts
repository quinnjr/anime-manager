import { defineConfig } from 'vitest/config';
import { sveltekit } from '@sveltejs/kit/vite';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import adapter from '@sveltejs/adapter-static';
import tailwindcss from '@tailwindcss/vite';

export default defineConfig({
  plugins: [
    tailwindcss(),
    // SvelteKit 3 reads its config here; svelte.config.js is no longer supported.
    sveltekit({
      preprocess: vitePreprocess(),
      adapter: adapter({ fallback: 'index.html' })
    })
  ],
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  test: { include: ['src/**/*.test.ts'] }
});
