/**
 * Canvas2D renderer. Draws video frames (from <video> element or VideoFrame) to canvas.
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

    constructor(canvas) {
        this.#canvas = canvas;
        this.#ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
        setInterval(() => this.#updateFps(), 1000);
    }

    get fps() { return this.#fps; }
    get remoteWidth() { return this.#remoteWidth; }
    get remoteHeight() { return this.#remoteHeight; }

    /**
     * Draw a <video> element or VideoFrame to the canvas.
     * @param {HTMLVideoElement|VideoFrame} source
     */
    drawVideoFrame(source) {
        let w, h;
        if (source instanceof HTMLVideoElement) {
            w = source.videoWidth;
            h = source.videoHeight;
            if (w === 0 || h === 0) return;
        } else {
            w = source.displayWidth;
            h = source.displayHeight;
        }

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
            // Ignore transient errors (e.g., video not ready)
        }

        if (source instanceof VideoFrame) {
            source.close();
        }

        this.#frameTimes.push(performance.now());
    }

    #updateFps() {
        const now = performance.now();
        this.#frameTimes = this.#frameTimes.filter(t => now - t < 1000);
        this.#fps = this.#frameTimes.length;
    }
}
