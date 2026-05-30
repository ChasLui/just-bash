import type { Metadata } from "next";
import { GeistMono } from "geist/font/mono";
import { Analytics } from "@vercel/analytics/next"
import "./globals.css";

const RUNTIME_DISCLAIMER =
  "Historical TypeScript in-browser demo (legacy path). Active runtime is Rust-first in packages/just-bash/rust-core (`just-bash-rs` CLI).";

export const metadata: Metadata = {
  title: "just-bash",
  description: RUNTIME_DISCLAIMER,
  metadataBase: new URL("https://justbash.dev"),
  openGraph: {
    title: "just-bash",
    description: RUNTIME_DISCLAIMER,
    type: "website",
  },
  twitter: {
    card: "summary_large_image",
    title: "just-bash",
    description: RUNTIME_DISCLAIMER,
  },
};

export const viewport = {
  width: "device-width",
  initialScale: 1,
  viewportFit: "cover",
  interactiveWidget: "resizes-content",
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en">
      <body className={`${GeistMono.variable} antialiased`}>
        {children}
        <Analytics/>
      </body>
    </html>
  );
}
