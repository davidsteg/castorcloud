import type { OssProject } from "@pc/pricing";

// Screenshots of the open-source projects we run, stored in public/screenshots
// and always shown with source and licence. Ours are taken from real instances
// with our branding and demo data of a fictional household (docs/branding.md);
// third-party logos the apps still show are cropped out.
export type Screenshot = { src: string; width: number; height: number; license: string; source: string };

export const SCREENSHOTS: Partial<Record<OssProject, Screenshot>> = {
  immich: {
    // Ours (Immich v3.2.2). Demo photos from Unsplash (Unsplash License), no
    // people; Immich's top bar with its logo is cropped.
    src: "/screenshots/immich.webp", width: 2880, height: 1680, license: "AGPL-3.0",
    source: "https://github.com/immich-app/immich/tree/v3.2.2",
  },
  paperless: {
    // Ours (Paperless-ngx 3.2.1), demo documents generated for it.
    src: "/screenshots/paperless.webp", width: 2880, height: 1800, license: "GPL-3.0",
    source: "https://github.com/paperless-ngx/paperless-ngx/tree/v3.2.1",
  },
  opencloud: {
    // Ours (OpenCloud 7.2.4, sold as Files): our theme, so none of OpenCloud's
    // marks show; the version line at the bottom is cropped (docs/files.md).
    src: "/screenshots/files.webp", width: 2880, height: 1680, license: "AGPL-3.0",
    source: "https://github.com/opencloud-eu/opencloud/tree/v7.2.4",
  },
  // dawarich: none until we take our own (the website's image had no clear
  // licence for our use); the pages show a text placeholder.
  airtrail: {
    src: "/screenshots/airtrail.webp", width: 2880, height: 1800, license: "GPL-3.0",
    source: "https://github.com/johanohly/AirTrail/blob/v3.13.0/docs/public/screenshots/map-overview.light.webp",
  },
};

// The sign-in page all services share (Authentik 2025.10.4 with our brand
// settings, docs/branding.md), cropped to the sign-in card. Authentik isn't a
// booked service, so it isn't in OSS_PROJECTS and gets no donation share; the
// open-source page shows and credits it separately.
export const LOGIN_SCREENSHOT: Screenshot & { name: string; url: string } = {
  name: "authentik", url: "https://goauthentik.io",
  src: "/screenshots/login.webp", width: 1920, height: 1200, license: "MIT",
  source: "https://github.com/goauthentik/authentik/tree/version/2025.10.4",
};
