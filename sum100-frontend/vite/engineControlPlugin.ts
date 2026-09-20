import { spawn, type ChildProcess } from 'node:child_process'
import { randomBytes, randomUUID } from 'node:crypto'
import type { IncomingMessage, ServerResponse } from 'node:http'
import { createConnection } from 'node:net'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import type { Plugin, ViteDevServer } from 'vite'

const CONTROL_PREFIX = '/__sum100/engine'
const CONTROL_HEADER = 'x-sum100-control'
const CONTROL_HEADER_VALUE = 'local-dashboard'
const ENGINE_HOST = '127.0.0.1'
const ENGINE_PORT = 8080
const START_TIMEOUT_MS = 30_000
const GRACEFUL_STOP_MS = 8_000
const TERMINATE_STOP_MS = 3_000

type LifecycleState = 'stopped' | 'starting' | 'running' | 'stopping' | 'error'

interface EngineControlStatus {
  state: LifecycleState
  managed: boolean
  apiReachable: boolean
  error: string | null
}

interface ActiveOperation {
  kind: 'start' | 'stop'
  promise: Promise<EngineControlStatus>
}

const repoRoot = resolve(fileURLToPath(new URL('../..', import.meta.url)))
const releaseBinary = join(repoRoot, 'target', 'release', 'sum100')
const liveRegistry = join(repoRoot, 'config', 'registry.live.toml')

function isLoopback(address: string | undefined) {
  return (
    address === '127.0.0.1' ||
    address === '::1' ||
    address === '::ffff:127.0.0.1'
  )
}

function isAuthorized(request: IncomingMessage) {
  if (!isLoopback(request.socket.remoteAddress)) return false
  if (request.headers[CONTROL_HEADER] !== CONTROL_HEADER_VALUE) return false

  const fetchSite = request.headers['sec-fetch-site']
  if (fetchSite && fetchSite !== 'same-origin') return false

  const origin = request.headers.origin
  const host = request.headers.host
  if (!host) return false
  if (!origin) return fetchSite === 'same-origin'

  try {
    const originUrl = new URL(origin)
    return (
      (originUrl.protocol === 'http:' || originUrl.protocol === 'https:') &&
      originUrl.host === host
    )
  } catch {
    return false
  }
}

function sendJson(
  response: ServerResponse,
  statusCode: number,
  body: EngineControlStatus | { error: string },
) {
  response.statusCode = statusCode
  response.setHeader('Content-Type', 'application/json')
  response.setHeader('Cache-Control', 'no-store')
  response.end(JSON.stringify(body))
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error)
}

function delay(milliseconds: number) {
  return new Promise<void>((resolveDelay) => {
    setTimeout(resolveDelay, milliseconds)
  })
}

function isPortOpen(timeoutMs = 250) {
  return new Promise<boolean>((resolvePort) => {
    const socket = createConnection({ host: ENGINE_HOST, port: ENGINE_PORT })
    let response = ''
    let settled = false

    const finish = (open: boolean) => {
      if (settled) return
      settled = true
      socket.destroy()
      resolvePort(open)
    }

    socket.setTimeout(timeoutMs)
    socket.once('connect', () => {
      const key = randomBytes(16).toString('base64')
      socket.write(
        [
          'GET /ws HTTP/1.1',
          `Host: ${ENGINE_HOST}:${ENGINE_PORT}`,
          'Connection: Upgrade',
          'Upgrade: websocket',
          'Sec-WebSocket-Version: 13',
          `Sec-WebSocket-Key: ${key}`,
          '',
          '',
        ].join('\r\n'),
      )
    })
    socket.on('data', (chunk) => {
      response += String(chunk)
      if (response.includes('\r\n\r\n')) {
        finish(/^HTTP\/1\.1 101\b/.test(response))
      }
    })
    socket.once('error', () => finish(false))
    socket.once('timeout', () => finish(false))
  })
}

function hasExited(process: ChildProcess) {
  return process.exitCode !== null || process.signalCode !== null
}

function waitForExit(process: ChildProcess, timeoutMs: number) {
  if (hasExited(process)) return Promise.resolve(true)

  return new Promise<boolean>((resolveExit) => {
    const timer = setTimeout(() => {
      process.removeListener('exit', onExit)
      resolveExit(false)
    }, timeoutMs)

    const onExit = () => {
      clearTimeout(timer)
      resolveExit(true)
    }

    process.once('exit', onExit)
  })
}

function signalProcess(childProcess: ChildProcess, signal: NodeJS.Signals) {
  if (!childProcess.pid || hasExited(childProcess)) return

  try {
    if (process.platform === 'win32') {
      childProcess.kill(signal)
    } else {
      // The engine is started as its own process group so its feed tasks and
      // recorder receive the same graceful shutdown signal.
      process.kill(-childProcess.pid, signal)
    }
  } catch (error) {
    const code = (error as NodeJS.ErrnoException).code
    if (code !== 'ESRCH') throw error
  }
}

async function waitForEngine(process: ChildProcess) {
  const deadline = Date.now() + START_TIMEOUT_MS
  while (Date.now() < deadline) {
    if (hasExited(process)) return false
    if (await isPortOpen()) return true
    await delay(100)
  }
  return false
}

function logProcessOutput(
  process: ChildProcess,
  server: ViteDevServer,
  prefix: string,
) {
  process.stdout?.on('data', (chunk) => {
    const output = String(chunk).trimEnd()
    if (output) server.config.logger.info(`[${prefix}] ${output}`)
  })
  process.stderr?.on('data', (chunk) => {
    const output = String(chunk).trimEnd()
    if (output) server.config.logger.info(`[${prefix}] ${output}`)
  })
}

async function buildRelease(server: ViteDevServer) {
  await new Promise<void>((resolveBuild, rejectBuild) => {
    const build = spawn('cargo', ['build', '--release', '--locked'], {
      cwd: repoRoot,
      env: process.env,
      shell: false,
      stdio: ['ignore', 'pipe', 'pipe'],
    })
    build.stdout?.on('data', (chunk) => {
      server.config.logger.info(`[engine build] ${String(chunk).trimEnd()}`)
    })
    build.stderr?.on('data', (chunk) => {
      const output = String(chunk)
      server.config.logger.info(`[engine build] ${output.trimEnd()}`)
    })
    build.once('error', rejectBuild)
    build.once('exit', (code, signal) => {
      if (code === 0) {
        resolveBuild()
        return
      }
      rejectBuild(
        new Error(
          `Engine build failed (${signal || `exit ${code ?? 'unknown'}`}); see development server logs`,
        ),
      )
    })
  })
}

export function engineControlPlugin(): Plugin {
  let child: ChildProcess | null = null
  let lifecycle: LifecycleState = 'stopped'
  let lastError: string | null = null
  let activeOperation: ActiveOperation | null = null
  let shuttingDown = false

  const snapshot = async (): Promise<EngineControlStatus> => {
    const apiReachable = await isPortOpen()
    const managed = Boolean(child && !hasExited(child))
    let state = lifecycle

    if (!managed && apiReachable && state !== 'starting' && state !== 'stopping') {
      state = 'running'
    } else if (!managed && !apiReachable && state === 'running') {
      state = 'stopped'
    }

    return {
      state,
      managed,
      apiReachable,
      error: lastError,
    }
  }

  const startInternal = async (
    server: ViteDevServer,
  ): Promise<EngineControlStatus> => {
    if (shuttingDown) throw new Error('Development server is shutting down')

    if (await isPortOpen()) {
      lifecycle = 'running'
      lastError = null
      return snapshot()
    }

    lifecycle = 'starting'
    lastError = null

    try {
      await buildRelease(server)
      if (shuttingDown) throw new Error('Development server is shutting down')

      // Another local process may have claimed the API port while Cargo built.
      if (await isPortOpen()) {
        lifecycle = 'running'
        return snapshot()
      }

      const outputDirectory = join(
        tmpdir(),
        'sum100-ui',
        `${Date.now()}-${randomUUID()}`,
      )
      const engine = spawn(
        releaseBinary,
        [
          'scan',
          '--live',
          '--prod',
          '--registry',
          liveRegistry,
          '--out',
          outputDirectory,
          '--serve',
          `${ENGINE_HOST}:${ENGINE_PORT}`,
        ],
        {
          cwd: repoRoot,
          detached: process.platform !== 'win32',
          env: process.env,
          shell: false,
          stdio: ['ignore', 'pipe', 'pipe'],
        },
      )

      child = engine
      logProcessOutput(engine, server, 'engine')
      engine.once('exit', (code, signal) => {
        if (child !== engine) return
        const expected = lifecycle === 'stopping'
        child = null
        lifecycle = expected ? 'stopped' : code === 0 ? 'stopped' : 'error'
        if (!expected && code !== 0) {
          lastError = `Engine exited unexpectedly (${signal || `exit ${code ?? 'unknown'}`})`
        }
      })

      if (!(await waitForEngine(engine))) {
        throw new Error(
          hasExited(engine)
            ? 'Engine exited before opening the dashboard API'
            : 'Timed out waiting for the dashboard API',
        )
      }

      lifecycle = 'running'
      lastError = null
      return snapshot()
    } catch (error) {
      lifecycle = 'error'
      lastError = errorMessage(error)
      if (child && !hasExited(child)) signalProcess(child, 'SIGTERM')
      throw error
    }
  }

  const stopInternal = async (): Promise<EngineControlStatus> => {
    const engine = child
    if (!engine || hasExited(engine)) {
      child = null
      if (await isPortOpen()) {
        throw new Error(
          'The engine is running but was not started by this dashboard, so it was not stopped',
        )
      }
      lifecycle = 'stopped'
      lastError = null
      return snapshot()
    }

    lifecycle = 'stopping'
    lastError = null

    signalProcess(engine, 'SIGINT')
    if (!(await waitForExit(engine, GRACEFUL_STOP_MS))) {
      signalProcess(engine, 'SIGTERM')
      if (!(await waitForExit(engine, TERMINATE_STOP_MS))) {
        signalProcess(engine, 'SIGKILL')
        await waitForExit(engine, TERMINATE_STOP_MS)
      }
    }

    if (!hasExited(engine)) {
      lifecycle = 'error'
      lastError = 'Engine process did not stop'
      throw new Error(lastError)
    }

    if (child === engine) child = null
    lifecycle = 'stopped'
    lastError = null
    return snapshot()
  }

  const start = (server: ViteDevServer) => {
    if (activeOperation) {
      if (activeOperation.kind === 'start') return activeOperation.promise
      return Promise.reject(new Error('Engine is currently stopping'))
    }

    const promise = startInternal(server).finally(() => {
      activeOperation = null
    })
    activeOperation = { kind: 'start', promise }
    return promise
  }

  const stop = () => {
    if (activeOperation) {
      if (activeOperation.kind === 'stop') return activeOperation.promise
      return Promise.reject(new Error('Engine is currently starting'))
    }

    const promise = stopInternal().finally(() => {
      activeOperation = null
    })
    activeOperation = { kind: 'stop', promise }
    return promise
  }

  const stopOnExit = () => {
    if (child && !hasExited(child)) {
      try {
        signalProcess(child, 'SIGTERM')
      } catch {
        // The process is already exiting; there is nothing useful to do here.
      }
    }
  }

  return {
    name: 'sum100-engine-control',
    apply: 'serve',
    configureServer(server) {
      process.once('exit', stopOnExit)
      server.httpServer?.once('close', () => {
        shuttingDown = true
        process.removeListener('exit', stopOnExit)
        void stopInternal().catch((error) => {
          server.config.logger.error(
            `[engine control] shutdown failed: ${errorMessage(error)}`,
          )
        })
      })

      server.middlewares.use(async (request, response, next) => {
        const requestUrl = new URL(request.url || '/', 'http://localhost')
        if (!requestUrl.pathname.startsWith(CONTROL_PREFIX)) {
          next()
          return
        }

        if (!isAuthorized(request)) {
          sendJson(response, 403, { error: 'Forbidden' })
          return
        }

        try {
          if (
            request.method === 'GET' &&
            requestUrl.pathname === `${CONTROL_PREFIX}/status`
          ) {
            sendJson(response, 200, await snapshot())
            return
          }
          if (
            request.method === 'POST' &&
            requestUrl.pathname === `${CONTROL_PREFIX}/start`
          ) {
            sendJson(response, 200, await start(server))
            return
          }
          if (
            request.method === 'POST' &&
            requestUrl.pathname === `${CONTROL_PREFIX}/stop`
          ) {
            sendJson(response, 200, await stop())
            return
          }

          sendJson(response, 404, { error: 'Not found' })
        } catch (error) {
          const message = errorMessage(error)
          server.config.logger.error(`[engine control] ${message}`)
          sendJson(response, 409, { error: message })
        }
      })
    },
  }
}
