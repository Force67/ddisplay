/**
 * H.264 decoder using jMuxer (Annex B -> fMP4 -> MSE -> <video>).
 * Works in all modern browsers (Chrome, Edge, Firefox, Safari).
 */

export class H264Decoder {
    /** @type {JMuxer|null} */
    #muxer = null;
    /** @type {HTMLVideoElement} */
    #video;
    /** @type {number} */
    #fps;
    /** @type {number} */
    #frameCount = 0;

    /**
     * Called when video is ready/updated for rendering.
     * @type {((video: HTMLVideoElement) => void)|null}
     */
    onFrame = null;

    get queueSize() {
        return 0;
    }

    /**
     * @param {HTMLVideoElement} videoElement
     * @param {number} fps
     */
    constructor(videoElement, fps = 30) {
        this.#video = videoElement;
        this.#fps = fps;
    }

    /**
     * Feed H.264 Annex B data.
     * @param {Uint8Array} data       Raw Annex B bitstream
     * @param {boolean}    isKeyframe (unused - jMuxer detects this)
     * @param {number}     timestamp  (unused - jMuxer manages timing)
     * @param {number}     width      (unused)
     * @param {number}     height     (unused)
     */
    decode(data, isKeyframe, timestamp, width, height) {
        if (!this.#muxer) {
            this.#init();
        }

        this.#muxer.feed({
            video: data,
            duration: Math.round(1000 / this.#fps),
        });

        this.#frameCount++;
        if (this.#frameCount <= 3 || this.#frameCount % 300 === 0) {
            console.log(`[decoder] fed frame #${this.#frameCount}, kf=${isKeyframe}, size=${data.byteLength}`);
        }
    }

    destroy() {
        if (this.#muxer) {
            this.#muxer.destroy();
            this.#muxer = null;
        }
    }

    #init() {
        console.log(`[decoder] initializing jMuxer, fps=${this.#fps}`);
        this.#muxer = new JMuxer({
            node: this.#video,
            mode: 'video',
            flushingTime: 0,
            fps: this.#fps,
            clearBuffer: true,
            debug: false,
            onReady: () => {
                console.log('[decoder] jMuxer ready');
                this.#video.play().catch(() => {});
            },
            onError: (e) => {
                console.error('[decoder] jMuxer error:', e);
            },
        });
    }
}
