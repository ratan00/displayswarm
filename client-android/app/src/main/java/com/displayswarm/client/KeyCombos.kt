package com.displayswarm.client

import android.view.KeyEvent

/** A key with modifiers, as sent to the host: the host presses the modifiers from `meta`. */
data class KeyCombo(val keyCode: Int, val meta: Int = 0) {
    /** Press then release. The release carries no modifiers, which makes the host let them go. */
    fun messages(): List<Wire.Message> = listOf(
        Wire.Message.Key(Wire.ACTION_DOWN, keyCode, 0, meta, ""),
        Wire.Message.Key(Wire.ACTION_UP, keyCode, 0, 0, "")
    )

    /** Human form, e.g. `Ctrl+Alt+T`. */
    fun label(): String {
        val parts = ArrayList<String>()
        if (meta and Wire.KEY_FLAG_CTRL != 0) parts.add("Ctrl")
        if (meta and Wire.KEY_FLAG_ALT != 0) parts.add("Alt")
        if (meta and Wire.KEY_FLAG_SHIFT != 0) parts.add("Shift")
        if (meta and Wire.KEY_FLAG_META != 0) parts.add("Super")
        parts.add(KeyCombos.nameOf(keyCode))
        return parts.joinToString("+")
    }
}

/** Key combos: constants, and parsing of the text form used by macros (`ctrl+shift+t`). */
object KeyCombos {
    val MOD_NAMES = mapOf(
        "ctrl" to Wire.KEY_FLAG_CTRL, "control" to Wire.KEY_FLAG_CTRL,
        "alt" to Wire.KEY_FLAG_ALT,
        "shift" to Wire.KEY_FLAG_SHIFT,
        "super" to Wire.KEY_FLAG_META, "meta" to Wire.KEY_FLAG_META, "win" to Wire.KEY_FLAG_META
    )

    private val NAMED = linkedMapOf(
        "esc" to KeyEvent.KEYCODE_ESCAPE,
        "tab" to KeyEvent.KEYCODE_TAB,
        "enter" to KeyEvent.KEYCODE_ENTER,
        "space" to KeyEvent.KEYCODE_SPACE,
        "backspace" to KeyEvent.KEYCODE_DEL,
        "delete" to KeyEvent.KEYCODE_FORWARD_DEL,
        "left" to KeyEvent.KEYCODE_DPAD_LEFT,
        "right" to KeyEvent.KEYCODE_DPAD_RIGHT,
        "up" to KeyEvent.KEYCODE_DPAD_UP,
        "down" to KeyEvent.KEYCODE_DPAD_DOWN,
        "home" to KeyEvent.KEYCODE_MOVE_HOME,
        "end" to KeyEvent.KEYCODE_MOVE_END,
        "pageup" to KeyEvent.KEYCODE_PAGE_UP,
        "pagedown" to KeyEvent.KEYCODE_PAGE_DOWN,
        "insert" to KeyEvent.KEYCODE_INSERT,
        "printscreen" to KeyEvent.KEYCODE_SYSRQ,
        "super" to KeyEvent.KEYCODE_META_LEFT
    )

    private val ALIASES = mapOf(
        "escape" to "esc", "return" to "enter", "del" to "delete",
        "pgup" to "pageup", "pgdn" to "pagedown", "prtsc" to "printscreen"
    )

    val SUPER = KeyCombo(KeyEvent.KEYCODE_META_LEFT)
    val WORKSPACE_LEFT = KeyCombo(KeyEvent.KEYCODE_DPAD_LEFT, Wire.KEY_FLAG_CTRL or Wire.KEY_FLAG_ALT)
    val WORKSPACE_RIGHT = KeyCombo(KeyEvent.KEYCODE_DPAD_RIGHT, Wire.KEY_FLAG_CTRL or Wire.KEY_FLAG_ALT)

    /** The key code for a name (`a`, `7`, `f5`, `esc`, ...), or null. */
    fun keyCodeOf(name: String): Int? {
        val lower = name.trim().lowercase()
        val n = ALIASES[lower] ?: lower
        NAMED[n]?.let { return it }
        if (n.length == 1) {
            val c = n[0]
            if (c in 'a'..'z') return KeyEvent.KEYCODE_A + (c - 'a')
            if (c in '0'..'9') return KeyEvent.KEYCODE_0 + (c - '0')
        }
        if (n.length in 2..3 && n[0] == 'f') {
            val f = n.substring(1).toIntOrNull()
            if (f != null && f in 1..12) return KeyEvent.KEYCODE_F1 + (f - 1)
        }
        return null
    }

    fun nameOf(keyCode: Int): String {
        if (keyCode in KeyEvent.KEYCODE_A..KeyEvent.KEYCODE_Z) return ('A' + (keyCode - KeyEvent.KEYCODE_A)).toString()
        if (keyCode in KeyEvent.KEYCODE_0..KeyEvent.KEYCODE_9) return ('0' + (keyCode - KeyEvent.KEYCODE_0)).toString()
        if (keyCode in KeyEvent.KEYCODE_F1..KeyEvent.KEYCODE_F12) return "F${keyCode - KeyEvent.KEYCODE_F1 + 1}"
        val name = NAMED.entries.firstOrNull { it.value == keyCode }?.key ?: return "Key$keyCode"
        return name.replaceFirstChar { it.uppercase() }
    }

    /** Parses `ctrl+alt+t`, `f5`, `super`; null when a part is unknown or there is no key. */
    fun parse(text: String): KeyCombo? {
        val parts = text.split('+').map { it.trim() }.filter { it.isNotEmpty() }
        if (parts.isEmpty()) return null
        var meta = 0
        for (m in parts.dropLast(1)) meta = meta or (MOD_NAMES[m.lowercase()] ?: return null)
        val key = keyCodeOf(parts.last()) ?: return null
        return KeyCombo(key, meta)
    }
}
