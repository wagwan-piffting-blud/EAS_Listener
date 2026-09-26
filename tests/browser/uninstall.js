/**
 * Drives the configuration page's Uninstall section against a scratch listener: it says what
 * goes, the button waits for the typed name, and uninstalling hands off and says so. It really
 * uninstalls that listener, so only run it against one made for the purpose; the harness around
 * it checks the listener stopped and its files are gone.
 */
const { chromium } = require("playwright");

const BASE = process.env.EAS_BASE || "http://127.0.0.1:8080";
const USER = process.env.EAS_USER || "probe";
const PASS = process.env.EAS_PASS || "probe-pass";
const SHOT_DIR = process.argv[2] || process.env.TEMP || ".";

function line(label, value) {
    console.log(`  ${label.padEnd(52)} ${value}`);
}

(async () => {
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

    try {
        console.log("=== sign in ===");
        await page.goto(`${BASE}/login.html`, { waitUntil: "networkidle" });
        await page.fill('input[name="username"]', USER);
        await page.fill('input[name="password"]', PASS);
        await Promise.all([page.waitForURL((url) => !url.pathname.endsWith("/login.html")), page.click('button[type="submit"]')]);
        await page.waitForLoadState("networkidle");

        console.log("=== the section ===");
        await page.goto(`${BASE}/config.html#cfg-group-uninstall`, { waitUntil: "networkidle" });
        const section = page.locator("#cfg-group-uninstall");
        await section.waitFor();
        await section.locator(".unin-list").waitFor();
        check("Uninstall section renders", await section.isVisible());
        check("it is the last section", await page.evaluate(() => {
            const groups = [...document.querySelectorAll("#cfgForm > section")];
            return groups[groups.length - 1].id === "cfg-group-uninstall";
        }));
        check("nav links to it", (await page.locator('.cfg-nav a[href="#cfg-group-uninstall"]').count()) === 1);
        const listed = (await section.locator(".unin-list").textContent()).trim();
        check("it names the folder that goes", listed.includes(process.env.EAS_EXPECT_FOLDER || ""), listed);

        const confirm = section.locator('input[aria-label^="Type"]');
        const button = section.getByRole("button", { name: "Uninstall" });
        check("the button waits for the name", await button.isDisabled());
        await confirm.fill("not-the-name");
        check("a wrong name keeps it disabled", await button.isDisabled());
        await confirm.fill(process.env.EAS_EXPECT_INSTANCE || "default");
        check("the right name enables it", await button.isEnabled());
        check("it says the archive is kept", /are kept/.test(await section.locator(".unin-body").textContent()));
        check("deleting the archive is not ticked", !(await section.locator("#uninData").isChecked()));
        await section.locator("#uninProgram").check();
        await page.screenshot({ path: `${SHOT_DIR}/uninstall.png`, fullPage: false });

        console.log("=== uninstall ===");
        await button.click();
        const done = section.locator(".ntf-result.is-ok, .ntf-result.is-bad").first();
        await done.waitFor({ timeout: 20000 });
        const text = (await done.textContent()).trim();
        check("it hands off and says so", (await done.getAttribute("class")).includes("is-ok"), text);
        check("and names the log on the computer", (await section.locator("code").last().textContent()).includes("eas-listener-uninstall-"));
        check("the save buttons are disabled", await page.locator("#saveButton").isDisabled());

        console.log("=== diagnostics ===");
        check("no console errors", consoleErrors.length === 0, consoleErrors.join(" | "));
    } catch (err) {
        console.error(err);
        failures++;
    } finally {
        await browser.close();
    }

    console.log(failures === 0 ? "ALL PASS" : `${failures} FAILED`);
    process.exit(failures === 0 ? 0 : 1);
})();
