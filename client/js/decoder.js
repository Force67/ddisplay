/**
 * H.264 decoder using Media Source Extensions (MSE).
 *
 * Converts H.264 Annex B NAL units into fragmented MP4 segments and
 * feeds them into a <video> element via MSE SourceBuffer. Works in
 * all modern browsers (Chrome, Edge, Firefox, Safari).
 */
import { parseNALUs, createInitSegment, createMediaSegment } from './muxer.js';

export class H264Decoder {
    /** @type {HTMLVideoElement} */
    #video;
    /** @type {MediaSource|null} */
    #mediaSource = null;
    /** @type {SourceBuffer|null} */
    #sourceBuffer = null;
    /** @type {boolean} */
    #initialized = false;
    /** @type {number} */
    #sequenceNumber = 1;
    /** @type {number} */
    #baseDecodeTime = 0;
    /** @type {number} Frame duration in timescale units (90000 Hz) */
    #frameDuration = 3000; // 90000/30 = 3000 for 30fps
    /** @type {Uint8Array|null} */
    #cachedSPS = null;
    /** @type {Uint8Array|null} */
    #cachedPPS = null;
    /** @type {Array} Queue for segments while SourceBuffer is updating */
    #queue = [];
    /** @type {number} */
    #frameCount = 0;
    /** @type {number} */
    #configuredWidth = 0;
    /** @type {number} */
    #configuredHeight = 0;

    /**
     * Called with each decoded video frame (as the video element).
     * For MSE, we use the video element directly for rendering.
     * @type {((video: HTMLVideoElement) => void)|null}
     */
    onFrame = null;

    get queueSize() {
        return this.#queue.length;
    }

    /**
     * @param {HTMLVideoElement} videoElement
     * @param {number} fps
     */
    constructor(videoElement, fps = 30) {
        this.#video = videoElement;
        this.#frameDuration = Math.round(90000 / fps);
    }

    /**
     * Feed an H.264 Annex B frame for decoding.
     * @param {Uint8Array} data       Raw Annex B bitstream
     * @param {boolean}    isKeyframe
     * @param {number}     timestamp  (unused for MSE, we track internally)
     * @param {number}     width
     * @param {number}     height
     */
    decode(data, isKeyframe, timestamp, width, height) {
        const nalus = parseNALUs(data);
        if (nalus.length === 0) return;

        // Extract SPS/PPS from keyframes
        const spsNALUs = nalus.filter(n => (n[0] & 0x1f) === 7);
        const ppsNALUs = nalus.filter(n => (n[0] & 0x1f) === 8);
        const videoNALUs = nalus.filter(n => {
            const t = n[0] & 0x1f;
            return t === 1 || t === 5; // P-slice or IDR
        });

        if (spsNALUs.length > 0) this.#cachedSPS = spsNALUs[0];
        if (ppsNALUs.length > 0) this.#cachedPPS = ppsNALUs[0];

        // Need SPS/PPS before we can initialize
        if (!this.#initialized) {
            if (!this.#cachedSPS || !this.#cachedPPS || !isKeyframe) {
                return;
            }
            this.#init(width, height);
        }

        if (videoNALUs.length === 0) return;

        // Create media segment
        const segment = createMediaSegment(
            videoNALUs,
            this.#sequenceNumber++,
            this.#frameDuration,
            isKeyframe,
            this.#baseDecodeTime,
        );
        this.#baseDecodeTime += this.#frameDuration;

        this.#appendSegment(segment);
        this.#frameCount++;

        if (this.#frameCount <= 3 || this.#frameCount % 300 === 0) {
            console.log(`[decoder] frame #${this.#frameCount}, kf=${isKeyframe}, nalus=${videoNALUs.length}, segment=${segment.byteLength}b`);
        }
    }

    destroy() {
        if (this.#mediaSource && this.#mediaSource.readyState === 'open') {
            try { this.#mediaSource.endOfStream(); } catch (_) {}
        }
        this.#mediaSource = null;
        this.#sourceBuffer = null;
        this.#initialized = false;
    }

    #init(width, height) {
        if (this.#initialized) return;

        const mimeType = 'video/mp4; codecs="avc1.42c028"';
        if (!MediaSource.isTypeSupported(mimeType)) {
            console.error('[decoder] MSE does not support:', mimeType);
            // Try more generic codec string
            const alt = 'video/mp4; codecs="avc1.42E01E"';
            if (!MediaSource.isTypeSupported(alt)) {
                console.error('[decoder] MSE does not support:', alt);
                return;
            }
        }

        this.#configuredWidth = width;
        this.#configuredHeight = height;

        this.#mediaSource = new MediaSource();
        this.#video.src = URL.createObjectURL(this.#mediaSource);

        this.#mediaSource.addEventListener('sourceopen', () => {
            try {
                this.#sourceBuffer = this.#mediaSource.addSourceBuffer(mimeType);
                this.#sourceBuffer.mode = 'sequence';

                this.#sourceBuffer.addEventListener('updateend', () => {
                    // Process queued segments
                    if (this.#queue.length > 0 && !this.#sourceBuffer.updating) {
                        this.#sourceBuffer.appendBuffer(this.#queue.shift());
                    }
                    // Keep the buffer trimmed to avoid memory bloat (keep last 2 seconds)
                    if (this.#sourceBuffer.buffered.length > 0) {
                        const end = this.#sourceBuffer.buffered.end(0);
                        if (end > 4) {
                            try { this.#sourceBuffer.remove(0, end - 2); } catch (_) {}
                        }
                    }
                    // Notify renderer
                    if (this.onFrame) {
                        this.onFrame(this.#video);
                    }
                });

                // Append initialization segment
                const initSeg = createInitSegment(
                    this.#cachedSPS,
                    this.#cachedPPS,
                    width,
                    height,
                );
                this.#sourceBuffer.appendBuffer(initSeg);
                this.#initialized = true;

                // Start playback
                this.#video.play().catch(() => {});

                console.log(`[decoder] MSE initialized: ${width}x${height}`);
            } catch (e) {
                console.error('[decoder] sourceopen error:', e);
            }
        });
    }

    #appendSegment(segment) {
        if (!this.#sourceBuffer) {
            this.#queue.push(segment);
            return;
        }
        if (this.#sourceBuffer.updating || this.#queue.length > 0) {
            // Drop old segments if queue is too long (backpressure)
            if (this.#queue.length > 30) {
                this.#queue.splice(0, this.#queue.length - 10);
            }
            this.#queue.push(segment);
        } else {
            try {
                this.#sourceBuffer.appendBuffer(segment);
            } catch (e) {
                console.warn('[decoder] appendBuffer error:', e);
                this.#queue.push(segment);
            }
        }
    }
}
