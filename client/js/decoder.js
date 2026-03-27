/**
 * H.264 decoder using the WebCodecs API.
 * Accepts raw Annex B NAL data from the server and outputs VideoFrame objects.
 */
export class H264Decoder {
    /** @type {VideoDecoder|null} */
    #decoder = null;
    /** @type {number} */
    #configuredWidth = 0;
    /** @type {number} */
    #configuredHeight = 0;
    /** @type {boolean} */
    #awaitingKeyframe = true;
    /** @type {number} */
    #frameCount = 0;

    static MAX_QUEUE_SIZE = 8;

    /**
     * Called with each decoded VideoFrame.
     * @type {((frame: VideoFrame) => void)|null}
     */
    onFrame = null;

    get queueSize() {
        return this.#decoder ? this.#decoder.decodeQueueSize : 0;
    }

    /**
     * @param {Uint8Array} data       Raw Annex B bitstream
     * @param {boolean}    isKeyframe
     * @param {number}     timestamp  PTS in microseconds
     * @param {number}     width
     * @param {number}     height
     */
    decode(data, isKeyframe, timestamp, width, height) {
        // Check WebCodecs support
        if (typeof VideoDecoder === 'undefined') {
            if (this.#frameCount === 0) {
                console.error('[decoder] WebCodecs VideoDecoder not available in this browser');
            }
            this.#frameCount++;
            return;
        }

        const needsConfigure =
            !this.#decoder ||
            this.#decoder.state === 'closed' ||
            width !== this.#configuredWidth ||
            height !== this.#configuredHeight;

        if (needsConfigure) {
            if (!isKeyframe) {
                return;
            }
            this.#configure(width, height);
        }

        if (this.#awaitingKeyframe && !isKeyframe) {
            return;
        }

        if (!isKeyframe && this.#decoder.decodeQueueSize >= H264Decoder.MAX_QUEUE_SIZE) {
            return;
        }

        const chunk = new EncodedVideoChunk({
            type: isKeyframe ? 'key' : 'delta',
            timestamp,
            data,
        });

        try {
            this.#decoder.decode(chunk);
            this.#awaitingKeyframe = false;
            this.#frameCount++;
            if (this.#frameCount <= 3 || this.#frameCount % 300 === 0) {
                console.log(`[decoder] decoded frame #${this.#frameCount}, kf=${isKeyframe}, size=${data.byteLength}, queue=${this.#decoder.decodeQueueSize}`);
            }
        } catch (e) {
            console.warn('[decoder] decode error, awaiting next keyframe:', e);
            this.#awaitingKeyframe = true;
        }
    }

    destroy() {
        if (this.#decoder && this.#decoder.state !== 'closed') {
            this.#decoder.close();
        }
        this.#decoder = null;
    }

    #configure(width, height) {
        if (this.#decoder && this.#decoder.state !== 'closed') {
            try { this.#decoder.close(); } catch (_) {}
        }

        this.#configuredWidth = width;
        this.#configuredHeight = height;
        this.#awaitingKeyframe = true;

        this.#decoder = new VideoDecoder({
            output: (frame) => {
                if (this.onFrame) {
                    this.onFrame(frame);
                } else {
                    frame.close();
                }
            },
            error: (e) => {
                console.error('[decoder] VideoDecoder error:', e);
                this.#awaitingKeyframe = true;
            },
        });

        // Try configuring with the codec from the SPS.
        // OpenH264 Constrained Baseline Level 4.0
        const codec = 'avc1.42c028';

        console.log(`[decoder] configuring: codec=${codec}, ${width}x${height}`);

        this.#decoder.configure({
            codec,
            codedWidth: width,
            codedHeight: height,
            optimizeForLatency: true,
            hardwareAcceleration: 'prefer-software',
        });

        console.log(`[decoder] configured, state=${this.#decoder.state}`);
    }
}
