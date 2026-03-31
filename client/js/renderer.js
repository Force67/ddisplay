/**
 * Canvas2D renderer. Draws video from <video> to canvas and overlays a cursor.
 */
export class Renderer {
    /** @type {HTMLCanvasElement} */
    #canvas;
    /** @type {CanvasRenderingContext2D} */
    #ctx;
    #remoteWidth = 0;
    #remoteHeight = 0;
    /** @type {number[]} */
    #frameTimes = [];
    #fps = 0;

    // Remote cursor state
    #cursorX = 0;
    #cursorY = 0;
    #cursorVisible = true;

    constructor(canvas) {
        this.#canvas = canvas;
        this.#ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
        setInterval(() => this.#updateFps(), 1000);
    }

    get fps() { return this.#fps; }
    get remoteWidth() { return this.#remoteWidth; }
    get remoteHeight() { return this.#remoteHeight; }

    /**
     * Update the remote cursor position.
     * @param {number} x
     * @param {number} y
     * @param {boolean} visible
     */
    setCursor(x, y, visible) {
        this.#cursorX = x;
        this.#cursorY = y;
        this.#cursorVisible = visible;
    }

    /**
     * Draw a <video> element to the canvas with cursor overlay.
     * @param {HTMLVideoElement} source
     */
    drawVideoFrame(source) {
        const w = source.videoWidth;
        const h = source.videoHeight;
        if (w === 0 || h === 0) return;

        if (w !== this.#remoteWidth || h !== this.#remoteHeight) {
            console.log(`[renderer] resolution: ${w}x${h}`);
            this.#remoteWidth = w;
            this.#remoteHeight = h;
            this.#canvas.width = w;
            this.#canvas.height = h;
        }

        try {
            this.#ctx.drawImage(source, 0, 0, w, h);
        } catch (e) {
            return;
        }

        // Draw cursor overlay (X11 SHM doesn't capture hardware cursor)
        if (this.#cursorVisible) {
            this.#drawCursor(this.#cursorX, this.#cursorY);
        }

        this.#frameTimes.push(performance.now());
    }

    /**
     * Draw a simple arrow cursor at the given position.
     */
    #drawCursor(x, y) {
        const ctx = this.#ctx;
        ctx.save();
        ctx.translate(x, y);

        // White arrow with black outline
        ctx.beginPath();
        ctx.moveTo(0, 0);
        ctx.lineTo(0, 18);
        ctx.lineTo(4, 14);
        ctx.lineTo(8, 22);
        ctx.lineTo(11, 21);
        ctx.lineTo(7, 13);
        ctx.lineTo(12, 13);
        ctx.closePath();

        ctx.fillStyle = '#fff';
        ctx.fill();
        ctx.strokeStyle = '#000';
        ctx.lineWidth = 1.2;
        ctx.stroke();

        ctx.restore();
    }

    #updateFps() {
        const now = performance.now();
        this.#frameTimes = this.#frameTimes.filter(t => now - t < 1000);
        this.#fps = this.#frameTimes.length;
    }
}
