# Branding of the apps

Every app a household uses carries our name, logo and colours, as far as the
app itself offers it. The rule (David, 2026-11-02): **light branding through
each app's official settings only**, so that upgrades stay as easy as before
and we stay compatible with upstream:

- no forks, no patched or rebuilt images, no changes to AGPL/GPL source;
- no CSS that hides or moves parts of an app's page (it breaks on upgrades);
- colours only through an app's own theme variables, so an upgrade can at
  worst bring back the app's default colour, never a broken page;
- where an app has no official option, it stays as upstream ships it.

Logo and colours come from `packages/brand` (`logo.svg`, `icon.svg`,
`tokens.css`: Swiss red `#d52b1e`, navy `#243447`).

| App | What is ours | How (official option) | Not branded |
|---|---|---|---|
| Sign-in (Authentik) | Logo, favicon, page title, "Sign in to …", red buttons and links, Inter, plain background | Brand settings in `infra/control-plane/authentik/blueprints/20-signup.yaml` (`branding_logo`, `branding_favicon`, `branding_default_flow_background`, `branding_custom_css` with PatternFly/Authentik variables only, light theme). Own flow `castor-login` that reuses the upstream default stages, because Authentik's default blueprint resets the default flow's title. Files: `logo.svg`, `icon.svg`, `login-background.svg` mounted read-only at `/media/public/castor/` (only these three) | "Powered by authentik" footer |
| Files (OpenCloud) | Name "Files", logo in the top bar and on the login page, favicon, colours (light and dark) | OpenCloud's theme loader: `WEB_ASSET_THEMES_PATH=/etc/castor/themes` (our `packages/brand/opencloud`, read-only) and `WEB_UI_THEME_PATH=/themes/castor/theme.json`. Mandatory keys (share roles) are added by OpenCloud itself. Also `WEB_OPTION_DISABLE_FEEDBACK_LINK` (no link to OpenCloud's survey site) | The emblem on the short "signing in" screen (built into the web app), the version line in the sidebar |
| Photos (Immich) | Colours (light and dark) | Immich's "custom CSS" setting (`theme.customCss` in the `immich.json` castord renders, `render::IMMICH_CSS`): only `--immich-ui-primary-*` and `--immich-primary` / `--immich-dark-primary` | Immich's logo: Immich has no option for it |
| Documents (Paperless-ngx) | Name, logo, colour | `PAPERLESS_APP_TITLE`, `PAPERLESS_APP_LOGO` (our `icon.svg` mounted read-only into `media/logo/`). Colour: Paperless only has a per-user theme colour, which sets `--pngx-primary` / `--pngx-primary-lightness`; the skin proxy's `services/docs.css` sets the same two variables as the default, and a member's own choice still wins | "by Paperless-ngx" under the name |
| Timeline (Dawarich) | The sign-in button says our name | `OIDC_PROVIDER_NAME` | Everything else: Dawarich has no theming |
| Flights (AirTrail) | – | – | Everything: AirTrail has no theming |

The skin proxy still injects `theme.css` (our font) and `bridge.js` (the
portal's iframe bridge) as before; `services/*.css` hold colour variables
only.

## Upgrading an app

1. Bump the version in `packages/stack-templates/versions.env` as usual.
2. Open the app once (light and dark): logo, name and colour should be ours.
   If a colour is back to the app's default, the app renamed a variable:
   look up the new name in the app's theme and update the file above. Nothing
   else can break.
3. OpenCloud: when a release adds new colour roles to its own theme, they
   use OpenCloud's defaults until we add them to `theme.json`.

## Website screenshots

`apps/portal/public/screenshots` are taken from real instances with demo data
(fictional household "Anna Muster", no real people or companies) and our
branding, listed in `apps/portal/src/lib/screenshots.ts`. Third-party logos
that the apps still show (Immich's top bar) are cropped out of the website
images; the apps credit their projects in text on the open-source page.
The sign-in screenshot (`login.webp`) is Authentik with the brand settings from
`20-signup.yaml`, cropped to the sign-in card; the demo address is
`anna@muster.example`.

Still to take: Timeline (Dawarich, no screenshot shown until then: the
website's image had no clear licence for our use) and Flights (AirTrail,
still upstream's GPL screenshot). Both need map tiles reachable from the
environment that takes them.
