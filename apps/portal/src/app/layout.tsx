import type { Metadata } from "next";
import { Inter, Nunito } from "next/font/google";
import { headers } from "next/headers";
import { BRAND_NAME } from "@/lib/config";
import { siteOrigin } from "@/lib/seo";
import "./globals.css";

const inter = Inter({ subsets: ["latin"], variable: "--font-inter", display: "swap" });
// Rounded geometric face for the castorcloud.ch wordmark.
const nunito = Nunito({ subsets: ["latin"], weight: ["800"], variable: "--font-nunito", display: "swap" });

export const metadata: Metadata = { title: BRAND_NAME, metadataBase: siteOrigin() };

export default async function RootLayout({ children }: { children: React.ReactNode }) {
  const lang = (await headers()).get("x-pc-locale") ?? "de";
  return (
    <html lang={lang} className={`${inter.variable} ${nunito.variable}`}>
      <body>{children}</body>
    </html>
  );
}
