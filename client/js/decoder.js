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

    /** Maximum number of chunks queued before dropping non-keyframes. */
    static MAX_QUEUE_SIZE = 8;

    /**
     * Called with each decoded VideoFrame. The receiver MUST call frame.close()
     * when finished to release GPU memory.
     * @type {((frame: VideoFrame) => void)|null}
     */
    onFrame = null;

    /** Number of chunks currently queued in the underlying decoder. */
    get queueSize() {
        return this.#decoder ? this.#decoder.decodeQueueSize : 0;
    }

    /**
     * Submit an H.264 Annex B frame for decoding.
     *
     * @param {Uint8Array} data       Raw Annex B bitstream data
     * @param {boolean}    isKeyframe Whether this is an IDR / keyframe
     * @param {number}     timestamp  Presentation timestamp in microseconds
     * @param {number}     width      Frame width signalled by the server
     * @param {number}     height     Frame height signalled by the server
     */
    decode(data, isKeyframe, timestamp, width, height) {
        const needsConfigure =
            !this.#decoder ||
            this.#decoder.state === 'closed' ||
            width !== this.#configuredWidth ||
            height !== this.#configuredHeight;

        if (needsConfigure) {
            if (!isKeyframe) {
                // Cannot configure without a keyframe; wait.
                return;
            }
            this.#configure(width, height);
        }

        if (this.#awaitingKeyframe && !isKeyframe) {
            return;
        }

        // Backpressure: drop non-keyframes when the queue backs up.
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
        } catch (e) {
            console.warn('[decoder] decode error, awaiting next keyframe:', e);
            this.#awaitingKeyframe = true;
        }
    }

    /** Release decoder resources. */
    destroy() {
        if (this.#decoder && this.#decoder.state !== 'closed') {
            this.#decoder.close();
        }
        this.#decoder = null;
        this.#configuredWidth = 0;
        this.#configuredHeight = 0;
        this.#awaitingKeyframe = true;
    }

    // -- internals --

    /**
     * (Re-)configure the underlying VideoDecoder for the given resolution.
     * @param {number} width
     * @param {number} height
     */
    #configure(width, height) {
        // Tear down previous decoder if any.
        if (this.#decoder && this.#decoder.state !== 'closed') {
            try { this.#decoder.close(); } catch (_) { /* ignore */ }
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

        this.#decoder.configure({
            codec: 'avc1.42c028',
            codedWidth: width,
            codedHeight: height,
            optimizeForLatency: true,
        });
    }
}
