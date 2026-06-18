#!/usr/bin/env python3
"""Screenshot an ignis (ratatui) TUI to a PNG so it can actually be *seen*.

Tool output can't convey terminal colors, so for visual changes you must render
the real TUI to an image and look at it. This launches the binary in a pty,
replays timed input steps, captures the raw terminal bytes, reconstructs the
screen with pyte (a terminal emulator), and draws it with Pillow — exact
per-cell fg/bg/bold. Then `Read` the PNG to view it.

Deps: pip install pyte pillow ; a monospace TTF (DejaVuSansMono ships on Linux).

Example — capture an edit_file diff via a real model edit:
  python3 tui_shot.py --bin target/release/ignis --cwd /tmp/scratch \\
    --out /tmp/shot.png \\
    --step 'wait:1.5' \\
    --step $'type:edit greet.rs: replace "hello world" with "hi"\\r' \\
    --step 'wait:25'

Example — drive the /model picker (no network):
  python3 tui_shot.py --bin target/release/ignis --cwd /tmp/scratch --out /tmp/m.png \\
    --step 'wait:1.5' --step $'type:/model\\r' --step 'wait:0.6' \\
    --step 'key:down' --step 'key:right' --step 'wait:0.6'

Steps (repeatable, in order):
  wait:<seconds>   drain/keep reading output for N seconds
  type:<text>      send text; backslash escapes honored (\\r \\n \\t \\e)
  key:<name>       up|down|left|right|enter|esc|tab|backspace|ctrl-d|pageup|pagedown
"""
import argparse, os, pty, select, signal, struct, termios, fcntl, time, sys

KEYS = {
    "up": "\x1b[A", "down": "\x1b[B", "right": "\x1b[C", "left": "\x1b[D",
    "enter": "\r", "esc": "\x1b", "tab": "\t", "backspace": "\x7f",
    "ctrl-d": "\x04", "pageup": "\x1b[5~", "pagedown": "\x1b[6~",
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--cwd", default=os.getcwd())
    ap.add_argument("--out", required=True)
    ap.add_argument("--cols", type=int, default=100)
    ap.add_argument("--rows", type=int, default=35)
    ap.add_argument("--args", default="", help="space-separated args for the binary")
    ap.add_argument("--font", default="/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf")
    ap.add_argument("--font-bold", default="/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf")
    ap.add_argument("--step", action="append", default=[], help="wait:N | type:TXT | key:NAME")
    args = ap.parse_args()

    import pyte
    from PIL import Image, ImageDraw, ImageFont

    # Resolve before fork: the child chdir's to --cwd, so a relative --bin would
    # no longer resolve.
    bin_path = os.path.abspath(args.bin)
    cwd = os.path.abspath(args.cwd)

    pid, fd = pty.fork()
    if pid == 0:
        os.environ.setdefault("TERM", "xterm-256color")
        os.environ["COLORTERM"] = "truecolor"
        # PTY screenshot harnesses inherit the parent shell env; if the parent has
        # NO_COLOR/CI set, the captured TUI will be monochrome. Remove those so
        # screenshots reflect the real user-facing colors.
        os.environ.pop("NO_COLOR", None)
        os.environ.pop("CI", None)
        os.chdir(cwd)
        os.execv(bin_path, [bin_path, *args.args.split()])

    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", args.rows, args.cols, 0, 0))
    buf = bytearray()

    def drain(t):
        end = time.time() + t
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.2)
            if r:
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    return
                buf.extend(chunk)
                # Answer cursor-position queries (DSR ESC[6n) so an inline-viewport
                # TUI (ratatui Viewport::Inline) can initialize — a real terminal
                # replies; pyte does not.
                for _ in range(chunk.count(b"\x1b[6n")):
                    os.write(fd, b"\x1b[1;1R")

    drain(1.0)
    for step in args.step:
        kind, _, val = step.partition(":")
        if kind == "wait":
            drain(float(val))
        elif kind == "type":
            os.write(fd, val.encode().decode("unicode_escape").encode())
            drain(0.4)
        elif kind == "key":
            os.write(fd, KEYS[val].encode())
            drain(0.4)
        else:
            sys.exit(f"unknown step: {step}")

    os.write(fd, b"\x04"); time.sleep(0.3); os.write(fd, b"\x04")
    drain(1.0)
    try:
        os.kill(pid, signal.SIGKILL)
    except OSError:
        pass

    screen = pyte.Screen(args.cols, args.rows)
    pyte.ByteStream(screen).feed(bytes(buf))

    CW, CH, FS = 11, 22, 18
    reg = ImageFont.truetype(args.font, FS)
    try:
        bold = ImageFont.truetype(args.font_bold, FS)
    except OSError:
        bold = reg
    named = {"black": (0, 0, 0), "red": (243, 139, 168), "green": (166, 227, 161),
             "yellow": (249, 226, 175), "blue": (137, 180, 250), "magenta": (203, 166, 247),
             "cyan": (148, 226, 213), "white": (205, 214, 244), "brown": (250, 179, 135)}

    def color(s, default):
        if s == "default":
            return default
        if len(s) == 6:
            try:
                return tuple(int(s[i:i + 2], 16) for i in (0, 2, 4))
            except ValueError:
                pass
        return named.get(s, default)

    img = Image.new("RGB", (args.cols * CW, args.rows * CH), (17, 17, 27))
    d = ImageDraw.Draw(img)
    for y in range(args.rows):
        for x in range(args.cols):
            ch = screen.buffer[y][x]
            d.rectangle([x * CW, y * CH, x * CW + CW, y * CH + CH], fill=color(ch.bg, (17, 17, 27)))
            if ch.data and ch.data.strip():
                d.text((x * CW, y * CH - 1), ch.data,
                       font=(bold if ch.bold else reg), fill=color(ch.fg, (205, 214, 244)))
    img.save(args.out)
    print(f"wrote {args.out} ({args.cols * CW}x{args.rows * CH})")


if __name__ == "__main__":
    main()
