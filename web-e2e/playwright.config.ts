import { defineConfig, devices } from "@playwright/test";

// The suite drives the REAL stack: `make start` from the repository root
// boots the chain, the oracle, the gateway (with the simulation switch on)
// and the web app on port 3000. Nothing here is mocked. `E2E_BASE_URL` and
// `E2E_API` point the same suite at a stack somebody else started.
export const BASE_URL = process.env.E2E_BASE_URL ?? "http://127.0.0.1:3000";
export const API = process.env.E2E_API ?? "http://127.0.0.1:8081/api/v1";

export default defineConfig({
  testDir: "./tests",
  timeout: 90_000,
  expect: { timeout: 20_000 },
  fullyParallel: false,
  retries: 0,
  workers: 1,
  reporter: [["list"], ["html", { open: "never", outputFolder: "report" }]],
  outputDir: "test-results",
  use: {
    baseURL: BASE_URL,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    viewport: { width: 1440, height: 960 },
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
