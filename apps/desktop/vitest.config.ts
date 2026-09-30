import { defineConfig } from "vitest/config";

// Unit tests for the pure TS modules (format / session helpers / store
// reducers). Node environment on purpose: nothing here touches the DOM, and
// the Tauri bridge is mocked per test file.
export default defineConfig({
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
