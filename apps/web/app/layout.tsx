import type { Metadata } from "next";
import { GeistPixelSquare } from "geist/font/pixel";
import "./globals.css";

export const metadata: Metadata = {
  title: "kyora — unlocking the full potential of coding agents",
  description:
    "Queryable runtime state, multi-model code review, and councils of other model families — on the coding subscriptions you already pay for.",
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
