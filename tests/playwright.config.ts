import { defineConfig, devices } from '@playwright/test';

/**
 * One browser, one worker, generous timeouts.
 *
 * Chromium alone: this spec tests *this application's* behaviour, not browser
 * compatibility, and running the same flow three times in three engines would
 * triple the wall clock to re-assert the same server responses.
 *
 * The timeouts are not padding. Every assertion here waits on a round trip to a
 * server that is itself waiting on PostgreSQL and on a separate auth service,
 * and the first interaction of a cold page also waits for the WASM bundle to
 * download and hydrate. Playwright's 5-second default fails on a slow network
 * long before it fails on a real defect.
 */
export default defineConfig({
  testDir: '.',
  // Sign-up and sign-out are global to a browser context, so two of these
  // running at once would fight over the session.
  workers: 1,
  timeout: 120_000,
  expect: { timeout: 20_000 },
  reporter: process.env.CI ? 'line' : 'list',
  use: {
    baseURL: process.env.BASE_URL ?? 'http://localhost:5174',
    // Kept only for a failure: a passing run leaves nothing behind.
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
});
