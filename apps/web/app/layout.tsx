import type { Metadata } from "next";
import { GeistPixelSquare } from "geist/font/pixel";
import "./globals.css";

const title = "Kyora — unlocking the full potential of coding agents";
const description =
  "Queryable runtime state, multi-model code review, and councils of other model families — on the coding subscriptions you already pay for.";

export const metadata: Metadata = {
  metadataBase: new URL("https://kyora.sh"),
  title: {
    default: title,
    template: "%s — Kyora",
  },
  description,
  applicationName: "Kyora",
  keywords: [
    "coding agents",
    "AI code review",
    "multi-model code review",
    "runtime observability",
    "MCP server",
    "Claude Code",
    "Codex",
    "agent tooling",
  ],
  authors: [{ name: "Elia Hilse", url: "https://x.com/eliahilse" }],
  creator: "Elia Hilse",
  alternates: { canonical: "/" },
  openGraph: {
    type: "website",
    url: "https://kyora.sh",
    siteName: "Kyora",
    title,
    description,
    images: [{ url: "/og.png", width: 1200, height: 630, alt: title }],
  },
  twitter: {
    card: "summary_large_image",
    title,
    description,
    creator: "@eliahilse",
    images: ["/og.png"],
  },
  icons: { icon: "/icon.svg" },
  robots: {
    index: true,
    follow: true,
    googleBot: { index: true, follow: true, "max-image-preview": "large" },
  },
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html lang="en" className={GeistPixelSquare.variable}>
      <body>{children}</body>
    </html>
  );
}
