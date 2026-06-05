# WSL Terminal — feature TODO

## ✅ Implemented

- [x] Real Linux PTY in WSL over **vsock** (`wslptyd`: auto-start, shared by all
      windows, auto-exit; `wslg` pipe fallback) — a local multiplexer.
- [x] **GPU rendering** — Direct2D/DirectWrite into a DirectComposition swapchain:
      true per-pixel transparency, **color emoji**, **true color**; CPU fallback.
- [x] Font family config + font **fallback**; font zoom (`Ctrl+=` / `Ctrl+-` / `Ctrl+0`).
- [x] **Tabs** (`Ctrl+Shift+T`), next/prev (`Ctrl+Tab` / `Ctrl+Shift+Tab`).
- [x] **Split panes** (`Alt+Shift+=` / `Alt+Shift+-`).
- [x] **Multiple windows** (`Ctrl+Shift+N`), opens in the current directory.
- [x] File **sidebar** (icons, browse-on-click, right-click → Open / Open in new window).
- [x] **Scrollback** — draggable hover-expand scrollbar, wheel, `Shift+PageUp/PageDown`.
- [x] **xterm mouse selection**, double-click word select, paste via `Shift+Insert`,
      **bracketed paste**.
- [x] **SGR mouse reporting** (works in vim/tmux/zellij) — DEC modes 9/1000/1002/1003/1006.
- [x] **OSC 52 clipboard** writes (zellij/tmux `copy_on_select`, vim OSC52 yank).
- [x] OSC 7 working-directory tracking (new tabs/windows inherit the cwd).
- [x] **Background image** (fit modes) + background **opacity/transparency**.
- [x] Tab-tile borders; reserved-width scrollbar; bottom margin; **Tux** app icon.
- [x] Open files in a configurable **`Editor`** in a new tab; `Ctrl+,` edits `settings.json`.
- [x] `settings.json` config (reloads on `Ctrl+,` close).
- [x] **`--cd` accepts Windows paths** (`C:\Users` → `/mnt/c/Users`) + Explorer
      "Open WSL in here" (`openinwsl.reg`).
- [x] GitHub Actions **CI** (build + test on `rust/`/`native/` changes; the
      Windows job runs `cargo test --workspace`).
- [x] **Hyperlinks** — auto-detect http(s) URLs; Ctrl-hover underlines + hand
      cursor, Ctrl-click opens in the default browser. (OSC 8 still a follow-up.)
- [x] **Searchable scrollback** (`Ctrl+Shift+F`) — case-insensitive, match
      highlight (current match brighter), Enter / Shift+Enter to step
      next / previous (also ↓/↑), Esc to close.
- [x] **Shell integration** — OSC 7 (cwd) + OSC 133 (prompt/exit marks) via
      `assets/shell-integration.sh` (bash/zsh): `Ctrl+Shift+Up/Down` jump to
      prompt, red scrollbar ticks for failed commands. (Auto-install + OSC 133
      B/C output-selection are follow-ups.)

## ⬜ Planned (not yet implemented)

- [ ] **Render the remaining text attributes** — underline, double-underline,
      italic, strikethrough, faint, curly/undercurl. *Parsed today but not drawn*
      (renderer only does bold-as-color + reverse). **S–M** — renderer in
      `rust/wslterm/src/main.rs` + GPU path; add DOUBLE/CURLY to `cell.rs`.
- [ ] **OSC 8 hyperlinks** — app-emitted links (`ls --hyperlink`, delta, gcc
      errors). Needs a per-cell link id + URI pool. **M** — `cell.rs` + `parser.rs`.
- [ ] **Config hot-reload** — watch `settings.json` and apply on save (today it
      only reloads on `Ctrl+,` close). **S** — `settings.rs` + a watch thread.
- [ ] **Go-to-tab hotkeys** (`Ctrl+1`–`9`). **S** — `handle_key`.
- [ ] **Dynamic color schemes** — named schemes + switch at runtime. **S–M** —
      `settings.rs` + GUI.
- [ ] **Ligatures** — programming ligatures via DirectWrite run shaping (instead
      of per-cell glyphs). **L** — GPU renderer; trades against per-cell speed.
- [ ] **Inline images** — iTerm2 (OSC 1337), Sixel, Kitty graphics. **M–L** —
      `parser.rs` + renderer (iTerm2 easiest → Sixel → Kitty).
