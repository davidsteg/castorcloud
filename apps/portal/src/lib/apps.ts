import type { ServiceKey } from "@pc/pricing";

// Mobile apps per service (store links verified 27.09.2026).
export type MobileApp = {
  name: string;
  /** Published by the project itself, or a community app. */
  official: boolean;
  ios?: string;
  android?: string;
};

export const MOBILE_APPS: Partial<Record<ServiceKey, MobileApp[]>> = {
  photos: [
    {
      name: "Immich",
      official: true,
      ios: "https://apps.apple.com/app/immich/id1613945652",
      android: "https://play.google.com/store/apps/details?id=app.alextran.immich",
    },
  ],
  docs: [
    { name: "Swift Paperless", official: false, ios: "https://apps.apple.com/app/swift-paperless/id6448698521" },
    { name: "Paperless Mobile", official: false, android: "https://play.google.com/store/apps/details?id=de.astubenbord.paperless_mobile" },
  ],
  places: [
    {
      name: "Dawarich",
      official: true,
      ios: "https://apps.apple.com/app/dawarich/id6739544999",
      android: "https://play.google.com/store/apps/details?id=com.zeitflow.dawarich",
    },
  ],
};
