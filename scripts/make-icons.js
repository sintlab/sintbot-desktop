'use strict'
// Render the app icon (1024 px, macOS rounded square) and the menu-bar template
// icon from the SintBot mark with headless Chrome. One-off asset generator:
//   node scripts/make-icons.js <dir containing the `ws` module>
// then: npx tauri icon src-tauri/icons/app-icon.png
const { spawn } = require('node:child_process'); const fs = require('node:fs'); const os = require('node:os'); const path = require('node:path')
const WebSocket = require(path.join(path.resolve(process.argv[2]), 'ws'))
const GLYPH = 'M22.08,42.76,64,63.69l-7,3.49H43l-7-3.49v-7l-14,7v7l14,7H64l14-7v-14l-14-7,14-7v-14l-14-7v14l-14,7-14-7v-14l-14,7Z'
const DISC = 'M100,50A50,50,0,1,1,50,0,50,50,0,0,1,100,50Z'
const page = (size, body) => `data:text/html,${encodeURIComponent(`<html><body style="margin:0;background:transparent"><svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 ${size} ${size}">${body}</svg></body></html>`)}`
const jobs = [
  // Apple's icon grid: 824 px body inside 1024, corner radius ≈ 185.
  ['app-icon.png', 1024, `<rect x="100" y="100" width="824" height="824" rx="185" fill="#0d1311"/><g transform="translate(232,232) scale(5.6)"><path fill="#f3eee5" d="${GLYPH}"/></g>`],
  // Template image: alpha only. Disc with the mark cut out, 2 px breathing room.
  ['tray-template.png', 44, `<g transform="translate(3,3) scale(0.38)"><path fill="#000" fill-rule="nonzero" d="${DISC}${GLYPH}"/></g>`],
]
const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'icons-chrome-'))
const chrome = spawn('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', ['--headless=new', '--remote-debugging-port=9334',
  `--user-data-dir=${profile}`, '--no-first-run', 'about:blank'], { stdio: 'ignore' })
const sleep = ms => new Promise(r => setTimeout(r, ms))
;(async () => {
  let target
  for (let i = 0; i < 50 && !target; i++) { await sleep(200); try { target = (await (await fetch('http://127.0.0.1:9334/json')).json()).find(t => t.type === 'page') } catch {} }
  const ws = new WebSocket(target.webSocketDebuggerUrl); await new Promise(r => ws.once('open', r))
  let id = 0; const pending = new Map()
  ws.on('message', m => { const d = JSON.parse(m); if (d.id && pending.has(d.id)) { pending.get(d.id)(d.result); pending.delete(d.id) } })
  const send = (method, params = {}) => new Promise(r => { pending.set(++id, r); ws.send(JSON.stringify({ id, method, params })) })
  await send('Page.enable')
  await send('Emulation.setDefaultBackgroundColorOverride', { color: { r: 0, g: 0, b: 0, a: 0 } })
  const out = path.join(__dirname, '..', 'src-tauri', 'icons'); fs.mkdirSync(out, { recursive: true })
  for (const [name, size, body] of jobs) {
    await send('Emulation.setDeviceMetricsOverride', { width: size, height: size, deviceScaleFactor: 1, mobile: false })
    await send('Page.navigate', { url: page(size, body) }); await sleep(600)
    const shot = await send('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: size, height: size, scale: 1 } })
    fs.writeFileSync(path.join(out, name), Buffer.from(shot.data, 'base64'))
  }
  ws.close(); chrome.kill(); process.exit(0)
})().catch(e => { console.error(e); chrome.kill(); process.exit(1) })
