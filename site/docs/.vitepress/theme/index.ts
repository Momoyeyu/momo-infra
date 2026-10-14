import type { Theme } from 'vitepress'
import DefaultTheme from 'vitepress/theme'
import MermaidDiagram from './MermaidDiagram.vue'
import InfraHome from './InfraHome.vue'
import './editorial.css'

export default {
  extends: DefaultTheme,
  enhanceApp({ app }) {
    app.component('MermaidDiagram', MermaidDiagram)
    app.component('InfraHome', InfraHome)
  },
} satisfies Theme
