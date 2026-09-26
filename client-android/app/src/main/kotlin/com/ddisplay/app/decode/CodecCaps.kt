package com.ddisplay.app.decode

import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.os.Build

/** Queries the device's decoders so the session layer can fill ClientCaps. */
object CodecCaps {
    private const val MIME_AV1 = "video/av01"
    private const val MIME_H264 = "video/avc"

    /**
     * Decoders this device can use, best first. Hardware ranks ahead of
     * software, and AV1 ahead of H.264 only when both are hardware, so a
     * software-only AV1 never outranks a hardware H.264 the GPU decodes cheaply.
     * The server arbitrates the codec from this order.
     */
    fun supportedCodecs(): List<String> {
        var av1Hw = false
        var av1Sw = false
        var h264Hw = false
        var h264Sw = false
        for (info in decoders()) {
            val hw = isHardware(info)
            for (type in info.supportedTypes) {
                when {
                    type.equals(MIME_AV1, ignoreCase = true) -> if (hw) av1Hw = true else av1Sw = true
                    type.equals(MIME_H264, ignoreCase = true) -> if (hw) h264Hw = true else h264Sw = true
                }
            }
        }
        val ranked = ArrayList<Pair<String, Int>>(2)
        when {
            av1Hw -> ranked += "av1" to 0
            av1Sw -> ranked += "av1" to 2
        }
        when {
            h264Hw -> ranked += "h264" to 1
            h264Sw -> ranked += "h264" to 3
        }
        return ranked.sortedBy { it.second }.map { it.first }
    }

    /** Preferred decoder name for [mime] (hardware if present), or null to let the platform pick. */
    fun preferredDecoderName(mime: String): String? {
        var fallback: String? = null
        for (info in decoders()) {
            if (info.supportedTypes.none { it.equals(mime, ignoreCase = true) }) continue
            if (isHardware(info)) return info.name
            if (fallback == null) fallback = info.name
        }
        return fallback
    }

    private fun decoders(): List<MediaCodecInfo> =
        MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.filter { !it.isEncoder }

    private fun isHardware(info: MediaCodecInfo): Boolean =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            info.isHardwareAccelerated
        } else {
            // Pre-Q has no flag: Google's bundled software codecs carry these name
            // prefixes; anything else is a vendor (hardware) decoder.
            val name = info.name.lowercase()
            !(name.startsWith("omx.google.") || name.startsWith("c2.android."))
        }
}
