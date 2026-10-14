<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, useId, watch } from 'vue'
import { useData } from 'vitepress'
import { diagramWidth } from '../diagram-size.mjs'
import { queueMermaidRender } from './diagram-renderer'

const props = defineProps<{ source: string }>()
const { isDark, page } = useData()
const rawId = useId()
const diagramId = `infra-mermaid-${rawId.replace(/[^a-zA-Z0-9_-]/g, '')}`
const decodedSource = computed(() => decodeURIComponent(props.source))
const svg = ref('')
const error = ref('')
const loading = ref(true)
const mode = ref<'readable' | 'fit'>('fit')
const zoom = ref(1)
const naturalWidth = ref(1)
const viewportWidth = ref(1)
const dialog = ref<HTMLDialogElement>()
const dialogSvg = ref('')
const dialogError = ref('')
const dialogLoading = ref(false)
const dialogViewportWidth = ref(1)
const canvas = ref<HTMLElement>()
const dialogCanvas = ref<HTMLElement>()
const opener = ref<HTMLElement>()
const renderedWidth = computed(() => diagramWidth(naturalWidth.value, viewportWidth.value, mode.value, zoom.value))
const dialogRenderedWidth = computed(() => diagramWidth(naturalWidth.value, dialogViewportWidth.value, 'readable', 1))
const canScroll = ref(false)
let generation = 0
let dialogGeneration = 0
let bindFunctions: ((element: Element) => void) | undefined
let canvasObserver: ResizeObserver | undefined
let dialogObserver: ResizeObserver | undefined
let positionedMainSource = ''
let dialogPositioned = false

function readNaturalWidth(markup: string) {
  const document = new DOMParser().parseFromString(markup, 'image/svg+xml')
  const svgElement = document.documentElement
  const viewBox = svgElement.getAttribute('viewBox')?.trim().split(/[ ,]+/).map(Number)
  if (viewBox?.length === 4 && Number.isFinite(viewBox[2]) && viewBox[2] > 0) return viewBox[2]
  const width = Number.parseFloat(svgElement.getAttribute('width') ?? '')
  return Number.isFinite(width) && width > 0 ? width : 1
}

function centerTopDownEntry(container: HTMLElement, source: string, isCurrent: () => boolean, positioned: () => void) {
  if (!/^\s*(flowchart|graph)\s+(TD|TB)\b/.test(source)) {
    positioned()
    return
  }
  requestAnimationFrame(() => requestAnimationFrame(() => {
    if (!isCurrent()) return
    const node = container.querySelector<SVGGraphicsElement>('svg .node')
    if (node && container.scrollWidth > container.clientWidth) {
      const entry = node.getBoundingClientRect()
      const view = container.getBoundingClientRect()
      const scrollTop = container.scrollTop
      container.scrollLeft += entry.left + entry.width / 2 - view.left - container.clientWidth / 2
      container.scrollTop = scrollTop
    }
    positioned()
  }))
}

function updateScrollHint() {
  if (!canvas.value) {
    canScroll.value = false
    return
  }
  canScroll.value = canvas.value.scrollWidth > canvas.value.clientWidth + 1
}

function scheduleScrollHint() {
  requestAnimationFrame(() => requestAnimationFrame(updateScrollHint))
}

function observeCanvas() {
  canvasObserver?.disconnect()
  if (!canvas.value) return
  const update = () => {
    viewportWidth.value = Math.max(1, canvas.value?.clientWidth ?? 1)
    scheduleScrollHint()
  }
  update()
  canvasObserver = new ResizeObserver(update)
  canvasObserver.observe(canvas.value)
}

function observeDialog() {
  dialogObserver?.disconnect()
  if (!dialogCanvas.value) return
  const update = () => {
    dialogViewportWidth.value = Math.max(1, dialogCanvas.value?.clientWidth ?? 1)
  }
  update()
  dialogObserver = new ResizeObserver(update)
  dialogObserver.observe(dialogCanvas.value)
}

async function render() {
  const request = ++generation
  loading.value = true
  error.value = ''
  try {
    const result = await queueMermaidRender(decodedSource.value, `${diagramId}-${request}`, isDark.value)
    if (request !== generation) return
    naturalWidth.value = readNaturalWidth(result.svg)
    svg.value = result.svg
    bindFunctions = result.bindFunctions
    loading.value = false
    await nextTick()
    if (request !== generation) return
    observeCanvas()
    if (canvas.value && bindFunctions) bindFunctions(canvas.value)
    const source = decodedSource.value
    if (canvas.value && positionedMainSource !== source) {
      centerTopDownEntry(canvas.value, source, () => request === generation, () => {
        if (request === generation) positionedMainSource = source
      })
    }
  } catch (reason) {
    if (request !== generation) return
    svg.value = ''
    error.value = reason instanceof Error ? reason.message : '图表渲染失败'
  } finally {
    if (request === generation) loading.value = false
  }
}

function setZoom(next: number) {
  zoom.value = Math.min(2.4, Math.max(0.65, next))
}

function setMode(next: 'readable' | 'fit') {
  mode.value = next
  zoom.value = 1
}

async function renderDialog() {
  if (!dialog.value?.open) return
  const request = ++dialogGeneration
  dialogSvg.value = ''
  dialogError.value = ''
  dialogLoading.value = true
  try {
    const result = await queueMermaidRender(decodedSource.value, `${diagramId}-dialog-${request}`, isDark.value)
    if (request !== dialogGeneration || !dialog.value?.open) return
    naturalWidth.value = readNaturalWidth(result.svg)
    dialogSvg.value = result.svg
    await nextTick()
    if (request === dialogGeneration) {
      observeDialog()
      if (dialogCanvas.value && !dialogPositioned) {
        centerTopDownEntry(dialogCanvas.value, decodedSource.value, () => request === dialogGeneration && !!dialog.value?.open, () => {
          if (request === dialogGeneration) dialogPositioned = true
        })
      }
    }
  } catch (reason) {
    if (request !== dialogGeneration) return
    dialogError.value = reason instanceof Error ? reason.message : '展开视图渲染失败'
  } finally {
    if (request === dialogGeneration) dialogLoading.value = false
  }
}

async function expand(event: Event) {
  opener.value = event.currentTarget as HTMLElement
  dialogPositioned = false
  dialog.value?.showModal()
  await nextTick()
  dialog.value?.querySelector<HTMLElement>('[data-dialog-close]')?.focus()
  await renderDialog()
}

function closeDialog() {
  dialogGeneration += 1
  dialogPositioned = false
  dialogObserver?.disconnect()
  dialog.value?.close()
}

function restoreFocus() {
  opener.value?.focus()
}

function trapFocus(event: KeyboardEvent) {
  if (event.key === 'Escape') {
    event.preventDefault()
    closeDialog()
    return
  }
  if (event.key !== 'Tab' || !dialog.value) return
  const focusable = [...dialog.value.querySelectorAll<HTMLElement>('button, a[href], summary, [tabindex]:not([tabindex="-1"])')].filter((element) => !element.hasAttribute('disabled'))
  if (!focusable.length) return
  const first = focusable[0]
  const last = focusable[focusable.length - 1]
  if (event.shiftKey && document.activeElement === first) {
    event.preventDefault()
    last.focus()
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault()
    first.focus()
  }
}

async function refresh() {
  await render()
  if (dialog.value?.open) await renderDialog()
}

watch([isDark, () => page.value.relativePath, () => props.source], refresh)
watch(renderedWidth, scheduleScrollHint)
onMounted(render)
onBeforeUnmount(() => {
  generation += 1
  dialogGeneration += 1
  canvasObserver?.disconnect()
  dialogObserver?.disconnect()
})
</script>

<template>
  <figure class="mermaid-figure" :data-mermaid-state="loading ? 'loading' : error ? 'error' : 'ready'">
    <div class="mermaid-toolbar" aria-label="图表控制">
      <button type="button" @click="setZoom(zoom + 0.2)">放大</button>
      <button type="button" @click="setZoom(zoom - 0.2)">缩小</button>
      <button type="button" @click="setMode('fit')">适应宽度</button>
      <button type="button" @click="setMode('readable')">原始字号</button>
      <button type="button" @click="expand">展开</button>
    </div>
    <p v-if="!loading && !error && canScroll" class="mermaid-scroll-hint">可在图内横向滚动</p>
    <div v-if="loading" class="mermaid-status">正在绘制设计图…</div>
    <div v-else-if="error" class="mermaid-status mermaid-error"><strong>图表暂时没有画出来</strong><span>{{ error }}</span><button type="button" @click="render">重试</button></div>
    <div v-else ref="canvas" class="mermaid-canvas" tabindex="0" aria-label="可滚动的 Mermaid 设计图" :style="{ '--diagram-width': `${renderedWidth}px` }" v-html="svg" />
    <details class="mermaid-source"><summary>查看 Mermaid 源码</summary><pre><code>{{ decodedSource }}</code></pre></details>
    <dialog ref="dialog" class="mermaid-dialog" @close="restoreFocus" @cancel.prevent="closeDialog" @keydown="trapFocus">
      <div class="mermaid-dialog-head"><strong>设计图展开视图</strong><button type="button" data-dialog-close @click="closeDialog">关闭</button></div>
      <div v-if="dialogSvg" ref="dialogCanvas" class="mermaid-dialog-canvas" tabindex="0" aria-label="展开的可滚动 Mermaid 设计图" :style="{ '--diagram-width': `${dialogRenderedWidth}px` }" v-html="dialogSvg" />
      <div v-else-if="dialogError" class="mermaid-status mermaid-error"><strong>展开视图暂时没有画出来</strong><span>{{ dialogError }}</span><button type="button" @click="renderDialog">重试</button></div>
      <div v-else-if="dialogLoading" class="mermaid-status">正在展开设计图…</div>
    </dialog>
  </figure>
</template>
