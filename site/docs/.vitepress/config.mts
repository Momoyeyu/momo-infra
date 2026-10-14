import { defineConfig } from 'vitepress'
import { installMermaidFence } from './markdown-fence.mjs'
import { remapCanonicalLink } from './markdown-links.mjs'

export default defineConfig({
  title: 'momo-infra',
  description: 'momo-infra 是自有 AI 基础设施：模型训练 pipeline、serving 底座与统一对外 gateway。',
  base: '/momo-infra/',
  lang: 'zh-CN',
  cleanUrls: true,
  appearance: true,
  head: [
    ['link', { rel: 'icon', href: '/momo-infra/favicon.svg', type: 'image/svg+xml' }],
    ['script', {}, "try{const k='vitepress-theme-appearance';if(!localStorage.getItem(k))localStorage.setItem(k,'light')}catch(e){}"],
  ],
  themeConfig: {
    nav: [
      { text: '介绍', link: '/' },
      { text: '定位与边界', link: '/guide/overview' },
      { text: '训练与 serving', link: '/guide/momo-infra' },
      { text: '运维手册', link: '/guide/deployment' },
      { text: 'GitHub', link: 'https://github.com/Momoyeyu/momo-infra' },
    ],
    sidebar: [
      {
        text: 'momo-infra',
        items: [
          { text: '项目介绍', link: '/guide/' },
          { text: '定位与边界', link: '/guide/overview' },
          { text: '配置规范', link: '/guide/config' },
        ],
      },
      {
        text: '模块与训练',
        items: [
          { text: 'infra 控制面', link: '/guide/momo-infra' },
          { text: 'models 模块', link: '/guide/momo-models' },
          { text: '声线训练轨道', link: '/guide/voice-clone' },
        ],
      },
      {
        text: '运维 runbook',
        items: [
          { text: '部署手册', link: '/guide/deployment' },
          { text: '降级手册', link: '/guide/degradation' },
          { text: '模型获取', link: '/guide/model-fetch' },
          { text: '借用机器规则', link: '/guide/borrowed-etiquette' },
        ],
      },
      {
        text: '参考与工程',
        collapsed: true,
        items: [
          { text: '文档索引', link: '/guide/docs-index' },
          { text: '架构决策', link: '/guide/decisions' },
          { text: '技术研究', link: '/guide/research' },
          { text: 'GitHub Project 策略', link: '/guide/github-project' },
        ],
      },
    ],
    outline: { level: [2, 3], label: '本页内容' },
    socialLinks: [{ icon: 'github', link: 'https://github.com/Momoyeyu/momo-infra' }],
    footer: {
      message: '训练、评测、serving，一条自己的链路。',
      copyright: 'momo-infra · 设计文档',
    },
    search: { provider: 'local' },
  },
  markdown: {
    math: false,
    config(md) {
      installMermaidFence(md)
      const originalNormalizeLink = md.normalizeLink.bind(md)
      md.normalizeLink = (url) => originalNormalizeLink(remapCanonicalLink(url))
    },
  },
})
