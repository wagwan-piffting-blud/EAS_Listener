/**
 * Drives the notification editor on the configuration page: building a URL from Apprise's service
 * list, a test message that really goes out, pasting a URL, the unsaved-change count, saving, and
 * removing. The test messages go to a capture server this script runs itself, through json://.
 */
const http = require("http");
const { chromium } = require("playwright");

const BASE = process.env.EAS_BASE || "http://127.0.0.1:8080";
const USER = process.env.EAS_USER || "probe";
const PASS = process.env.EAS_PASS || "probe-pass";
const SHOT_DIR = process.argv[2] || process.env.TEMP || ".";

function line(label, value) {
    console.log(`  ${label.padEnd(52)} ${value}`);
}

function captureServer() {
    const received = [];
    const server = http.createServer((req, res) => {
        let body = "";
        req.on("data", (chunk) => (body += chunk));
        req.on("end", () => {
            received.push({ path: req.url, body });
            res.writeHead(200);
            res.end();
        });
    });
    return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve({ server, received, port: server.address().port })));
}

(async () => {
    const capture = await captureServer();
    const executablePath = process.env.EAS_CHROME || undefined;
    const browser = await chromium.launch(executablePath ? { executablePath } : {});
    const page = await browser.newPage();

    const consoleErrors = [];
    page.on("console", (msg) => {
        if (msg.type() === "error") consoleErrors.push(msg.text());
    });
    page.on("pageerror", (err) => consoleErrors.push(`pageerror: ${err.message}`));

    let failures = 0;
    const check = (label, ok, detail) => {
        line(label, ok ? "PASS" : `FAIL ${detail || ""}`);
        if (!ok) failures++;
    };
    const api = (path, options) => page.evaluate(async ([path, options]) => {
        const response = await fetch(path, Object.assign({ credentials: "same-origin" }, options || {}));
        return { status: response.status, body: await response.json().catch(() => null) };
    }, [path, options]);

    try {
        console.log("=== sign in ===");
        await page.goto(`${BASE}/login.html`, { waitUntil: "networkidle" });
        await page.fill('input[name="username"]', USER);
        await page.fill('input[name="password"]', PASS);
        await Promise.all([page.waitForURL((url) => !url.pathname.endsWith("/login.html")), page.click('button[type="submit"]')]);
        // Leaving the dashboard mid-load cancels its fetches, which it logs as errors.
        await page.waitForLoadState("networkidle");

        const before = await api("/api/notifications");
        check("GET /api/notifications answers", before.status === 200 && Array.isArray(before.body.urls), JSON.stringify(before));
        const startCount = before.body.urls.length;

        console.log("=== the section ===");
        await page.goto(`${BASE}/config.html`, { waitUntil: "networkidle" });
        const section = page.locator("#cfg-group-notifications");
        await section.waitFor();
        check("Notifications section renders", await section.isVisible());
        check("nav links to it", (await page.locator('.cfg-nav a[href="#cfg-group-notifications"]').count()) === 1);
        check("it sits after the relay section",
            await page.evaluate(() => document.querySelector("#cfg-group-relay + #cfg-group-notifications") !== null));

        console.log("=== building a json:// URL ===");
        await section.getByRole("button", { name: "Add a service" }).click();
        const search = section.locator('input[type="search"]');
        await search.waitFor();
        const serviceCount = await section.locator(".ntf-catalog-item").count();
        check("the service list comes from Apprise", serviceCount > 100, `${serviceCount} services`);
        await search.fill("json");
        await section.locator(".ntf-catalog-item", { hasText: "JSON" }).first().click();
        await section.locator("#ntf-host").fill("127.0.0.1");
        await section.locator("#ntf-port").fill(String(capture.port));
        const preview = (await section.locator(".ntf-preview").textContent()).trim();
        check("preview shows the built URL", /^jsons?:\/\/127\.0\.0\.1:\d+/.test(preview), preview);

        // The builder may pick jsons:// by default; the capture server is plain HTTP.
        const scheme = section.locator("#ntf-scheme");
        if (await scheme.count()) await scheme.selectOption("json");
        const built = (await section.locator(".ntf-preview").textContent()).trim();
        check("plain json:// can be chosen", built.startsWith("json://"), built);

        const builder = section.locator(".ntf-builder");
        await builder.getByRole("button", { name: "Send a test" }).click();
        const result = section.locator(".ntf-builder .ntf-result.is-ok, .ntf-builder .ntf-result.is-bad").last();
        await result.waitFor({ timeout: 60000 });
        const resultText = (await result.textContent()).trim();
        check("the test message is reported sent", (await result.getAttribute("class")).includes("is-ok"), resultText);
        const delivered = capture.received.find((each) => each.body.includes("EAS Listener test notification"));
        check("and it really arrived", Boolean(delivered), `${capture.received.length} requests`);

        await builder.getByRole("button", { name: "Add", exact: true }).click();
        check("the URL is added to the list", (await section.locator(".ntf-row").count()) === startCount + 1);
        check("unsaved change is counted", /unsaved change/.test(await page.locator("#cfgDirty").textContent()));

        console.log("=== a URL that is not one ===");
        await section.getByRole("button", { name: "Paste a URL" }).click();
        await section.locator('.ntf-builder input[aria-label="Apprise URL"]').fill("not a url");
        await section.locator(".ntf-builder").getByRole("button", { name: "Add", exact: true }).click();
        check("is refused before it reaches the list",
            (await section.locator(".ntf-builder .ntf-result.is-bad").count()) === 1 && (await section.locator(".ntf-row").count()) === startCount + 1);
        await section.locator(".ntf-builder").getByRole("button", { name: "Cancel" }).click();

        console.log("=== a service with a checked field ===");
        await section.getByRole("button", { name: "Add a service" }).click();
        await section.locator('input[type="search"]').fill("telegram");
        await section.locator(".ntf-catalog-item", { hasText: "Telegram" }).first().click();
        await section.locator("#ntf-bot_token").fill("not-a-token");
        const complaint = (await section.locator(".ntf-builder .ntf-result").first().textContent()).trim();
        check("a malformed bot token is flagged", /does not look right/.test(complaint), complaint);
        await section.locator("#ntf-bot_token").fill("123456:ABCdef_ghi");
        await section.locator("#ntf-targets").fill("-100200300, @alerts");
        const telegram = (await section.locator(".ntf-preview").textContent()).trim();
        check("secrets are masked in the preview", telegram.startsWith("tgram://****/") && !telegram.includes("ABCdef"), telegram);
        await section.locator(".ntf-builder .cfg-toggle input").check();
        const revealed = (await section.locator(".ntf-preview").textContent()).trim();
        check("and shown on request, encoded",
            revealed === "tgram://123456%3AABCdef_ghi/-100200300/%40alerts", revealed);
        await section.locator(".ntf-builder").getByRole("button", { name: "Cancel" }).click();
        await page.screenshot({ path: `${SHOT_DIR}/notifications.png`, fullPage: false });

        console.log("=== save ===");
        await page.click("#saveButton");
        await page.locator("#configStatus.ok, #configStatus.warn, #configStatus.bad").waitFor();
        const saveText = await page.locator("#configStatus").textContent();
        check("the save reports the notification list", /notification list/.test(saveText), saveText);
        const after = await api("/api/notifications");
        check("the file holds the new URL",
            after.body.urls.length === startCount + 1 && after.body.urls.some((url) => url.startsWith(`json://127.0.0.1:${capture.port}`)),
            JSON.stringify(after.body.urls));
        check("nothing is left unsaved", /No unsaved changes/.test(await page.locator("#cfgDirty").textContent()));

        console.log("=== a saved row ===");
        const row = section.locator(".ntf-row").last();
        check("shows the service and hides the URL",
            (await row.locator(".ntf-service").textContent()) === "JSON" && (await row.locator(".ntf-url").textContent()).startsWith("json://****"));
        capture.received.length = 0;
        await row.getByRole("button", { name: "Send a test" }).click();
        await row.locator(".ntf-result.is-ok, .ntf-result.is-bad").waitFor({ timeout: 60000 });
        check("can be tested on its own", capture.received.length === 1, `${capture.received.length} requests`);

        await row.getByRole("button", { name: /^Remove/ }).click();
        await page.click("#saveButton");
        await page.waitForFunction(() => /No unsaved changes/.test(document.getElementById("cfgDirty").textContent));
        const restored = await api("/api/notifications");
        check("removing and saving puts the list back", restored.body.urls.length === startCount, JSON.stringify(restored.body.urls));

        console.log("=== diagnostics ===");
        check("no console errors", consoleErrors.length === 0, consoleErrors.join(" | "));
    } catch (err) {
        console.error(err);
        failures++;
    } finally {
        await browser.close();
        capture.server.close();
    }

    console.log(failures === 0 ? "ALL PASS" : `${failures} FAILED`);
    process.exit(failures === 0 ? 0 : 1);
})();
