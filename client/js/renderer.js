/**
 * Canvas2D renderer for decoded VideoFrame objects.
 */
export class Renderer {
    /** @type {HTMLCanvasElement} */
    #canvas;
    /** @type {CanvasRenderingContext2D} */
    #ctx;

    /** @type {number} Remote desktop width. */
    #remoteWidth = 0;
    /** @type {number} Remote desktop height. */
    #remoteHeight = 0;

    // FPS tracking
    /** @type {number[]} Timestamps of recent frames for FPS calculation. */
    #frameTimes = [];
    /** @type {number} Calculated FPS, updated once per second. */
    #fps = 0;
    /** @type {number} */
    #fpsUpdateTimer = 0;

    /**
     * @param {HTMLCanvasElement} canvas
     */
    constructor(canvas) {
        this.#canvas = canvas;
        this.#ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
        this.#fpsUpdateTimer = setInterval(() => this.#updateFps(), 1000);
    }

    /** Current FPS value. */
    get fps() {
        return this.#fps;
    }

    /** Current remote resolution width. */
    get remoteWidth() {
        return this.#remoteWidth;
    }

    /** Current remote resolution height. */
    get remoteHeight() {
        return this.#remoteHeight;
    }

    /**
     * Draw a decoded VideoFrame to the canvas and close it.
     * Automatically resizes the canvas backing store when the remote resolution changes.
     *
     * @param {VideoFrame} frame
     */
    drawFrame(frame) {
        const w = frame.displayWidth;
        const h = frame.displayHeight;

        if (w !== this.#remoteWidth || h !== this.#remoteHeight) {
            this.#remoteWidth = w;
            this.#remoteHeight = h;
            this.#canvas.width = w;
            this.#canvas.height = h;
        }

        try {
            this.#ctx.drawImage(frame, 0, 0, w, h);
        } finally {
            frame.close();
        }

        this.#frameTimes.push(performance.now());
    }

    /** Release resources. */
    destroy() {
        clearInterval(this.#fpsUpdateTimer);
    }

    // -- internals --

    #updateFps() {
        const now = performance.now();
        // Keep only frames from the last 1000 ms.
        this.#frameTimes = this.#frameTimes.filter(t => now - t < 1000);
        this.#fps = this.#frameTimes.length;
    }
}
