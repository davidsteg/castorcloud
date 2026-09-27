import type { MetadataRoute } from "next";
import { BRAND_NAME } from "@/lib/config";

// Lets customers install their cloud (and with it Flights and the Assistant)
// as an app on the phone's home screen.
export default function manifest(): MetadataRoute.Manifest {
  return {
    name: BRAND_NAME,
    short_name: "Castor",
    description: "Private cloud hosted in Switzerland",
    start_url: "/",
    display: "standalone",
    background_color: "#f7f9fb",
    theme_color: "#243447",
    icons: [{ src: "/icon.svg", sizes: "any", type: "image/svg+xml", purpose: "any" }],
  };
}
