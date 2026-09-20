export class MockWebSocket {
  static readonly CONNECTING = 0
  static readonly OPEN = 1
  static readonly CLOSING = 2
  static readonly CLOSED = 3
  static instances: MockWebSocket[] = []

  readonly url: string
  readyState = MockWebSocket.CONNECTING
  onopen: ((event: Event) => void) | null = null
  onmessage: ((event: MessageEvent) => void) | null = null
  onerror: ((event: Event) => void) | null = null
  onclose: ((event: CloseEvent) => void) | null = null

  constructor(url: string | URL) {
    this.url = String(url)
    MockWebSocket.instances.push(this)
  }

  open() {
    this.readyState = MockWebSocket.OPEN
    this.onopen?.(new Event('open'))
  }

  message(data: string) {
    this.onmessage?.(new MessageEvent('message', { data }))
  }

  fail() {
    this.onerror?.(new Event('error'))
  }

  disconnect(reason = '') {
    this.readyState = MockWebSocket.CLOSED
    this.onclose?.(new CloseEvent('close', { reason }))
  }

  close() {
    this.readyState = MockWebSocket.CLOSED
  }

  static reset() {
    MockWebSocket.instances = []
  }
}
