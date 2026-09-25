/**
 * Drives the PHP-free dashboard in a real browser: login redirect, sign-in, dashboard render,
 * WebSocket, and the archive page. Reports console errors and failed network requests.
 */
const { chromium } = require("playwright");

const BASE = process.env.EAS_BASE || "http://127.0.0.1:8080";
const USER = process.env.EAS_USER || "probe";
const PASS = process.env.EAS_PASS || "probe-pass";
const SHOT_DIR = process.argv[2] || process.env.TEMP || ".";

function line(label, value) {
    console.log(`  ${label.padEnd(44)} ${value}`);
}

(async () => {
    // The globally installed playwright expects a different browser build number than the one
    // present, so point it at the Chromium that is actually on disk.
    const executablePath = process.env.EAS_CHROME || undefined;
    const browser = await chromium.launch(executablePath ? { executablePath } : {});
    const context = await browser.newContext();
    const page = await context.newPage();

    const consoleErrors = [];
    const failedRequests = [];
    // Responses a check deliberately provokes, so the diagnostics at the end stay meaningful.
    // Chromium also logs a failed request as a console error, so each one needs an allowance on
    // both lists; they are matched at report time because the two events have no fixed order.
    const expectedFailures = [];
    const expectedConsoleErrors = [];
    page.on("console", (msg) => {
        if (msg.type() === "error") consoleErrors.push(msg.text());
    });
    page.on("pageerror", (err) => consoleErrors.push(`pageerror: ${err.message}`));
    page.on("response", (res) => {
        if (res.status() < 400) return;
        const entry = `${res.status()} ${res.url()}`;
        const expected = expectedFailures.indexOf(entry);
        if (expected !== -1) {
            expectedFailures.splice(expected, 1);
            return;
        }
        failedRequests.push(entry);
    });

    let failures = 0;
    const check = (label, ok, detail) => {
        line(label, ok ? "PASS" : `FAIL ${detail || ""}`);
        if (!ok) failures++;
    };

    console.log("=== signed out ===");
    await page.goto(`${BASE}/`, { waitUntil: "networkidle" });
    check("GET / lands on the login page", page.url().includes("/login.html"), page.url());
    check("login form is present", (await page.locator("#loginForm").count()) === 1);

    console.log("=== rejected sign in ===");
    expectedFailures.push(`401 ${BASE}/api/login`);
    expectedConsoleErrors.push(/failed to load resource.*\b401\b/i);
    await page.fill('input[name="username"]', USER);
    await page.fill('input[name="password"]', "definitely-not-the-password");
    await page.click('button[type="submit"]');
    await page.locator("#loginError").waitFor({ state: "visible", timeout: 10000 });

    const errorText = (await page.locator("#loginError").textContent()) || "";
    check("the rejection is explained", /invalid username or password/i.test(errorText),
        `got "${errorText.trim()}"`);

    // The regression this guards: #loginError used to live outside the 100vh .container, which put
    // it exactly one viewport below the fold every time.
    const errorBox = await page.locator("#loginError").boundingBox();
    const formBox = await page.locator("#loginForm").boundingBox();
    const viewport = page.viewportSize();
    check("the error is inside the viewport",
        !!errorBox && errorBox.y >= 0 && errorBox.y + errorBox.height <= viewport.height,
        errorBox ? `y=${Math.round(errorBox.y)} h=${Math.round(errorBox.height)} viewport=${viewport.height}` : "no box");
    check("the error sits above the login form",
        !!errorBox && !!formBox && errorBox.y + errorBox.height <= formBox.y + 1,
        errorBox && formBox ? `error ends at ${Math.round(errorBox.y + errorBox.height)}, form starts at ${Math.round(formBox.y)}` : "no box");
    line("#loginError", `"${errorText.trim()}"`);
    // Not fullPage: the point is what fits on the first screen.
    await page.screenshot({ path: `${SHOT_DIR}/login-error.png` });

    console.log("=== sign in ===");
    await page.fill('input[name="username"]', USER);
    await page.fill('input[name="password"]', PASS);
    await Promise.all([
        page.waitForURL((url) => !url.pathname.includes("login"), { timeout: 15000 }),
        page.click('button[type="submit"]'),
    ]);
    check("redirected off the login page", !page.url().includes("login.html"), page.url());

    const cookies = await context.cookies();
    const session = cookies.find((c) => c.name === "eas_session");
    check("session cookie set", !!session);
    check("session cookie is HttpOnly", !!session && session.httpOnly === true);

    console.log("=== dashboard ===");
    await page.waitForLoadState("networkidle");
    await page.waitForTimeout(2500); // let bootstrap.js load the scripts and the socket settle

    const version = (await page.locator("#currentVersion").textContent().catch(() => "")) || "";
    check("version rendered from the API", /^\d+\.\d+\.\d+$/.test(version.trim()), `got "${version}"`);

    // index.js signals a live socket with the text "Live updates" and the class "connected".
    const wsStatus = (await page.locator("#wsStatus").textContent().catch(() => "")) || "";
    const wsClass = (await page.locator("#wsStatus").getAttribute("class").catch(() => "")) || "";
    check("websocket connected", wsClass.includes("connected") && !wsClass.includes("disconnected"),
        `text="${wsStatus}" class="${wsClass}"`);
    line("#wsStatus", `"${wsStatus}" (${wsClass})`);

    const globals = await page.evaluate(() => ({
        apiBase: window.API_BASE,
        version: window.APP_VERSION,
        maxLogs: window.MONITORING_MAX_LOGS,
        hasApiFetch: typeof window.apiFetch === "function",
        streamCount: document.getElementById("streamCount")?.textContent || "",
    }));
    check("bootstrap populated the globals", !!globals.apiBase && !!globals.version && globals.hasApiFetch,
        JSON.stringify(globals));
    line("window.MONITORING_MAX_LOGS", globals.maxLogs);
    line("#streamCount", globals.streamCount);

    await page.screenshot({ path: `${SHOT_DIR}/dashboard.png`, fullPage: true });

    console.log("=== archive page ===");
    await page.goto(`${BASE}/archive.html`, { waitUntil: "networkidle" });
    await page.waitForTimeout(1500);
    const cards = await page.locator("#oldAlertList .alert-card").count();
    check("archive rendered alert cards", cards > 0, `count=${cards}`);
    const archiveText = await page.locator("#oldAlertList").innerText();
    check("archive shows the seeded TOR alert", /tornado/i.test(archiveText), archiveText.slice(0, 80));
    await page.screenshot({ path: `${SHOT_DIR}/archive.png`, fullPage: true });

    console.log("=== config page ===");
    await page.goto(`${BASE}/config.html`, { waitUntil: "networkidle" });
    await page.waitForTimeout(1200);
    const editorText = await page.locator("#configEditor").inputValue();
    check("config editor loaded config.json", editorText.trim().startsWith("{"), editorText.slice(0, 40));
    const formFields = await page.locator(".cfg-field").count();
    check("config form built from the schema", formFields > 40, `fields=${formFields}`);
    await page.click("#validateButton");
    await page.waitForTimeout(1200);
    const validateMsg = await page.locator("#configStatus").innerText();
    check("validate reports the config is valid", /valid/i.test(validateMsg), validateMsg.slice(0, 90));
    await page.screenshot({ path: `${SHOT_DIR}/config.png`, fullPage: true });

    console.log("=== logout ===");
    await page.goto(`${BASE}/`, { waitUntil: "networkidle" });
    await page.waitForTimeout(800);
    if ((await page.locator("#logoutButton").count()) > 0) {
        await Promise.all([
            page.waitForURL((url) => url.pathname.includes("login"), { timeout: 10000 }),
            page.click("#logoutButton"),
        ]);
        check("logout returns to the login page", page.url().includes("login.html"), page.url());
        const after = await context.cookies();
        check("session cookie cleared", !after.find((c) => c.name === "eas_session" && c.value));
    } else {
        check("logout button present", false, "not found");
    }

    console.log("=== diagnostics ===");
    const unexpectedConsoleErrors = consoleErrors.filter((text) => {
        const allowed = expectedConsoleErrors.findIndex((pattern) => pattern.test(text));
        if (allowed === -1) return true;
        expectedConsoleErrors.splice(allowed, 1);
        return false;
    });

    check("no unexpected console errors", unexpectedConsoleErrors.length === 0,
        `${unexpectedConsoleErrors.length} of ${consoleErrors.length} logged`);
    unexpectedConsoleErrors.slice(0, 8).forEach((e) => console.log(`      ${e.slice(0, 160)}`));
    check("no unexpected failed requests", failedRequests.length === 0,
        `${failedRequests.length} at 400 or above`);
    failedRequests.slice(0, 8).forEach((r) => console.log(`      ${r.slice(0, 160)}`));

    await browser.close();

    console.log("");
    console.log(failures === 0 ? "ALL CHECKS PASSED" : `${failures} CHECK(S) FAILED`);
    process.exit(failures === 0 ? 0 : 1);
})().catch((err) => {
    console.error("harness error:", err);
    process.exit(2);
});
