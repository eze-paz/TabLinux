// A small VT100/ANSI terminal.
//
// The demo page dumped bytes into a <pre> with the escape sequences stripped,
// which is fine for reading boot logs and useless for using a shell: no
// cursor addressing, so `clear`, line editing, vi and anything that redraws
// produce garbage. This keeps a real character grid and interprets the
// sequences busybox and Linux actually emit.
//
// Not a complete terminal — no scroll regions, no alternate screen, no
// mouse. It handles what a login shell on a serial console needs.

const DEFAULT_FG = 7;
const DEFAULT_BG = 0;

export class Terminal {
    constructor(el, { cols = 100, rows = 30, onInput = () => {} } = {}) {
        this.el = el;
        this.cols = cols;
        this.rows = rows;
        this.onInput = onInput;
        this.x = 0;
        this.y = 0;
        this.fg = DEFAULT_FG;
        this.bg = DEFAULT_BG;
        this.bold = false;
        this.reverse = false;
        // Parser state: 'text' | 'esc' | 'csi'
        this.state = "text";
        this.params = "";
        /// UTF-8 reassembly. The guest emits UTF-8 (box-drawing in a MOTD, an
        /// accented filename), so a byte >= 0x80 is part of a multibyte
        /// character, not a Latin-1 glyph. `_u8cp` accumulates the code point
        /// and `_u8need` counts the continuation bytes still to come.
        this._u8cp = 0;
        this._u8need = 0;
        /// Cursor and attributes stashed by DECSC (ESC 7), restored by ESC 8.
        this.saved = null;
        this.grid = [];
        for (let r = 0; r < rows; r++) this.grid.push(this.blankRow());
        // Keystrokes arrive at a real <textarea>, not at the grid.
        //
        // A <div tabindex=0> can take keydown on a desktop, but no amount of
        // listeners makes a phone open its keyboard for one — that needs a
        // genuinely focusable form control. This also fixes Android, where a
        // soft keyboard reports every key as 229/"Unidentified" and the only
        // way to learn what was typed is the input event.
        //
        // It lives in the parent, not in `el`: paint() replaces `el.innerHTML`
        // wholesale and would delete it on the next frame.
        const ta = document.createElement("textarea");
        ta.id = "kbd";
        ta.setAttribute("autocapitalize", "off");
        ta.setAttribute("autocorrect", "off");
        ta.setAttribute("autocomplete", "off");
        ta.setAttribute("spellcheck", "false");
        ta.setAttribute("aria-label", "Terminal input");
        (this.el.parentElement || document.body).appendChild(ta);
        this.input = ta;
        /// Whether the key bar's Ctrl is armed for the next character (sticky
        /// modifier). Consumed by the next keystroke, from the bar or this input.
        this.ctrlArmed = false;
        /// While `Date.now()` is below this, tap-to-focus is suppressed — set on a
        /// deliberate keyboard dismiss so the closing tap does not reopen it.
        this._suppressFocusUntil = 0;
        /// The visual-viewport height with the keyboard down, so its shrinking
        /// signals the keyboard opening regardless of the interactive-widget mode.
        this._vvBase = window.visualViewport ? window.visualViewport.height : 0;

        ta.addEventListener("keydown", e => this.key(e));

        // What a soft keyboard produces. Anything key() handled has already
        // called preventDefault, so the textarea never sees it and this does
        // not double-send; what reaches here is what keydown could not name.
        ta.addEventListener("input", () => {
            const v = ta.value;
            ta.value = "";
            if (!v) return;
            // Sticky Ctrl armed on the key bar: fold the next character into a
            // control code (c -> 0x03) and release it.
            if (this.ctrlArmed) {
                this.setCtrl(false);
                const b = v[v.length - 1].toUpperCase().charCodeAt(0) & 0x1f;
                this.onInput(new Uint8Array([b]));
                return;
            }
            this.onInput(new TextEncoder().encode(v));
        });
        // Backspace and Enter on a soft keyboard: the textarea is always empty,
        // so a delete produces no input event at all and Enter would arrive as
        // a newline rather than the CR a shell wants.
        ta.addEventListener("beforeinput", e => {
            if (e.inputType === "deleteContentBackward") {
                e.preventDefault();
                this.onInput(new TextEncoder().encode("\x7f"));
            } else if (e.inputType === "insertLineBreak") {
                e.preventDefault();
                this.onInput(new TextEncoder().encode("\r"));
            }
        });

        // Focus on release rather than press, and only when nothing was
        // selected — focusing on pointerdown would collapse a drag-select
        // before it could finish, which is the only way to copy.
        //
        // Three events, because summoning the soft keyboard is finicky per OS:
        // desktop mice fire `pointerup`; iOS Safari does NOT reliably present
        // the keyboard for a programmatic focus() from pointerup and wants a
        // `click` (or `touchend`); some Androids only honour `touchend`. Binding
        // all three (each guarded on selection, and focus() is idempotent) opens
        // the keyboard on every platform without double-sending anything.
        const wrap = this.el.parentElement || this.el;
        const focusIfNoSelection = () => {
            // Suppress the re-focus for a moment after a deliberate keyboard
            // dismiss: closing the keyboard grows the viewport and the tap's
            // delayed synthetic click lands on the terminal, which would
            // otherwise reopen the keyboard immediately.
            if (Date.now() < this._suppressFocusUntil) return;
            if (!this.selectionText()) this.focus();
        };
        wrap.addEventListener("pointerup", focusIfNoSelection);
        wrap.addEventListener("click", focusIfNoSelection);
        wrap.addEventListener("touchend", focusIfNoSelection);

        // Paste. A plain <div> is not editable, so the browser never fires a
        // `paste` event at it and Ctrl-V did nothing but send 0x16 to the
        // guest. Read the clipboard directly instead, and accept the two other
        // gestures people reach for: middle-click (the X11 habit) and
        // right-click, which otherwise just opens a context menu over a
        // terminal that has no use for one.
        this.input.addEventListener("paste", e => {
            e.preventDefault();
            this.pasteText(e.clipboardData?.getData("text") || "");
        });
        this.el.addEventListener("auxclick", e => {
            if (e.button === 1) { e.preventDefault(); this.pasteClipboard(); }
        });
        // Right-click copies when something is selected and pastes otherwise,
        // the PuTTY convention. Binding it to paste unconditionally, as this
        // did at first, removed the only remaining way to copy: Ctrl-C is
        // SIGINT and Chrome takes Ctrl-Shift-C for DevTools.
        this.el.addEventListener("contextmenu", e => {
            e.preventDefault();
            if (this.selectionText()) this.copySelection();
            else this.pasteClipboard();
        });

        this.buildKeyBar();
        this.paint();
    }

    /// A touch-only accessory bar of the keys a soft keyboard lacks — Esc, Tab,
    /// a sticky Ctrl, the arrows, and a button to dismiss the keyboard. It floats
    /// just above the keyboard (positioned via the visual viewport) and only
    /// exists on coarse-pointer devices, so desktop is untouched. Each key sends
    /// straight through `onInput`, and taps preventDefault on pointerdown so the
    /// textarea keeps focus (otherwise tapping a key would close the keyboard).
    buildKeyBar() {
        if (!matchMedia("(pointer: coarse)").matches) return;
        const ESC = 0x1b;
        const keys = [
            { label: "⌄", aria: "Hide keyboard", act: () => { this._suppressFocusUntil = Date.now() + 700; this.input.blur(); } },
            { label: "esc", bytes: [ESC] },
            { label: "tab", bytes: [0x09] },
            { label: "ctrl", ctrl: true },
            { label: "↑", aria: "Up", bytes: [ESC, 0x5b, 0x41] },
            { label: "↓", aria: "Down", bytes: [ESC, 0x5b, 0x42] },
            { label: "←", aria: "Left", bytes: [ESC, 0x5b, 0x44] },
            { label: "→", aria: "Right", bytes: [ESC, 0x5b, 0x43] },
        ];
        const bar = document.createElement("div");
        bar.id = "keybar";
        for (const k of keys) {
            const b = document.createElement("div");
            b.className = "kb";
            b.textContent = k.label;
            b.setAttribute("role", "button");
            if (k.aria) b.setAttribute("aria-label", k.aria);
            if (k.ctrl) this.ctrlKey = b;
            b.addEventListener("pointerdown", e => {
                // Keep focus on the textarea so the keyboard stays open.
                e.preventDefault();
                if (k.act) k.act();
                else if (k.ctrl) this.setCtrl(!this.ctrlArmed);
                else if (k.bytes) {
                    if (this.ctrlArmed) this.setCtrl(false);
                    this.onInput(new Uint8Array(k.bytes));
                }
            });
            bar.appendChild(b);
        }
        document.body.appendChild(bar);
        this.keyBar = bar;

        // Visibility tracks whether the keyboard is actually up, NOT focus:
        // the input is focused at load and stays focused after the keyboard is
        // dismissed, so keying off focus left the bar always showing. The
        // keyboard overlays the layout viewport, so a shrunken visual viewport
        // is the signal it is open.
        const update = () => this.updateKeyBar();
        this.input.addEventListener("focus", update);
        this.input.addEventListener("blur", () => { this.setCtrl(false); update(); });
        const vv = window.visualViewport;
        if (vv) {
            vv.addEventListener("resize", update);
            vv.addEventListener("scroll", update);
        }
    }

    /// Show the bar just above the keyboard while the keyboard is open, hide it
    /// otherwise. The gap below the (shrunken) visual viewport is the keyboard's
    /// height; a meaningful gap while the input holds focus means it is up. The
    /// threshold clears the ~60-90px URL-bar collapse so that alone never shows it.
    updateKeyBar() {
        if (!this.keyBar) return;
        const vv = window.visualViewport;
        if (!vv) return;
        // While unfocused the keyboard is down, so the current height is the
        // baseline (also re-captured on rotation). When focused, the keyboard is
        // open once the visual viewport has shrunk well below it — true whether
        // the keyboard overlays the viewport (resizes-visual) or shrinks it
        // (resizes-content); in both, vv.height drops when it opens.
        if (document.activeElement !== this.input) {
            this._vvBase = vv.height;
            this.keyBar.style.display = "none";
            return;
        }
        const open = this._vvBase - vv.height > 120;
        this.keyBar.style.display = open ? "flex" : "none";
        if (open) {
            const gap = window.innerHeight - vv.height - vv.offsetTop;
            this.keyBar.style.bottom = Math.max(0, gap) + "px";
        }
    }

    setCtrl(on) {
        this.ctrlArmed = on;
        if (this.ctrlKey) this.ctrlKey.classList.toggle("on", on);
    }

    blankRow() {
        return Array.from({ length: this.cols }, () => ({
            ch: " ", fg: DEFAULT_FG, bg: DEFAULT_BG, bold: false,
        }));
    }

    /** Put the caret in the input sink, opening the keyboard on a phone. */
    focus() {
        // preventScroll: the sink is pinned to the terminal's top-left, and
        // without this a focus on mobile scrolls it into view and shoves the
        // layout around.
        this.input.focus({ preventScroll: true });
    }

    /**
     * Change the grid to `cols` x `rows`, keeping what is on screen.
     *
     * The BOTTOM rows are kept, not the top: a terminal's interesting content
     * is the most recent, and preserving the head would scroll the prompt off
     * on every shrink.
     *
     * The guest is told separately (main.js sends stty). Without that it goes
     * on wrapping at whatever width it last believed, and a wider window just
     * gains empty space on the right.
     */
    resize(cols, rows) {
        cols = Math.max(20, cols | 0);
        rows = Math.max(4, rows | 0);
        if (cols === this.cols && rows === this.rows) return false;

        const old = this.grid, oldRows = this.rows, oldCols = this.cols;
        const keep = Math.min(rows, oldRows);
        const dropped = oldRows - keep;

        this.cols = cols;
        this.rows = rows;
        this.grid = Array.from({ length: rows }, () => this.blankRow());
        const copyCols = Math.min(cols, oldCols);
        for (let r = 0; r < keep; r++) {
            const src = old[dropped + r];
            for (let c = 0; c < copyCols; c++) this.grid[r][c] = src[c];
        }

        this.x = Math.min(this.x, cols - 1);
        this.y = Math.max(0, Math.min(this.y - dropped, rows - 1));
        this.paint();
        return true;
    }

    // ── output ────────────────────────────────────────────────────────────

    write(bytes) {
        for (const b of bytes) this.byte(b);
        this.schedule();
    }

    /// Coalesce a burst of output into one paint.
    ///
    /// rAF alone is not enough: a hidden or non-compositing tab never fires it,
    /// so the screen freezes at whatever was last painted while the guest keeps
    /// running. The timer is the floor that guarantees the display eventually
    /// matches the grid; whichever fires first wins and cancels the other.
    /// (This also replaced a self-perpetuating rAF loop that ran a callback
    /// every frame forever, painting or not.)
    schedule() {
        if (this.pending) return;
        this.pending = true;
        const go = () => {
            if (!this.pending) return;
            this.pending = false;
            cancelAnimationFrame(this.rafId);
            clearTimeout(this.timerId);
            this.paint();
        };
        this.rafId = requestAnimationFrame(go);
        this.timerId = setTimeout(go, 100);
    }

    byte(b) {
        // Continuation of a UTF-8 multibyte character. Valid continuation bytes
        // are 10xxxxxx; anything else means the sequence was truncated, so drop
        // it and let this byte be handled fresh.
        if (this._u8need > 0) {
            if ((b & 0xc0) === 0x80) {
                this._u8cp = (this._u8cp << 6) | (b & 0x3f);
                if (--this._u8need === 0) this.put(String.fromCodePoint(this._u8cp));
                return;
            }
            this._u8need = 0;
        }
        if (this.state === "esc") {
            this.state = "text";
            if (b === 0x5b) { this.state = "csi"; this.params = ""; }
            // DECSC / DECRC — save and restore the cursor.
            //
            // Not decoration. This is how a program redraws in place without
            // knowing where it is: save, write, restore, erase to end of line,
            // write again. apk's progress bar is exactly that, and while these
            // were being dropped the restore never happened, so every redraw
            // started where the last one finished and the bar walked down the
            // screen instead of overwriting itself.
            //
            // The saved state includes the attributes, per DEC: a program may
            // set a colour, save, reset, and restore expecting the colour back.
            else if (b === 0x37) {
                this.saved = {
                    x: this.x, y: this.y, fg: this.fg, bg: this.bg,
                    bold: this.bold, reverse: this.reverse,
                };
            } else if (b === 0x38) {
                const s = this.saved;
                if (s) {
                    // Clamped, because the grid may have been resized smaller
                    // since the save.
                    this.x = Math.min(s.x, this.cols - 1);
                    this.y = Math.min(s.y, this.rows - 1);
                    this.fg = s.fg; this.bg = s.bg;
                    this.bold = s.bold; this.reverse = s.reverse;
                }
            }
            // ESC ( charset selection and everything else: ignored.
            return;
        }
        if (this.state === "csi") {
            const c = String.fromCharCode(b);
            if ((b >= 0x30 && b <= 0x3f) || b === 0x3b) { this.params += c; return; }
            this.csi(c);
            this.state = "text";
            return;
        }
        switch (b) {
            case 0x1b: this.state = "esc"; return;
            case 0x0d: this.x = 0; return;
            case 0x0a: this.newline(); return;
            case 0x08: if (this.x > 0) this.x--; return;
            case 0x09: this.x = Math.min(this.cols - 1, (this.x + 8) & ~7); return;
            case 0x07: return;               // bell
            case 0x00: return;
            default:
                if (b < 0x20) return;        // other C0: ignore
                if (b < 0x80) { this.put(String.fromCharCode(b)); return; }
                // UTF-8 lead byte: 110xxxxx / 1110xxxx / 11110xxx start a 2/3/4
                // byte character; the low bits seed the code point, and the
                // continuation bytes above finish it. A stray continuation or an
                // invalid lead shows the replacement character rather than noise.
                if ((b & 0xe0) === 0xc0) { this._u8cp = b & 0x1f; this._u8need = 1; }
                else if ((b & 0xf0) === 0xe0) { this._u8cp = b & 0x0f; this._u8need = 2; }
                else if ((b & 0xf8) === 0xf0) { this._u8cp = b & 0x07; this._u8need = 3; }
                else this.put("�");
        }
    }

    put(ch) {
        if (this.x >= this.cols) { this.x = 0; this.newline(); }
        this.grid[this.y][this.x] = {
            ch,
            fg: this.reverse ? this.bg : this.fg,
            bg: this.reverse ? this.fg : this.bg,
            bold: this.bold,
        };
        this.x++;
    }

    newline() {
        this.y++;
        if (this.y >= this.rows) {
            this.grid.shift();
            this.grid.push(this.blankRow());
            this.y = this.rows - 1;
        }
    }

    csi(cmd) {
        const nums = this.params.replace(/^\?/, "").split(";")
            .map(s => (s === "" ? NaN : parseInt(s, 10)));
        const n = (i, dflt) => (Number.isNaN(nums[i]) || nums[i] === undefined ? dflt : nums[i]);
        switch (cmd) {
            case "A": this.y = Math.max(0, this.y - n(0, 1)); break;
            case "B": this.y = Math.min(this.rows - 1, this.y + n(0, 1)); break;
            case "C": this.x = Math.min(this.cols - 1, this.x + n(0, 1)); break;
            case "D": this.x = Math.max(0, this.x - n(0, 1)); break;
            case "G": this.x = Math.min(this.cols - 1, n(0, 1) - 1); break;
            case "H": case "f":
                this.y = Math.min(this.rows - 1, n(0, 1) - 1);
                this.x = Math.min(this.cols - 1, n(1, 1) - 1);
                break;
            case "J": this.erase(n(0, 0), true); break;
            case "K": this.erase(n(0, 0), false); break;
            case "m": this.sgr(nums); break;
            case "n":
                // Device status report: busybox asks for the cursor position
                // (CSI 6n) to find the window size. Ignoring it makes the
                // shell hang briefly and then guess; answering is one line.
                if (n(0, 0) === 6) {
                    this.onInput(new TextEncoder().encode(
                        `\x1b[${this.y + 1};${this.x + 1}R`));
                }
                break;
            default: break; // unsupported: better ignored than rendered
        }
    }

    erase(mode, whole) {
        const blank = () => ({ ch: " ", fg: this.fg, bg: this.bg, bold: false });
        if (whole) {
            if (mode === 2 || mode === 3) {
                this.grid = Array.from({ length: this.rows }, () => this.blankRow());
                this.x = this.y = 0;
                return;
            }
            const from = mode === 0 ? this.y + 1 : 0;
            const to = mode === 0 ? this.rows : this.y;
            for (let r = from; r < to; r++) this.grid[r] = this.blankRow();
        }
        const row = this.grid[this.y];
        const [a, b] = mode === 0 ? [this.x, this.cols]
            : mode === 1 ? [0, this.x + 1] : [0, this.cols];
        for (let i = a; i < b; i++) row[i] = blank();
    }

    sgr(nums) {
        if (!nums.length || (nums.length === 1 && Number.isNaN(nums[0]))) nums = [0];
        for (const raw of nums) {
            const v = Number.isNaN(raw) ? 0 : raw;
            if (v === 0) { this.fg = DEFAULT_FG; this.bg = DEFAULT_BG; this.bold = false; this.reverse = false; }
            else if (v === 1) this.bold = true;
            else if (v === 22) this.bold = false;
            else if (v === 7) this.reverse = true;
            else if (v === 27) this.reverse = false;
            else if (v >= 30 && v <= 37) this.fg = v - 30;
            else if (v === 39) this.fg = DEFAULT_FG;
            else if (v >= 40 && v <= 47) this.bg = v - 40;
            else if (v === 49) this.bg = DEFAULT_BG;
            else if (v >= 90 && v <= 97) { this.fg = v - 90; this.bold = true; }
        }
    }

    // ── rendering ─────────────────────────────────────────────────────────
    // One <div> per row, one <span> per run of identical attributes. Rebuilding
    // 30 rows is cheap and avoids the bookkeeping a diffing renderer needs;
    // it runs on a rAF so a chatty boot cannot cause one reflow per byte.

    paint() {
        {
            const out = [];
            for (let r = 0; r < this.rows; r++) {
                const row = this.grid[r];
                let html = "";
                let run = "";
                let cur = null;
                const flush = () => {
                    if (!run) return;
                    const cls = `f${cur.fg} b${cur.bg}${cur.bold ? " bo" : ""}`;
                    html += `<span class="${cls}">${esc(run)}</span>`;
                    run = "";
                };
                for (let c = 0; c < this.cols; c++) {
                    const cell = row[c];
                    const isCursor = r === this.y && c === this.x;
                    const key = `${cell.fg}|${cell.bg}|${cell.bold}|${isCursor}`;
                    if (!cur || key !== cur.key) { flush(); cur = { ...cell, key, isCursor }; }
                    run += cell.ch;
                    if (isCursor) {
                        flush();
                        html = html.replace(/<span class="([^"]*)">([^<]*)<\/span>$/,
                            (_m, c1, t) => `<span class="${c1} cur">${t}</span>`);
                        cur = null;
                    }
                }
                flush();
                out.push(`<div class="row">${html || "&nbsp;"}</div>`);
            }
            this.el.innerHTML = out.join("");
        }
    }

    // ── input ─────────────────────────────────────────────────────────────

    /** Selected text, but only if the selection is inside this terminal. */
    selectionText() {
        const sel = getSelection();
        if (!sel || sel.isCollapsed) return "";
        const t = sel.toString();
        return t && this.el.contains(sel.anchorNode) ? t : "";
    }

    /**
     * Copy the selection, trimming the padding the grid is made of.
     *
     * Every row is `cols` characters wide, so a naive copy of a multi-line
     * selection brings back a block of trailing spaces on each line — which
     * then pastes into a shell as a mess.
     */
    copySelection() {
        const text = this.selectionText().split("\n").map(l => l.replace(/\s+$/, "")).join("\n");
        if (!text) return;
        navigator.clipboard.writeText(text).catch(() => {});
        getSelection()?.removeAllRanges();
    }

    /** Send clipboard text as if typed. */
    async pasteClipboard() {
        try {
            this.pasteText(await navigator.clipboard.readText());
        } catch {
            // Denied, or no permission. Say so on screen rather than looking
            // like a dead keystroke — the usual cause is a non-secure origin.
            this.write(new TextEncoder().encode(
                "\r\n[paste unavailable: clipboard permission denied]\r\n"));
        }
    }

    pasteText(text) {
        if (!text) return;
        // A shell reads Enter as CR. Text off the clipboard has LF (or CRLF)
        // line endings, which would submit nothing at all.
        this.onInput(new TextEncoder().encode(text.replace(/\r?\n/g, "\r")));
    }

    key(e) {
        const send = s => {
            e.preventDefault();
            this.onInput(new TextEncoder().encode(s));
        };
        // Paste before the control-code path, or Ctrl-V would send 0x16.
        // Ctrl-Shift-V is the terminal convention; plain Ctrl-V is bound too
        // because that is what everyone actually presses, and a literal 0x16
        // is close to useless in a shell.
        if (e.ctrlKey && !e.altKey && (e.key === "v" || e.key === "V")) {
            e.preventDefault();
            this.pasteClipboard();
            return;
        }
        // Ctrl-C copies when there is a selection and interrupts otherwise —
        // what xterm and every browser terminal does. Ctrl-Shift-C is not a
        // usable alternative here: Chrome takes it for DevTools, so binding
        // copy to it means there is no way to copy at all.
        if (e.ctrlKey && !e.altKey && (e.key === "c" || e.key === "C") && this.selectionText()) {
            e.preventDefault();
            this.copySelection();
            return;
        }
        if (e.ctrlKey && !e.altKey && e.key.length === 1) {
            const code = e.key.toUpperCase().charCodeAt(0) - 64;
            if (code >= 0 && code < 32) return send(String.fromCharCode(code));
        }
        switch (e.key) {
            case "Enter": return send("\r");
            case "Backspace": return send("\x7f");
            case "Tab": return send("\t");
            case "Escape": return send("\x1b");
            case "ArrowUp": return send("\x1b[A");
            case "ArrowDown": return send("\x1b[B");
            case "ArrowRight": return send("\x1b[C");
            case "ArrowLeft": return send("\x1b[D");
            case "Home": return send("\x1b[H");
            case "End": return send("\x1b[F");
            case "Delete": return send("\x1b[3~");
            default:
                if (e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey) send(e.key);
        }
    }
}

const esc = s => s.replace(/[&<>]/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c]));
