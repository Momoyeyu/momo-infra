export function encodeMermaidSource(source) {
  return encodeURIComponent(source).replace(/'/g, '%27')
}

export function installMermaidFence(md) {
  const originalFence = md.renderer.rules.fence
  md.renderer.rules.fence = (tokens, index, options, env, self) => {
    if (tokens[index].info.trim() === 'mermaid') {
      const source = encodeMermaidSource(tokens[index].content)
      return `<MermaidDiagram source="${source}" />`
    }
    return originalFence(tokens, index, options, env, self)
  }
}
