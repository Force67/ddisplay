/**
 * Video decoder abstraction.
 *
 * - H.264: uses jMuxer (Annex B -> fMP4 -> MSE -> <video>)
 * - AV1:   uses WebCodecs VideoDecoder (OBU -> frames -> canvas)
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

    onFrame = null;

    get queueSize() {
        return 0;
    }

    constructor(videoElement, fps = 30) {
        this.#video = videoElement;
        this.#fps = fps;
    }

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
            console.log(`[h264-decoder] fed frame #${this.#frameCount}, kf=${isKeyframe}, size=${data.byteLength}`);
        }
    }

    destroy() {
        if (this.#muxer) {
            this.#muxer.destroy();
            this.#muxer = null;
        }
    }

    #init() {
        console.log(`[h264-decoder] initializing jMuxer, fps=${this.#fps}`);
        this.#muxer = new JMuxer({
            node: this.#video,
            mode: 'video',
            flushingTime: 0,
            maxDelay: 100,
            fps: this.#fps,
            clearBuffer: true,
            debug: false,
            onReady: () => {
                console.log('[h264-decoder] jMuxer ready');
                this.#video.playbackRate = 1.0;
                this.#video.play().catch(() => {});
            },
            onError: (e) => {
                console.error('[h264-decoder] jMuxer error:', e);
            },
        });
    }
}

/**
 * AV1 decoder using the WebCodecs API.
 *
 * Decodes OBU frames from NVENC and draws to a canvas via VideoFrame.
 */
export class AV1Decoder {
    /** @type {VideoDecoder|null} */
    #decoder = null;
    /** @type {number} */
    #width = 0;
    /** @type {number} */
    #height = 0;
    /** @type {number} */
    #frameCount = 0;
    /** @type {HTMLVideoElement} */
    #video;
    /** @type {HTMLCanvasElement|null} */
    #offscreen = null;

    onFrame = null;

    get queueSize() {
        return this.#decoder ? this.#decoder.decodeQueueSize : 0;
    }

    constructor(videoElement, fps = 30) {
        this.#video = videoElement;
    }

    decode(data, isKeyframe, timestamp, width, height) {
        if (!this.#decoder || width !== this.#width || height !== this.#height) {
            this.#width = width;
            this.#height = height;
            this.#initDecoder(width, height);
        }

        const chunk = new EncodedVideoChunk({
            type: isKeyframe ? 'key' : 'delta',
            timestamp: timestamp,
            data: data,
        });

        try {
            this.#decoder.decode(chunk);
        } catch (e) {
            console.warn('[av1-decoder] decode error:', e);
        }

        this.#frameCount++;
        if (this.#frameCount <= 3 || this.#frameCount % 300 === 0) {
            console.log(`[av1-decoder] fed frame #${this.#frameCount}, kf=${isKeyframe}, size=${data.byteLength}, queue=${this.queueSize}`);
        }
    }

    destroy() {
        if (this.#decoder && this.#decoder.state !== 'closed') {
            this.#decoder.close();
        }
        this.#decoder = null;
    }

    #initDecoder(width, height) {
        if (this.#decoder && this.#decoder.state !== 'closed') {
            this.#decoder.close();
        }

        // AV1 codec string: av01.0.08M.08
        //   0 = seq_profile (Main)
        //   08 = seq_level_idx (Level 4.0, suitable for 1080p)
        //   M = seq_tier (Main)
        //   08 = bitDepth (8-bit)
        const codecString = 'av01.0.08M.08';

        console.log(`[av1-decoder] initializing WebCodecs, ${width}x${height}, codec=${codecString}`);

        this.#decoder = new VideoDecoder({
            output: (frame) => {
                this.#handleFrame(frame);
            },
            error: (e) => {
                console.error('[av1-decoder] decoder error:', e);
            },
        });

        this.#decoder.configure({
            codec: codecString,
            codedWidth: width,
            codedHeight: height,
            hardwareAcceleration: 'prefer-hardware',
        });
    }

    #handleFrame(frame) {
        // Draw the VideoFrame to the hidden video element's canvas
        // We use an offscreen canvas to convert VideoFrame → ImageBitmap → drawImage
        if (!this.#offscreen) {
            this.#offscreen = document.createElement('canvas');
        }
        this.#offscreen.width = frame.displayWidth;
        this.#offscreen.height = frame.displayHeight;
        const ctx = this.#offscreen.getContext('2d');
        ctx.drawImage(frame, 0, 0);
        frame.close();

        // Signal to the renderer that we have a new frame
        if (this.onFrame) {
            this.onFrame(this.#offscreen);
        }
    }
}

/**
 * Check if the browser supports AV1 WebCodecs decoding.
 */
export async function isAV1Supported() {
    if (typeof VideoDecoder === 'undefined') {
        return false;
    }
    try {
        const support = await VideoDecoder.isConfigSupported({
            codec: 'av01.0.08M.08',
            codedWidth: 1920,
            codedHeight: 1080,
        });
        return support.supported === true;
    } catch {
        return false;
    }
}
