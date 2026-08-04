import './styles.css'
import type {
  CanvasAddonMountContext,
  JsonValue,
  NativeConnection,
} from '../generated/mywallpaper-runtime'

interface Settings {
  showCpu: boolean
  showMemory: boolean
  showGpu: boolean
  backgroundColor: string
  textColor: string
  accentColor: string
  opacity: number
  cornerRadius: number
  blurAmount: number
}

interface DeviceSettings {
  refreshInterval: '1s' | '2s' | '5s'
}

interface SystemSample {
  kind: 'system.sample'
  capturedAtUnixMs: number
  cpu: { usagePercent: number; logicalProcessors: number }
  memory: { usedBytes: number; totalBytes: number }
  gpu: null | {
    name: string
    usagePercent: number | null
    dedicatedTotalBytes: number
  }
}

const defaults: Settings = {
  showCpu: true,
  showMemory: true,
  showGpu: true,
  backgroundColor: '#10131a',
  textColor: '#e7edf7',
  accentColor: '#61dafb',
  opacity: 0.9,
  cornerRadius: 18,
  blurAmount: 18,
}
const deviceDefaults: DeviceSettings = { refreshInterval: '1s' }

export function mount({ layer, runtime, bus }: CanvasAddonMountContext): () => void {
  const root = layer.root
  root.classList.add('monitor-root')
  root.innerHTML = `
    <main class="monitor" aria-live="polite">
      <header>
        <div><span class="eyebrow">SYSTEM</span><h1>Performance</h1></div>
        <span class="connection" data-state="connecting">Connecting</span>
      </header>
      <p class="feedback">Waiting for the native companion…</p>
      <section class="metrics" hidden>
        <article data-metric="cpu">
          <div class="metric-title"><span>CPU</span><strong data-value="cpu">—</strong></div>
          <div class="track"><i data-bar="cpu"></i></div>
          <small data-detail="cpu">— logical processors</small>
        </article>
        <article data-metric="memory">
          <div class="metric-title"><span>Memory</span><strong data-value="memory">—</strong></div>
          <div class="track"><i data-bar="memory"></i></div>
          <small data-detail="memory">—</small>
        </article>
        <article data-metric="gpu">
          <div class="metric-title"><span>GPU</span><strong data-value="gpu">—</strong></div>
          <div class="track"><i data-bar="gpu"></i></div>
          <small data-detail="gpu">—</small>
        </article>
      </section>
      <footer>Updated <time>—</time></footer>
    </main>
  `

  const monitor = required<HTMLElement>('.monitor')
  const feedback = required<HTMLElement>('.feedback')
  const metrics = required<HTMLElement>('.metrics')
  const connectionBadge = required<HTMLElement>('.connection')
  const updatedTime = required<HTMLTimeElement>('time')
  let settings = readSettings(layer.settings.get())
  let deviceSettings = readDeviceSettings(layer.deviceSettings.get())
  let latestSample: SystemSample | null = null
  let nativeConnection: NativeConnection | null = null
  let disposed = false

  applySettings(settings)
  const stopSettings = layer.settings.subscribe((next) => {
    settings = readSettings(next)
    applySettings(settings)
    if (latestSample) renderSample(latestSample)
  })
  const stopDeviceSettings = layer.deviceSettings.subscribe((next) => {
    deviceSettings = readDeviceSettings(next)
  })
  void connectNative()

  const freshnessTimer = window.setInterval(() => {
    if (!latestSample || nativeConnection?.state !== 'open') return
    const staleAfter = intervalMs(deviceSettings.refreshInterval) * 3 + 1_000
    if (Date.now() - latestSample.capturedAtUnixMs > staleAfter) {
      showFeedback('Live hardware data stopped updating. Retry the add-on if this persists.', 'warning')
    }
  }, 1_000)

  async function connectNative(): Promise<void> {
    try {
      // The verified companion is attached after Canvas mounts the layer.
      // `connect()` deliberately waits across that reconciliation boundary;
      // a one-time `available` read here would turn a normal startup race into
      // a permanent false-negative.
      const connection = await layer.native.companion.connect()
      if (disposed) {
        connection.close()
        return
      }
      nativeConnection = connection
      connection.onStateChange((state) => {
        if (state === 'open') {
          setConnection('open', 'Live')
          if (!latestSample) showFeedback('Connected. Waiting for the first hardware sample…', 'neutral')
        } else if (state === 'reconnecting') {
          setConnection('connecting', 'Reconnecting')
          showFeedback('The native companion is restarting. Existing values may be stale.', 'warning')
        } else {
          setConnection('closed', state === 'failed' ? 'Failed' : 'Closed')
          showFeedback('Native monitoring stopped. Open Settings → Add-ons for the runtime cause and retry action.', 'error')
        }
      })
      connection.onMessage(receiveNativeMessage)
    } catch (error) {
      setConnection('closed', 'Failed')
      showFeedback(`Native monitoring could not start: ${error instanceof Error ? error.message : String(error)}`, 'error')
    }
  }

  function receiveNativeMessage(payload: JsonValue): void {
    if (!isRecord(payload)) return
    if (payload['kind'] === 'system.error' && typeof payload['message'] === 'string') {
      showFeedback(`Hardware sampling failed: ${payload['message']}`, 'error')
      return
    }
    if (!isSystemSample(payload)) return
    const sample = payload as unknown as SystemSample
    latestSample = sample
    if (runtime.instance.canonical) bus.emit('mywallpaper.hardware/v1/metrics', payload)
    renderSample(sample)
  }

  function renderSample(sample: SystemSample): void {
    metrics.hidden = false
    feedback.hidden = true
    renderMetricVisibility('cpu', settings.showCpu)
    renderMetricVisibility('memory', settings.showMemory)
    renderMetricVisibility('gpu', settings.showGpu)

    const cpu = clamp(sample.cpu.usagePercent, 0, 100)
    setText('[data-value="cpu"]', `${cpu.toFixed(0)}%`)
    setText('[data-detail="cpu"]', `${sample.cpu.logicalProcessors} logical processors`)
    setBar('cpu', cpu)

    const memoryPercent = percentage(sample.memory.usedBytes, sample.memory.totalBytes)
    setText('[data-value="memory"]', `${memoryPercent.toFixed(0)}%`)
    setText('[data-detail="memory"]', `${formatBytes(sample.memory.usedBytes)} of ${formatBytes(sample.memory.totalBytes)}`)
    setBar('memory', memoryPercent)

    if (sample.gpu) {
      const gpuPercent = sample.gpu.usagePercent
      setText('[data-value="gpu"]', gpuPercent === null ? 'N/A' : `${gpuPercent.toFixed(0)}%`)
      setText('[data-detail="gpu"]', `${sample.gpu.name} · ${formatBytes(sample.gpu.dedicatedTotalBytes)} dedicated memory`)
      setBar('gpu', gpuPercent ?? 0)
    } else {
      setText('[data-value="gpu"]', 'N/A')
      setText('[data-detail="gpu"]', 'No hardware DXGI adapter reported telemetry')
      setBar('gpu', 0)
    }
    updatedTime.dateTime = new Date(sample.capturedAtUnixMs).toISOString()
    updatedTime.textContent = new Date(sample.capturedAtUnixMs).toLocaleTimeString()
  }

  function applySettings(next: Settings): void {
    monitor.style.setProperty('--panel', next.backgroundColor)
    monitor.style.setProperty('--text', next.textColor)
    monitor.style.setProperty('--accent', next.accentColor)
    monitor.style.setProperty('--opacity', String(next.opacity))
    monitor.style.setProperty('--corner-radius', `${next.cornerRadius}px`)
    monitor.style.setProperty('--blur', `${next.blurAmount}px`)
  }

  function showFeedback(message: string, tone: 'neutral' | 'warning' | 'error'): void {
    feedback.hidden = false
    feedback.dataset['tone'] = tone
    feedback.textContent = message
  }

  function setConnection(state: string, label: string): void {
    connectionBadge.dataset['state'] = state
    connectionBadge.textContent = label
  }

  function renderMetricVisibility(metric: string, visible: boolean): void {
    required<HTMLElement>(`[data-metric="${metric}"]`).hidden = !visible
  }

  function setText(selector: string, value: string): void {
    required<HTMLElement>(selector).textContent = value
  }

  function setBar(metric: string, value: number): void {
    required<HTMLElement>(`[data-bar="${metric}"]`).style.width = `${clamp(value, 0, 100)}%`
  }

  function required<TElement extends Element>(selector: string): TElement {
    const element = root.querySelector<TElement>(selector)
    if (!element) throw new Error(`System Monitor UI is missing ${selector}`)
    return element
  }

  return () => {
    disposed = true
    window.clearInterval(freshnessTimer)
    stopSettings()
    stopDeviceSettings()
    nativeConnection?.close()
    root.classList.remove('monitor-root')
    root.replaceChildren()
  }
}

function readSettings(value: Record<string, JsonValue>): Settings {
  return {
    showCpu: typeof value['showCpu'] === 'boolean' ? value['showCpu'] : defaults.showCpu,
    showMemory: typeof value['showMemory'] === 'boolean' ? value['showMemory'] : defaults.showMemory,
    showGpu: typeof value['showGpu'] === 'boolean' ? value['showGpu'] : defaults.showGpu,
    backgroundColor: typeof value['backgroundColor'] === 'string' ? value['backgroundColor'] : defaults.backgroundColor,
    textColor: typeof value['textColor'] === 'string' ? value['textColor'] : defaults.textColor,
    accentColor: typeof value['accentColor'] === 'string' ? value['accentColor'] : defaults.accentColor,
    opacity: typeof value['opacity'] === 'number' ? clamp(value['opacity'], 0.2, 1) : defaults.opacity,
    cornerRadius: typeof value['cornerRadius'] === 'number' ? clamp(value['cornerRadius'], 0, 48) : defaults.cornerRadius,
    blurAmount: typeof value['blurAmount'] === 'number' ? clamp(value['blurAmount'], 0, 40) : defaults.blurAmount,
  }
}

function readDeviceSettings(value: Record<string, JsonValue>): DeviceSettings {
  return {
    refreshInterval: value['refreshInterval'] === '2s' || value['refreshInterval'] === '5s'
      ? value['refreshInterval'] : deviceDefaults.refreshInterval,
  }
}

function isSystemSample(value: Record<string, JsonValue>): boolean {
  const gpu = value['gpu']
  return value['kind'] === 'system.sample'
    && typeof value['capturedAtUnixMs'] === 'number'
    && isRecord(value['cpu'])
    && typeof value['cpu']['usagePercent'] === 'number'
    && typeof value['cpu']['logicalProcessors'] === 'number'
    && isRecord(value['memory'])
    && typeof value['memory']['usedBytes'] === 'number'
    && typeof value['memory']['totalBytes'] === 'number'
    && (gpu === null || (isRecord(gpu)
      && typeof gpu['name'] === 'string'
      && (gpu['usagePercent'] === null || typeof gpu['usagePercent'] === 'number')
      && typeof gpu['dedicatedTotalBytes'] === 'number'))
}

function intervalMs(interval: DeviceSettings['refreshInterval']): number {
  return interval === '5s' ? 5_000 : interval === '2s' ? 2_000 : 1_000
}

function percentage(used: number, total: number): number {
  return total > 0 ? clamp((used / total) * 100, 0, 100) : 0
}

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1)
  return `${(bytes / 1024 ** exponent).toFixed(exponent >= 3 ? 1 : 0)} ${units[exponent]}`
}

function clamp(value: number, minimum: number, maximum: number): number {
  return Math.min(maximum, Math.max(minimum, value))
}

function isRecord(value: unknown): value is Record<string, JsonValue> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
