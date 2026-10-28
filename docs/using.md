---
title: Using it
nav_order: 3
---

# Using it

- **Plasma:** Alt+Space opens KRunner. Results come in **vinoSearch Text**, **vinoSearch Documents** and **vinoSearch Images** groups, vinoWhisper sessions in **vinoWhisper sessions**, and `code:` queries in **vinoSearch Code**.
- **GNOME:** the overview's search, from the next login.
- **Sway, Hyprland, COSMIC and the rest:** see [below](#other-wayland-desktops).
- **Terminal:** see [the CLI](#the-cli).

Results need 3 or more characters. Enter opens the file: a text file at the matching line in your
editor, a PDF at the matching page in Okular, Evince, Papers or Zathura. Other files, and apps that
can't be told where, open at the top. [The D-Bus page](dbus.md#opening-at-the-match) has which
editors and viewers count.

## All results

KRunner's **Show all results** on any match, or the vinoSearch icon beside GNOME's results, opens the
query in a search window (needs `vinosearch-settings`): up to 100 files of each kind, photos as a grid
of thumbnails, text and documents with their snippet and line or page, filtered by kind, folder, date
and file type. Enter or a double click opens a result at its match; **Open folder** and **Copy file**
(pastes as the file in a file manager; needs `wl-copy`) are below.

`vinosearch-settings --search "query"` opens it from a key binding.

## Other Wayland desktops

`vinosearch menu` asks for a query in fuzzel, wofi, rofi, tofi or bemenu (the first it finds), shows
the results in it, and opens the one you pick. Bind it to a key:

```
# sway
bindsym $mod+s exec vinosearch menu

# Hyprland
bind = SUPER, S, exec, vinosearch menu
```

`--launcher "rofi -dmenu -i"` (or `VINOSEARCH_LAUNCHER`) picks another dmenu-style command. It asks
the running service, so a warm query is as quick as KRunner's.

## The CLI

```bash
vinosearch search "how do I rotate the signing key"
vinosearch search "code: retry a failed download"   # code only
vinosearch similar ~/Pictures/IMG_2041.jpg           # indexed photos that look like one
vinosearch read-text screenshot.png                  # the text OCR reads, as indexing reads it
vinosearch estimate ~/Development                    # how long indexing would take
vinosearch doctor                                    # check the NPU, driver and runtime
```

`vinosearch --help` lists the rest.
