# XClip client

A Windows clipboard bridge for [XClip](https://github.com/RyoLee/XClip).
Copy/upload and download/paste are bound to configurable hotkeys and run
synchronously in a single process.

## Install

```
cargo build --release
```

Place `xclip.exe` anywhere; `xclip.ini` is generated next to it on first
run. The release build is a single self-contained `.exe` with no installer.

## CLI

| Command | Behaviour |
|---|---|
| `xclip.exe` | Resident: hotkey hook plus tray icon |
| `xclip.exe c` / `copy` | One-shot: capture the selection and upload |
| `xclip.exe v` / `paste` | One-shot: download and paste |
| `xclip.exe selftest` | Install/stop/reinstall the hook and report timings; exits 0 on success |

## Hotkeys

Default: press the backtick key, then `C` to upload or `V` to download and
paste. A bare backtick still types a backtick; Shift+backtick types `~`.

```ini
[hotkey]
prefix = SC029        ; SCnnn | letter | F9 | Ctrl+X ; empty = direct mode
combo_timeout = 350

[hotkey.copy]
key = C

[hotkey.paste]
key = V
```

| Form | Meaning |
|---|---|
| `SC029` | Physical scancode, layout independent. Use this for punctuation. |
| `C`, `5` | Virtual key for a letter or digit. |
| `F9`, `Space`, `PageUp`, `Menu` | Named keys (F1-F24 and the usual set). |
| `Ctrl+Shift+V` | Modifiers: `Ctrl`/`Alt`/`Shift`/`Win`, any order. |

An empty `prefix` enables direct-chord mode: an action key such as
`Ctrl+Alt+C` fires by itself, and must carry modifiers or the key would be
swallowed globally.

## Configuration

`xclip.ini` lives next to the executable. Invalid values fall back to their
defaults and are logged to `xclip.log`; a typo never prevents startup.
Edits made while running are detected and logged; use "Reload config" in the
tray menu to apply them without restarting.

| Section / key | Default | Meaning |
|---|---|---|
| `main.server` | - | Base URL of the XClip server |
| `main.id` | - | Username (hashed to the request path) |
| `main.password` | - | Password (hashed into the request token) |
| `hotkey.enabled` | `yes` | Install the keyboard hook at all |
| `hotkey.prefix` | `SC029` | Prefix key, empty for direct-chord mode |
| `hotkey.prefix_output` | backtick | Character emitted for a bare prefix |
| `hotkey.prefix_output_shift` | `~` | Character emitted with Shift held |
| `hotkey.combo_timeout` | `350` | Combination window in ms (10-5000) |
| `hotkey.restore_on_fail` | `yes` | Re-emit the prefix when an action fails |
| `hotkey.require_exact_mods` | `yes` | Reject combinations with extra modifiers |
| `hotkey.allow_repeat` | `no` | Fire repeatedly while the key is held |
| `hotkey.copy.key` | `C` | Action key for upload |
| `hotkey.copy.backend` | `auto` | `auto` / `keystroke` / `wm_copy` / `uia` |
| `hotkey.copy.capture` | `Ctrl+C` | Chord injected to make the target copy |
| `hotkey.copy.timeout` | `600` | How long to wait for the clipboard to change (50-10000) |
| `hotkey.copy.verify_focus` | `yes` | Abort if the foreground window changed |
| `hotkey.paste.key` | `V` | Action key for download and paste |
| `hotkey.paste.backend` | `auto` | `auto` / `keystroke` / `wm_paste` |
| `hotkey.paste.paste_chord` | `Ctrl+V` | Chord injected to paste |
| `hotkey.paste.settle` | `20` | Delay before injecting the paste chord (<=1000) |
| `hotkey.paste.verify_focus` | `yes` | Abort if the foreground window changed |
| `action.http_timeout` | `3000` | Request timeout in ms (100-60000) |
| `action.feedback` | `toast` | `toast`, `beep` or `none` |

## Environment variables

| Variable | Purpose |
|---|---|
| `XCLIP_INSTANCE_SUFFIX` | Run a second instance (distinct single-instance name). Useful for testing, or to start fresh when a wedged process still holds the mutex. |
| `XCLIP_TEST_INPUT` | Accept synthetic keystrokes in the hook. Automated tests only; leave unset normally, otherwise foreign injected input can trigger actions. |

## Tests

```
cargo test
```

Covers key parsing, configuration validation and fallbacks, the prefix state
machine, the clipboard sequence guard, the HTTP client against an in-process
fake server, and the ordering contract (nothing pasted before a successful
download, nothing uploaded before the clipboard changed).

## Licence

BSD 3-Clause License. Copyright (c) 2021, Lee. See [LICENSE](LICENSE) for the full text.
