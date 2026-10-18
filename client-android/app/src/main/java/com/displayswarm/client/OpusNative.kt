package com.displayswarm.client

import android.util.Log

/**
 * libopus through JNI (see `src/main/cpp`). Decoding the host's 10 ms packets
 * with MediaCodec cost ~35% of a core on a mid-range phone; libopus takes
 * about 1%, and its loss concealment beats inserting silence.
 */
object OpusNative {
    /** False if the native library could not be loaded; callers fall back to MediaCodec. */
    val available: Boolean = try {
        System.loadLibrary("dsopus")
        true
    } catch (e: Throwable) {
        Log.w("OpusNative", "libdsopus unavailable, using MediaCodec", e)
        false
    }

    @JvmStatic external fun decoderCreate(channels: Int): Long
    @JvmStatic external fun decoderDestroy(handle: Long)
    /** `data` null conceals one lost frame. Returns samples per channel, or < 0 on error. */
    @JvmStatic external fun decode(handle: Long, data: ByteArray?, len: Int, pcm: ShortArray, maxFrames: Int): Int
    @JvmStatic external fun encoderCreate(channels: Int, bitrate: Int, voice: Boolean): Long
    @JvmStatic external fun encoderDestroy(handle: Long)
    /** Returns the packet length, or < 0 on error. */
    @JvmStatic external fun encode(handle: Long, pcm: ShortArray, frames: Int, out: ByteArray): Int

    /** Longest frame a packet may hold: 120 ms at 48 kHz. */
    const val MAX_FRAME_SAMPLES = 5760
}
