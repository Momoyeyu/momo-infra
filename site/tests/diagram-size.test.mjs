import assert from 'node:assert/strict'
import test from 'node:test'
import { diagramWidth } from '../docs/.vitepress/diagram-size.mjs'

test('defaults to fit width for wide diagrams', () => {
  assert.equal(diagramWidth(1800, 700), 700)
})

test('does not stretch a diagram narrower than the viewport', () => {
  assert.equal(diagramWidth(600, 700), 600)
})

test('readable mode preserves near-intrinsic text size', () => {
  assert.equal(diagramWidth(1800, 700, 'readable'), 1680)
})

test('zoom applies after readable sizing', () => {
  assert.equal(diagramWidth(1800, 700, 'readable', 2), 3360)
})
