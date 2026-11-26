// ThinkTerm chat mod: lets ThinkTerm's app follow this Claude Code session
// and talk to it from a phone. ThinkTerm puts it here and keeps it current.
//
// It only relays. While no chat is open it looks, once a second, for a
// ThinkTerm bridge listening for this process (a Unix socket in ThinkTerm's
// chat folder); when one is there it tells the bridge what happens in the
// session and does what the bridge asks: send what was said, stop the turn,
// change the model, answer a permission prompt or a question. The bridge
// turns all of it into the chat.
import type { Register } from 'claude-code'

const MOD_VERSION = 2

/** ThinkTerm's chat folder, under the home folder. */
const CHAT_DIR = '/.local/state/thinkterm/agent-chat'
/** How often the folder is looked at for a bridge, in milliseconds. */
const LOOK_EVERY = 1000
/** How long a bridge that could not be reached is left alone. */
const RETRY_AFTER = 5000
/** The most text one string carries across. */
const TEXT_KEPT = 16 << 10
/** The most of a command's output carried across: its end. */
const OUTPUT_KEPT = 8 << 10
/** The most text one string of history carries: thousands of rows go at once. */
const HISTORY_KEPT = 4 << 10
/** How long one ask for the phone's answer waits, in seconds: short, so a
 * prompt answered in the terminal, or a turn stopped, lets go soon. */
const ANSWER_WAIT = 3
/** The most rows of history one ask reads. */
const ROWS_MOST = 4096
/** The rows the bridge is told of as they are kept. */
const DOORS = ['prompt', 'command', 'response', 'tool-result', 'notice', 'compaction']
/** This mod's name, as Claude Code knows it. */
const NAME = 'thinkterm-chat'

type Link = {
  path: string
  queue: unknown[]
  sending: boolean
  alive: boolean
  acked: number
}

type Ask = {
  t: 'ask'
  key: string
  tool: string
  input: unknown
  mode?: string
  /** The prompt offers to remember the yes (rules, or a mode, for the session). */
  remember: boolean
  held: boolean
}

/** This process, as the bridge finds it: set once the session starts. */
let me: { pid: string; pane: string; mux?: string; dir: string; note: string } | undefined
/** This copy of the module: a reload is another, which the bridge tells apart. */
const copy = `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`
const links = new Map<string, Link>()
/** Bridges that could not be reached, and when to try them again. */
const failed = new Map<string, number>()
let watching = false
let looking = false
/** The main loop's running turn. */
let turnId: string | undefined
let sessionId = ''
let mode: string | undefined
let effort: string | undefined
/** The tool calls being decided, newest last: what a permission prompt is about. */
const checks: { id: string; tool: string; input: unknown; agent?: string; claimed: boolean }[] = []
/** The permission prompts and questions waiting, by key. */
const asks = new Map<string, Ask>()
/** Asks answered or dropped, which a hook still holding one lets go of. */
const settled = new Set<string>()
/** Files the phone sent, which Claude reads without asking. */
const sent = new Set<string>()
/** The commands the session takes, by name, as last listed. */
let commandNames = new Set<string>()

/**
 * The most pictures told of one call's result, and the largest (base64):
 * a result's row stays well inside what one request to a bridge carries.
 */
const PICTURES_TOLD = 3
const PICTURE_TOLD_LARGEST = 8 << 20
/** The most pictures told with history, the newest calls', and the most
 *  bytes of them (base64): the history's request stays well inside what a
 *  bridge takes, its rows besides. */
const HISTORY_PICTURES = 6
const HISTORY_PICTURES_MOST = 12 << 20
/** About the most one request to a bridge carries: what is past it waits
 *  for the next (the bridge takes at most 48 MiB). */
const REQUEST_MOST = 16 << 20

/**
 * The pictures calls gave back in a result row (a picture read, a
 * screenshot taken), by call: what `slim` leaves out of the row.
 */
function picturesOf(content: unknown): { id: string; images: unknown[] }[] {
  if (!Array.isArray(content)) return []
  const out: { id: string; images: unknown[] }[] = []
  for (const block of content as any[]) {
    if (block?.type !== 'tool_result' || typeof block.tool_use_id !== 'string' || !Array.isArray(block.content)) continue
    const images = block.content
      .filter(
        (part: any) =>
          part?.type === 'image' &&
          part.source?.type === 'base64' &&
          typeof part.source.data === 'string' &&
          part.source.data.length <= PICTURE_TOLD_LARGEST,
      )
      .slice(0, PICTURES_TOLD)
    if (images.length) out.push({ id: block.tool_use_id, images })
  }
  return out
}

/** The pictures in a call's result as history keeps it, in whatever shape. */
function imagesIn(value: unknown, depth: number, found: unknown[], most: number) {
  if (found.length >= most || depth > 6 || value === null || typeof value !== 'object') return
  if (Array.isArray(value)) {
    for (const part of value) imagesIn(part, depth + 1, found, most)
    return
  }
  const part: any = value
  if (part.type === 'image') {
    const data = part.source?.type === 'base64' ? part.source.data : part.data
    if (typeof data === 'string' && data.length <= PICTURE_TOLD_LARGEST) found.push(part)
    return
  }
  for (const inner of Object.values(part)) imagesIn(inner, depth + 1, found, most)
}

/** A value cut to what crosses: long strings shortened, pictures left out. */
function slim(value: unknown, depth: number, kept: number = TEXT_KEPT): unknown {
  if (typeof value === 'string') return value.length > kept ? value.slice(0, kept) : value
  if (value === null || typeof value !== 'object') return value
  if (depth > 8) return undefined
  if (Array.isArray(value)) return value.slice(0, 400).map((x) => slim(x, depth + 1, kept))
  const out: Record<string, unknown> = {}
  for (const [key, x] of Object.entries(value as Record<string, unknown>)) {
    if (key === 'base64' || key === 'originalFile' || key === 'originalFileContents') continue
    if (key === 'data' && typeof x === 'string' && x.length > 512) continue
    if ((key === 'stdout' || key === 'stderr') && typeof x === 'string') {
      const most = Math.min(OUTPUT_KEPT, kept)
      out[key] = x.length > most ? x.slice(x.length - most) : x
      continue
    }
    out[key] = slim(x, depth + 1, kept)
  }
  return out
}

function shortText(text: unknown): string | undefined {
  if (typeof text !== 'string') return undefined
  return text.length > OUTPUT_KEPT ? text.slice(text.length - OUTPUT_KEPT) : text
}

/** Tells every bridge. */
function push($: any, ev: unknown) {
  for (const link of links.values()) {
    if (!link.alive) continue
    link.queue.push(ev)
    void flush($, link)
  }
}

/** Tells one bridge. */
function tell($: any, link: Link, ev: unknown) {
  if (!link.alive) return
  link.queue.push(ev)
  void flush($, link)
}

/** Sends what waits for a bridge, in order, one request at a time. */
async function flush($: any, link: Link) {
  if (link.sending) return
  link.sending = true
  try {
    while (link.alive && link.queue.length) {
      // As many as fit one request: one larger than that goes alone.
      const events: string[] = []
      let size = 0
      while (link.queue.length && events.length < 500) {
        const event = JSON.stringify(link.queue[0])
        if (events.length && size + event.length > REQUEST_MOST) break
        link.queue.shift()
        events.push(event)
        size += event.length
      }
      const answer = await $.http.fetch('http://bridge/ev', {
        method: 'POST',
        socketPath: link.path,
        headers: { 'content-type': 'application/json' },
        body: `{"copy":${JSON.stringify(copy)},"events":[${events.join(',')}]}`,
      })
      // Too much for one request (one event alone, at most): left out,
      // the link kept; anything else wrong ends the link.
      if (!answer.ok && answer.status !== 413) throw new Error(`the bridge answered ${answer.status}`)
    }
  } catch (err) {
    drop(link)
  } finally {
    link.sending = false
  }
}

function drop(link: Link) {
  if (!link.alive) return
  link.alive = false
  link.queue.length = 0
  links.delete(link.path)
  failed.set(link.path, Date.now() + RETRY_AFTER)
}

/** Where this process is, and the note that says so to a bridge. */
async function setup($: any) {
  if (me) return
  const pane = await $.env.get('WEZTERM_PANE')
  const home = await $.env.get('HOME')
  // Not in a ThinkTerm pane: there is no chat to have.
  if (!pane || !home) return
  const mux = await $.env.get('WEZTERM_UNIX_SOCKET')
  const told = await $.process.hookEnv()
  const pid = told.CLAUDE_PID
  if (!pid) return
  const dir = `${home}${CHAT_DIR}`
  me = { pid, pane, mux: mux || undefined, dir, note: `${dir}/claude-${pid}.json` }
  sessionId = await $.session.id()
  await writeNote($)
}

async function writeNote($: any) {
  if (!me) return
  const version = await $.session.version()
  await $.fs.write(
    me.note,
    JSON.stringify({
      agent: 'claude',
      pid: Number(me.pid),
      pane: Number(me.pane),
      mux_socket: me.mux,
      version: MOD_VERSION,
      claude: version.version,
      session: sessionId,
    }),
  )
}

/** Looks for bridges listening for this process, and links to new ones. */
async function look($: any) {
  if (!me || looking) return
  looking = true
  try {
    if (!(await $.fs.exists(me.note))) await writeNote($)
    const prefix = `claude-${me.pid}-`
    const entries = await $.fs.list(me.dir)
    const now = Date.now()
    for (const entry of entries) {
      const name = String(entry.name)
      if (!name.startsWith(prefix) || !name.endsWith('.sock')) continue
      const path = `${me.dir}/${name}`
      if (links.has(path) || (failed.get(path) ?? 0) > now) continue
      void link($, path)
    }
  } catch (err) {
    // The folder is not there yet: no bridge has listened.
  } finally {
    looking = false
  }
}

async function link($: any, path: string) {
  const link: Link = { path, queue: [], sending: false, alive: true, acked: 0 }
  links.set(path, link)
  failed.delete(path)
  try {
    const version = await $.session.version()
    sessionId = await $.session.id()
    tell($, link, {
      t: 'hello',
      mod: MOD_VERSION,
      claude: version.version,
      session: sessionId,
      cwd: await $.session.cwd(),
      working: turnId !== undefined,
      mode,
    })
    // Held, a prompt is answered from here too; else only in the terminal.
    for (const ask of asks.values()) tell($, link, { ...ask })
    void opsFrom($, link)
    void refresh($)
  } catch (err) {
    drop(link)
  }
}

/** Takes the bridge's asks, as they come, until it goes. */
async function opsFrom($: any, link: Link) {
  while (link.alive) {
    let got: any
    try {
      const answer = await $.http.fetch(`http://bridge/ops?copy=${copy}&ack=${link.acked}`, { socketPath: link.path })
      if (!answer.ok) throw new Error(`the bridge answered ${answer.status}`)
      got = JSON.parse(answer.text)
    } catch (err) {
      drop(link)
      return
    }
    // A newer copy of this module took over (a reload): this one asks no more.
    if (got?.gone) return
    for (const op of Array.isArray(got?.ops) ? got.ops : []) {
      if (typeof op?.seq === 'number' && op.seq <= link.acked) continue
      if (typeof op?.seq === 'number') link.acked = op.seq
      void obey($, link, op)
    }
  }
}

async function obey($: any, link: Link, op: any) {
  try {
    switch (op.op) {
      case 'history':
        return await history($, link, Number(op.last) || 200)
      case 'send':
        return await say($, link, op)
      case 'interrupt':
        if (turnId === undefined) return tell($, link, { t: 'refused', message: 'nothing is running' })
        return await $.turn.abort({ turnId })
      case 'model':
        return await setModel($, link, op)
      case 'refresh':
        return await refresh($)
      default:
        return tell($, link, { t: 'refused', message: `unknown ask: ${String(op.op)}` })
    }
  } catch (err) {
    tell($, link, { t: 'refused', message: String((err as any)?.message ?? err) })
  }
}

async function history($: any, link: Link, last: number) {
  const rows = await $.session.messages({ last: Math.max(1, Math.min(ROWS_MOST, last)) })
  sessionId = await $.session.id()
  // The pictures the newest calls gave back, as Claude saw them: a few,
  // newest first, within a budget of bytes.
  const pictures = new Map<string, unknown[]>()
  let room = HISTORY_PICTURES
  let bytes = HISTORY_PICTURES_MOST
  for (const row of Array.isArray(rows) ? [...rows].reverse() : []) {
    for (const use of [...(row.toolUses ?? [])].reverse()) {
      if (room <= 0 || bytes <= 0 || typeof use.tool_use_id !== 'string') continue
      const found: unknown[] = []
      imagesIn(use.result, 0, found, Math.min(room, PICTURES_TOLD))
      const fits = found.filter((part: any) => {
        const size = String(part.source?.data ?? part.data ?? '').length
        if (size > bytes) return false
        bytes -= size
        return true
      })
      if (fits.length) {
        pictures.set(use.tool_use_id, fits)
        room -= fits.length
      }
    }
  }
  tell($, link, {
    t: 'rows',
    session: sessionId,
    asked: last,
    rows: Array.isArray(rows)
      ? rows.map((row: any) => ({
          role: row.role,
          uuid: row.uuid,
          text: slim(row.text, 0, HISTORY_KEPT),
          tools: (row.toolUses ?? []).map((use: any) => ({
            id: use.tool_use_id,
            tool: use.tool,
            input: slim(use.input, 0, HISTORY_KEPT),
            result: slim(use.result, 0, HISTORY_KEPT),
            text: slim(use.text, 0, HISTORY_KEPT),
            error: use.isError === true,
            pictures: pictures.get(use.tool_use_id),
          })),
          results: (row.toolResults ?? []).length,
        }))
      : [],
  })
}

/** What was said on the phone, as typed: a command runs; else it is a prompt. */
async function say($: any, link: Link, op: any) {
  const text = String(op.text ?? '')
  const pictures: string[] = Array.isArray(op.pictures) ? op.pictures.map(String) : []
  const files: string[] = Array.isArray(op.files) ? op.files.map(String) : []
  for (const path of pictures) sent.add(path)
  const typed = /^\/([A-Za-z][\w:.-]*)(?:\s+([\s\S]*))?$/.exec(text.trim())
  if (typed && !pictures.length && !files.length) {
    if (!commandNames.size) await listCommands($)
    const name = typed[1] ?? ''
    if (commandNames.has(name)) {
      const ran = await $.command.run({ command: name, args: typed[2] ?? '' })
      if (name === 'model' || name === 'effort') void models($)
      return ran
    }
  }
  // Files go by their paths after the words, a line each, as a person
  // pasting them would write them; Claude reads a picture to see it.
  const words = [text, ...pictures, ...files].filter((part) => part.length).join('\n')
  if (!words.trim()) return
  const taken = await $.prompt.submit({ text: words, asUser: true })
  // A hook of the session's turned it away: the phone shows it was not sent.
  if (taken && typeof taken === 'object' && 'drop' in taken) {
    tell($, link, { t: 'refused', message: `Claude Code did not take it: ${String(taken.drop)}`, dropped: true })
  }
  return taken
}

async function setModel($: any, link: Link, op: any) {
  const model = typeof op.model === 'string' ? op.model : ''
  const level = typeof op.effort === 'string' ? op.effort : ''
  if (model) {
    const done = await $.config.set({ key: 'model', value: model })
    if (done && done.deny) tell($, link, { t: 'refused', message: String(done.deny) })
  }
  if (level && level !== effort) {
    await $.command.run({ command: 'effort', args: level })
    effort = level
  }
  await models($)
}

async function listCommands($: any) {
  const list = await $.command.list()
  commandNames = new Set(list.map((c: any) => String(c.name)))
  return list
}

async function refresh($: any) {
  const list = await listCommands($)
  push($, {
    t: 'commands',
    commands: list.map((c: any) => ({
      name: String(c.name),
      description: String(c.description ?? '').slice(0, 300),
      source: c.source,
    })),
  })
  await models($)
  await usage($)
}

async function models($: any) {
  const rows = await $.config.list()
  const row = rows.find((r: any) => r.key === 'model')
  push($, {
    t: 'models',
    options: Array.isArray(row?.options) ? row.options : [],
    value: row?.value,
    model: await $.session.model(),
    effort,
  })
}

async function usage($: any) {
  const used = await $.session.usage()
  push($, { t: 'usage', tokens: used.context?.tokens, window: used.context?.window, percent: used.context?.percent })
}

/** The session was cleared or another taken up: the chat starts over. */
async function resync($: any) {
  const now = await $.session.id()
  if (now === sessionId) return
  sessionId = now
  turnId = undefined
  asks.clear()
  await writeNote($)
  push($, { t: 'reset', session: sessionId })
}

function noteMode($: any, now: unknown) {
  if (typeof now !== 'string' || now === mode) return
  mode = now
  push($, { t: 'mode', mode })
}

/** Whether `asked` (a prompt's input) is `checked` (a call's), all it says
 * the same; the call's may say more, as filled in. */
function covers(checked: any, asked: any): boolean {
  if (asked === null || typeof asked !== 'object' || checked === null || typeof checked !== 'object') {
    return JSON.stringify(checked) === JSON.stringify(asked)
  }
  return Object.keys(asked).every((key) => JSON.stringify(checked[key]) === JSON.stringify(asked[key]))
}

/** The call a permission prompt is about: the one decided last of its tool,
 * loop and input that no other prompt took. None: a key of its own. */
function callOf(e: any): string | undefined {
  const tool = String(e.tool_name ?? '')
  const agent = typeof e.agent_id === 'string' ? e.agent_id : undefined
  for (let at = checks.length - 1; at >= 0; at--) {
    const check = checks[at]
    if (!check || check.claimed || settled.has(check.id)) continue
    if (check.tool !== tool || check.agent !== agent || !covers(check.input, e.tool_input)) continue
    check.claimed = true
    return check.id
  }
  return undefined
}

let asked = 0

/** A permission prompt or a question, put to the phone while a chat is open. */
async function hold($: any, e: any, signal: AbortSignal | undefined) {
  const tool = String(e.tool_name ?? '')
  const key = callOf(e) ?? `ask-${Date.now().toString(36)}-${++asked}`
  const remember = Array.isArray(e.permission_suggestions) && e.permission_suggestions.length > 0
  const ask: Ask = { t: 'ask', key, tool, input: slim(e.tool_input, 0), mode: e.permission_mode, remember, held: true }
  noteMode($, e.permission_mode)
  asks.set(key, ask)
  if (!links.size) {
    ask.held = false
    return undefined
  }
  push($, ask)
  let turn = 0
  while (!settled.has(key) && !signal?.aborted) {
    const live = [...links.values()].filter((l) => l.alive)
    if (!live.length) break
    const link = live[turn++ % live.length]
    if (!link) break
    let got: any
    try {
      const wait = live.length > 1 ? 1 : ANSWER_WAIT
      const answer = await $.http.fetch(
        `http://bridge/answer?key=${encodeURIComponent(key)}&wait=${wait}&copy=${copy}`,
        { socketPath: link.path },
      )
      if (!answer.ok) throw new Error(`the bridge answered ${answer.status}`)
      got = JSON.parse(answer.text)
    } catch (err) {
      drop(link)
      continue
    }
    if (got?.gone) break
    const said = got?.answer
    if (!said) continue
    settle($, key)
    if (said.behavior === 'deny') {
      return { decision: { behavior: 'deny', message: String(said.message ?? 'Declined on the phone.') } }
    }
    const decision: any = { behavior: 'allow' }
    if (said.answers && typeof said.answers === 'object') decision.updatedInput = { ...e.tool_input, answers: said.answers }
    // "For this session" on the phone: what the prompt offers to remember,
    // for this session only, never written to a settings file.
    if (said.session && Array.isArray(e.permission_suggestions)) {
      decision.updatedPermissions = e.permission_suggestions.map((update: any) => ({ ...update, destination: 'session' }))
    }
    return { decision }
  }
  // Answered in the terminal, or the turn stopped: the phone's card goes.
  if (signal?.aborted) settle($, key)
  ask.held = false
  return undefined
}

function settle($: any, key: string) {
  settled.add(key)
  if (asks.delete(key)) push($, { t: 'resolved', key })
}

export const register: Register = (on) => {
  on('session.start', async ($, e, next) => {
    const started = await next(e)
    try {
      // A `claude -p` run (one a session started, say) is no chat: only the
      // session a person sees in the pane is followed.
      if (e.isInteractive) await setup($)
      if (me && !watching) {
        watching = true
        $.clock.every(LOOK_EVERY, () => look($))
        void look($)
      }
    } catch (err) {
      // Without where it is, the session goes on as it would without the mod.
    }
    return started
  })

  on('session.end', async ($, e, next) => {
    // A /clear or a /resume: the chat follows the session taken up.
    if (e.reason === 'clear' || e.reason === 'resume') {
      $.clock.after(300, () => resync($))
    }
    return next(e)
  })

  on('turn.start', async ($, e, next) => {
    if (e.agentId === undefined) {
      turnId = e.turnId
      if (links.size) {
        push($, { t: 'turn', phase: 'start', turn: e.turnId })
        void resync($)
      }
    }
    return next(e)
  })

  on('turn.step', async function* ($, e, next) {
    if (typeof e.effort === 'string' && e.agentId === undefined && e.effort !== effort) {
      effort = e.effort
      // The phone's model menu says how hard it thinks.
      if (links.size) void models($).catch(() => undefined)
    }
    if (e.agentId !== undefined || !links.size) return yield* next(e)
    const turn = e.turnId
    const step = e.index
    const stream = next(e)
    for (;;) {
      const piece = await stream.next()
      if (piece.done) return piece.value
      const chunk: any = piece.value
      if (chunk.kind === 'text' || chunk.kind === 'thinking') {
        push($, { t: 'chunk', turn, step, index: chunk.index, kind: chunk.kind, text: chunk.text })
      } else if (chunk.kind === 'tool') {
        push($, { t: 'chunk', turn, step, index: chunk.index, kind: 'tool', id: chunk.id, name: chunk.name })
      } else if (chunk.kind === 'stop') {
        push($, { t: 'step', turn, step, stop: chunk.stopReason })
      }
      yield piece.value
    }
  })

  on('turn.complete', async ($, e, next) => {
    if (e.agentId === undefined) {
      if (turnId === e.turnId) turnId = undefined
      for (const key of [...asks.keys()]) settle($, key)
      if (links.size) {
        push($, { t: 'turn', phase: 'end', turn: e.turnId, ms: e.durationMs, aborted: e.isAborted, reason: e.reason })
        void usage($).catch(() => undefined)
      }
    }
    return next(e)
  })

  on('tool.check', async ($, e, next) => {
    const input: any = e.input
    // A picture sent from the phone is read without asking: the person
    // sent it to be seen.
    if (e.tool === 'Read' && typeof input?.file_path === 'string' && sent.has(input.file_path) && e.tool_use_id) {
      return { decision: 'allow', reason: 'sent from the ThinkTerm app' }
    }
    if (e.tool_use_id) {
      checks.push({ id: e.tool_use_id, tool: e.tool, input: e.input, agent: e.agentId, claimed: false })
      if (checks.length > 64) checks.splice(0, checks.length - 64)
    }
    return next(e)
  })

  // How a call ended, with what the tool kept of it (its output, a change's
  // patch). Not a `tool.call` hook: one there, even one that only passes the
  // call on, keeps a command from moving to the background.
  on('classic.PostToolUse', async ($, e, next) => {
    const ended: any = e
    if (ended.tool_use_id) settle($, ended.tool_use_id)
    if (links.size && !ended.agent_id) {
      push($, { t: 'done', id: ended.tool_use_id, tool: ended.tool_name, result: slim(ended.tool_response, 0) })
    }
    return next(e)
  })

  on('classic.PostToolUseFailure', async ($, e, next) => {
    const ended: any = e
    if (ended.tool_use_id) settle($, ended.tool_use_id)
    if (links.size && !ended.agent_id) {
      push($, {
        t: 'done',
        id: ended.tool_use_id,
        tool: ended.tool_name,
        failed: shortText(ended.error) ?? 'failed',
        interrupted: ended.is_interrupt === true,
      })
    }
    return next(e)
  })

  // A call allowed in the terminal runs before its result is kept: the pill
  // that offers to move it to the background says it runs, so its prompt,
  // shown on the phone too, is no longer waiting there.
  on('ui.render', { component: 'ToolProgress' }, ($, e, next) => {
    const id = (e as any).props?.tool_use_id
    if (typeof id === 'string' && asks.has(id)) settle($, id)
    return next(e)
  })

  on('classic.PermissionRequest', async ($, e, next) => {
    let held: any
    try {
      held = await hold($, e, next.signal)
    } catch (err) {
      held = undefined
    }
    return held ?? next(e)
  })

  on('classic.UserPromptSubmit', async ($, e, next) => {
    noteMode($, (e as any).permission_mode)
    return next(e)
  })

  on('classic.Stop', async ($, e, next) => {
    noteMode($, (e as any).permission_mode)
    return next(e)
  })

  on('session.append', async ($, e, next) => {
    const kept: any = await next(e)
    const message: any = kept?.message ?? e.message
    // A call's result, in any loop: its prompt, if one waits, is answered.
    if (e.door === 'tool-result' && Array.isArray(message?.content)) {
      for (const block of message.content) {
        if (typeof block?.tool_use_id === 'string') settle($, block.tool_use_id)
      }
    }
    if (links.size && e.agentId === undefined && DOORS.includes(e.door)) {
      const origin: any = e.origin
      push($, {
        t: 'row',
        door: e.door,
        uuid: kept?.uuid ?? e.uuid,
        type: message.type,
        name: message.name,
        role: message.role,
        meta: message.isMeta === true,
        origin: origin?.kind,
        // This mod's own prompt (what the phone said), told apart.
        ours: origin?.kind === 'plugin' && origin?.name === NAME,
        content: slim(message.content, 0),
        pictures: e.door === 'tool-result' ? picturesOf(message.content) : undefined,
      })
    }
    return kept
  })

  on('session.compact', async ($, e, next) => {
    const done = await next(e)
    if (links.size) {
      push($, { t: 'compacted' })
      void usage($).catch(() => undefined)
    }
    return done
  })
}
