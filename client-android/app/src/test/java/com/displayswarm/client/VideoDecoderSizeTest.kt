package com.displayswarm.client

import org.junit.Assert.assertEquals
import org.junit.Test

class VideoDecoderSizeTest {
    private val fullHd: (Int, Int) -> Boolean = { w, h -> w * h <= 1920 * 1088 }

    @Test
    fun supportedSizeIsKept() {
        assertEquals(1920 to 1080, VideoDecoder.shrinkToFit(1920, 1080, fullHd))
    }

    @Test
    fun oversizedPanelShrinksToAlignedSameShape() {
        val (w, h) = VideoDecoder.shrinkToFit(2340, 1080, fullHd)
        assert(fullHd(w, h)) { "$w x $h" }
        assert(w % 16 == 0 && h % 16 == 0)
        assert(Math.abs(w.toDouble() / h - 2340.0 / 1080) < 0.1)
    }
}
