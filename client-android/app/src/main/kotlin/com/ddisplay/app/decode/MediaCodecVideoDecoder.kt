package com.ddisplay.app.decode

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.view.Surface

/**
 * Hardware [VideoDecoder] over MediaCodec in async mode, rendering straight onto
 * the target [Surface]. One instance drives one head's stream. Callbacks run on
 * a private HandlerThread; [submit] and [close] run on the session thread. A
 * single lock guards all codec lifecycle and shared state.
 */
class MediaCodecVideoDecoder : VideoDecoder {
    private val lock = Any()

    private var codec: MediaCodec? = null
    private var callbackThread: HandlerThread? = null
    private var handler: Handler? = null
    private var events: DecoderEvents? = null
    private var surface: Surface? = null
    private var mime: String = MIME_H264
    private var width = 0
    private var height = 0

    private var closed = false

    // Drop non-keyframes until the next IDR is queued: a fresh decoder can't
    // parse inter-frames, and after any drop the reference chain is broken.
    private var awaitingKeyframe = true

    // Deduplicates the resync request so a stalled decoder can't emit a request
    // per dropped frame. Cleared when a keyframe is queued.
    private var keyframeRequested = true

    // A codec-level failure recreate is in flight on the handler thread; drop
    // all input (even keyframes) until the new codec is running.
    private var recreatePending = false

    private var decodeEma = 0f

    private val availableInputs = ArrayDeque<Int>()
    private val submitTimes = HashMap<Long, Long>()

    override fun configure(
        codec: String,
        width: Int,
        height: Int,
        surface: Surface,
        events: DecoderEvents,
    ) {
        synchronized(lock) {
            releaseCodecLocked()
            this.mime = mimeFor(codec)
            this.width = width
            this.height = height
            this.surface = surface
            this.events = events
            this.closed = false
            this.decodeEma = 0f
            this.awaitingKeyframe = true
            // The server sends the first IDR on connect, so we wait for it
            // rather than asking; keyframeRequested starts set to suppress a
            // request until we actually lose sync mid-stream.
            this.keyframeRequested = true
            this.recreatePending = false
            ensureHandlerLocked()
            buildCodecLocked()
        }
    }

    override fun submit(data: ByteArray, keyframe: Boolean, pts: Long) {
        synchronized(lock) {
            if (closed || recreatePending) return
            val c = codec ?: return
            if (awaitingKeyframe && !keyframe) return

            val index = availableInputs.removeFirstOrNull()
            if (index == null) {
                // No free input buffer: the decoder is behind. Drop and resync
                // on the next IDR, mirroring the native client.
                requestResyncLocked()
                return
            }
            val buffer = try {
                c.getInputBuffer(index)
            } catch (e: IllegalStateException) {
                scheduleRecreateLocked()
                return
            }
            if (buffer == null) {
                scheduleRecreateLocked()
                return
            }
            if (data.size > buffer.capacity()) {
                availableInputs.addFirst(index)
                requestResyncLocked()
                return
            }
            try {
                buffer.clear()
                buffer.put(data)
                if (submitTimes.size >= MAX_INFLIGHT) submitTimes.clear()
                submitTimes[pts] = System.nanoTime()
                c.queueInputBuffer(index, 0, data.size, pts, 0)
                if (keyframe) {
                    awaitingKeyframe = false
                    keyframeRequested = false
                }
            } catch (e: IllegalStateException) {
                // CodecException is an IllegalStateException, so this catches both.
                scheduleRecreateLocked()
            }
        }
    }

    override fun close() {
        val thread = synchronized(lock) {
            if (closed) return
            closed = true
            callbackThread
        }
        thread?.quitSafely()
        thread?.join(JOIN_TIMEOUT_MS)
        synchronized(lock) {
            releaseCodecLocked()
            handler = null
            callbackThread = null
            events = null
            surface = null
        }
    }

    private val callback = object : MediaCodec.Callback() {
        override fun onInputBufferAvailable(mc: MediaCodec, index: Int) {
            synchronized(lock) {
                if (closed || mc !== codec) return
                availableInputs.addLast(index)
            }
        }

        override fun onOutputBufferAvailable(
            mc: MediaCodec,
            index: Int,
            info: MediaCodec.BufferInfo,
        ) {
            var reported: DecoderEvents? = null
            var decodeMs = 0f
            synchronized(lock) {
                if (closed || mc !== codec) {
                    try {
                        mc.releaseOutputBuffer(index, false)
                    } catch (_: IllegalStateException) {
                    }
                    return
                }
                try {
                    mc.releaseOutputBuffer(index, true)
                } catch (e: IllegalStateException) {
                    scheduleRecreateLocked()
                    return
                }
                val submittedAt = submitTimes.remove(info.presentationTimeUs)
                if (submittedAt != null) {
                    val sample = (System.nanoTime() - submittedAt) / 1_000_000f
                    decodeEma = if (decodeEma <= 0f) sample else decodeEma * 0.9f + sample * 0.1f
                    reported = events
                    decodeMs = decodeEma
                }
            }
            reported?.onDecodedFrame(decodeMs)
        }

        override fun onError(mc: MediaCodec, e: MediaCodec.CodecException) {
            synchronized(lock) {
                if (closed || mc !== codec) return
                scheduleRecreateLocked()
            }
        }

        override fun onOutputFormatChanged(mc: MediaCodec, format: MediaFormat) {
            // Adaptive codecs report a mid-stream resolution change here and keep
            // decoding onto the same Surface, so there is nothing to do. Codecs
            // without adaptive playback instead raise onError on the resize,
            // which recreates the codec and requests a fresh keyframe.
        }
    }

    private fun ensureHandlerLocked() {
        if (callbackThread == null) {
            val thread = HandlerThread("ddisplay-decode")
            thread.start()
            callbackThread = thread
            handler = Handler(thread.looper)
        }
    }

    private fun buildCodecLocked() {
        val target = surface ?: return
        val created = CodecCaps.preferredDecoderName(mime)
            ?.let { MediaCodec.createByCodecName(it) }
            ?: MediaCodec.createDecoderByType(mime)
        // Configure without codec-specific-data: H.264 carries SPS/PPS and AV1
        // its sequence header in-band on every keyframe, so the first IDR fed as
        // a normal input buffer initialises the decoder.
        val format = MediaFormat.createVideoFormat(mime, width, height).apply {
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, maxInputSize(width, height))
            setInteger(MediaFormat.KEY_PRIORITY, 0)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R && supportsLowLatency(created)) {
                setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            }
        }
        created.setCallback(callback, handler)
        created.configure(format, target, null, 0)
        created.start()
        codec = created
        availableInputs.clear()
        submitTimes.clear()
    }

    private fun supportsLowLatency(codec: MediaCodec): Boolean {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return false
        return try {
            codec.codecInfo
                .getCapabilitiesForType(mime)
                .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency)
        } catch (e: IllegalArgumentException) {
            false
        }
    }

    /** Codec-level failure: rebuild on the handler thread and ask for an IDR. */
    private fun scheduleRecreateLocked() {
        if (closed || recreatePending) return
        recreatePending = true
        awaitingKeyframe = true
        keyframeRequested = true
        handler?.post { recreate() }
        events?.onNeedsKeyframe()
    }

    /** Dropped a frame but the codec is healthy: resync on the next IDR, once. */
    private fun requestResyncLocked() {
        awaitingKeyframe = true
        if (!keyframeRequested) {
            keyframeRequested = true
            events?.onNeedsKeyframe()
        }
    }

    private fun recreate() {
        synchronized(lock) {
            recreatePending = false
            if (closed) return
            releaseCodecLocked()
            try {
                buildCodecLocked()
                awaitingKeyframe = true
            } catch (e: Exception) {
                // Rebuild failed (transient): keep dropping and retry shortly.
                recreatePending = true
                awaitingKeyframe = true
                handler?.postDelayed({ recreate() }, RETRY_DELAY_MS)
            }
        }
    }

    private fun releaseCodecLocked() {
        val current = codec
        codec = null
        availableInputs.clear()
        submitTimes.clear()
        if (current != null) {
            try {
                current.release()
            } catch (_: Exception) {
            }
        }
    }

    private companion object {
        const val MIME_H264 = "video/avc"
        const val MIME_AV1 = "video/av01"
        const val MAX_INFLIGHT = 32
        const val RETRY_DELAY_MS = 100L
        const val JOIN_TIMEOUT_MS = 500L

        fun mimeFor(codec: String): String = if (codec == "av1") MIME_AV1 else MIME_H264

        fun maxInputSize(width: Int, height: Int): Int = maxOf(width * height, 1 shl 20)
    }
}
