package com.displayswarm.client

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AppSettingsTest {
    private val store = MapStore()
    private val settings = AppSettings(store)

    @Test
    fun defaults() {
        // Speakers on, as the playback service always defaulted; mic off.
        assertTrue(settings.audioOut)
        assertFalse(settings.mic)
        assertTrue(settings.keepScreenOn)
        assertTrue(settings.autoReconnect)
        assertTrue(settings.palmRejection)
        assertFalse(settings.strokePrediction)
        assertEquals(TouchMode.DIRECT, settings.touchMode)
        assertEquals(PenButtonAction.RIGHT_CLICK, settings.penButton1)
        assertEquals(PenButtonAction.MIDDLE_CLICK, settings.penButton2)
        assertEquals(Macro.DEFAULTS, settings.macros)
    }

    @Test
    fun audioAndMicKeysAreStable() {
        settings.audioOut = false
        settings.mic = true
        assertEquals(false, store.map["audio_out"])
        assertEquals(true, store.map["mic"])
    }

    @Test
    fun valuesRoundTripAndNotifyListeners() {
        val seen = ArrayList<String>()
        settings.addListener { seen.add(it) }
        settings.touchMode = TouchMode.TOUCHPAD
        settings.penButton1 = PenButtonAction.ERASER
        settings.pointerSpeed = 9f
        assertEquals(TouchMode.TOUCHPAD, settings.touchMode)
        assertEquals(PenButtonAction.ERASER, settings.penButton1)
        assertEquals(2.5f, settings.pointerSpeed, 0f)
        assertEquals(listOf("touch_mode", "pen_button_1", "pointer_speed"), seen)
    }

    @Test
    fun unknownStoredValuesFallBackToDefaults() {
        store.map["touch_mode"] = "garbage"
        store.map["pen_button_2"] = "garbage"
        assertEquals(TouchMode.DIRECT, settings.touchMode)
        assertEquals(PenButtonAction.MIDDLE_CLICK, settings.penButton2)
    }

    @Test
    fun macrosRoundTripIncludingTextAndOddCharacters() {
        val list = listOf(Macro("Sig", "text:Best,\nMe: a=b"), Macro("Copy", "ctrl+c"))
        settings.macros = list
        assertEquals(list, settings.macros)
    }

    @Test
    fun macroMessages() {
        val copy = Macro("Copy", "ctrl+c").messages().filterIsInstance<Wire.Message.Key>()
        assertEquals(listOf(Wire.ACTION_DOWN, Wire.ACTION_UP), copy.map { it.action })
        assertEquals(Wire.KEY_FLAG_CTRL, copy[0].meta)
        assertEquals(0, copy[1].meta)
        assertEquals(listOf(Wire.Message.Text("hi")), Macro("Hi", "text:hi").messages())
        assertTrue(Macro("Bad", "ctrl+nonsense").messages().isEmpty())
        assertFalse(Macro("Bad", "text:").isValid)
        assertFalse(Macro(" ", "ctrl+c").isValid)
        assertTrue(Macro("Ok", "alt+tab").isValid)
    }

    @Test
    fun keyComboParsing() {
        val c = KeyCombos.parse("Ctrl+Shift+T")!!
        assertEquals(Wire.KEY_FLAG_CTRL or Wire.KEY_FLAG_SHIFT, c.meta)
        assertEquals(android.view.KeyEvent.KEYCODE_T, c.keyCode)
        assertEquals("Ctrl+Shift+T", c.label())
        assertEquals(android.view.KeyEvent.KEYCODE_F5, KeyCombos.parse("f5")!!.keyCode)
        assertEquals(android.view.KeyEvent.KEYCODE_ESCAPE, KeyCombos.parse("escape")!!.keyCode)
        assertEquals(Wire.KEY_FLAG_META, KeyCombos.parse("win+e")!!.meta)
        assertNull(KeyCombos.parse(""))
        assertNull(KeyCombos.parse("ctrl+"))
        assertNull(KeyCombos.parse("ctrl+f13"))
        assertNull(KeyCombos.parse("bogus+c"))
        assertEquals("F5", KeyCombo(android.view.KeyEvent.KEYCODE_F5).label())
    }

    @Test
    fun penButtonsMapToBitsAndTool() {
        val d = PenButtonMapping()
        assertEquals(PenButtonMapping.Result(Wire.PEN_TOOL_PEN, 0), d.apply(false, false, Wire.PEN_TOOL_PEN))
        assertEquals(PenButtonMapping.Result(Wire.PEN_TOOL_PEN, Wire.PEN_BUTTON_PRIMARY), d.apply(true, false, Wire.PEN_TOOL_PEN))
        assertEquals(PenButtonMapping.Result(Wire.PEN_TOOL_PEN, Wire.PEN_BUTTON_SECONDARY), d.apply(false, true, Wire.PEN_TOOL_PEN))
        assertEquals(
            PenButtonMapping.Result(Wire.PEN_TOOL_PEN, Wire.PEN_BUTTON_PRIMARY or Wire.PEN_BUTTON_SECONDARY),
            d.apply(true, true, Wire.PEN_TOOL_PEN)
        )
    }

    @Test
    fun penButtonRemapping() {
        val swapped = PenButtonMapping(PenButtonAction.MIDDLE_CLICK, PenButtonAction.RIGHT_CLICK)
        assertEquals(Wire.PEN_BUTTON_SECONDARY, swapped.apply(true, false, Wire.PEN_TOOL_PEN).buttons)
        assertEquals(Wire.PEN_BUTTON_PRIMARY, swapped.apply(false, true, Wire.PEN_TOOL_PEN).buttons)

        val eraser = PenButtonMapping(PenButtonAction.ERASER, PenButtonAction.NONE)
        assertEquals(PenButtonMapping.Result(Wire.PEN_TOOL_ERASER, 0), eraser.apply(true, false, Wire.PEN_TOOL_PEN))
        assertEquals(PenButtonMapping.Result(Wire.PEN_TOOL_PEN, 0), eraser.apply(false, true, Wire.PEN_TOOL_PEN))

        // The pen's own eraser end stays an eraser whatever the buttons do.
        assertEquals(Wire.PEN_TOOL_ERASER, PenButtonMapping(PenButtonAction.NONE, PenButtonAction.NONE).apply(false, false, Wire.PEN_TOOL_ERASER).tool)
    }
}
