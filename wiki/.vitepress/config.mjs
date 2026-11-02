import { copyFile, mkdir } from 'node:fs/promises'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'

const require = createRequire(process.env.HOGMAP_DOCS_TOOLCHAIN_PACKAGE || import.meta.url)
const vitepressRoot = dirname(require.resolve('vitepress/package.json'))

export default {
  lang: 'en-US',
  title: 'HogMap',
  description: 'Documentation for the Windows disk-space treemap currently in development.',
  base: '/hogmap/',
  outDir: process.env.HOGMAP_DOCS_OUT_DIR || '.vitepress/dist',
  cacheDir: process.env.HOGMAP_DOCS_CACHE_DIR || '.vitepress/cache',
  vite: { build: { license: { fileName: 'third-party/bundled-licenses.md' } } },
  async buildEnd({ outDir }) {
    const noticesDir = join(outDir, 'third-party')
    await mkdir(noticesDir, { recursive: true })
    await Promise.all([
      copyFile(join(vitepressRoot, 'LICENSE'), join(noticesDir, 'vitepress-LICENSE.txt')),
      copyFile(join(vitepressRoot, 'THIRD-PARTY-NOTICES.md'), join(noticesDir, 'vitepress-THIRD-PARTY-NOTICES.md'))
    ])
  },
  themeConfig: {
    nav: [
      { text: 'Guide', link: '/' },
      { text: 'Development', link: '/development' },
      { text: 'Source', link: 'https://github.com/Yivas/hogmap' }
    ],
    sidebar: [
      {
        text: 'HogMap',
        items: [
          { text: 'Overview and status', link: '/' },
          { text: 'Understanding sizes', link: '/sizes' },
          { text: 'Working with filters', link: '/filters' },
          { text: 'Development and checks', link: '/development' },
          { text: 'Third-party licenses', link: '/licenses' }
        ]
      }
    ],
    search: { provider: 'local' },
    editLink: {
      pattern: 'https://github.com/Yivas/hogmap/edit/main/wiki/:path',
      text: 'Edit this page on GitHub'
    },
    footer: {
      message: 'MIT licensed. No application release yet.',
      copyright: 'Copyright © 2026 Yivas'
    }
  }
}
