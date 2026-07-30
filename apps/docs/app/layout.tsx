import type { Metadata } from 'next';
import { RootProvider } from 'fumadocs-ui/provider/next';
import './global.css';
import { Inter } from 'next/font/google';

const inter = Inter({
  subsets: ['latin'],
});

const description = 'Documentation for Kyora, the runtime observability SDK for coding agents.';

export const metadata: Metadata = {
  metadataBase: new URL('https://docs.kyora.sh'),
  title: {
    default: 'Kyora Docs',
    template: '%s | Kyora Docs',
  },
  description,
  applicationName: 'Kyora Docs',
  openGraph: {
    type: 'website',
    url: 'https://docs.kyora.sh',
    title: 'Kyora Docs',
    siteName: 'Kyora Docs',
    description,
  },
  twitter: {
    card: 'summary_large_image',
    title: 'Kyora Docs',
    description,
  },
};

export default function Layout({ children }: LayoutProps<'/'>) {
  return (
    <html lang="en" className={inter.className} suppressHydrationWarning>
      <body className="flex flex-col min-h-screen">
        <RootProvider>{children}</RootProvider>
      </body>
    </html>
  );
}
