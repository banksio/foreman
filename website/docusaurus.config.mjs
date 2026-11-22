const config = {
  title: 'Foreman',
  tagline: 'Maintenance for Debian-family hosts',
  favicon: 'img/foreman-favicon.svg',
  url: 'https://banksio.github.io',
  baseUrl: '/foreman/',
  organizationName: 'banksio',
  projectName: 'foreman',
  trailingSlash: true,
  onBrokenLinks: 'throw',
  i18n: {defaultLocale: 'en', locales: ['en']},
  presets: [
    [
      'classic',
      {
        docs: {
          routeBasePath: '/',
          sidebarPath: './sidebars.mjs',
          editUrl: 'https://github.com/banksio/foreman/edit/main/website/',
        },
        blog: false,
        theme: {customCss: './src/css/custom.css'},
      },
    ],
  ],
  themeConfig: {
    colorMode: {respectPrefersColorScheme: true},
    navbar: {
      title: 'Foreman',
      logo: {alt: 'Foreman', src: 'img/foreman.svg'},
      items: [
        {type: 'docSidebar', sidebarId: 'guide', position: 'left', label: 'Documentation'},
        {href: 'https://github.com/banksio/foreman/releases', label: 'Releases', position: 'right'},
        {href: 'https://github.com/banksio/foreman', label: 'GitHub', position: 'right'},
      ],
    },
    prism: {additionalLanguages: ['bash', 'toml', 'json']},
  },
};

export default config;
