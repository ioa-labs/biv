# Better Image View

A deliberately small, keyboard-first image viewer for Linux and an experimental
Surface component.

The first milestone is optimized for launching a file from Midnight Commander and
moving through large images on a NAS without waiting for every key press:

- native GTK4 window;
- screen-sized, shrink-on-load decoding through libvips;
- background loading away from GTK's main thread;
- bounded decoded-image cache;
- predictive preloading in the current navigation direction.

Future image actions such as similarity search, OCR, and description should remain
separate services. The viewer will invoke them for the current image through a narrow
command interface, which can later become a Nexus client.

## Build

Ubuntu packages:

```bash
sudo apt install rustc cargo build-essential pkg-config libgtk-4-dev libvips-dev
```

Build and run:

```bash
cargo build --release
target/release/better-image-view /path/to/image.jpg
```

If [`just`](https://just.systems/) is installed, the project also provides short,
repeatable commands for the usual options:

```bash
just build
just run /path/to/image.jpg
just debug-print /path/to/image.jpg
just check
just test
```

Run `just` without arguments to list all available recipes.

Keys and controls:

- Page Down, Space, or mouse wheel down: next image
- Page Up, Backspace, or mouse wheel up: previous image
- Left / Right at default zoom: previous / next image
- Arrow keys while zoomed in: pan the image
- Home / End: first / last image
- `+` / `-`: zoom in / out
- `0`: fit image to window
- `F`: toggle fullscreen (the viewer starts fullscreen)
- `I`: toggle a minimal filename, resolution, and file-type overlay
- `E`: toggle quick editing with rotate, downsize, and Save Copy
- `P`: toggle the right-side metadata and EXIF panel
- Ctrl+P: open the print dialog for the current source image
- Delete: check whether Trash is available, then show one confirmation for either
  recoverable “Move to Trash” or “Delete Permanently”; the delete action is the default
  button, and an optional checkbox can remember confirmation for the current session
- Right-click: Open With…, Copy Bitmap, or Copy Filename
- Escape or Q: quit

Enlargement uses nearest-neighbor filtering so individual pixels remain crisp.
Fit-to-window and reduction use smooth filtering.
By default, small images remain at their natural size while oversized images shrink
to fit the viewport.

Quick Edit is non-destructive. Rotation previews use the already decoded display image;
Save Copy applies the selected rotation and maximum-edge downsize to the full-resolution
source on a background worker. The source file is never changed.

Printing drives the in-process GTK print dialog (`GtkPrintUnixDialog`) directly and
submits the rendered page as a print job. On current GTK, `GtkPrintOperation` routes
through the desktop print portal, and the portal dialog cannot embed custom tabs — the
in-process dialog is the only way to get a GIMP-style “Image Settings” tab. That tab
contains the live paper preview, fit-to-page and image-DPI controls, and placement
controls: horizontal/vertical alignment (corners, edges, center), millimetre X/Y
offsets, and dragging the image directly on the preview; the standard tabs
in the same dialog own printer, paper, and orientation settings, and changes there update
the preview in place. Embedded DPI is used when it looks trustworthy; otherwise the
fallback is 300 DPI. The source is decoded at the printer’s reported resolution
(fallback 300 DPI) only when the job is rendered.

For debugging, `BIV_DEBUG_PRINT=1` opens the print dialog on startup and jumps to the
Image Settings tab.

The cache defaults to 512 MiB. Override it with `BETTER_IMAGE_VIEW_CACHE_MB`.

The viewer defaults GTK to its OpenGL renderer to avoid repeated Vulkan
`VK_SUBOPTIMAL_KHR` warnings when changing images on affected Linux graphics stacks.
Set `GSK_RENDERER` explicitly to override that choice.

## Status

Early Rust + GTK4 + libvips prototype. The only source-file mutation is the explicit,
confirmed Delete action. It checks the location first and presents one appropriately
worded confirmation for either recoverable trashing or permanent deletion.
