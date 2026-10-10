import { expect, test } from "@playwright/test";
import { ADMIN_MNEMONIC, USER_MNEMONIC, clickMenu, gotoLive, signIn, watchErrors } from "./helpers";

/**
 * The whole product, in the order a person meets it: land, sign in with a
 * phrase, find the menu and the RPC facts, claim from the faucet, buy, sell,
 * read the dashboard, and then, as the operator, move the simulation switch.
 *
 * Serial on purpose. The buy needs the faucet; the dashboard's transactions
 * need somebody to have traded; the switch test leaves the stack in the
 * position `make start` gave it.
 */
test.describe.configure({ mode: "serial" });

test("the landing page paints the 16-bit skin on a live chain with no errors", async ({ page }) => {
  const errors = watchErrors(page);
  await gotoLive(page, "/");
  await expect(page.locator(".lp-logo img")).toBeVisible();
  await expect(page.getByText("Insert coin")).toBeVisible();
  // The rail reads a real height from the gateway, not the genesis zero a
  // stalled block stream used to report.
  const height = page.locator(".rail .val").first();
  await expect(height).not.toHaveText(/^[—0]$/, { timeout: 30_000 });
  await expect(height).toHaveText(/\d/);
  // The pixel face actually loaded rather than falling back.
  const family = await page.locator(".lp-copy h1").evaluate((el) => getComputedStyle(el).fontFamily);
  expect(family).toMatch(/PressStart2P/);
  expect(errors, errors.join("\n")).toEqual([]);
});

test("a user signs in with a phrase and finds every menu and the RPC facts", async ({ page }) => {
  const handle = await signIn(page, USER_MNEMONIC, "pw_user");
  expect(handle.length).toBeGreaterThan(0);

  await gotoLive(page, "/account");
  const menu = page.locator('[data-testid="menu-panel"]');
  await expect(menu).toBeVisible();
  for (const entry of ["faucet", "buy", "sell", "trade", "portfolio", "leaderboard", "dashboard", "account"]) {
    await expect(menu.locator(`[data-menu="${entry}"]`), `menu has ${entry}`).toBeVisible();
  }
  await expect(menu.locator('[data-menu="admin"]'), "a user is not shown the admin entry").toHaveCount(0);

  const rpc = page.locator('[data-testid="rpc-panel"]');
  await expect(rpc).toBeVisible();
  await expect(rpc.getByText("Gateway API")).toBeVisible();
  await expect(rpc.getByText("Node RPC (Cap'n Proto)")).toBeVisible();
  await expect(rpc.locator(".rpc-line", { hasText: "Node RPC" }).locator("code")).toHaveText(/127\.0\.0\.1:\d+/);
  await expect(rpc.locator(".rpc-line", { hasText: "Gateway WebSocket" }).locator("code")).toHaveText(/^ws:\/\//);
  await expect(rpc.locator(".rpc-line", { hasText: "Chain id" }).locator("code")).toHaveText(/\d+/);
  await expect(rpc.locator(".rpc-line", { hasText: "Simulation switch" }).locator("code")).toHaveText(/SIMULATION/);
});

test("the faucet pays, and the balance lands on chain", async ({ page }) => {
  await signIn(page, USER_MNEMONIC, "pw_user");
  await gotoLive(page, "/account");
  await clickMenu(page, "faucet");
  await expect(page).toHaveURL(/\/faucet/);

  const claim = page.getByRole("button", { name: "Claim grant" });
  await expect(claim).toBeVisible();
  if (await claim.isEnabled()) {
    await claim.click();
    await expect(page.getByText("Grant claimed")).toBeVisible({ timeout: 30_000 });
  } else {
    // Claimed within the hour by an earlier run: the cooldown is on screen
    // and the money is already there.
    await expect(page.getByText(/left|cooldown|next claim/i).first()).toBeVisible();
  }
  // Equity in the top bar is a real number once the grant is on chain.
  await expect(page.locator(".topbar .stat .v")).toHaveText(/[1-9]/, { timeout: 30_000 });
});

test("Buy and Sell from the menu open the ticket on that side and rest orders on the book", async ({ page }) => {
  await signIn(page, USER_MNEMONIC, "pw_user");
  await gotoLive(page, "/account");
  await clickMenu(page, "buy");
  await expect(page).toHaveURL(/\/trade\?side=buy/);
  const ticket = page.locator(".c-entry");
  const side = ticket.locator('.seg-side button[data-side="buy"]');
  await expect(side).toHaveAttribute("aria-pressed", "true");

  // Make sure the market is the spot one the faucet funds.
  await expect(page.locator(".strip .sym")).toBeVisible();
  await ticket.locator("#tk-price").fill("30000");
  await ticket.locator("#tk-qty").fill("0.001");
  const buy = ticket.locator("button.btn-buy.btn-block");
  await expect(buy).toBeEnabled({ timeout: 20_000 });
  await buy.click();
  await expect(page.getByText(/Submitted as|appears in the book|Order placed|resting/i).first()).toBeVisible({ timeout: 30_000 });

  await gotoLive(page, "/account");
  await clickMenu(page, "sell");
  await expect(page).toHaveURL(/\/trade\?side=sell/);
  await expect(ticket.locator('.seg-side button[data-side="sell"]')).toHaveAttribute("aria-pressed", "true");
  await ticket.locator("#tk-price").fill("90000");
  await ticket.locator("#tk-qty").fill("0.001");
  const sell = ticket.locator("button.btn-sell.btn-block");
  await expect(sell).toBeEnabled({ timeout: 20_000 });
  await sell.click();
  await expect(page.getByText(/Submitted as|appears in the book|Order placed|resting/i).first()).toBeVisible({ timeout: 30_000 });
});

test("the dashboard shows the chain, its transactions and the RPC facts", async ({ page }) => {
  await gotoLive(page, "/chain");
  await expect(page.getByRole("heading", { name: "Dashboard" })).toBeVisible();
  await expect(page.locator('[data-testid="rpc-panel"]')).toBeVisible();
  const txs = page.locator('[data-testid="tx-panel"]');
  await expect(txs).toBeVisible();
  // With the switch on, the default buyer and seller keep the feed full;
  // the orders the previous test placed are in there too.
  await expect(txs.locator("tbody tr").first()).toBeVisible({ timeout: 40_000 });
  await expect(txs.locator("tbody tr td:nth-child(2)").first()).toHaveText(/[A-Z][A-Za-z]+/);
  const height = page.locator(".ch-stats .stat .v").first();
  await expect(height).toHaveText(/[1-9]/);
});

test("the operator moves the simulation switch: OFF, MAX, then back ON", async ({ page }) => {
  const handle = await signIn(page, ADMIN_MNEMONIC, "pw_admin");
  expect(handle.length).toBeGreaterThan(0);
  await gotoLive(page, "/account");
  await expect(page.locator('[data-testid="menu-panel"] [data-menu="admin"]')).toBeVisible();
  await clickMenu(page, "admin");
  await expect(page).toHaveURL(/\/admin/);

  const panel = page.locator('[data-testid="sim-panel"]');
  await expect(panel).toBeVisible();
  const badge = panel.locator('[data-testid="sim-mode"]');
  await expect(badge).toBeVisible({ timeout: 30_000 });

  const setMode = async (mode: "off" | "on" | "max", label: RegExp) => {
    await panel.locator(`.mode-option[data-mode="${mode}"]`).click();
    await expect(badge).toHaveText(label, { timeout: 30_000 });
    await expect(panel.locator(`.mode-option[data-mode="${mode}"]`)).toHaveAttribute("aria-checked", "true");
  };
  await setMode("off", /SIMULATION OFF/);
  await setMode("max", /MAX SIMULATION/);
  // Do not linger on max: it is a stress test, and the browser is on the
  // same machine as the node it is stressing.
  await setMode("on", /SIMULATION ON/);

  // The switch position is a public RPC fact.
  await gotoLive(page, "/chain");
  await expect(
    page.locator('[data-testid="rpc-panel"] .rpc-line', { hasText: "Simulation switch" }).locator("code"),
  ).toHaveText(/SIMULATION ON/, { timeout: 30_000 });
});

test("the language switch turns the chrome Korean and remembers it", async ({ page }) => {
  await gotoLive(page, "/chain");
  const nav = page.locator(".topbar .nav");
  await expect(nav.getByRole("link", { name: "Trade" })).toBeVisible();
  await page.locator('[data-testid="lang-switch"] button[data-lang="ko"]').first().click();
  // The switch reloads the page in the chosen language.
  await expect(page.locator("html")).toHaveAttribute("lang", "ko", { timeout: 30_000 });
  await expect(nav.getByRole("link", { name: "거래" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "대시보드" })).toBeVisible();
  await expect(page.locator('[data-testid="rpc-panel"]').getByText("게이트웨이 API")).toBeVisible();
  // A fresh navigation keeps the choice.
  await gotoLive(page, "/account");
  await expect(page.locator("html")).toHaveAttribute("lang", "ko");
  // And English comes back the same way.
  await page.locator('[data-testid="lang-switch"] button[data-lang="en"]').first().click();
  await expect(page.locator("html")).toHaveAttribute("lang", "en", { timeout: 30_000 });
  await expect(nav.getByRole("link", { name: "Trade" })).toBeVisible();
});
