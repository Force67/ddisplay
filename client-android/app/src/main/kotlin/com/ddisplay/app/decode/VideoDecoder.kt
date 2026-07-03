package com.ddisplay.app.decode

import android.view.Surface

/**
 * Feeds one head's encoded stream into a hardware decoder that renders onto a
 * [Surface]. Implemented over MediaCodec in a later phase.
 */
interface VideoDecoder {
    /** (Re)initialise for a codec ("av1"/"h264") and frame size, drawing onto [surface]. */
    fun configure(codec: String, width: Int, height: Int, surface: Surface, events: DecoderEvents)

    /** Queue one access unit. [keyframe] marks an IDR; [pts] is the frame timestamp. */
    fun submit(data: ByteArray, keyframe: Boolean, pts: Long)

    fun close()
}

/** Decoder-side signals the session loop reacts to. */
interface DecoderEvents {
    /** The decoder lost sync; the session should send a RequestKeyframe. */
    fun onNeedsKeyframe()

    /** A frame was decoded and presented. [decodeMs] feeds ClientStats. */
    fun onDecodedFrame(decodeMs: Float)
}
