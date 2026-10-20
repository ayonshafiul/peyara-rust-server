# Shortcut Deck (`action` + `active-app`)

Phone acts as a virtual Elgato Stream Deck. The server reports the focused
app; the phone fetches a per-app button list from a public GitHub repo and
sends presses back over the existing `wss://<host>:1313/ws` socket.

Games are out of scope for v1 (see §8). The schema keeps a reserved `code`
field so v2 can add scancode/raw-input without breaking the wire.

## 1. Crate: `active-win-pos-rs@0.11`

```toml
active-win-pos-rs = "0.11"
```

Returns `ActiveWindow { title, process_path: PathBuf, app_name, window_id,
process_id: u64, position }` on Windows/macOS/Linux. Wrapped by
`src/active_app.rs` into:

```json
{"type":"active-app","app":"Google Chrome","slug":"google-chrome",
 "title":"Inbox — Gmail","process":"/usr/bin/google-chrome","pid":1234,"os":"linux"}
```

* `slug = lowercase, [^a-z0-9]+ -> "-", trim "-"`.
  `After Effects` -> `after-effects`.
* `os = std::env::consts::OS` → `windows` | `macos` | `linux`.
* Empty `app_name` (some Wayland cases) falls back to the process file stem.
* Unresolvable → `app/slug = "unknown", pid = 0` (always replies, never hangs).

Linux build needs (README apt line):
`libxcb-ewmh-dev libxcb-randr0-dev libdbus-1-dev pkg-config`.
macOS `title` is `""` without Screen Recording permission (`app_name` still works).

## 2. WebSocket messages

All frames are UTF-8 JSON text on `wss://<host>:1313/ws` (paired, same as before).

### 2.1 Server → phone (auto-push)

Immediately after `{"type":"hello","pair":"…"}` succeeds, the server sends one
`active-app` frame. No phone action needed.

### 2.2 Phone → server: `get-active-app`

```json
{"type":"get-active-app"}
```

Reply: one `active-app` frame (same shape as §2.1, `payload_or_unknown()`).
Use when the user opens the deck tab, returns to the app, or after a
configurable refresh button. No server-side polling in v1.

### 2.3 Phone → server: `action`

```json
{"type":"action","actionId":"chrome-new-tab","key":"t",
 "modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1}
```

Server: fire-and-forget (`None` reply). Parses via `parse_action()` in
`src/server.rs`, executes via `InputCommand::Shortcut` in `src/input.rs`:

* `pressDurationMs` → hold main key `0..=5000ms` (clamped, default `0` = tap).
* `pressCount` → repeat taps `1..=10` (clamped, default `1`), 20ms gap.
* Unknown `key` → ignored + logged, no disconnect.
* `code` → stored, ignored with a log line in v1 (reserved for v2).

Legacy tolerance: `modifier: "ctrl"` (string) and `modifier: ["ctrl"]`
(array) accepted like old `edit-key`; prefer `modifiers: [...]`.

## 3. JSON schemas (preset repo)

Public repo layout (phone fetches via `raw.githubusercontent.com`):

```
apps.json
google-chrome_windows.json
google-chrome_linux.json
photoshop_macos.json
...
<slug>_<os>.json
```

`os` is `windows` | `macos` | `linux` (matches `active-app.os`).

### 3.1 `apps.json` — alias map

Maps variant native names to one canonical slug so PRs fix naming without a
server release.

```json
{
  "version": 1,
  "apps": [
    {"slug":"google-chrome","label":"Chrome",
     "match":["Google Chrome","Chrome","chrome","chrome.exe","com.google.Chrome"]},
    {"slug":"firefox","label":"Firefox",
     "match":["Firefox","firefox","firefox.exe","org.mozilla.firefox"]},
    {"slug":"photoshop","label":"Photoshop",
     "match":["Adobe Photoshop","Photoshop","Photoshop.exe","com.adobe.Photoshop"]},
    {"slug":"vscode","label":"VS Code",
     "match":["Visual Studio Code","Code","code.exe","com.microsoft.VSCode"]}
  ]
}
```

Phone flow: `active-app.app` → find `match` (case-insensitive) → `slug`.
Fallback: slugify locally. Then fetch `<slug>_<os>.json`.

### 3.2 `<slug>_<os>.json` — action list

Array of buttons:

```json
[
  {"id":"chrome-new-tab","label":"New Tab","key":"t","modifiers":["ctrl"],
   "pressDurationMs":50,"pressCount":1},
  {"id":"chrome-reopen-tab","label":"Reopen Tab","key":"t",
   "modifiers":["ctrl","shift"],"pressDurationMs":50,"pressCount":1}
]
```

| field | type | required | rules |
|---|---|---|---|
| `id` | string | yes | unique per file, `^[a-z0-9-]+$`, sent back as `actionId` |
| `label` | string | yes | human button text |
| `icon` | string | no | emoji or asset name, phone-rendered |
| `key` | string | yes | §4 key names |
| `modifiers` | string[] | no (default `[]`) | §4 modifier names |
| `pressDurationMs` | int | no (default `0`) | `0..=5000`, hold before release |
| `pressCount` | int | no (default `1`) | `1..=10`, repeat taps |
| `code` | string | no | **reserved v2**, e.g. `"KeyW"`; v1 ignores |

## 4. Key / modifier names (reuse current server names)

Modifiers (`map_modifier`): `shift`, `ctrl`/`control`, `alt`/`option`,
`cmd`/`command`/`meta`/`super`/`windows`/`win`.

Keys (`map_named_key` + single chars via `resolve_edit_key`):
`a-z`, `0-9`, `f1-f12`, `backspace`, `enter`/`return`, `tab`,
`esc`/`escape`, `space`, `delete`/`del`, `home`, `end`, `pageup`,
`pagedown`, `up`/`down`/`left`/`right` (+ `uparrow`…),
`shift`, `ctrl`, `alt`, `cmd`, `capslock`,
`insert`, `pause`, `printscreen`, `scrolllock` (Unix, non-macOS),
`numlock` (non-macOS). Case-insensitive; single chars typed directly
(macOS ANSI-safe).

Media keys (`media-key` path, not `action`): `audio_mute`, `audio_vol_down`,
`audio_vol_up`, `audio_play`/`audio_pause`, `audio_prev`, `audio_next`
(+ `audio_stop` non-macOS).

## 5. Full preset examples

### 5.1 `apps.json` (root)

```json
{
  "version": 1,
  "apps": [
    {"slug":"google-chrome","label":"Chrome","match":["Google Chrome","Chrome","chrome","chrome.exe","com.google.Chrome"]},
    {"slug":"firefox","label":"Firefox","match":["Mozilla Firefox","Firefox","firefox","firefox.exe"]},
    {"slug":"photoshop","label":"Photoshop","match":["Adobe Photoshop 2025","Adobe Photoshop","Photoshop","photoshop.exe","com.adobe.Photoshop"]},
    {"slug":"illustrator","label":"Illustrator","match":["Adobe Illustrator","Illustrator","illustrator.exe"]},
    {"slug":"premiere","label":"Premiere Pro","match":["Adobe Premiere Pro","Premiere","premiere.exe"]},
    {"slug":"vscode","label":"VS Code","match":["Visual Studio Code","Code","code.exe","com.microsoft.VSCode"]},
    {"slug":"word","label":"Word","match":["Microsoft Word","Word","winword.exe","com.microsoft.Word"]},
    {"slug":"excel","label":"Excel","match":["Microsoft Excel","Excel","excel.exe"]},
    {"slug":"powerpoint","label":"PowerPoint","match":["Microsoft PowerPoint","PowerPoint","powerpnt.exe"]}
  ]
}
```

### 5.2 `google-chrome_windows.json` (also `_linux.json`, Cmd on `_macos.json`)

```json
[
  {"id":"chrome-new-tab","label":"New Tab","key":"t","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1},
  {"id":"chrome-close-tab","label":"Close Tab","key":"w","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1},
  {"id":"chrome-reopen-tab","label":"Reopen Tab","key":"t","modifiers":["ctrl","shift"],"pressDurationMs":50,"pressCount":1},
  {"id":"chrome-next-tab","label":"Next Tab","key":"tab","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1},
  {"id":"chrome-prev-tab","label":"Prev Tab","key":"tab","modifiers":["ctrl","shift"],"pressDurationMs":50,"pressCount":1},
  {"id":"chrome-address","label":"Address Bar","key":"l","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1},
  {"id":"chrome-reload","label":"Reload","key":"r","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1},
  {"id":"chrome-devtools","label":"DevTools","key":"j","modifiers":["ctrl","shift"],"pressDurationMs":80,"pressCount":1},
  {"id":"chrome-bookmark","label":"Bookmark","key":"d","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1}
]
```

### 5.3 `firefox_windows.json`

```json
[
  {"id":"ff-new-tab","label":"New Tab","key":"t","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1},
  {"id":"ff-close-tab","label":"Close Tab","key":"w","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1},
  {"id":"ff-reopen-tab","label":"Reopen Tab","key":"t","modifiers":["ctrl","shift"],"pressDurationMs":50,"pressCount":1},
  {"id":"ff-address","label":"Address Bar","key":"l","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1},
  {"id":"ff-library","label":"Library","key":"b","modifiers":["ctrl","shift"],"pressDurationMs":80,"pressCount":1}
]
```

### 5.4 `photoshop_windows.json` / `photoshop_macos.json`

```json
[
  {"id":"ps-undo","label":"Undo","key":"z","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1},
  {"id":"ps-redo","label":"Redo","key":"z","modifiers":["ctrl","shift"],"pressDurationMs":50,"pressCount":1},
  {"id":"ps-save","label":"Save","key":"s","modifiers":["ctrl"],"pressDurationMs":80,"pressCount":1},
  {"id":"ps-brush-bigger","label":"Brush +","key":"]","modifiers":[],"pressDurationMs":0,"pressCount":1},
  {"id":"ps-brush-smaller","label":"Brush −","key":"[","modifiers":[],"pressDurationMs":0,"pressCount":1},
  {"id":"ps-zoom-in","label":"Zoom In","key":"+","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1},
  {"id":"ps-zoom-out","label":"Zoom Out","key":"-","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1}
]
```

macOS variants swap `ctrl` → `cmd` per entry.

### 5.5 `vscode_windows.json`

```json
[
  {"id":"code-command","label":"Command Palette","key":"p","modifiers":["ctrl","shift"],"pressDurationMs":80,"pressCount":1},
  {"id":"code-terminal","label":"Terminal","key":"`","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1},
  {"id":"code-comment","label":"Comment","key":"/","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1},
  {"id":"code-save","label":"Save","key":"s","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1}
]
```

### 5.6 `word_windows.json` (same shape for Excel/PowerPoint)

```json
[
  {"id":"word-save","label":"Save","key":"s","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1},
  {"id":"word-bold","label":"Bold","key":"b","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1},
  {"id":"word-italic","label":"Italic","key":"i","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1},
  {"id":"word-underline","label":"Underline","key":"u","modifiers":["ctrl"],"pressDurationMs":50,"pressCount":1}
]
```

## 6. Client flow

1. Pair (`hello`) → receive `active-app`.
2. `GET https://raw.githubusercontent.com/<org>/<repo>/main/apps.json` (cache 24h).
3. Map `app` → `slug`, `GET .../<slug>_<os>.json`.
4. Render `label` (+`icon`) buttons.
5. Tap → send `action` JSON (§2.3) with matching `actionId`.
6. Optional refresh button → send `get-active-app`.

Missing file/slug → show generic deck or empty state, never crash.

## 7. Server implementation notes

* `src/active_app.rs`: `slugify()`, `os_name()`, `current()`, `payload_or_unknown()`.
* `src/server.rs`: `handle_socket` auto-pushes `active-app` via `reply_tx`;
  `dispatch()` returns `Option<Value>` (`Some` only for `get-active-app`);
  `parse_action()` clamps timing, resolves keys via `resolve_edit_key`.
* `src/input.rs`: `InputCommand::Shortcut { key, modifiers, hold, repeats, code }`;
  `execute()` presses modifiers, holds/taps, releases reverse, 20ms inter-tap gap.

## 8. Non-goals (v1)

Games / raw-input / scancode injection deferred. `code` is accepted and
ignored so v2 can implement `KEYEVENTF_SCANCODE` / XTest-scancode paths
without changing the phone payloads or preset files.
