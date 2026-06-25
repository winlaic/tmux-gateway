# tmux-gateway

`tmux-gateway` is a Rust TUI for managing tmux trees across multiple SSH hosts. It shows a unified `server -> session -> window -> pane` tree and attaches through a real `ssh -t ... tmux ...` session when you choose a window or pane.

It does not modify or inject bindings into your local tmux.

## Features

- Scan multiple SSH hosts in parallel.
- Read hosts explicitly or from `~/.ssh/config`.
- Browse remote tmux server/session/window/pane trees.
- Attach to remote windows or panes.
- Create sessions, windows, and panes.
- Kill sessions, windows, and panes with confirmation.
- Rename sessions, windows, and panes.
- Search with `/`, `?`, `n`, and `N`.
- Mouse support for tree toggling, menus, scrolling, and modal selection.
- Background auto refresh without blocking the foreground TUI.
- Configurable row format strings.
- Busy pane detection and process elapsed time display.

## Requirements

- Rust toolchain with `cargo`.
- `ssh` available locally.
- `tmux` available on remote hosts.
- Hosts must be reachable with `ssh <hostname>`.

## Build

```bash
cargo build --release
```

The binary is created at:

```bash
target/release/tmux-gateway
```

## Install

Install to your user-local bin directory:

```bash
mkdir -p ~/.local/bin
cp target/release/tmux-gateway ~/.local/bin/tmux-gateway
```

Make sure `~/.local/bin` is in `PATH`:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

Then run:

```bash
tmux-gateway
```

## Configuration

By default, `tmux-gateway` reads:

```bash
~/.config/tmux-gateway/config.toml
```

If the file does not exist, `tmux-gateway` creates an empty file there and uses built-in defaults. An empty config is valid.

To start from the full example:

```bash
mkdir -p ~/.config/tmux-gateway
cp example-configs/config.full.toml ~/.config/tmux-gateway/config.toml
```

You can also pass a config path explicitly:

```bash
tmux-gateway --config /path/to/config.toml
```

## Example Config

See [example-configs/config.full.toml](example-configs/config.full.toml) for a complete documented config.

Important options:

- `hosts`: either an array like `["t1", "t3"]` or the string `"all"` to read concrete `Host` entries from `~/.ssh/config`.
- `connect_timeout_secs`: SSH timeout in seconds.
- `scan_concurrency`: number of hosts scanned in parallel.
- `mouse_scroll_lines`: number of lines per mouse wheel tick.
- `auto_refresh_secs`: background refresh interval. Set to `0` to disable.
- `default_expand_level`: initial visible tree depth: `server`, `session`, `window`, or `pane`.
- `log_path`: optional diagnostic log path. If omitted, no log file is written.

## Row Formats

Rows are configured with format strings:

```toml
server_line_text = "[Server] {server_name}"
session_line_text = "[Session] {session_name}"
window_line_text = "[Window] {is_active}{window_index}: {window_name}"
pane_line_text = "[Pane] {is_active}{pane_index} {pane_id} {process_elapsed_time} {pane_commandline}"
```

Unknown placeholders are preserved as written.

Common placeholders:

- Server: `{server_name}`, `{host}`, `{session_count}`, `{window_count}`, `{pane_count}`, `{process_status}`, `{process_elapsed_time}`
- Session: `{server_name}`, `{host}`, `{session_name}`, `{session_uptime}`, `{window_count}`, `{pane_count}`, `{process_status}`, `{process_elapsed_time}`
- Window: `{server_name}`, `{host}`, `{session_name}`, `{session_uptime}`, `{window_index}`, `{window_name}`, `{window_panes}`, `{window_uptime}`, `{is_active}`, `{process_status}`, `{process_elapsed_time}`
- Pane: `{server_name}`, `{host}`, `{session_name}`, `{session_uptime}`, `{window_index}`, `{window_name}`, `{window_uptime}`, `{pane_index}`, `{pane_id}`, `{pane_uptime}`, `{pane_pid}`, `{pane_current_command}`, `{pane_command}`, `{pane_commandline}`, `{pane_current_path}`, `{pane_title}`, `{pane_title_prefix}`, `{is_active}`, `{process_status}`, `{process_elapsed_time}`

## Controls

- `j/k` or arrow keys: move selection.
- `h/l` or left/right arrows: collapse/expand.
- `gg` / `G`: jump to top/bottom.
- `Ctrl-u` / `Ctrl-d`: half-page up/down.
- `Ctrl-b` / `Ctrl-f`: page up/down.
- `/` / `?`: search down/up.
- `n` / `N`: repeat search forward/backward.
- `Enter`: attach to selected window or pane.
- `a`: create. On ambiguous levels, opens a menu such as `new session(s)` / `new window(w)`.
- `x`: kill selected session/window/pane.
- `.`: on the `panes` page, toggle hiding idle panes.
- `r`: refresh now.
- Right click: open context menu.
- Mouse wheel: scroll the tree viewport.
- `q` or `Esc`: quit or cancel the active modal.

Menus support both selection plus Enter and direct shortcut keys shown in parentheses, such as `OK(y)` and `Cancel(n)`.

## Logs

By default, `tmux-gateway` does not write logs.

To debug failed remote tmux operations, set `log_path` in your config:

```toml
log_path = "tmux-gateway.log"
```

Then inspect recent entries:

```bash
tail -n 80 tmux-gateway.log
```
