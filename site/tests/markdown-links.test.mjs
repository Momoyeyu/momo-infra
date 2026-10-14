import assert from 'node:assert/strict'
import test from 'node:test'
import { remapCanonicalLink } from '../docs/.vitepress/markdown-links.mjs'

test('remaps canonical relative markdown links', () => {
  assert.equal(remapCanonicalLink('../01-architecture/config.md'), '/guide/config')
  assert.equal(remapCanonicalLink('./degradation.md'), '/guide/degradation')
  assert.equal(remapCanonicalLink('../02-repos/momo-infra.md'), '/guide/momo-infra')
  assert.equal(remapCanonicalLink('../03-tracks/voice-clone.md'), '/guide/voice-clone')
})

test('preserves hash and query suffixes', () => {
  assert.equal(remapCanonicalLink('./config.md#变量全集'), '/guide/config#变量全集')
  assert.equal(remapCanonicalLink('./decisions.md?view=current#adr-029'), '/guide/decisions?view=current#adr-029')
})

test('leaves external, hash-only, assets, and rooted routes unchanged', () => {
  assert.equal(remapCanonicalLink('https://example.com/file.md'), 'https://example.com/file.md')
  assert.equal(remapCanonicalLink('#核心类型'), '#核心类型')
  assert.equal(remapCanonicalLink('../assets/diagram.svg'), '../assets/diagram.svg')
  assert.equal(remapCanonicalLink('/momo-infra/guide/overview'), '/momo-infra/guide/overview')
  assert.equal(remapCanonicalLink('/guide/overview'), '/guide/overview')
})
