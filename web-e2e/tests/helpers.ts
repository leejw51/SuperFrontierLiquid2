import { expect, type Page } from "@playwright/test";
import { API } from "../playwright.config";

/**
 * The sign-in phrases. They come from the environment only (`make web-e2e`
 * passes the ones `make start` used); no phrase is written into this suite.
 */
function phraseFromEnv(name: string): string {
  const v = process.env[name];
  if (!v) throw new Error(`${name} is not set: run the suite through \`make web-e2e\`, which passes it`);
  return v;
}
export const ADMIN_MNEMONIC = phraseFromEnv("ADMIN_MNEMONIC");
export const USER_MNEMONIC = phraseFromEnv("USER_MNEMONIC");

/** Open a route on the LIVE gateway. The selection is remembered by the app. */
export async function gotoLive(page: Page, path: string): Promise<void> {
  const join = path.includes("?") ? "&" : "?";
  await page.goto(`${path}${join}gateway=live&api=${encodeURIComponent(API)}`);
  await page.waitForSelector("#cb-root *", { timeout: 30_000 });
}

/** Collect console errors and page errors for an assertion at the end. */
export function watchErrors(page: Page): string[] {
  const errors: string[] = [];
  page.on("pageerror", (err) => errors.push(String(err)));
  page.on("console", (msg) => {
    if (msg.type() === "error") errors.push(msg.text());
  });
  return errors;
}

/**
 * Sign in through the real landing flow: paste the phrase, request a
 * challenge, sign it in the browser, and — on a first login — pick a handle.
 */
export async function signIn(page: Page, mnemonic: string, handleHint: string): Promise<string> {
  await gotoLive(page, "/");
  await page.getByRole("button", { name: "Sign in" }).first().click();
  const phrase = page.locator("#ph");
  await expect(phrase).toBeVisible();
  await phrase.fill(mnemonic);
  await expect(page.getByText("Checksum valid")).toBeVisible();
  await page.getByRole("button", { name: "Continue" }).click();

  // The signature step. A first login for this address asks for a handle;
  // a returning one does not.
  const signButton = page.getByRole("button", { name: /Sign( again)? and continue/ });
  await expect(signButton).toBeVisible();
  let handle = handleHint;
  const handleField = page.locator("#c-handle");
  const submitOnce = async () => {
    await signButton.click();
  };
  await submitOnce();
  // Either we are in, or the gateway asked for a handle.
  const pill = page.locator(".topbar .pill");
  await Promise.race([
    pill.waitFor({ state: "visible", timeout: 20_000 }),
    handleField.waitFor({ state: "visible", timeout: 20_000 }),
  ]);
  if (await handleField.isVisible()) {
    handle = `${handleHint}_${Date.now().toString(36)}`.slice(0, 20);
    await handleField.fill(handle);
    await signButton.click();
    await expect(pill).toBeVisible({ timeout: 20_000 });
  }
  return (await pill.innerText()).trim();
}

/** Click a menu entry on the account page. */
export async function clickMenu(page: Page, entry: string): Promise<void> {
  await page.locator(`[data-testid="menu-panel"] [data-menu="${entry}"]`).click();
}
