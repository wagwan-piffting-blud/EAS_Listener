/**
 * Drives first-run setup in a real browser against a listener started with no config.json: the
 * token gate, each step's checks, the saved file holding only what was entered, the handover to
 * the listener, and signing in with the new credentials.
 */
const { chromium } = require("playwright");
const fs = require("fs");
const path = require("path");

const BASE = process.env.EAS_BASE || "http://127.0.0.1:8080";
const TOKEN = process.env.EAS_SETUP_TOKEN;
const CONFIG = process.env.EAS_CONFIG_JSON;
const SHOT_DIR = process.argv[2] || process.env.TEMP || ".";
const USER = "setup-probe";
const PASS = "setup-probe-pass";
// Never sent anything: the run only saves it, and nothing listens on port 9.
const SETUP_URL = "json://127.0.0.1:9/setup-check";

function line(label, value) {
    console.log(`  ${label.padEnd(44)} ${value}`);
}

(async () => {
    if (!TOKEN) {
        console.error("Set EAS_SETUP_TOKEN to the token the listener printed.");
        process.exit(2);
    }

    const executablePath = process.env.EAS_CHROME || undefined;
    const browser = await chromium.launch(executablePath ? { executablePath } : {});
    const page = await (await browser.newContext()).newPage();

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
    const statusText = async () => (await page.locator("#setupStatus").innerText()).trim();
    const next = async () => {
        await page.click("#nextButton");
        await page.waitForTimeout(250);
    };

    console.log("=== token gate ===");
    await page.goto(`${BASE}/`, { waitUntil: "networkidle" });
    check("GET / lands on the setup page", page.url().includes("/setup.html"), page.url());
    await page.fill("#setupToken", "not-the-token");
    await page.click("button[type=submit]");
    await page.waitForTimeout(400);
    check("a wrong token is refused", /not the one/i.test(await statusText()), await statusText());
    await page.fill("#setupToken", TOKEN);
    await page.click("button[type=submit]");
    await page.waitForSelector("#cfg-DASHBOARD_USERNAME");
    check("the right token opens step one", true);
    await page.screenshot({ path: `${SHOT_DIR}/setup-signin.png`, fullPage: true });

    console.log("=== sign-in step ===");
    await next();
    check("an empty step does not advance", await page.locator("#cfg-DASHBOARD_USERNAME").isVisible());
    await page.fill("#cfg-DASHBOARD_USERNAME", "admin");
    await page.fill("#cfg-DASHBOARD_PASSWORD", PASS);
    await page.fill('[aria-label="Password, again"]', PASS);
    await next();
    check("the shipped username is refused", /cannot be admin/i.test(await statusText()), await statusText());
    await page.fill("#cfg-DASHBOARD_USERNAME", USER);
    await page.fill('[aria-label="Password, again"]', "different");
    await next();
    check("a mismatched password is refused", /do not match/i.test(await statusText()), await statusText());
    await page.fill('[aria-label="Password, again"]', PASS);
    await next();

    console.log("=== streams step ===");
    await next();
    check("no stream does not advance", /cannot start without/i.test(await statusText()), await statusText());
    await page.click("text=Use the sample stream");
    await page.fill('[aria-label="Nickname"]', "Sample");
    await next();

    console.log("=== location step ===");
    check("the time zone is suggested from the browser", (await page.inputValue("#cfg-TZ")) !== "");
    await page.fill("#cfg-WATCHED_FIPS", "031055");
    await page.press("#cfg-WATCHED_FIPS", "Enter");
    await page.waitForTimeout(600);
    const chips = await page.locator(".cfg-chip").allInnerTexts();
    check("a typed code becomes a named chip", chips.some((chip) => /Douglas County, NE/.test(chip)), JSON.stringify(chips));
    await next();

    console.log("=== CAP step ===");
    await next();

    console.log("=== notifications step ===");
    const notifications = page.locator("#cfg-group-notifications");
    check("the notifications step shows the editor", await notifications.isVisible());
    await notifications.getByRole("button", { name: "Add a service" }).click();
    await notifications.locator('.ntf-builder input[type="search"], .ntf-builder input[aria-label="Apprise URL"]').first().waitFor();
    const services = await notifications.locator(".ntf-catalog-item").count();
    check("Apprise's service list loads here too", services > 100, `${services} services`);
    await notifications.locator(".ntf-builder").getByRole("button", { name: "Cancel" }).click();
    await notifications.getByRole("button", { name: "Paste a URL" }).click();
    await notifications.locator('input[aria-label="Apprise URL"]').fill(SETUP_URL);
    await notifications.locator(".ntf-builder").getByRole("button", { name: "Add", exact: true }).click();
    check("a pasted URL joins the list", (await notifications.locator(".ntf-row").count()) >= 1);
    await next();

    console.log("=== review ===");
    const preview = await page.locator(".setup-preview").innerText();
    const planned = JSON.parse(preview);
    check("only what was entered is planned", Object.keys(planned).sort().join(",") ===
        "DASHBOARD_PASSWORD,DASHBOARD_USERNAME,ICECAST_STREAM_URL_ARRAY,ICECAST_STREAM_URL_MAPPING,TZ,WATCHED_FIPS",
        Object.keys(planned).join(","));
    await page.screenshot({ path: `${SHOT_DIR}/setup-review.png`, fullPage: true });

    // Only asked where this machine could act on it: a Windows `service` build, or Linux with
    // systemd. It is never answered for the tester; "no" keeps this run from installing anything.
    const autostart = page.locator(".setup-autostart");
    if ((await autostart.count()) > 0 && (await autostart.locator('input[type="radio"]').count()) > 0) {
        console.log("=== start with this computer ===");
        await page.click("#nextButton");
        await page.waitForTimeout(300);
        check("saving without an answer is refused",
            (await autostart.getAttribute("class")).includes("is-missing"),
            await page.locator("#setupStatus").innerText());
        await autostart.locator('input[value="no"]').check();
        check("answering clears the warning", !(await autostart.getAttribute("class")).includes("is-missing"));
    }

    console.log("=== finish and hand over ===");
    await page.click("#nextButton");
    await page.waitForURL((url) => url.pathname.includes("login"), { timeout: 120000 });
    check("the listener takes over and asks for sign-in", true);
    if (CONFIG) {
        const saved = JSON.parse(fs.readFileSync(CONFIG, "utf8"));
        check("config.json holds exactly the plan", JSON.stringify(saved) === JSON.stringify(planned));
        const apprise = path.join(path.dirname(CONFIG), "apprise.yml");
        const written = fs.existsSync(apprise) ? fs.readFileSync(apprise, "utf8") : "";
        check("apprise.yml holds the pasted URL", written.includes(`- "${SETUP_URL}"`), apprise);
    }

    await page.fill('input[name="username"]', USER);
    await page.fill('input[name="password"]', PASS);
    await Promise.all([
        page.waitForURL((url) => !url.pathname.includes("login"), { timeout: 15000 }),
        page.click('button[type="submit"]'),
    ]);
    check("the new credentials sign in", !page.url().includes("login"), page.url());

    await page.goto(`${BASE}/config.html`, { waitUntil: "networkidle" });
    await page.waitForTimeout(1200);
    const fields = await page.locator(".cfg-field").count();
    check("the editor renders the form", fields > 40, `fields=${fields}`);
    await page.goto(`${BASE}/setup.html`, { waitUntil: "networkidle" });
    await page.waitForTimeout(800);
    check("setup is closed once configured", !page.url().includes("setup.html"), page.url());

    console.log("=== diagnostics ===");
    // The wrong token is refused with a 401 on purpose; a request cut short by navigation is noise.
    const unexpected = consoleErrors.filter((text) => !/\b401\b/.test(text) && !/Failed to fetch/.test(text));
    check("no unexpected console errors", unexpected.length === 0, `${unexpected.length} logged`);
    unexpected.slice(0, 8).forEach((text) => console.log(`      ${text.slice(0, 160)}`));

    await browser.close();
    console.log("");
    console.log(failures === 0 ? "ALL CHECKS PASSED" : `${failures} CHECK(S) FAILED`);
    process.exit(failures === 0 ? 0 : 1);
})().catch((err) => {
    console.error("harness error:", err);
    process.exit(2);
});
