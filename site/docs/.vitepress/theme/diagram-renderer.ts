type MermaidRenderResult = {
  svg: string
  bindFunctions?: (element: Element) => void
}

type MermaidModule = typeof import('mermaid')

let renderQueue: Promise<void> = Promise.resolve()
let mermaidModule: Promise<MermaidModule> | undefined

function loadMermaid() {
  mermaidModule ??= import('mermaid').catch((error) => {
    mermaidModule = undefined
    throw error
  })
  return mermaidModule
}

function palette(isDark: boolean) {
  return isDark
    ? {
        background: '#1d1b23',
        primaryColor: '#29262f',
        primaryTextColor: '#f2eee8',
        primaryBorderColor: '#6f6875',
        lineColor: '#a49aa8',
        secondaryColor: '#332d37',
        tertiaryColor: '#242129',
        edgeLabelBackground: '#1d1b23',
        clusterBkg: '#242129',
        clusterBorder: '#58515e',
        noteBkgColor: '#332d37',
        noteTextColor: '#f2eee8',
        noteBorderColor: '#785f6b',
        actorBkg: '#29262f',
        actorBorder: '#806b76',
        actorTextColor: '#f2eee8',
        signalColor: '#d6a6b9',
        signalTextColor: '#f2eee8',
        labelTextColor: '#f2eee8',
      }
    : {
        background: '#f8f5ef',
        primaryColor: '#f2ece7',
        primaryTextColor: '#302c3d',
        primaryBorderColor: '#9d8993',
        lineColor: '#716875',
        secondaryColor: '#e8efeb',
        tertiaryColor: '#f5f1eb',
        edgeLabelBackground: '#f8f5ef',
        clusterBkg: '#f5f1eb',
        clusterBorder: '#c8c0ba',
        noteBkgColor: '#f2ece7',
        noteTextColor: '#302c3d',
        noteBorderColor: '#a38995',
        actorBkg: '#f2ece7',
        actorBorder: '#9d8993',
        actorTextColor: '#302c3d',
        signalColor: '#785464',
        signalTextColor: '#302c3d',
        labelTextColor: '#302c3d',
      }
}

export function queueMermaidRender(source: string, id: string, isDark: boolean) {
  return new Promise<MermaidRenderResult>((resolve, reject) => {
    const run = async () => {
      try {
        const module = await loadMermaid()
        const mermaid = module.default
        mermaid.initialize({
          startOnLoad: false,
          securityLevel: 'strict',
          theme: 'base',
          htmlLabels: false,
          suppressErrorRendering: true,
          fontFamily: 'Inter, ui-sans-serif, system-ui, sans-serif',
          flowchart: { curve: 'linear', htmlLabels: false, useMaxWidth: true },
          themeVariables: {
            ...palette(isDark),
            fontSize: '15px',
          },
        })
        resolve(await mermaid.render(id, source))
      } catch (error) {
        reject(error)
      }
    }

    renderQueue = renderQueue.then(run, run)
  })
}
