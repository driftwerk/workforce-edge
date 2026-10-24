//! The built-in manual test page.
//!
//! `GET /demo/{agent}` serves a minimal stand-in for a customer webapp: it signs
//! the tester in with Google (Google Identity Services), embeds the agent's
//! widget, and answers the widget's `postMessage` token request with the
//! signed-in user's `id_token` — exactly the handshake a real host page does.
//!
//! Gated by `[demo] enabled` (default off) so production installs never expose
//! it. The only dynamic values are the agent id and the Google client id (a
//! public identifier); both are embedded as JSON string literals so neither can
//! break out into markup or script.
//!
//! Google note: Google only authorises `http://localhost*` or `https` origins
//! for Sign-In, so open this page via an ssh tunnel (`ssh -L`) or an https
//! front, and register that same origin both in the Google OAuth client and in
//! the agent's `allowed_origins`.

/// Render the demo host page for one agent.
pub fn demo_html(agent_id: &str, google_client_id: &str) -> String {
    // JSON-escape, then additionally escape `<` as the unicode escape u003c:
    // this literal lands inside an HTML `<script>` block, where a raw
    // `</script>` in a JSON string would still terminate the block (the
    // classic JSON-in-HTML pitfall).
    let json_str = |v: &str| {
        serde_json::to_string(v)
            .unwrap_or_else(|_| "\"\"".into())
            .replace('<', "\\u003c")
    };
    DEMO_TEMPLATE
        .replace("__AGENT__", &json_str(agent_id))
        .replace("__CLIENT_ID__", &json_str(google_client_id))
}

const DEMO_TEMPLATE: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Agent test page</title>
<style>
  body { font-family: system-ui, sans-serif; margin: 0; padding: 48px; background: #eef2f7; color: #334; }
  main { max-width: 560px; }
  h1 { font-size: 20px; }
  p { line-height: 1.5; }
  #signin { margin: 24px 0; }
  #who { font-size: 14px; color: #567; }
</style>
</head>
<body>
<main>
  <h1>Agent test page</h1>
  <p>A stand-in for a customer webapp: sign in with Google, then chat with the
     agent via the launcher in the corner. The widget receives your id_token the
     same way a real host page would hand it one.</p>
  <div id="signin"></div>
  <p id="who">Not signed in.</p>
</main>
<script>
  var AGENT = __AGENT__, CLIENT_ID = __CLIENT_ID__;
  var idToken = null;

  // Answer the widget's token requests, and re-offer the token whenever it
  // (re)loads. Sending to `location.origin` keeps it in this same page.
  function sendToken() {
    if (idToken) window.postMessage({ type: "wf-embed-token", token: idToken }, window.location.origin);
  }
  window.addEventListener("message", function (e) {
    if (e.source !== window || !e.data) return;
    // Usage events a real host page would forward to its analytics.
    if (e.data.type === "wf-embed-event") console.log("widget event:", e.data.event, e.data.agent);
    if (e.data.type !== "wf-embed-token-request") return;
    sendToken();
  });

  function decodeName(jwt) {
    try {
      var payload = JSON.parse(atob(jwt.split(".")[1].replace(/-/g, "+").replace(/_/g, "/")));
      return payload.email || payload.name || payload.sub || "signed in";
    } catch (_) { return "signed in"; }
  }

  // The launcher is always present; the chat itself needs a token, so a
  // not-signed-in visitor sees the bubble but can't talk until they sign in.
  function injectWidget() {
    if (document.getElementById("wf-embed")) return;
    var s = document.createElement("script");
    s.id = "wf-embed";
    s.src = "/embed/" + encodeURIComponent(AGENT);
    document.body.appendChild(s);
  }

  function onCredential(response) {
    idToken = response.credential;
    document.getElementById("who").textContent = "Signed in as " + decodeName(idToken) + ". Open the chat launcher.";
    injectWidget();
    sendToken(); // push to a widget that loaded before sign-in completed
  }

  function initGoogle() {
    // auto_select lets a returning tester re-authenticate from the button
    // without re-consenting. No One Tap `prompt()` — it pops unsolicited.
    google.accounts.id.initialize({ client_id: CLIENT_ID, callback: onCredential, auto_select: true });
    google.accounts.id.renderButton(document.getElementById("signin"), { theme: "outline", size: "large" });
    injectWidget();
  }

  var gsi = document.createElement("script");
  gsi.src = "https://accounts.google.com/gsi/client";
  gsi.onload = initGoogle;
  document.head.appendChild(gsi);
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_agent_and_client_id_as_json_literals() {
        let html = demo_html("assistant", "123-abc.apps.googleusercontent.com");
        assert!(html.contains(r#""assistant""#));
        assert!(html.contains(r#""123-abc.apps.googleusercontent.com""#));
        assert!(html.contains("accounts.google.com/gsi/client"));
        assert!(html.contains("wf-embed-token-request"));
    }

    #[test]
    fn a_hostile_agent_id_cannot_break_out_of_its_script_block() {
        let html = demo_html("</script><script>alert(1)</script>", "cid");
        // `<` becomes the JS unicode escape (backslash + u003c), so the HTML
        // parser cannot end the script block early; the raw sequence never
        // appears in the page.
        assert!(!html.contains("</script><script>alert(1)"));
        let esc = format!("{}u003c", '\u{5c}'); // a literal backslash then u003c
        assert!(html.contains(&format!("{esc}/script>{esc}script>alert(1)")));
    }

    #[test]
    fn a_quote_in_the_client_id_stays_inside_its_literal() {
        let html = demo_html("a", "\";alert(1);//");
        assert!(html.contains(r#""\";alert(1);//""#));
    }
}
