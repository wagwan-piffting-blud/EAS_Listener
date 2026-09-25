# Browser smoke test

`smoke.js` drives the dashboard in a real Chromium via Playwright and checks the things that only
break in a browser: the signed-out redirect, a rejected sign-in explaining itself above the fold,
sign-in, the session cookie, the bootstrap globals, the WebSocket reaching its connected state, the
archive rendering alert cards, and logout clearing the cookie. It also fails on any console error or
any request returning 400 or above — except the one 401 the rejected sign-in provokes on purpose.

It is not part of `cargo test` -- it needs a running instance.

## Running it

```bash
# against a listener already running on 127.0.0.1:8080 with these credentials
EAS_BASE=http://127.0.0.1:8080 EAS_USER=admin2 EAS_PASS=hunter2 node tests/browser/smoke.js
```

Screenshots are written to the directory given as the first argument, or `$TEMP` by default:
`login-error.png`, `dashboard.png`, `archive.png` and `config.png`. `login-error.png` is
deliberately not full-page, because what it is there to show is what fits on the first screen.

## First-run setup

`setup.js` walks first-run setup against a listener started with no `config.json` (or `{}`): the
token gate, each step's checks, that the saved file holds only what was entered, the handover to the
listener, and signing in with the new credentials. It leaves that instance configured, so start a
fresh one for each run.

```bash
# token from the listener's console output or setup-token.txt; EAS_CONFIG_JSON is optional
EAS_BASE=http://127.0.0.1:8080 EAS_SETUP_TOKEN=... EAS_CONFIG_JSON=/path/to/config.json \
    node tests/browser/setup.js
```

It writes `setup-signin.png` and `setup-review.png` to the same screenshot directory.

It also adds a pasted URL at the notifications step and checks it lands in `apprise.yml` beside
`config.json`.

## Notifications

`notifications.js` drives the notification editor on the configuration page: building a `json://`
URL from Apprise's service list, a test message that really goes out (to a capture server the
script runs itself), a URL that is refused, Telegram's field check and secret masking, saving, and
removing. It leaves the same URLs in `apprise.yml` as it found, though saving rewrites the file in
the dashboard's own layout. It needs Apprise where the listener can find it -- `APPRISE_PATH`,
`tools/`, or `PATH`.

```bash
EAS_BASE=http://127.0.0.1:8080 EAS_USER=admin2 EAS_PASS=hunter2 node tests/browser/notifications.js
```

It writes `notifications.png` to the screenshot directory.

## Requirements

Playwright and a Chromium build:

```bash
npm install -g playwright
npx playwright install chromium
```

If Playwright's bundled browser revision does not match what is installed, point the test at the
browser directly:

```bash
EAS_CHROME="/path/to/chrome.exe" node tests/browser/smoke.js
```

On Windows, a globally installed Playwright may also need `NODE_PATH` set to the global
`node_modules` directory for `require("playwright")` to resolve.
