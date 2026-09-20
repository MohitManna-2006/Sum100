import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  getEngineControlStatus,
  startEngine,
  stopEngine,
} from './engineControl'

const running = {
  state: 'running',
  managed: true,
  apiReachable: true,
  error: null,
}

describe('engine control client', () => {
  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it('uses same-origin protected control requests', async () => {
    const fetchMock = vi.fn().mockImplementation(() =>
      Promise.resolve(
        new Response(JSON.stringify(running), {
          status: 200,
        }),
      ),
    )
    vi.stubGlobal('fetch', fetchMock)

    await getEngineControlStatus()
    await startEngine()
    await stopEngine()

    expect(fetchMock).toHaveBeenNthCalledWith(
      1,
      '/__sum100/engine/status',
      expect.objectContaining({
        method: 'GET',
        headers: { 'X-Sum100-Control': 'local-dashboard' },
      }),
    )
    expect(fetchMock).toHaveBeenNthCalledWith(
      2,
      '/__sum100/engine/start',
      expect.objectContaining({ method: 'POST' }),
    )
    expect(fetchMock).toHaveBeenNthCalledWith(
      3,
      '/__sum100/engine/stop',
      expect.objectContaining({ method: 'POST' }),
    )
  })

  it('surfaces controller errors without accepting malformed state', async () => {
    vi.stubGlobal(
      'fetch',
      vi
        .fn()
        .mockResolvedValue(
          new Response(JSON.stringify({ error: 'Engine build failed' }), {
            status: 409,
          }),
        ),
    )

    await expect(startEngine()).rejects.toThrow('Engine build failed')
  })
})
