## Setup

```bash
obscura serve --port 9222
npm install playwright
```

## Connect

```js
const { chromium } = require('playwright');

const browser = await chromium.connectOverCDP('ws://127.0.0.1:9222');
const context = browser.contexts()[0] || await browser.newContext();
const page = await context.newPage();
```

Use `connectOverCDP`, not `connect`. Playwright's `connect` speaks Playwright's own protocol.

## Navigate

```js
await page.goto('https://example.com');
await page.goto('https://example.com', { waitUntil: 'load' });
await page.goto('https://example.com', { waitUntil: 'networkidle' });
```

Default is `domcontentloaded`. Other values: `load`, `networkidle`.

## Evaluate

```js
const title = await page.evaluate(() => document.title);

const items = await page.$$eval('.item', els => els.map(el => ({
  text: el.textContent,
  href: el.querySelector('a')?.href,
})));
```

## Interact

```js
await page.click('#login-button');
await page.fill('#username', 'alice');
await page.fill('#password', 'secret');

await page.waitForSelector('#dashboard');
await page.waitForFunction(() => window.appReady === true);
```

## Locators

```js
await page.locator('button.submit').click();
await page.getByRole('button', { name: 'Submit' }).click();
await page.getByLabel('Email').fill('alice@example.com');
```

## Cookies

```js
await context.addCookies([{
  name: 'session',
  value: 'abc123',
  domain: 'example.com',
  path: '/',
}]);

const cookies = await context.cookies();
```

## Intercept requests

```js
await page.route('**/*', route => {
  if (route.request().resourceType() === 'image') {
    route.abort();
  } else {
    route.continue();
  }
});
```

## Multiple pages

```js
const page1 = await context.newPage();
const page2 = await context.newPage();

await Promise.all([
  page1.goto('https://a.example.com'),
  page2.goto('https://b.example.com'),
]);
```

Pages share one V8 isolate. CPU-bound JS on one page blocks the others.

## Disconnect

```js
await browser.close();  // closes the CDP connection, leaves obscura serve running
```

## Not supported

- `page.screenshot()`, `page.pdf()`: no pixel rendering.
- `page.video()`, tracing artifacts that need a real browser.
- `BrowserContext` storage state save/restore: use `--storage-dir` on `obscura serve` instead, see [Persist cookies and storage](Persist-cookies-and-storage.md).
- Service workers.
