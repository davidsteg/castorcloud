import type { OssProject } from "@pc/pricing";

// Official screenshots of the open-source projects we run, stored in
// public/screenshots. Always shown with source and licence.
export type Screenshot = { src: string; width: number; height: number; license: string; source: string };

export const SCREENSHOTS: Partial<Record<OssProject, Screenshot>> = {
  immich: {
    src: "/screenshots/immich.png", width: 1802, height: 1118, license: "AGPL-3.0",
    source: "https://github.com/immich-app/immich/blob/v3.2.2/design/immich-screenshots.png",
  },
  paperless: {
    src: "/screenshots/paperless.png", width: 3824, height: 2786, license: "GPL-3.0",
    source: "https://github.com/paperless-ngx/paperless-ngx/blob/v3.2.1/docs/assets/screenshots/documents-smallcards.png",
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
