import { createMDX } from 'fumadocs-mdx/next';
import { initOpenNextCloudflareForDev } from '@opennextjs/cloudflare';

if (process.argv.includes('dev')) {
  initOpenNextCloudflareForDev();
}

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const config = {
  serverExternalPackages: ['@takumi-rs/image-response'],
  reactStrictMode: true,
  async rewrites() {
    return [
      {
        source: '/docs/:path*.mdx',
        destination: '/llms.mdx/docs/:path*',
      },
    ];
  },
};

export default withMDX(config);
