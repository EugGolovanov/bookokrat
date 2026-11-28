# Bookokrat

Terminal EPUB/PDF/DJVU reader focused on speed, smooth navigation, and Vim-style workflows.

https://github.com/user-attachments/assets/0ebe61c6-4629-4bde-8bd4-50feb9a424a3

## Highlights

- EPUB, PDF, and DJVU support in one TUI app
- Split layout: library/TOC on the left, reader on the right
- Reading assistant: `Space` then `y` opens a tab or Zen overlay. Plain questions use [Pi](https://pi.dev); choose a model under **Settings → Integrations → Pi reader model**. `/agent` uses OMP and resumes the book’s session. See [setup and privacy](readme.txt).
- Fast PDF/DJVU pipeline with Kitty SHM image transfer in supported terminals
- Search, bookmarks, jump list history, reading stats
- Inline comments/annotations with persistent storage and Markdown export; note bodies can be hidden and peeked on demand
- Image rendering, link handling, and external viewer handoff
- Theme selection, adjustable margins, zen mode
- Vim-style keybindings and normal mode

## Install

### Homebrew (macOS)

```bash
brew install bookokrat
```

### Arch Linux (AUR)

Install from the [AUR](https://aur.archlinux.org/packages/bookokrat-bin):

```bash
yay -S bookokrat-bin
# or
paru -S bookokrat-bin
```

### Nix / NixOS (flakes)

```bash
nix run github:bugzmanov/bookokrat
# or install into profile
nix profile install github:bugzmanov/bookokrat
```

### Prebuilt Linux binaries

Download from [GitHub Releases](https://github.com/bugzmanov/bookokrat/releases).

### Cargo (all platforms)

Build from source. Requires [Rust](https://rustup.rs) and a C compiler/linker.

<details>
<summary>Prerequisites (click to expand)</summary>

**Linux (Ubuntu/Debian):**
```bash
sudo apt update
sudo apt install build-essential

# For PDF / DJVU support:
sudo apt install pkg-config libfontconfig1-dev clang libclang-dev
```

**Linux (Fedora/RHEL):**
```bash
sudo dnf install gcc make
```

**macOS:**
```bash
xcode-select --install
```

**Windows:**
Install [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio-2022) with the \"Desktop development with C++\" workload.

> **Windows notes:**
> - PDF/DJVU might not work in Windows PowerShell.
> - For full Kitty graphics protocol support, consider using WSL with [Ghostty](https://ghostty.org/) or [Kitty](https://sw.kovidgoyal.net/kitty/).
> - If MuPDF fails to build, disable PDF/DJVU support: `cargo install bookokrat --no-default-features`

</details>

```bash
cargo install bookokrat
```

Build without PDF / DJVU support:

```bash
cargo install bookokrat --no-default-features
```

## Quick Start

```bash
bookokrat
```

Optional direct open:

```bash
bookokrat path/to/book.epub
bookokrat path/to/book.pdf
bookokrat path/to/book.djvu
bookokrat path/to/book.epub --zen-mode
bookokrat path/to/book.epub --no-images   # text only; PDF and DJVU can't be opened
```

### Read or save a web article

If the welcome Help screen is open, press `Esc` to reach the library. Click **Website** in the bottom bar, or press `Space` and then `u` (also works in Zen mode). Paste an HTTP(S) article URL.

- **Save & open** (`Enter`) adds the EPUB to the active library and opens it.
- **Save only** (`Ctrl+s`) adds it without changing what you're reading.
- **Close** (`Esc`) dismisses the prompt. During a download, **Hide** (`Esc`) keeps it saving without opening it.

Choose a destination library with `bookokrat -d /path/to/library`. If your installed copy has no **Website** button outside Zen mode, run `cargo run --locked` from this checkout to launch the updated reader.

From the command line:

```bash
bookokrat 'https://example.org/'                      # save and open
bookokrat save 'https://example.org/' -o article.epub  # save only
bookokrat article.epub                                # read offline
```

Without `-o`, Bookokrat saves a URL-based `.epub` in the current directory. Opening the same URL there reuses the saved copy, even offline. To capture an updated version, use `save -o` with a new filename. Existing files are never overwritten.

The importer extracts one HTML page and embeds supported images from the same origin. It does not crawl the site, execute JavaScript, or use browser login sessions.

### Reader controls

Press `?` inside the app to open the built-in help.

EPUBs open in a two-page spread when the reader pane has at least 67 usable columns. Narrower panes show one page. Press `Space` then `D` to switch layouts, or `Space` then `z` to give the reader the full window.

By default, Bookokrat uses the terminal background and its own theme for text and highlights. For a solid background, press `Space` then `t` and select **Background → Theme color**. Existing layout and background choices are preserved.

## SyncTeX (LaTeX ↔ PDF)

Bookokrat supports bidirectional SyncTeX navigation between LaTeX sources and PDF output. Compile your LaTeX document with `pdflatex --synctex=1` (or equivalent) to generate a `.synctex.gz` sidecar file. When a PDF is opened and its sidecar is found, SyncTeX activates automatically.

**Inverse search** (PDF → source): Ctrl+click, right-click, or `gd` in normal mode jumps to the corresponding LaTeX source line. `gd` also works on selected text (the mouse selection anchor is used). Configure which editor to open in `~/.bookokrat_settings.yaml`:

```yaml
# Neovim via remote socket (recommended for VimTeX \lv workflow)
synctex_editor: "nvim --server /tmp/nvim.sock --remote-send '<C-\\><C-n>:e {file}<CR>:{line}<CR>'"

# Open in a new terminal nvim
synctex_editor: "nvim +{line} {file}"
```

Placeholders: `{file}`, `{line}`, `{column}`.

**Forward search** (source → PDF): From your editor, send a forward search command to the running instance via Unix socket. VimTeX setup:

```vim
let g:vimtex_view_method = 'general'
let g:vimtex_view_general_cmd = 'bookokrat --synctex-forward @line:@col:@tex @pdf'
```

Then press `\lv` in Neovim to jump from source to PDF. Any editor that can shell out works — the generic form is:

```bash
bookokrat --synctex-forward LINE:COLUMN:FILE path/to/document.pdf
```

The `synctex_editor` setting can also be configured in the Settings popup (`Space+s`), under the Integrations tab.

## Documentation

- Full usage and keyboard reference: [`readme.txt`](readme.txt)

## Terminal Notes

PDF and DJVU viewing require a graphics-capable terminal.

- Best experience: Kitty, Ghostty
- Good: iTerm2, WezTerm, Warp, Konsole (with some limitations)
- Terminals without graphics protocol support - EPUB support without images. PDFs and DJVUs are not supported

When Bookokrat uses the iTerm2 graphics protocol (including WezTerm), it keeps PDF pages as MuPDF drawing commands instead of caching full-page bitmaps. It rasterizes visible page regions as you read and reuses overlapping strips when you scroll. This is automatic: no optimizer, converted copy, or change to the original PDF is needed. Kitty, Sixel, and DjVu use their existing rendering paths.

EPUB text uses the terminal's font. In WezTerm, change `config.font` or `config.font_size` in `wezterm.lua`; Bookokrat has no separate font setting. If increasing the font size leaves room for only one page, widen the window or switch to zen mode.

For protocol details and troubleshooting, see the in-app help (`?`) and [`readme.txt`](readme.txt).

## Data Storage

Bookokrat stores state in XDG-compliant locations and keeps project directories clean.

| Data | Location |
|------|----------|
| Bookmarks | `<data_dir>/bookokrat/libraries/<library>/bookmarks.json` |
| Comments | `<data_dir>/bookokrat/libraries/<library>/comments/` |
| Image cache | `<cache_dir>/bookokrat/libraries/<library>/temp_images/` |
| Log file | `<state_dir>/bookokrat/bookokrat.log` |
| Settings | `~/.bookokrat_settings.yaml` |

Typical `<data_dir>` paths:

- macOS: `~/Library/Application Support`
- Linux: `~/.local/share`

## Vendored Dependency Notice

`tui-textarea` is vendored in `vendor/tui-textarea` because upstream is currently unmaintained and behind current `ratatui` compatibility. The vendored base comes from PR #118:
https://github.com/rhysd/tui-textarea/pull/118/changes

The native web importer adapts [Rustpub](https://github.com/gicrisf/rustpub/tree/eb1753257b8fa76337004f27db7b97639fbfb857) for use without its CLI or working-directory templates. Its MIT notice is in [`src/vendored/rustpub/LICENSE`](src/vendored/rustpub/LICENSE).

## Attribution

Bookokrat is based on [bookrat](https://github.com/dmitrysobolev/bookrat) by Dmitry Sobolev (MIT).

## License

GNU Affero General Public License v3.0 or later (AGPL-3.0-or-later).
See [`LICENSE`](LICENSE).
