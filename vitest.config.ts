import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

export default defineConfig({
  resolve: {
    alias: {
      "@novacode/ui": fileURLToPath(new URL("./packages/ui/src/index.ts", import.meta.url))
    }
  },
  test: {
    environment: "jsdom",
    include: ["packages/**/*.test.ts", "apps/**/*.test.ts", "apps/**/*.test.tsx"],
    setupFiles: ["./vitest.setup.ts"]
  }
});
