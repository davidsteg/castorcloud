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
  dawarich: {
    // From the project website; usage to be confirmed with the author.
    src: "/screenshots/dawarich.webp", width: 1600, height: 900, license: "dawarich.app",
    source: "https://dawarich.app",
  },
  airtrail: {
    src: "/screenshots/airtrail.webp", width: 2880, height: 1800, license: "GPL-3.0",
    source: "https://github.com/johanohly/AirTrail/blob/v3.13.0/docs/public/screenshots/map-overview.light.webp",
  },
};
