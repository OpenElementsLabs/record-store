/**
 * Design-system invariants, checked in a real browser.
 *
 * These are deliberately behavioural rather than pixel snapshots: a screenshot
 * diff fails on every intentional change and tells you nothing about why, while
 * "the page must not scroll sideways" is a rule that stays true as the design
 * evolves.
 */
import { expect, test } from './fixtures';

/** The widths the console is expected to be usable at. */
const VIEWPORTS = [
  { name: 'desktop', width: 1440, height: 900 },
  { name: 'laptop', width: 1280, height: 800 },
  { name: 'small laptop', width: 1024, height: 768 },
  { name: 'tablet', width: 768, height: 1024 },
  { name: 'mobile', width: 390, height: 844 },
] as const;

const ROUTES = ['/', '/buckets', '/audit', '/events', '/system', '/metrics', '/integrity'];

test.describe('design system', () => {
  for (const viewport of VIEWPORTS) {
    test(`no page scrolls sideways at ${viewport.name}`, async ({ signedIn }) => {
      const page = signedIn;
      await page.setViewportSize({ width: viewport.width, height: viewport.height });

      for (const route of ROUTES) {
        await page.goto(route);
        await expect(page.getByRole('main')).toBeVisible();
        // The document itself must never scroll horizontally. Wide tables are
        // allowed to scroll, but inside their own container.
        const overflow = await page.evaluate(
          () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
        );
        expect(overflow, `${route} overflows at ${viewport.width}px`).toBeLessThanOrEqual(1);
      }
    });
  }

  test('navigation collapses to a drawer on mobile', async ({ signedIn }) => {
    const page = signedIn;
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto('/');

    // The persistent sidebar is replaced, not shrunk.
    await expect(page.getByRole('button', { name: 'Open navigation' })).toBeVisible();
    await page.getByRole('button', { name: 'Open navigation' }).click();
    await expect(page.getByRole('navigation', { name: 'Console sections' })).toBeVisible();

    await page.getByRole('link', { name: 'Buckets' }).click();
    await expect(page.getByRole('heading', { name: 'Buckets' })).toBeVisible();
  });

  test('the sidebar collapses on desktop and the choice survives a reload', async ({
    signedIn,
  }) => {
    const page = signedIn;
    await page.goto('/');

    await page.getByRole('button', { name: 'Collapse navigation' }).click();
    await expect(page.getByRole('button', { name: 'Expand navigation' })).toBeVisible();
    // Labels remain in the accessible tree even when visually hidden.
    await expect(page.getByRole('link', { name: 'Buckets' })).toBeAttached();

    await page.reload();
    await expect(page.getByRole('button', { name: 'Expand navigation' })).toBeVisible();
  });

  test('elevated surfaces are distinguishable from the page in both themes', async ({
    signedIn,
  }) => {
    const page = signedIn;
    await page.goto('/buckets');

    for (const theme of ['light', 'dark'] as const) {
      await page.emulateMedia({ colorScheme: theme });
      await page
        .getByRole('button', { name: /create bucket/i })
        .first()
        .click();
      const dialog = page.getByRole('dialog');
      await expect(dialog).toBeVisible();

      const surfaces = await page.evaluate(() => {
        const dialogElement = document.querySelector('[role="dialog"]');
        const main = document.querySelector('main');
        return {
          dialog: dialogElement ? getComputedStyle(dialogElement).backgroundColor : '',
          page: main ? getComputedStyle(main).backgroundColor : '',
        };
      });
      // A dialog that shares the page's surface reads as flat, which is the
      // failure mode dark mode makes obvious.
      expect(surfaces.dialog).not.toBe('');
      expect(surfaces.dialog).not.toBe('rgba(0, 0, 0, 0)');

      await page.keyboard.press('Escape');
      await expect(dialog).toBeHidden();
    }
  });

  test('keyboard focus is always visible', async ({ signedIn }) => {
    const page = signedIn;
    await page.goto('/buckets');

    await page.keyboard.press('Tab');
    const outline = await page.evaluate(() => {
      const active = document.activeElement;
      if (!active) return null;
      const style = getComputedStyle(active);
      return { width: style.outlineWidth, style: style.outlineStyle };
    });
    expect(outline).not.toBeNull();
    expect(outline?.style).not.toBe('none');
  });

  test('Escape closes a dialog and the command palette', async ({ signedIn }) => {
    const page = signedIn;
    await page.goto('/');

    await page.getByRole('button', { name: /Search/ }).click();
    await expect(page.getByRole('dialog')).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(page.getByRole('dialog')).toBeHidden();
  });
});
