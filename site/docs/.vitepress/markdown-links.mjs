import path from 'node:path/posix'

export const canonicalRoutes = Object.freeze({
  'README.md': '/guide/docs-index',
  'overview.md': '/guide/overview',
  'research.md': '/guide/research',
  'decisions.md': '/guide/decisions',
  'config.md': '/guide/config',
  'gateway.md': '/guide/gateway',
  'provider-protocol.md': '/guide/provider-protocol',
  'momo-infra.md': '/guide/momo-infra',
  'momo-models.md': '/guide/momo-models',
  'voice-clone.md': '/guide/voice-clone',
  'deployment.md': '/guide/deployment',
  'degradation.md': '/guide/degradation',
  'model-fetch.md': '/guide/model-fetch',
  'borrowed-etiquette.md': '/guide/borrowed-etiquette',
  'github-project.md': '/guide/github-project',
})

const externalPattern = /^(?:[a-z][a-z\d+.-]*:|\/\/)/i

export function remapCanonicalLink(url) {
  if (!url || url.startsWith('#') || externalPattern.test(url)) return url
  if (url.startsWith('/')) return url

  const hashIndex = url.indexOf('#')
  const queryIndex = url.indexOf('?')
  const splitIndex = [hashIndex, queryIndex].filter((index) => index >= 0).sort((a, b) => a - b)[0] ?? url.length
  const pathname = url.slice(0, splitIndex)
  const suffix = url.slice(splitIndex)
  if (!pathname.toLowerCase().endsWith('.md')) return url

  const basename = path.basename(pathname)
  const route = canonicalRoutes[basename]
  return route ? `${route}${suffix}` : url
}
