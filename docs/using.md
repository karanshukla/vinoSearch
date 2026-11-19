---
title: Using it
nav_order: 3
---

# Using it

- **KDE Plasma:** Press `Alt+Space` to open KRunner. Results appear under **vinoSearch Text**, **vinoSearch Documents**, **vinoSearch Images**, **vinoWhisper sessions**, and (for `code:` queries) **vinoSearch Code**.
- **GNOME:** Search directly from the GNOME Shell overview (available starting on the next login).
- **Sway, Hyprland, COSMIC, and other Wayland desktops:** See [Other Wayland desktops](#other-wayland-desktops).
- **Terminal:** See [The CLI](#the-cli).

Queries require at least 3 characters. Pressing `Enter` opens the matched file directly at the relevant location:
- Text files open at the matching line in your configured or default editor.
- PDFs open at the matching page in Okular, Evince, Papers, or Zathura.
- Audio and video files play from the matching timestamp in mpv, VLC, or Celluloid.
- vinoWhisper sessions created from a recording play that recording from the matched timestamp when the recording is also indexed; choose **Open transcript** to open the text instead.
- Other file types and players that do not accept a start time on the command line (such as Haruna, Totem, and Showtime) open at the beginning.

See [Opening at the match](dbus.md#opening-at-the-match) for the full list of supported editors, viewers, and players.

## All results

Select **Show all results** on any KRunner match, or click the vinoSearch icon next to GNOME Shell results, to open the full search window (requires `vinosearch-settings`). It displays up to 100 files per category—photos in a thumbnail grid, and text/documents with snippets and line or page numbers—along with filters for category, folder, date, and file type.

Press `Enter` or double-click an entry to open it at the match. Action buttons at the bottom provide **Open folder** and **Copy file** (which copies the file URI for pasting in a file manager; requires `wl-copy`).

You can also open the search window directly from a shortcut with `vinosearch-settings --search "query"`.

## Other Wayland desktops

`vinosearch menu` prompts for a query using the first available dmenu-style launcher (`fuzzel`, `wofi`, `rofi`, `tofi`, or `bemenu`), displays matching results, and opens your selection. Bind it to a shortcut in your compositor configuration:

```
# sway
bindsym $mod+s exec vinosearch menu

# Hyprland
bind = SUPER, S, exec, vinosearch menu
```

Pass `--launcher "rofi -dmenu -i"` (or set `VINOSEARCH_LAUNCHER`) to specify a custom dmenu command. Because it queries the running background service, warm queries match KRunner's latency.

## The CLI

```bash
vinosearch search "how do I rotate the signing key"
vinosearch search "code: retry a failed download"   # search source code only
vinosearch similar ~/Pictures/IMG_2041.jpg           # find visually similar indexed photos
vinosearch read-text screenshot.png                  # print the OCR text extracted during indexing
vinosearch estimate ~/Development                    # estimate indexing duration and size
vinosearch doctor                                    # verify NPU, driver, and runtime
```

Run `vinosearch --help` to see all available subcommands and flags.
