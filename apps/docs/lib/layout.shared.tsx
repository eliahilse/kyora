import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared';

export const gitConfig = {
  user: 'eliahilse',
  repo: 'kyora',
  branch: 'main',
};

export function baseOptions(): BaseLayoutProps {
    return {
      nav: {
      title: 'Kyora Docs',
      },
    githubUrl: `https://github.com/${gitConfig.user}/${gitConfig.repo}`,
  };
}
