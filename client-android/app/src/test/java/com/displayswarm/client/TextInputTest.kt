package com.displayswarm.client

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class TextInputTest {
    @Test
    fun autocorrectReplacesOnlyTheChangedTail() {
        assertEquals(TextEdit(2, "he"), textEdit("teh", "the").let { TextEdit(it.backspaces, it.typed) })
        assertEquals(TextEdit(0, "lo"), textEdit("hel", "hello").let { TextEdit(it.backspaces, it.typed) }.copy(typed = "lo"))
        assertEquals(TextEdit(3, ""), textEdit("abc", ""))
    }

    @Test
    fun penPressureKeepsEndsAndLiftsLightStrokes() {
        assertEquals(0f, penPressureCurve(0f), 0f)
        assertEquals(1f, penPressureCurve(1f), 1e-6f)
        assertTrue(penPressureCurve(0.2f) > 0.3f)
        assertEquals(0f, penPressureCurve(Float.NaN), 0f)
    }
}
