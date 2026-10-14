import assert from 'node:assert/strict'
import test from 'node:test'
import { encodeMermaidSource, installMermaidFence } from '../docs/.vitepress/markdown-fence.mjs'

test('encodes hostile Mermaid source for a quoted component attribute', () => {
  const source = `flowchart LR\nA["x' onmouseover='alert(1)"] --> B`
  const encoded = encodeMermaidSource(source)
  assert.equal(encoded.includes('"'), false)
  assert.equal(encoded.includes("'"), false)
  assert.equal(decodeURIComponent(encoded), source)
})

test('replaces Mermaid fences and delegates other fences', () => {
  const md = {
    renderer: {
      rules: {
        fence: () => '<pre>fallback</pre>',
      },
    },
  }
  installMermaidFence(md)
  const tokens = [{ info: 'mermaid', content: 'flowchart LR\nA --> B' }]
  const rendered = md.renderer.rules.fence(tokens, 0, {}, {}, {})
  assert.match(rendered, /^<MermaidDiagram source="[^"]+" \/>$/)
  assert.equal(rendered.includes('flowchart LR'), false)
  assert.equal(md.renderer.rules.fence([{ info: 'ts', content: '' }], 0, {}, {}, {}), '<pre>fallback</pre>')
})
