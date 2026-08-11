# Diskalize

Disk space analyser for Windows. Reads the NTFS master file table directly, keeps
the index live through the USN journal, and draws everything on the GPU so the
interface never stutters.

## What it does

- **Instant scans** — the MFT is read from the raw volume, not walked directory
  by directory. A full drive takes seconds.
- **Live index** — the USN journal keeps it current; no rescanning.
- **Sunburst and treemap**, both click-through with animated zoom.
- **Search** across one drive or all of them: wildcards, `ext:`, `size:>1gb`,
  `date:`, `path:`, `type:audio`, regular expressions.
- **Search inside files**, scoped to a folder or to what a name query returned,
  with the match highlighted in the preview.
- **Preview pane** — images, video and audio (via libVLC if installed), PDF first
  pages, syntax-highlighted code, and `.nfo` art in its original code page 437.
- **Explorer integration** — "Open with Diskalize" on folders, drives and network
  locations.
- **Several windows** in one process, sharing the index and the graphics device.
- German and English, extensible with a text file (see `lang/`).

## Build and install

```bash
cargo build --release
```

Indexing runs in a Windows service, because reading a raw volume needs
LocalSystem — elevation alone is not enough on every drive. Installing it is the
only time admin rights are needed:

```bash
target\release\diskalize-service.exe --install
```

The interface offers to do this on first start. Afterwards `diskalize.exe` runs
with no elevation at all.

## Notes

- Settings live in `%APPDATA%\Diskalize\settings.txt`.
- Language files are read from `lang/` next to the executable. Copy
  `lang/_template.lang`, fill in the right-hand side, and it appears in the
  settings; anything left blank stays German.
- Network shares are walked by the interface rather than the service, which as
  LocalSystem has no credentials for them.

## Licence

MIT. See [LICENSE](LICENSE).

Idea and realisation: Ize.
