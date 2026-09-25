(function () {
    const form = document.getElementById("loginForm");
    const error = document.getElementById("loginError");
    if (!form) return;

    function showError(message) {
        if (!error) {
            alert(message);
            return;
        }
        error.textContent = message;
        error.hidden = false;
    }

    // Only same-origin paths are accepted, so ?redirect= cannot bounce a signed-in user off-site.
    function safeRedirect() {
        const requested = new URLSearchParams(window.location.search).get("redirect");
        if (!requested || !requested.startsWith("/") || requested.startsWith("//")) {
            return "/index.html";
        }
        if (requested.startsWith("/login.html")) return "/index.html";
        return requested;
    }

    form.addEventListener("submit", async (event) => {
        event.preventDefault();
        if (error) error.hidden = true;

        const button = form.querySelector("button[type=submit]");
        const original = button ? button.textContent : "";
        if (button) {
            button.disabled = true;
            button.textContent = "Signing in...";
        }

        try {
            const response = await fetch("/api/login", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                credentials: "same-origin",
                body: JSON.stringify({
                    username: form.username.value,
                    password: form.password.value,
                }),
            });

            if (response.ok) {
                window.location.href = safeRedirect();
                return;
            }

            showError(await response.text() || "Invalid username or password.");
        } catch (err) {
            showError(`Could not reach the server: ${err.message}`);
        } finally {
            if (button) {
                button.disabled = false;
                button.textContent = original;
            }
        }
    });
})();
