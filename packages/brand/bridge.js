// Injected into every embedded service page (served at /_brand/bridge.js).
// Tells the portal shell where the user is, and applies shell commands.
// Protocol: see bridge-protocol.ts.
(function () {
  if (window.parent === window) return; // opened directly, not embedded

  // photos-<slug>.<domain>  ->  service "photos", shell origin <slug>.<domain>
  var labels = location.hostname.split(".");
  var dash = labels[0].indexOf("-");
  var service = labels[0].slice(0, dash);
  labels[0] = labels[0].slice(dash + 1);
  var shellOrigin = location.protocol + "//" + labels.join(".") +
    (location.port ? ":" + location.port : "");

  document.documentElement.classList.add("pc-embedded");

  function currentPath() {
    return location.pathname + location.search + location.hash;
  }

  function post(type) {
    window.parent.postMessage(
      { type: type, service: service, path: currentPath(), title: document.title },
      shellOrigin
    );
  }

  // SPAs (Immich, Paperless, AirTrail) navigate via the History API.
  ["pushState", "replaceState"].forEach(function (name) {
    var original = history[name];
    history[name] = function () {
      var result = original.apply(this, arguments);
      post("pc:navigated");
      return result;
    };
  });
  window.addEventListener("popstate", function () { post("pc:navigated"); });
  new MutationObserver(function () { post("pc:navigated"); })
    .observe(document.querySelector("title") || document.head, { childList: true, subtree: true });

  window.addEventListener("message", function (event) {
    if (event.origin !== shellOrigin || !event.data) return;
    var msg = event.data;
    if (msg.type === "pc:theme") {
      document.documentElement.setAttribute("data-pc-theme", msg.mode);
    } else if (msg.type === "pc:go" && typeof msg.path === "string" && msg.path.charAt(0) === "/") {
      location.assign(msg.path);
    }
  });

  // Services without an SSO auto-launch setting: submit their SSO form so the
  // user (already signed in to the portal) never sees a foreign login page.
  var SSO_FORMS = {
    places: 'form[action$="/users/auth/openid_connect"]'
  };

  function autoSso() {
    var selector = SSO_FORMS[service];
    var form = selector && document.querySelector(selector);
    if (form && !sessionStorage.getItem("pc:sso-tried")) {
      sessionStorage.setItem("pc:sso-tried", "1"); // avoid loops if SSO fails
      form.submit();
    } else if (!form) {
      sessionStorage.removeItem("pc:sso-tried");
    }
  }

  function ready() {
    autoSso();
    post("pc:ready");
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", ready);
  } else {
    ready();
  }
})();
