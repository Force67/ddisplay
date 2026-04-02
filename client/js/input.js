/**
 * Captures mouse and keyboard input and encodes it into the ddisplay binary protocol.
 */
export class InputHandler {
    /** @type {HTMLElement} */
    #target;
    /** @type {number} Remote desktop width. */
    #remoteWidth = 0;
    /** @type {number} Remote desktop height. */
    #remoteHeight = 0;
    /** @type {boolean} */
    #pointerLocked = false;
    /** @type {number} Accumulated X while pointer-locked. */
    #lockX = 0;
    /** @type {number} Accumulated Y while pointer-locked. */
    #lockY = 0;
    /** @type {boolean} */
    #lockInitialised = false;
    /** @type {AbortController} */
    #abort;
    /** @type {Set<number>} */
    #pressedKeys = new Set();
    /** @type {Set<number>} */
    #pressedButtons = new Set();
    /** @type {number} */
    #lastEscapeTapAt = 0;
    /** @type {boolean} */
    #swallowEscapeKeyup = false;

    // Protocol message type constants.
    static MSG_MOUSE_MOVE   = 0x10;
    static MSG_MOUSE_BUTTON = 0x11;
    static MSG_MOUSE_SCROLL = 0x12;
    static MSG_KEY_EVENT    = 0x13;
    static MSG_PASTE_TEXT   = 0x15;

    /**
     * Called with an ArrayBuffer to send to the server.
     * @type {((buffer: ArrayBuffer) => void)|null}
     */
    onSend = null;

    /**
     * @param {HTMLElement} target  Element to capture events from (the canvas).
     */
    constructor(target) {
        this.#target = target;
        this.#abort = new AbortController();
        this.#bind();
    }

    /**
     * Update the remote desktop dimensions used for coordinate scaling.
     * @param {number} width
     * @param {number} height
     */
    setRemoteSize(width, height) {
        this.#remoteWidth = width;
        this.#remoteHeight = height;
    }

    /** Stop listening for events. */
    destroy() {
        this.#abort.abort();
        this.releaseCapture();
    }

    releaseCapture() {
        this.#releaseAllInputs();
        this.#lockInitialised = false;
        this.#lastEscapeTapAt = 0;
        if (document.pointerLockElement === this.#target) {
            document.exitPointerLock();
        }
        if (document.activeElement === this.#target) {
            this.#target.blur();
        }
    }

    // -- internals --

    #bind() {
        const opts = { signal: this.#abort.signal };
        const el = this.#target;

        // Request pointer lock on click.
        el.addEventListener('click', () => {
            el.focus({ preventScroll: true });
            if (document.pointerLockElement !== el) {
                el.requestPointerLock();
            }
        }, opts);

        document.addEventListener('pointerlockchange', () => {
            const wasLocked = this.#pointerLocked;
            this.#pointerLocked = document.pointerLockElement === el;
            if (wasLocked && !this.#pointerLocked) {
                this.#lockInitialised = false;
                this.#releaseAllInputs();
            }
        }, opts);

        window.addEventListener('blur', () => this.#releaseAllInputs(), opts);
        window.addEventListener('pagehide', () => this.#releaseAllInputs(), opts);
        document.addEventListener('visibilitychange', () => {
            if (document.visibilityState === 'hidden') {
                this.#releaseAllInputs();
            }
        }, opts);

        // ---- Mouse ----

        el.addEventListener('mousemove', (e) => {
            if (this.#remoteWidth === 0 || this.#remoteHeight === 0) return;

            let x, y;

            if (this.#pointerLocked) {
                if (!this.#lockInitialised) {
                    // Start from the centre of the remote display.
                    this.#lockX = this.#remoteWidth / 2;
                    this.#lockY = this.#remoteHeight / 2;
                    this.#lockInitialised = true;
                }
                // Scale movement by the ratio of remote to viewport size.
                const scaleX = this.#remoteWidth / el.clientWidth;
                const scaleY = this.#remoteHeight / el.clientHeight;
                this.#lockX = Math.max(0, Math.min(this.#remoteWidth - 1, this.#lockX + e.movementX * scaleX));
                this.#lockY = Math.max(0, Math.min(this.#remoteHeight - 1, this.#lockY + e.movementY * scaleY));
                x = Math.round(this.#lockX);
                y = Math.round(this.#lockY);
            } else {
                ({ x, y } = this.#scaleCoords(e));
            }

            this.#sendMouseMove(x, y);
        }, opts);

        el.addEventListener('mousedown', (e) => {
            e.preventDefault();
            el.focus({ preventScroll: true });
            const { x, y } = this.#currentCoords(e);
            this.#pressedButtons.add(e.button);
            this.#sendMouseButton(e.button, true, x, y);
        }, opts);

        el.addEventListener('mouseup', (e) => {
            e.preventDefault();
            const { x, y } = this.#currentCoords(e);
            this.#pressedButtons.delete(e.button);
            this.#sendMouseButton(e.button, false, x, y);
        }, opts);

        el.addEventListener('wheel', (e) => {
            e.preventDefault();
            const { x, y } = this.#currentCoords(e);
            // Clamp to i16 range.
            const dx = Math.max(-32768, Math.min(32767, Math.round(e.deltaX)));
            const dy = Math.max(-32768, Math.min(32767, Math.round(e.deltaY)));
            this.#sendMouseScroll(dx, dy, x, y);
        }, { ...opts, passive: false });

        // Prevent context menu on canvas.
        el.addEventListener('contextmenu', (e) => e.preventDefault(), opts);

        // ---- Keyboard ----

        window.addEventListener('keydown', (e) => {
            if (!this.#pointerLocked && document.activeElement !== el) return;
            // Allow browser F12 devtools shortcut through.
            if (e.key === 'F12') return;
            if (e.key === 'Escape') {
                this.#handleEscapeKey(e);
                return;
            }
            if ((e.ctrlKey || e.metaKey) && !e.altKey && e.key.toLowerCase() === 'v') {
                return;
            }
            e.preventDefault();
            this.#pressedKeys.add(e.keyCode);
            this.#sendKeyEvent(e.keyCode, true);
        }, opts);

        window.addEventListener('keyup', (e) => {
            if (!this.#pointerLocked && document.activeElement !== el) return;
            if (e.key === 'F12') return;
            if (e.key === 'Escape' && this.#swallowEscapeKeyup) {
                e.preventDefault();
                this.#swallowEscapeKeyup = false;
                return;
            }
            e.preventDefault();
            this.#pressedKeys.delete(e.keyCode);
            this.#sendKeyEvent(e.keyCode, false);
        }, opts);

        window.addEventListener('paste', (e) => {
            if (!this.#pointerLocked && document.activeElement !== el) return;
            const text = e.clipboardData?.getData('text/plain') ?? '';
            if (!text) return;
            e.preventDefault();
            this.#releaseAllInputs();
            this.#sendPasteText(text);
        }, opts);
    }

    /**
     * Scale viewport coordinates to remote desktop coordinates.
     * @param {MouseEvent} e
     * @returns {{ x: number, y: number }}
     */
    #scaleCoords(e) {
        const rect = this.#target.getBoundingClientRect();
        const sx = (e.clientX - rect.left) / rect.width;
        const sy = (e.clientY - rect.top) / rect.height;
        return {
            x: Math.round(Math.max(0, Math.min(this.#remoteWidth - 1, sx * this.#remoteWidth))),
            y: Math.round(Math.max(0, Math.min(this.#remoteHeight - 1, sy * this.#remoteHeight))),
        };
    }

    /**
     * Return current coordinates, using either locked or scaled coords.
     * @param {MouseEvent} e
     * @returns {{ x: number, y: number }}
     */
    #currentCoords(e) {
        if (this.#pointerLocked) {
            return { x: Math.round(this.#lockX), y: Math.round(this.#lockY) };
        }
        return this.#scaleCoords(e);
    }

    // ---- Protocol encoders ----

    /** @param {number} x @param {number} y */
    #sendMouseMove(x, y) {
        const buf = new ArrayBuffer(5);
        const view = new DataView(buf);
        view.setUint8(0, InputHandler.MSG_MOUSE_MOVE);
        view.setUint16(1, x, true);
        view.setUint16(3, y, true);
        this.#emit(buf);
    }

    /** @param {number} button @param {boolean} pressed @param {number} x @param {number} y */
    #sendMouseButton(button, pressed, x, y) {
        const buf = new ArrayBuffer(7);
        const view = new DataView(buf);
        view.setUint8(0, InputHandler.MSG_MOUSE_BUTTON);
        view.setUint8(1, button);
        view.setUint8(2, pressed ? 1 : 0);
        view.setUint16(3, x, true);
        view.setUint16(5, y, true);
        this.#emit(buf);
    }

    /** @param {number} dx @param {number} dy @param {number} x @param {number} y */
    #sendMouseScroll(dx, dy, x, y) {
        const buf = new ArrayBuffer(9);
        const view = new DataView(buf);
        view.setUint8(0, InputHandler.MSG_MOUSE_SCROLL);
        view.setInt16(1, dx, true);
        view.setInt16(3, dy, true);
        view.setUint16(5, x, true);
        view.setUint16(7, y, true);
        this.#emit(buf);
    }

    /** @param {number} keycode @param {boolean} pressed */
    #sendKeyEvent(keycode, pressed) {
        const buf = new ArrayBuffer(6);
        const view = new DataView(buf);
        view.setUint8(0, InputHandler.MSG_KEY_EVENT);
        view.setUint32(1, keycode, true);
        view.setUint8(5, pressed ? 1 : 0);
        this.#emit(buf);
    }

    /** @param {string} text */
    #sendPasteText(text) {
        const encoded = new TextEncoder().encode(text);
        const buf = new ArrayBuffer(1 + encoded.length);
        const view = new Uint8Array(buf);
        view[0] = InputHandler.MSG_PASTE_TEXT;
        view.set(encoded, 1);
        this.#emit(buf);
    }

    /** @param {ArrayBuffer} buf */
    #emit(buf) {
        if (this.onSend) {
            this.onSend(buf);
        }
    }

    #releaseAllInputs() {
        if (this.#pressedButtons.size > 0) {
            const { x, y } = { x: Math.round(this.#lockX), y: Math.round(this.#lockY) };
            for (const button of this.#pressedButtons) {
                this.#sendMouseButton(button, false, x, y);
            }
            this.#pressedButtons.clear();
        }

        if (this.#pressedKeys.size > 0) {
            for (const keycode of this.#pressedKeys) {
                this.#sendKeyEvent(keycode, false);
            }
            this.#pressedKeys.clear();
        }
    }

    /** @param {KeyboardEvent} e */
    #handleEscapeKey(e) {
        const now = performance.now();
        e.preventDefault();
        this.#swallowEscapeKeyup = true;

        if (now - this.#lastEscapeTapAt <= 450) {
            this.releaseCapture();
            return;
        }

        this.#lastEscapeTapAt = now;
        this.#sendKeyEvent(27, true);
        this.#sendKeyEvent(27, false);
    }
}
