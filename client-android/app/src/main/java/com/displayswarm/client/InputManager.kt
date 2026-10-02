package com.displayswarm.client

import android.content.Context
import android.os.Build
import android.util.AttributeSet
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import kotlin.math.atan
import kotlin.math.cos
import kotlin.math.sin
import kotlin.math.tan

/**
 * Largest stylus altitude accepted, in radians.
 *
 * [MotionEvent.AXIS_TILT] is the angle between the pen and the screen's surface
 * normal, so a pen lying flat on the glass reports +/-PI/2 - and `tan(PI/2)` is
 * infinity. Clamping to just under 90 degrees keeps `tan()` finite, which in
 * turn bounds each per-axis result to `atan(100)` = 89.4 degrees: the tilt can
 * never leave the -90..90 range that the host declares on
 * `ABS_TILT_X`/`ABS_TILT_Y` (Linux) and on `POINTER_PEN_INFO.tiltX/tiltY`
 * (Windows). The negative side is clamped symmetrically because a negative
 * altitude is a real, meaningful value: it encodes which way the pen is rolled
 * off vertical, and dropping the sign would lose that.
 */
private const val MAX_STYLUS_ALTITUDE = (Math.PI / 2.0) - 0.01

/**
 * The value `KeyEvent.getUnicodeChar()` returns for a key that produced no
 * character at all (arrows, modifiers, function keys, most non-QWERTY keys
 * with no layout). Spelled out here rather than referenced through
 * `KeyCharacterMap` so the meaning is unambiguous at the call site.
 */
private const val CHARACTER_UNDEFINED = 0xFFFF

/**
 * Every mouse button bit in [MotionEvent.getButtonState]. Used to snapshot "are
 * any buttons held right now" without caring which.
 */
private const val MOUSE_BUTTON_MASK =
    MotionEvent.BUTTON_PRIMARY or MotionEvent.BUTTON_SECONDARY or MotionEvent.BUTTON_TERTIARY

/**
 * Converts the pen's polar tilt report into the per-axis X/Y lean angles that
 * the evdev and Windows input models expect. Both results are in RADIANS; the
 * host multiplies by 180/PI before writing them to the absolute axes.
 *
 * WHAT WAS WRONG: this used to forward [MotionEvent.AXIS_TILT] as `tiltX` and
 * [MotionEvent.AXIS_ORIENTATION] as `tiltY`, as though the two were already
 * the X and Y components of a tilt vector. They are not, and the result is
 * wrong in a way users notice immediately:
 *
 *  - `AXIS_TILT` is the *altitude*: the angle away from the screen's surface
 *    normal (0 = held upright, PI/2 = flat on the glass). It says how far the
 *    pen leans, not which way.
 *  - `AXIS_ORIENTATION` is the *azimuth*: the compass direction the pen leans
 *    towards, measured from +X towards +Y. It says which way, not how far.
 *
 * So rotating an upright pen in your hand (altitude 0, arbitrary azimuth) used
 * to report a large "lean" that swung with the azimuth, while the one thing a
 * user actually perceives as tilt - the direction the pen is leaning - was
 * never reported at all.
 *
 * THE MATH: a pen tilted by `altitude` away from the normal, in the direction
 * `azimuth` from +X, has a lean vector whose projection onto the screen plane
 * is
 *
 *     v = (cos(azimuth), sin(azimuth)) * tan(altitude)
 *
 * and the per-axis lean angle is the angle of each component, hence
 *
 *     tiltX = atan(cos(azimuth) * tan(altitude))
 *     tiltY = atan(sin(azimuth) * tan(altitude))
 *
 * Taking `atan` of each component (instead of scaling the altitude by
 * cos/sin) is what keeps every result strictly inside (-PI/2, PI/2): a pen
 * leaning purely towards +X correctly reports tiltX = altitude and tiltY = 0,
 * and no single-axis value can ever leave the host's +/-90 degree range.
 *
 * Fingers report neither axis, so both read as 0 and this correctly yields
 * (0, 0) for touch contacts.
 *
 * Keep in sync with `altitude_azimuth_to_xy_tilt` in
 * `host/src/input/linux.rs`, which pins this formula with unit tests.
 */
internal fun stylusTiltComponents(altitude: Double, azimuth: Double): Pair<Double, Double> {
    val tanAlt = tan(altitude.coerceIn(-MAX_STYLUS_ALTITUDE, MAX_STYLUS_ALTITUDE))
    return atan(cos(azimuth) * tanAlt) to atan(sin(azimuth) * tanAlt)
}

/**
 * The transparent full-screen view that sits over the video and captures
 * pointer, stylus, mouse and keyboard input.
 *
 * WHY A SUBCLASS AT ALL: keyboard capture needs `dispatchKeyEvent`, and there is
 * no way to install that on a plain `View` instance. `setOnKeyListener` looks
 * like it would do, but an `OnKeyListener` is only consulted from
 * `View.onKeyDown`/`onKeyUp`, so it never sees the events an IME delivers and
 * it cannot expand the `ACTION_MULTIPLE` events that a soft keyboard produces.
 * Overriding `dispatchKeyEvent` is the only hook that sees the complete,
 * pre-dispatch stream - which is also the only place where the "exactly one
 * KEY_DOWN then one KEY_UP per physical press" rule can be enforced.
 *
 * It is declared here rather than in its own file so the whole input-capture
 * surface of the app stays in one reviewable place.
 *
 * The class must be referenced from `activity_main.xml`; see the `focusable`
 * attributes there, which are what let the view hold focus at all.
 */
class InputOverlayView @JvmOverloads constructor(
    context: Context,
    attrs: AttributeSet? = null,
    defStyleAttr: Int = 0
) : View(context, attrs, defStyleAttr) {

    /**
     * Receives every key event this view consumes, after the volume-key filter
     * and the ACTION_MULTIPLE expansion. Installed by [InputManager.attach];
     * null before that, in which case key events are left alone.
     *
     * Note this is a *raw event* sink, not a transport callback: [InputManager]
     * is the one that serialises and forwards through the single existing
     * `onPacketReady` path, so there is still exactly one place that puts bytes
     * on the wire.
     */
    internal var onKeyEvent: ((KeyEvent) -> Unit)? = null

    /**
     * Whether an IME should attach to this view. Off by default: this view
     * covers the mirrored screen, and an auto-shown soft keyboard would cover
     * the very thing the user is driving. Call [InputManager.setSoftKeyboardEnabled]
     * to turn it on from a UI affordance.
     */
    internal var softKeyboardEnabled: Boolean = false

    private var previewX: Float? = null
    private var previewY: Float? = null
    private val previewPaint = android.graphics.Paint(android.graphics.Paint.ANTI_ALIAS_FLAG).apply {
        color = 0x99FFFFFF.toInt()
    }

    /** Local-only dot at the predicted pen position; null hides it. */
    fun setPenPreview(x: Float?, y: Float?) {
        if (previewX == null && x == null) return
        previewX = x
        previewY = y
        invalidate()
    }

    override fun onDraw(canvas: android.graphics.Canvas) {
        val x = previewX
        val y = previewY
        if (x != null && y != null) canvas.drawCircle(x, y, 6f * resources.displayMetrics.density, previewPaint)
    }

    init {
        // Redundant with activity_main.xml, and deliberately so: the XML only
        // covers views inflated from it, and a focusable view that never gets
        // focus silently drops every key event.
        isFocusable = true
        isFocusableInTouchMode = true
    }

    override fun onAttachedToWindow() {
        super.onAttachedToWindow()
        // `onCreate` runs before the view hierarchy is attached and there is
        // nothing on screen to be "focused" yet, so take focus here instead.
        requestFocus()
    }

    /**
     * True when an IME is allowed to attach. See [softKeyboardEnabled].
     */
    override fun onCheckIsTextEditor(): Boolean = softKeyboardEnabled

    /**
     * Bridges IME text to key events.
     *
     * This is the soft-keyboard path. When an IME types "hi" it does not press
     * keys; it calls `InputConnection.commitText("hi")`, and the text would be
     * dropped on the floor by a view with no `InputConnection`. This converts
     * each committed character into the same KEY_DOWN/KEY_UP pair a physical
     * keyboard would produce, via the virtual keyboard's `KeyCharacterMap`, so
     * the host cannot tell the difference and there is only one code path to
     * keep correct.
     *
     * Composing text is deliberately ignored rather than forwarded: this view
     * displays no pre-edit text, and forwarding both the composing updates and
     * the final commit would report every character twice.
     */
    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection =
        KeyEventInputConnection(this)

    /**
     * The single entry point for keyboard capture.
     *
     * Returns true for every key it forwards, which stops the key from also
     * reaching the Activity and therefore guarantees it is reported to the host
     * exactly once. Returns false for the volume keys so they fall through to
     * `MainActivity.onKeyDown`, which uses them to toggle the HUD - swallowing
     * them here would both break the HUD and send a second, phantom volume
     * press to the host.
     */
    @Suppress("DEPRECATION") // ACTION_MULTIPLE: see dispatchMultiple.
    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (event.keyCode == KeyEvent.KEYCODE_VOLUME_UP ||
            event.keyCode == KeyEvent.KEYCODE_VOLUME_DOWN
        ) {
            return false
        }

        val sink = onKeyEvent ?: return false

        return when (event.action) {
            KeyEvent.ACTION_DOWN -> {
                sink(event)
                true
            }
            KeyEvent.ACTION_UP -> {
                sink(event)
                true
            }
            // Several characters delivered as one event, which is how an IME
            // commits a whole word. Expanded into individual down/up pairs so a
            // multi-character commit still satisfies the one-down-then-one-up
            // rule per character.
            KeyEvent.ACTION_MULTIPLE -> dispatchMultiple(event, sink)
            else -> false
        }
    }

    /**
     * Expands an [KeyEvent.ACTION_MULTIPLE] - the one event carrying more than
     * one character - into individual down/up pairs.
     *
     * `ACTION_MULTIPLE` and `KeyEvent.getCharacters` are deprecated from API 34,
     * but they are still the only path a hardware/Bluetooth keyboard uses to
     * deliver text, and the modern replacement (`KeyEvent.getKeyEvents` via a
     * connected `InputConnection`) is not available to a plain view that is not
     * an editor. Deprecated-but-working beats not working.
     */
    @Suppress("DEPRECATION")
    private fun dispatchMultiple(event: KeyEvent, sink: (KeyEvent) -> Unit): Boolean {
        val characters = event.characters
        if (characters.isNullOrEmpty()) {
            // A modifier pressed on its own arrives as ACTION_MULTIPLE with no
            // characters. There is nothing to inject, and inventing a key for it
            // would send a stray keypress to the host.
            return true
        }
        val expanded = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD)
            .getEvents(characters.toCharArray())
        if (expanded == null) {
            // No layout knows how to type these characters. Dropping them is the
            // only safe option: sending the raw characters as key codes would
            // put nonsense key codes on the host.
            return true
        }
        for (synthesised in expanded) {
            sink(synthesised)
        }
        return true
    }
}

/**
 * An [InputConnection] that turns IME editing operations into key events for
 * [InputOverlayView].
 *
 * Only the operations a remote-control surface can meaningfully act on are
 * handled; everything else is left to [BaseInputConnection]'s no-ops, because
 * this view has no text buffer to edit.
 */
private class KeyEventInputConnection(
    private val view: InputOverlayView
) : BaseInputConnection(view, /* fullEditor = */ true) {

    override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
        val chars = text?.toString().orEmpty()
        if (chars.isEmpty()) return true
        val events = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD)
            .getEvents(chars.toCharArray())
            ?: return false
        val sink = view.onKeyEvent
        if (sink != null) {
            for (event in events) sink(event)
        }
        return true
    }

    override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
        // The IME only knows how to delete "characters"; on a host keyboard that
        // is a Backspace or Forward-Delete key. `beforeLength` is the text the
        // IME believes it inserted, so deleting backwards means Backspace.
        val keyCode =
            if (afterLength <= 0) KeyEvent.KEYCODE_DEL else KeyEvent.KEYCODE_FORWARD_DEL
        val sink = view.onKeyEvent ?: return true
        val now = android.os.SystemClock.uptimeMillis()
        // Both halves by hand: BaseInputConnection.dispatchKeyEvent would
        // re-enter View.onKeyDown, which is not the capture path.
        sink(KeyEvent(now, now, KeyEvent.ACTION_DOWN, keyCode, 0))
        sink(KeyEvent(now, now, KeyEvent.ACTION_UP, keyCode, 0))
        return true
    }

    override fun sendKeyEvent(event: KeyEvent): Boolean {
        view.onKeyEvent?.invoke(event)
        return true
    }

    override fun performEditorAction(actionCode: Int): Boolean {
        val sink = view.onKeyEvent ?: return true
        val now = android.os.SystemClock.uptimeMillis()
        // The IME's "done"/"go"/"search" actions are all an Enter on the host.
        sink(KeyEvent(now, now, KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_ENTER, 0))
        sink(KeyEvent(now, now, KeyEvent.ACTION_UP, KeyEvent.KEYCODE_ENTER, 0))
        return true
    }

    // Composing text is intentionally ignored: this view renders nothing, and
    // forwarding composing updates *and* the final commit would report each
    // character twice.
    override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean = true
    override fun setComposingRegion(start: Int, end: Int): Boolean = true
    override fun finishComposingText(): Boolean = true
}

/**
 * Captures multi-touch, active stylus (pressure, tilt, buttons, every
 * historical sample), mouse (buttons, wheel) and keyboard events and encodes
 * them as protocol v2 input messages ([Wire.Message.Touch], [Wire.Message.Pen],
 * [Wire.Message.Mouse], [Wire.Message.Scroll], [Wire.Message.Key]).
 *
 * Every encoded message leaves through the single [onPacketReady] callback:
 * there is no second transport path to keep in sync with the first.
 */
class InputManager(
    private val view: InputOverlayView,
    private val onThreeFingerTap: (() -> Unit)? = null,
    private val onPacketReady: (ByteArray) -> Unit
) {
    private var isMultiFingerGesture = false
    private var threeStart = 0f to 0f
    private var threeLast = 0f to 0f

    private fun centroidOf(e: MotionEvent): Pair<Float, Float> {
        var x = 0f
        var y = 0f
        for (i in 0 until e.pointerCount) { x += e.getX(i); y += e.getY(i) }
        return x / e.pointerCount to y / e.pointerCount
    }

    /** Direct mode, three fingers: a tap toggles the toolbar; a swipe sideways switches app (Alt+Tab), up opens the overview. */
    private fun finishThreeFingers() {
        val dx = threeLast.first - threeStart.first
        val dy = threeLast.second - threeStart.second
        val dist = kotlin.math.hypot(dx, dy)
        val swipe = gestures.config.swipeDistance
        when {
            dist < swipe -> onThreeFingerTap?.invoke()
            kotlin.math.abs(dx) >= kotlin.math.abs(dy) -> KeyCombos.parse("alt+tab")?.messages()?.forEach { send(it) }
            dy < 0 -> KeyCombos.SUPER.messages().forEach { send(it) }
        }
    }

    /** Local zoom/pan of the video; touches are mapped through it. */
    val viewport = Viewport()

    /** Called (UI thread) when a local gesture changed [viewport]. */
    var onViewportChanged: (() -> Unit)? = null

    /** How single fingers behave. The pen is always direct. */
    var touchMode = TouchMode.DIRECT
        set(v) {
            if (field != v) gestures.cancel()
            field = v
        }

    /** Two-finger pinch/pan zooms the video locally (direct mode) instead of going to the host. */
    var localZoom = false

    /** Barrel button actions applied to every Pen message. */
    var penMapping = PenButtonMapping()

    /**
     * Local-only stroke prediction: the predicted pen position is drawn as a
     * dot on the overlay. Nothing predicted is ever sent, because the protocol
     * has no way to mark a sample as replaceable.
     */
    var strokePrediction = false
        set(v) {
            field = v
            if (!v) view.setPenPreview(null, null)
        }
    private var predictor: androidx.input.motionprediction.MotionEventPredictor? = null

    /** Modifiers latched from the on-screen bar; applied to the next key press, then cleared. */
    var stickyMeta = 0
    var onStickyConsumed: (() -> Unit)? = null

    /** Touchpad gestures. Its output takes the same single transport path. */
    val gestures = GestureEngine({ send(it) }).also { it.onThreeFingerTap = { onThreeFingerTap?.invoke() } }

    private var zoomGesture = false
    private var zoomPrevCx = 0f
    private var zoomPrevCy = 0f
    private var zoomPrevDist = 1f

    /** Sends one message through the single transport path. */
    fun send(msg: Wire.Message) = onPacketReady(msg.encode())

    fun setTouchpadConfig(pointerSpeed: Float, natural: Boolean, density: Float) {
        gestures.config = GestureEngine.Config.forDensity(density).copy(
            pointerGain = pointerSpeed, naturalScroll = natural
        )
    }

    private val tickRunnable = object : Runnable {
        override fun run() {
            gestures.tick(android.os.SystemClock.uptimeMillis())
            if (gestures.wantsTick) view.postOnAnimation(this)
        }
    }

    /** Drops finger touches while the pen is in use (see [PalmFilter]). */
    val palmFilter = PalmFilter()

    /**
     * Button state carried by the most recent mouse event, used to spot a click
     * that the platform reported as a button-state change on a hover/move
     * rather than as a contact down/up.
     */
    private var lastMouseButtons = 0

    fun attach() {
        view.setOnTouchListener { v, event ->
            // Android batches touch moves to the display's frame rate. A pen
            // stroke asks for every digitizer sample as it arrives instead,
            // which cuts latency and keeps fast strokes smooth.
            if (event.actionMasked == MotionEvent.ACTION_DOWN && isStylus(event.getToolType(0))) {
                v.requestUnbufferedDispatch(event)
            }
            handleMotionEvent(event)
            true
        }

        // Generic motion carries hover moves and anything the device reports
        // outside a normal touch stream. Scroll arrives on the touch path as
        // ACTION_SCROLL, but a hover-only device can also deliver it here, so
        // both go through the same handler.
        view.setOnGenericMotionListener { _, event ->
            handleMotionEvent(event)
        }

        view.onKeyEvent = ::handleKeyEvent

        view.addOnLayoutChangeListener { _, l, t, r, b, _, _, _, _ ->
            viewport.setSize((r - l).toFloat(), (b - t).toFloat())
            gestures.viewWidth = viewport.width
            gestures.viewHeight = viewport.height
            onViewportChanged?.invoke()
        }

        // A focusableInTouchMode view can hold focus, but only if it is actually
        // asked to: the OnTouchListener above returns true, so `onTouchEvent`
        // never runs and the framework's touch-to-focus path never fires.
        view.requestFocus()
    }

    /**
     * Turns IME text capture on or off.
     *
     * Off by default: the soft keyboard would cover the mirrored screen. Any
     * affordance that wants it (a HUD button, say) only has to call this.
     */
    fun setSoftKeyboardEnabled(enabled: Boolean) {
        view.softKeyboardEnabled = enabled
        val imm = view.context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        if (enabled) {
            view.requestFocus()
            imm.showSoftInput(view, InputMethodManager.SHOW_IMPLICIT)
        } else {
            imm.hideSoftInputFromWindow(view.windowToken, 0)
        }
    }

    // -----------------------------------------------------------------------
    // Pointer / stylus / mouse
    // -----------------------------------------------------------------------

    private fun isStylus(toolType: Int) =
        toolType == MotionEvent.TOOL_TYPE_STYLUS || toolType == MotionEvent.TOOL_TYPE_ERASER

    /** @return true when the event produced at least one message worth sending. */
    private fun handleMotionEvent(event: MotionEvent): Boolean {
        val actionMasked = event.actionMasked

        // ACTION_SCROLL is not a pointer event: its pointerCount is always 1, so
        // it can never be the three-finger tap. Checking it here would make a
        // three-finger trackpad swipe pop the HUD open.
        if (touchMode == TouchMode.DIRECT && actionMasked != MotionEvent.ACTION_SCROLL && event.pointerCount >= 3) {
            if (!isMultiFingerGesture) {
                isMultiFingerGesture = true
                threeStart = centroidOf(event)
            }
            threeLast = centroidOf(event)
        }

        if (touchMode == TouchMode.DIRECT && isMultiFingerGesture) {
            if (actionMasked == MotionEvent.ACTION_UP ||
                actionMasked == MotionEvent.ACTION_CANCEL
            ) {
                isMultiFingerGesture = false
                if (actionMasked == MotionEvent.ACTION_UP) finishThreeFingers()
            }
            return false
        }

        val width = view.width.toFloat().coerceAtLeast(1.0f)
        val height = view.height.toFloat().coerceAtLeast(1.0f)

        if (actionMasked == MotionEvent.ACTION_SCROLL) {
            return emitScrollEvent(event, width, height)
        }

        val actionIndex = event.actionIndex.coerceIn(0, event.pointerCount - 1)
        val actionTool = event.getToolType(actionIndex)

        // Mouse buttons live on the event, not on a pointer, and a mouse is
        // always a single pointer, so it is handled on its own.
        if (actionTool == MotionEvent.TOOL_TYPE_MOUSE) {
            return emitMouseEvent(event, actionMasked, actionIndex, width, height)
        }

        // A MotionEvent can mix tool types (fingers plus a pen). Fingers go out
        // as one Touch message, the stylus pointer as a Pen message. A
        // down/up is about the one pointer that changed, so only that pointer's
        // kind gets the action; on a MOVE or CANCEL both kinds are reported.
        val both = actionMasked == MotionEvent.ACTION_MOVE || actionMasked == MotionEvent.ACTION_CANCEL
        var sent = false
        if ((both || !isStylus(actionTool)) && emitFingers(event, actionMasked, actionIndex, width, height)) {
            sent = true
        }
        if ((both || isStylus(actionTool)) && emitPenEvent(event, actionMasked, actionIndex, actionTool, width, height)) {
            sent = true
        }
        return sent
    }

    /** Fingers, by mode: touchpad gestures, local zoom/pan, or real multitouch. */
    private fun emitFingers(event: MotionEvent, actionMasked: Int, actionIndex: Int, width: Float, height: Float): Boolean {
        if (touchMode == TouchMode.TOUCHPAD) {
            touchpad(event, actionMasked, actionIndex)
            return true
        }
        if (localZoom && localZoomGesture(event, actionMasked)) return true
        return emitTouchEvent(event, actionMasked, actionIndex, width, height)
    }

    private fun fingerPtrs(event: MotionEvent, skip: Int = -1): List<GestureEngine.Ptr> =
        (0 until event.pointerCount).filter {
            it != skip && !isStylus(event.getToolType(it)) && event.getToolType(it) != MotionEvent.TOOL_TYPE_MOUSE
        }.map { GestureEngine.Ptr(event.getPointerId(it), event.getX(it), event.getY(it)) }

    private fun touchpad(event: MotionEvent, actionMasked: Int, actionIndex: Int) {
        if (palmFilter.enabled && palmFilter.penActive(event.eventTime)) {
            gestures.cancel()
            return
        }
        val kind = when (actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> GestureEngine.Kind.DOWN
            MotionEvent.ACTION_MOVE -> GestureEngine.Kind.MOVE
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> GestureEngine.Kind.UP
            MotionEvent.ACTION_CANCEL -> GestureEngine.Kind.CANCEL
            else -> return
        }
        val pts = fingerPtrs(event, if (kind == GestureEngine.Kind.UP) actionIndex else -1)
        gestures.onTouch(kind, pts, event.eventTime)
        if (gestures.wantsTick) view.postOnAnimation(tickRunnable)
    }

    /**
     * Two or more fingers in direct mode with [localZoom]: pinch zooms and pan
     * moves the video locally and the host sees nothing. The first finger was
     * already sent as a touch, so it is cancelled when the second lands.
     * Returns true when the event was consumed here.
     */
    private fun localZoomGesture(event: MotionEvent, actionMasked: Int): Boolean {
        val fingers = fingerPtrs(event)
        if (!zoomGesture) {
            if (actionMasked != MotionEvent.ACTION_POINTER_DOWN || fingers.size < 2) return false
            zoomGesture = true
            val first = fingers.first()
            val cancel = Wire.Message.Touch(
                Wire.ACTION_CANCEL, first.id,
                listOf(Wire.TouchPoint(first.id, viewport.toContentX(first.x), viewport.toContentY(first.y), 0f))
            )
            palmFilter.filterTouch(cancel, event.eventTime).forEach { send(it) }
            zoomPrevCx = fingers.map { it.x }.average().toFloat()
            zoomPrevCy = fingers.map { it.y }.average().toFloat()
            zoomPrevDist = pinchDistance(fingers)
            return true
        }
        when (actionMasked) {
            MotionEvent.ACTION_MOVE -> if (fingers.size >= 2) {
                val cx = fingers.map { it.x }.average().toFloat()
                val cy = fingers.map { it.y }.average().toFloat()
                val d = pinchDistance(fingers)
                viewport.zoomBy(d / zoomPrevDist, cx, cy)
                viewport.panBy(cx - zoomPrevCx, cy - zoomPrevCy)
                zoomPrevCx = cx
                zoomPrevCy = cy
                zoomPrevDist = d
                onViewportChanged?.invoke()
            }
            MotionEvent.ACTION_POINTER_UP, MotionEvent.ACTION_POINTER_DOWN -> {
                // The finger set changed: re-base so the view does not jump.
                val rest = fingerPtrs(event, if (actionMasked == MotionEvent.ACTION_POINTER_UP) event.actionIndex else -1)
                if (rest.size >= 2) {
                    zoomPrevCx = rest.map { it.x }.average().toFloat()
                    zoomPrevCy = rest.map { it.y }.average().toFloat()
                    zoomPrevDist = pinchDistance(rest)
                }
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> zoomGesture = false
        }
        return true
    }

    private fun pinchDistance(p: List<GestureEngine.Ptr>): Float =
        kotlin.math.hypot(p[1].x - p[0].x, p[1].y - p[0].y).coerceAtLeast(1f)

    /** Fingers: every finger pointer in the event, with the action pointer's id. Hover is ignored. */
    private fun emitTouchEvent(
        event: MotionEvent,
        actionMasked: Int,
        actionIndex: Int,
        width: Float,
        height: Float
    ): Boolean {
        val touchAction = when (actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> Wire.ACTION_DOWN
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> Wire.ACTION_UP
            MotionEvent.ACTION_MOVE -> Wire.ACTION_MOVE
            MotionEvent.ACTION_CANCEL -> Wire.ACTION_CANCEL
            else -> return false
        }
        val points = ArrayList<Wire.TouchPoint>(event.pointerCount)
        for (i in 0 until event.pointerCount) {
            val tool = event.getToolType(i)
            if (isStylus(tool) || tool == MotionEvent.TOOL_TYPE_MOUSE) continue
            points.add(
                Wire.TouchPoint(
                    id = event.getPointerId(i),
                    x = viewport.toContentX(event.getX(i)),
                    y = viewport.toContentY(event.getY(i)),
                    pressure = event.getPressure(i).coerceIn(0.0f, 1.0f)
                )
            )
        }
        if (points.isEmpty()) return false
        val actionId = if (isStylus(event.getToolType(actionIndex))) points[0].id else event.getPointerId(actionIndex)
        val out = palmFilter.filterTouch(Wire.Message.Touch(touchAction, actionId, points), event.eventTime)
        out.forEach { onPacketReady(it.encode()) }
        return out.isNotEmpty()
    }

    /**
     * The stylus pointer: every historical sample, oldest first, then the
     * current one, so a fast stroke keeps the digitizer's full resolution.
     */
    private fun emitPenEvent(
        event: MotionEvent,
        actionMasked: Int,
        actionIndex: Int,
        actionTool: Int,
        width: Float,
        height: Float
    ): Boolean {
        val penAction = when (actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> Wire.ACTION_DOWN
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> Wire.ACTION_UP
            MotionEvent.ACTION_MOVE -> Wire.ACTION_MOVE
            MotionEvent.ACTION_CANCEL -> Wire.ACTION_CANCEL
            MotionEvent.ACTION_HOVER_ENTER, MotionEvent.ACTION_HOVER_MOVE -> Wire.ACTION_HOVER_MOVE
            MotionEvent.ACTION_HOVER_EXIT -> Wire.ACTION_HOVER_EXIT
            else -> return false
        }
        val idx = if (isStylus(actionTool)) {
            actionIndex
        } else {
            (0 until event.pointerCount).firstOrNull { isStylus(event.getToolType(it)) } ?: return false
        }
        val tool = if (event.getToolType(idx) == MotionEvent.TOOL_TYPE_ERASER) {
            Wire.PEN_TOOL_ERASER
        } else {
            Wire.PEN_TOOL_PEN
        }

        val state = event.buttonState
        val mapped = penMapping.apply(
            barrel1 = (state and (MotionEvent.BUTTON_STYLUS_PRIMARY or MotionEvent.BUTTON_SECONDARY)) != 0,
            barrel2 = (state and (MotionEvent.BUTTON_STYLUS_SECONDARY or MotionEvent.BUTTON_TERTIARY)) != 0,
            tool = tool
        )
        val buttons = mapped.buttons
        updatePrediction(event, penAction)

        val nowNanos = eventTimeNanos(event)
        val samples = ArrayList<Wire.PenSample>(event.historySize + 1)
        for (h in 0..event.historySize) {
            val current = h == event.historySize
            val ageUs = if (current) 0L else ((nowNanos - historicalTimeNanos(event, h)) / 1000L).coerceAtLeast(0L)
            fun axis(a: Int) = if (current) event.getAxisValue(a, idx) else event.getHistoricalAxisValue(a, idx, h)
            // Stylus tilt, converted from MotionEvent's polar report
            // (altitude/azimuth) into true per-axis X/Y lean angles in radians.
            // See `stylusTiltComponents` for why altitude and orientation are
            // NOT themselves the X and Y components.
            val (tiltX, tiltY) = stylusTiltComponents(
                axis(MotionEvent.AXIS_TILT).toDouble(),
                axis(MotionEvent.AXIS_ORIENTATION).toDouble()
            )
            samples.add(
                Wire.PenSample(
                    ageUs = ageUs,
                    x = viewport.toContentX(axis(MotionEvent.AXIS_X)),
                    y = viewport.toContentY(axis(MotionEvent.AXIS_Y)),
                    pressure = axis(MotionEvent.AXIS_PRESSURE).coerceIn(0.0f, 1.0f),
                    tiltX = tiltX.toFloat(),
                    tiltY = tiltY.toFloat()
                )
            )
        }
        val inRange = penAction != Wire.ACTION_UP && penAction != Wire.ACTION_CANCEL && penAction != Wire.ACTION_HOVER_EXIT
        palmFilter.onPen(inRange, event.eventTime)
        onPacketReady(Wire.Message.Pen(penAction, mapped.tool, buttons, samples).encode())
        return true
    }

    /** Feeds the predictor and shows/hides the local preview dot. Never sends anything. */
    private fun updatePrediction(event: MotionEvent, penAction: Int) {
        if (!strokePrediction) return
        try {
            val pr = predictor ?: androidx.input.motionprediction.MotionEventPredictor.newInstance(view).also { predictor = it }
            if (penAction == Wire.ACTION_UP || penAction == Wire.ACTION_CANCEL || penAction == Wire.ACTION_HOVER_EXIT) {
                view.setPenPreview(null, null)
                return
            }
            pr.record(event)
            val predicted = pr.predict()
            if (predicted != null) {
                view.setPenPreview(predicted.x, predicted.y)
                predicted.recycle()
            }
        } catch (e: Exception) {
            strokePrediction = false // unsupported on this device: stay off
        }
    }

    private fun eventTimeNanos(event: MotionEvent): Long =
        if (Build.VERSION.SDK_INT >= 34) event.eventTimeNanos else event.eventTime * 1_000_000L

    private fun historicalTimeNanos(event: MotionEvent, h: Int): Long =
        if (Build.VERSION.SDK_INT >= 34) {
            event.getHistoricalEventTimeNanos(h)
        } else {
            event.getHistoricalEventTime(h) * 1_000_000L
        }

    /** A mouse event: contact and hover, with buttons carried on every message. */
    private fun emitMouseEvent(
        event: MotionEvent,
        actionMasked: Int,
        idx: Int,
        width: Float,
        height: Float
    ): Boolean {
        val base = when (actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> Wire.ACTION_DOWN
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> Wire.ACTION_UP
            MotionEvent.ACTION_MOVE -> Wire.ACTION_MOVE
            MotionEvent.ACTION_CANCEL -> Wire.ACTION_CANCEL
            MotionEvent.ACTION_HOVER_MOVE -> Wire.ACTION_HOVER_MOVE
            else -> return false
        }
        val action = mouseContactAction(base, event.buttonState)
        var buttons = 0
        if ((event.buttonState and MotionEvent.BUTTON_PRIMARY) != 0) buttons = buttons or Wire.MOUSE_BUTTON_PRIMARY
        if ((event.buttonState and MotionEvent.BUTTON_SECONDARY) != 0) buttons = buttons or Wire.MOUSE_BUTTON_SECONDARY
        if ((event.buttonState and MotionEvent.BUTTON_TERTIARY) != 0) buttons = buttons or Wire.MOUSE_BUTTON_TERTIARY
        onPacketReady(
            Wire.Message.Mouse(
                action = action,
                buttons = buttons,
                relative = false,
                x = viewport.toContentX(event.getX(idx)),
                y = viewport.toContentY(event.getY(idx))
            ).encode()
        )
        return true
    }

    /**
     * Decides whether a mouse event is a click, and updates the button-state
     * memory.
     *
     * The platform's own `ACTION_DOWN`/`ACTION_UP` are authoritative and are
     * passed straight through. Without that rule a single physical click would
     * be reported twice, once from the action and once from the button state,
     * and the host would see a double-click.
     *
     * The case that does need help is a device that reports a button press while
     * the pointer is already down on the glass: it arrives as a hover or move
     * whose `buttonState` differs from the last one. Left as a MOVE it would
     * never reach a click handler at all, so a press/release *transition* on a
     * hover or move is promoted to a click here.
     */
    private fun mouseContactAction(baseAction: Int, buttonState: Int): Int {
        val buttons = buttonState and MOUSE_BUTTON_MASK
        val changed = buttons != lastMouseButtons
        lastMouseButtons = buttons

        return when (baseAction) {
            Wire.ACTION_DOWN, Wire.ACTION_UP, Wire.ACTION_CANCEL -> baseAction
            else ->
                if (changed) {
                    if (buttons != 0) Wire.ACTION_DOWN else Wire.ACTION_UP
                } else {
                    baseAction
                }
        }
    }

    /**
     * Forwards one [MotionEvent.ACTION_SCROLL] as a [Wire.Message.Scroll] with
     * the raw AXIS_HSCROLL/AXIS_VSCROLL values and the pointer position
     * (scrolling is positional). The host accumulates fractional deltas from
     * trackpads and high-resolution wheels into whole notches, so nothing is
     * rounded here. Positive vertical means the content moves towards the top
     * of the document, as Android reports it.
     */
    private fun emitScrollEvent(event: MotionEvent, width: Float, height: Float): Boolean {
        val vScroll = event.getAxisValue(MotionEvent.AXIS_VSCROLL)
        val hScroll = event.getAxisValue(MotionEvent.AXIS_HSCROLL)
        if (vScroll == 0f && hScroll == 0f) return false

        onPacketReady(
            Wire.Message.Scroll(
                phase = Wire.PHASE_NONE,
                x = viewport.toContentX(event.getX(0)),
                y = viewport.toContentY(event.getY(0)),
                dx = hScroll,
                dy = vScroll
            ).encode()
        )
        return true
    }

    // -----------------------------------------------------------------------
    // Keyboard
    // -----------------------------------------------------------------------

    /**
     * Sends one key event as a [Wire.Message.Key].
     *
     * Exactly one message per [KeyEvent]: the platform already guarantees one
     * `ACTION_DOWN` per press (with `repeatCount > 0` for auto-repeat) followed
     * by one `ACTION_UP`, so adding a second emit path here would double-report
     * every keystroke.
     */
    private fun handleKeyEvent(event: KeyEvent) {
        // KeyEvent only ever has ACTION_DOWN, ACTION_UP and ACTION_MULTIPLE, and
        // ACTION_MULTIPLE is expanded into individual down/up pairs before it
        // reaches here - so this pair is exhaustive.
        val action = when (event.action) {
            KeyEvent.ACTION_DOWN -> Wire.ACTION_DOWN
            KeyEvent.ACTION_UP -> Wire.ACTION_UP
            else -> return
        }

        var meta = keyFlags(event)
        if (action == Wire.ACTION_DOWN && stickyMeta != 0) {
            meta = meta or stickyMeta
            stickyMeta = 0
            onStickyConsumed?.invoke()
        }
        onPacketReady(
            Wire.Message.Key(
                action = action,
                keyCode = event.keyCode,
                scanCode = event.scanCode,
                meta = meta,
                // Only the press carries text. The release is fully described by
                // the key code, and repeating the character there would make the
                // host type two of everything.
                text = if (action == Wire.ACTION_DOWN) unicodeTextOf(event) else ""
            ).encode()
        )
    }

    /**
     * Modifier and repeat state for a key event.
     *
     * Modifiers are sent with every event rather than as separate presses: a
     * host that has to synthesise Shift from an Android keycode has to know the
     * shift state anyway for `KEYCODE_A` to mean "A" and not "a", and for a
     * shortcut to be more than a string of individual characters.
     */
    private fun keyFlags(event: KeyEvent): Int {
        var flags = 0
        if (event.isShiftPressed) flags = flags or Wire.KEY_FLAG_SHIFT
        if (event.isCtrlPressed) flags = flags or Wire.KEY_FLAG_CTRL
        if (event.isAltPressed) flags = flags or Wire.KEY_FLAG_ALT
        if (event.isMetaPressed) flags = flags or Wire.KEY_FLAG_META
        if (event.isCapsLockOn) flags = flags or Wire.KEY_FLAG_CAPS_LOCK
        if (event.isNumLockOn) flags = flags or Wire.KEY_FLAG_NUM_LOCK
        // Auto-repeat is distinguished from the initial press so the host can
        // decide whether to re-type the character or just repeat the key.
        if (event.repeatCount > 0) flags = flags or Wire.KEY_FLAG_REPEAT
        return flags
    }

    /**
     * The character a key press produced, or "" if it produced none.
     *
     * `KeyEvent.getUnicodeChar()` is an int-returning getter, so `unicodeChar`
     * is an [Int] here and has to be converted explicitly - using it directly
     * would send the string "97" rather than the letter "a".
     *
     * It returns [CHARACTER_UNDEFINED] for keys with no text meaning (arrows,
     * modifiers, function keys) and 0 for a few odd keys. Both mean "no text",
     * not "a NUL character to type", and forwarding either would make the host
     * type garbage.
     */
    @Suppress("DEPRECATION")
    private fun unicodeTextOf(event: KeyEvent): String {
        val code = event.unicodeChar
        if (code == 0 || code == CHARACTER_UNDEFINED) return ""
        return code.toChar().toString()
    }
}
