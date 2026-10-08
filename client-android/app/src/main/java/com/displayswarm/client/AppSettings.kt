package com.displayswarm.client

import android.content.Context
import android.content.SharedPreferences

/** Where settings live: SharedPreferences on the phone, a map in tests. */
interface SettingsStore {
    fun getBoolean(key: String, default: Boolean): Boolean
    fun getString(key: String, default: String): String
    fun getFloat(key: String, default: Float): Float
    fun put(key: String, value: Any)
}

class PrefsStore(private val prefs: SharedPreferences) : SettingsStore {
    override fun getBoolean(key: String, default: Boolean) = prefs.getBoolean(key, default)
    override fun getString(key: String, default: String) = prefs.getString(key, default) ?: default
    override fun getFloat(key: String, default: Float) = prefs.getFloat(key, default)
    override fun put(key: String, value: Any) {
        val e = prefs.edit()
        when (value) {
            is Boolean -> e.putBoolean(key, value)
            is String -> e.putString(key, value)
            is Float -> e.putFloat(key, value)
            else -> throw IllegalArgumentException("unsupported setting type for $key")
        }
        e.apply()
    }
}

class MapStore : SettingsStore {
    val map = HashMap<String, Any>()
    override fun getBoolean(key: String, default: Boolean) = map[key] as? Boolean ?: default
    override fun getString(key: String, default: String) = map[key] as? String ?: default
    override fun getFloat(key: String, default: Float) = map[key] as? Float ?: default
    override fun put(key: String, value: Any) {
        map[key] = value
    }
}

/** What a stylus barrel button does on the host. */
enum class PenButtonAction(val id: String, val label: String) {
    NONE("none", "Nothing"),
    RIGHT_CLICK("right", "Right click"),
    MIDDLE_CLICK("middle", "Middle click"),
    ERASER("eraser", "Eraser");

    companion object {
        fun of(id: String, default: PenButtonAction) = entries.firstOrNull { it.id == id } ?: default
    }
}

/**
 * Applies the configured barrel-button actions when building a `Pen` message.
 *
 * The wire has two button bits ([Wire.PEN_BUTTON_PRIMARY], which the host
 * reports as the pen's first button and desktops treat as right click, and
 * [Wire.PEN_BUTTON_SECONDARY], the second, middle click) and a tool
 * ([Wire.PEN_TOOL_ERASER]). A pressed barrel button therefore becomes one of:
 * the primary bit, the secondary bit, the eraser tool, or nothing.
 */
class PenButtonMapping(
    val button1: PenButtonAction = PenButtonAction.RIGHT_CLICK,
    val button2: PenButtonAction = PenButtonAction.MIDDLE_CLICK
) {
    /** Result of [apply]: the tool and button bits to put in the message. */
    data class Result(val tool: Int, val buttons: Int)

    /** [tool] is what the stylus reports (the eraser end reports eraser regardless of the mapping). */
    fun apply(barrel1: Boolean, barrel2: Boolean, tool: Int): Result {
        var buttons = 0
        var outTool = tool
        fun act(a: PenButtonAction) {
            when (a) {
                PenButtonAction.NONE -> {}
                PenButtonAction.RIGHT_CLICK -> buttons = buttons or Wire.PEN_BUTTON_PRIMARY
                PenButtonAction.MIDDLE_CLICK -> buttons = buttons or Wire.PEN_BUTTON_SECONDARY
                PenButtonAction.ERASER -> outTool = Wire.PEN_TOOL_ERASER
            }
        }
        if (barrel1) act(button1)
        if (barrel2) act(button2)
        return Result(outTool, buttons)
    }
}

/** A programmable button: tap it and the host receives a key combo or some text. */
data class Macro(val label: String, val spec: String) {
    /** The messages this button sends; empty when [spec] is not valid. */
    fun messages(): List<Wire.Message> {
        if (spec.startsWith(TEXT_PREFIX)) {
            val text = spec.removePrefix(TEXT_PREFIX)
            return if (text.isEmpty()) emptyList() else listOf(Wire.Message.Text(text))
        }
        return KeyCombos.parse(spec)?.messages() ?: emptyList()
    }

    val isValid: Boolean get() = label.isNotBlank() && messages().isNotEmpty()

    companion object {
        const val TEXT_PREFIX = "text:"
        private const val FIELD = '\u001f'
        private const val RECORD = '\u001e'

        val DEFAULTS = listOf(
            Macro("Copy", "ctrl+c"), Macro("Paste", "ctrl+v"), Macro("Cut", "ctrl+x"),
            Macro("Undo", "ctrl+z"), Macro("Redo", "ctrl+shift+z"), Macro("Save", "ctrl+s"),
            Macro("Select all", "ctrl+a"), Macro("Find", "ctrl+f"), Macro("New tab", "ctrl+t"),
            Macro("Close tab", "ctrl+w"), Macro("Switch app", "alt+tab"), Macro("Overview", "super"),
            Macro("Lock", "super+l"), Macro("Screenshot", "printscreen"), Macro("Terminal", "ctrl+alt+t"),
            Macro("Play/Pause", "space")
        )

        fun encode(list: List<Macro>): String =
            list.joinToString(RECORD.toString()) { "${clean(it.label)}$FIELD${clean(it.spec)}" }

        fun decode(text: String): List<Macro> =
            text.split(RECORD).mapNotNull {
                val f = it.split(FIELD)
                if (f.size == 2 && f[0].isNotBlank()) Macro(f[0], f[1]) else null
            }

        /** The separators cannot be typed into a field; strip them anyway. */
        private fun clean(s: String) = s.replace(FIELD.toString(), "").replace(RECORD.toString(), "")
    }
}

/** How finger touches are interpreted. */
enum class TouchMode(val id: String) {
    DIRECT("direct"), TOUCHPAD("touchpad");

    companion object {
        fun of(id: String) = entries.firstOrNull { it.id == id } ?: DIRECT
    }
}

/**
 * The app's persisted settings. Setters write through and notify listeners with
 * the changed key, so a service can react (the audio and mic services watch
 * [KEY_AUDIO_OUT] and [KEY_MIC]).
 */
class AppSettings(private val store: SettingsStore) {
    companion object {
        const val PREFS_NAME = "displayswarm"

        /** Speaker output from the host is on. */
        const val KEY_AUDIO_OUT = "audio_out"
        const val KEY_VIDEO_QUALITY = "video_quality"

        /** Phone microphone to the host is on. */
        const val KEY_MIC = "mic"
        const val KEY_TOUCH_MODE = "touch_mode"
        const val KEY_KEEP_SCREEN_ON = "keep_screen_on"
        const val KEY_AUTO_RECONNECT = "auto_reconnect"
        const val KEY_ROTATION_LOCK = "rotation_lock"
        const val KEY_SHOW_STATS = "show_stats"
        const val KEY_PALM_REJECTION = "palm_rejection"
        const val KEY_PREDICTION = "stroke_prediction"
        const val KEY_PEN_BUTTON_1 = "pen_button_1"
        const val KEY_PEN_BUTTON_2 = "pen_button_2"
        const val KEY_NATURAL_SCROLL = "natural_scroll"
        const val KEY_POINTER_SPEED = "pointer_speed"
        const val KEY_MACROS = "macros"
        const val KEY_LAST_HOST = "last_host"

        fun from(context: Context) =
            AppSettings(PrefsStore(context.applicationContext.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)))
    }

    private val listeners = java.util.concurrent.CopyOnWriteArrayList<(String) -> Unit>()

    fun addListener(l: (key: String) -> Unit) {
        listeners.add(l)
    }

    fun removeListener(l: (key: String) -> Unit) {
        listeners.remove(l)
    }

    private fun set(key: String, value: Any) {
        store.put(key, value)
        listeners.forEach { it(key) }
    }

    /** Picture quality asked of the host: -1 not chosen (the host keeps its own), else 0 auto .. 3 data saver. */
    var videoQuality: Int
        get() = store.getString(KEY_VIDEO_QUALITY, "-1").toIntOrNull() ?: -1
        set(v) = set(KEY_VIDEO_QUALITY, v.toString())

    var audioOut: Boolean
        get() = store.getBoolean(KEY_AUDIO_OUT, true)
        set(v) = set(KEY_AUDIO_OUT, v)

    var mic: Boolean
        get() = store.getBoolean(KEY_MIC, false)
        set(v) = set(KEY_MIC, v)

    var touchMode: TouchMode
        get() = TouchMode.of(store.getString(KEY_TOUCH_MODE, TouchMode.DIRECT.id))
        set(v) = set(KEY_TOUCH_MODE, v.id)

    var keepScreenOn: Boolean
        get() = store.getBoolean(KEY_KEEP_SCREEN_ON, true)
        set(v) = set(KEY_KEEP_SCREEN_ON, v)

    var autoReconnect: Boolean
        get() = store.getBoolean(KEY_AUTO_RECONNECT, true)
        set(v) = set(KEY_AUTO_RECONNECT, v)

    var rotationLock: Boolean
        get() = store.getBoolean(KEY_ROTATION_LOCK, true)
        set(v) = set(KEY_ROTATION_LOCK, v)

    var showStats: Boolean
        get() = store.getBoolean(KEY_SHOW_STATS, true)
        set(v) = set(KEY_SHOW_STATS, v)

    var palmRejection: Boolean
        get() = store.getBoolean(KEY_PALM_REJECTION, true)
        set(v) = set(KEY_PALM_REJECTION, v)

    /** Local-only cursor preview of where the pen is heading; nothing predicted is sent to the host. */
    var strokePrediction: Boolean
        get() = store.getBoolean(KEY_PREDICTION, false)
        set(v) = set(KEY_PREDICTION, v)

    var penButton1: PenButtonAction
        get() = PenButtonAction.of(store.getString(KEY_PEN_BUTTON_1, ""), PenButtonAction.RIGHT_CLICK)
        set(v) = set(KEY_PEN_BUTTON_1, v.id)

    var penButton2: PenButtonAction
        get() = PenButtonAction.of(store.getString(KEY_PEN_BUTTON_2, ""), PenButtonAction.MIDDLE_CLICK)
        set(v) = set(KEY_PEN_BUTTON_2, v.id)

    val penMapping: PenButtonMapping get() = PenButtonMapping(penButton1, penButton2)

    var naturalScroll: Boolean
        get() = store.getBoolean(KEY_NATURAL_SCROLL, true)
        set(v) = set(KEY_NATURAL_SCROLL, v)

    /** Touchpad cursor speed multiplier, 0.5..2.5. */
    var pointerSpeed: Float
        get() = store.getFloat(KEY_POINTER_SPEED, 1.5f).coerceIn(0.5f, 2.5f)
        set(v) = set(KEY_POINTER_SPEED, v.coerceIn(0.5f, 2.5f))

    var macros: List<Macro>
        get() {
            val raw = store.getString(KEY_MACROS, "")
            return if (raw.isEmpty()) Macro.DEFAULTS else Macro.decode(raw)
        }
        set(v) = set(KEY_MACROS, Macro.encode(v))

    var lastHost: String
        get() = store.getString(KEY_LAST_HOST, "")
        set(v) = set(KEY_LAST_HOST, v)
}
