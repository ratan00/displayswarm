package com.displayswarm.client

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class VideoDecoderCodecTest {
    private fun annexB(vararg first: Int) = byteArrayOf(0, 0, 0, 1) + first.map { it.toByte() }.toByteArray()

    @Test
    fun h264ConfigStartsWithSps() {
        assertEquals("video/avc", VideoDecoder.mimeOfConfig(annexB(0x67, 0x42, 0x00)))
    }

    @Test
    fun hevcConfigStartsWithVps() {
        assertEquals("video/hevc", VideoDecoder.mimeOfConfig(annexB(0x40, 0x01, 0x0c)))
    }

    @Test
    fun threeByteStartCodeAndGarbage() {
        assertEquals("video/hevc", VideoDecoder.mimeOfConfig(byteArrayOf(0, 0, 1, 0x40, 1)))
        assertNull(VideoDecoder.mimeOfConfig(byteArrayOf(1, 2, 3)))
        assertNull(VideoDecoder.mimeOfConfig(annexB(0x65)))
    }
}
