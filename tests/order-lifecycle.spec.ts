/**
 * The whole lifecycle, through a real browser, against a running deployment.
 *
 * One spec on purpose. The Rust suite already covers the rules exhaustively and
 * far faster; what it cannot cover is the half of this application that only
 * exists in a browser — that the server-rendered HTML hydrates, that a form
 * submission reaches a server function and comes back, that the page updates
 * without a reload, and that a session issued by a separate auth service is
 * accepted here. A second Playwright spec asserting business rules would be a
 * slow copy of tests that already pass.
 *
 * Black box throughout: no database access, no test hooks, nothing this
 * application would not do for a person with a browser.
 *
 *   cd tests
 *   npm install
 *   npx playwright install chromium
 *   BASE_URL=http://localhost:5174 npx playwright test
 *
 * BASE_URL must be the origin Better Auth trusts — `localhost`, not
 * `127.0.0.1`. They are different origins to both the browser and the auth
 * service, and sign-out from an untrusted one is refused with 403.
 *
 * Needs a running application, its PostgreSQL, and the shared Better Auth
 * service reachable from it. Every run signs up a brand new account, so runs do
 * not interfere with each other and no cleanup step is required.
 */

import { expect, Page, test } from '@playwright/test';

interface Account {
  name: string;
  email: string;
  password: string;
}

/** A fresh account per run, so two runs never share an order list. */
function newAccount(): Account {
  const stamp = `${Date.now()}-${Math.floor(Math.random() * 100_000)}`;
  return {
    name: `Test Owner ${stamp}`,
    email: `e2e-${stamp}@example.test`,
    // Over Better Auth's minimum, and not a credential to anything real.
    password: `e2e-password-${stamp}`,
  };
}

/**
 * Signs up and waits for the header to show the new account.
 *
 * The sign-in and sign-up forms sit side by side and both have Email and
 * Password fields, so the card is scoped by its heading first. Matching on the
 * label alone would fill whichever form Playwright happened to find first.
 */
async function signUp(page: Page, account: Account): Promise<void> {
  await page.goto('/auth');

  const card = page.locator('article').filter({ hasText: 'I am new here' });
  await card.getByLabel('Name').fill(account.name);
  await card.getByLabel('Email').fill(account.email);
  await card.getByLabel('Password').fill(account.password);
  await card.getByRole('button', { name: 'Create account' }).click();

  // Signing up signs in, and the header is the proof: it renders the caller's
  // email only once the session resolves.
  await expect(page.getByText(account.email)).toBeVisible();
}

/**
 * Waits for the WASM bundle to load and take over the page.
 *
 * This is not politeness, it is correctness. Leptos hydration re-binds every
 * input to its signal, so a value typed *before* the bundle finishes is wiped
 * the moment it does — the form looks filled, then silently empties, and the
 * submit that follows fails on validation for fields the test just filled.
 *
 * The line total is the signal that hydration is done. It is computed in the
 * browser by the same Rust code the server uses, so it stays "—" until the
 * bundle is live; typing a price and seeing the total appear proves the page is
 * interactive. The fill is repeated on each poll because an early one is
 * exactly what gets discarded.
 */
async function waitForHydration(page: Page): Promise<void> {
  const price = page.getByLabel('Item 1 unit price');
  const row = page.locator('tbody tr').first();

  await expect
    .poll(
      async () => {
        await price.fill('1.00');
        return row.innerText();
      },
      { timeout: 90_000, intervals: [250, 500, 1000] },
    )
    .toContain('$1.00');
}

/** Fills the create form and returns once the new order's own page is open. */
async function createOrder(
  page: Page,
  customer: string,
  dueDate: string,
  unitPrice: string,
): Promise<void> {
  await page.goto('/orders/new');
  await waitForHydration(page);

  await page.getByLabel('Customer').fill(customer);
  await page.getByLabel('Due date').fill(dueDate);
  // The line-item inputs are in a table with column headers rather than
  // per-row labels, so each carries its own aria-label.
  await page.getByLabel('Item 1 description').fill('Consulting');
  await page.getByLabel('Item 1 quantity').fill('1');
  await page.getByLabel('Item 1 unit price').fill(unitPrice);

  await page.getByRole('button', { name: 'Create order' }).click();

  // The detail page's heading is the customer name.
  await expect(page.getByRole('heading', { name: customer, level: 1 })).toBeVisible();
}

/**
 * Records a payment and waits for the button to settle.
 *
 * The amount is re-filled until it sticks, for the same reason
 * [`waitForHydration`] exists: the payment form is on a freshly rendered page
 * whose bundle may not have taken over yet.
 */
async function recordPayment(page: Page, amount: string): Promise<void> {
  const field = page.getByLabel('Amount');

  await expect
    .poll(
      async () => {
        await field.fill(amount);
        return field.inputValue();
      },
      { timeout: 90_000, intervals: [250, 500, 1000] },
    )
    .toBe(amount);

  await page.getByRole('button', { name: 'Record payment' }).click();
}

test('an order can be created, partly paid, settled, and never overpaid', async ({ page }) => {
  const account = newAccount();
  await signUp(page, account);

  // A new account sees its own empty dashboard, not everybody's orders.
  await expect(page.getByText('No orders yet.')).toBeVisible();

  await createOrder(page, 'Playwright Ltd', '2027-12-31', '1000.00');
  const orderUrl = page.url();

  // Nothing paid yet, so the whole total is outstanding and the record is
  // still the caller's to change.
  await expect(page.getByText('Pending')).toBeVisible();
  // A link with role="button": it navigates, but it is announced as an action,
  // so that is how it has to be asked for here too.
  await expect(page.getByRole('button', { name: 'Edit' })).toBeVisible();

  // --- A part payment ------------------------------------------------------
  await recordPayment(page, '400.00');

  // No reload: the badge, the totals, and the history all update in place.
  await expect(page.getByText('Partially paid')).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Payments' })).toBeVisible();
  expect(page.url()).toBe(orderUrl);

  // Money has moved, so the order is no longer editable or deletable.
  await expect(page.getByRole('button', { name: 'Edit' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Delete' })).toHaveCount(0);

  // --- Overpayment is refused ---------------------------------------------
  await recordPayment(page, '700.00');

  // Refused with the maximum that could be paid, not a generic failure.
  await expect(page.getByText('The most you can pay is $600.00.')).toBeVisible();
  // And nothing moved.
  await expect(page.getByText('Partially paid')).toBeVisible();

  // --- Settling it ---------------------------------------------------------
  await recordPayment(page, '600.00');

  await expect(page.getByText('This order is paid in full.')).toBeVisible();

  // --- The dashboard agrees ------------------------------------------------
  await page.goto('/');
  await expect(page.getByRole('link', { name: 'Playwright Ltd' })).toBeVisible();

  // The filter is the URL, and it is shareable: these are fresh navigations, so
  // what is asserted is what the *server* rendered, before any hydration.
  await page.goto('/?status=paid');
  await expect(page.getByRole('link', { name: 'Playwright Ltd' })).toBeVisible();

  await page.goto('/?status=overdue');
  await expect(page.getByText('No overdue orders.')).toBeVisible();

  // --- Another account cannot see it --------------------------------------
  await page.getByRole('button', { name: 'Sign out' }).click();
  await expect(page.getByRole('heading', { name: 'Sign in', level: 1 })).toBeVisible();

  await signUp(page, newAccount());
  await expect(page.getByText('No orders yet.')).toBeVisible();

  // Asked for by its exact address, which is the check that matters: a list can
  // be filtered in the browser, but this cannot be.
  await page.goto(orderUrl);
  await expect(page.getByText('That order does not exist, or it is not yours.')).toBeVisible();
});
