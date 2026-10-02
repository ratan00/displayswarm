package com.displayswarm.client

/**
 * The local zoom and pan of the video surface, as pure math.
 *
 * The video always fills the view at scale 1. Zooming scales it about a focus
 * point and panning slides it, but it never leaves the view: the visible window
 * is clamped to the content, so no black borders open up.
 *
 * State is a scale in `[1, MAX_SCALE]` and a translation `(panX, panY)` in view
 * pixels, applied as `screen = content * scale + pan` with content in view
 * pixels (0..width, 0..height). Because the content is never smaller than the
 * view, `pan` lies in `[size * (1 - scale), 0]` on each axis.
 *
 * [toContentX] / [toContentY] are the inverse used for input: a touch at a view
 * position becomes the normalised (0..1) position on the video, which is what
 * the host expects, so touches still land on the right host pixel while zoomed.
 */
class Viewport(width: Float = 1f, height: Float = 1f) {
    companion object {
        const val MAX_SCALE = 4f
    }

    var width = width.coerceAtLeast(1f)
        private set
    var height = height.coerceAtLeast(1f)
        private set

    var scale = 1f
        private set
    var panX = 0f
        private set
    var panY = 0f
        private set

    val isIdentity: Boolean get() = scale == 1f

    /** The view changed size (rotation, layout). Keeps the zoom, re-clamps the pan. */
    fun setSize(w: Float, h: Float) {
        width = w.coerceAtLeast(1f)
        height = h.coerceAtLeast(1f)
        clamp()
    }

    fun reset() {
        scale = 1f
        panX = 0f
        panY = 0f
    }

    /**
     * Multiplies the zoom by [factor] keeping the content under the view point
     * ([focusX], [focusY]) where it is.
     */
    fun zoomBy(factor: Float, focusX: Float, focusY: Float) {
        if (factor <= 0f || factor.isNaN()) return
        val next = (scale * factor).coerceIn(1f, MAX_SCALE)
        val k = next / scale
        // The content point under the focus stays put: focus = c * scale + pan.
        panX = focusX - (focusX - panX) * k
        panY = focusY - (focusY - panY) * k
        scale = next
        clamp()
    }

    /** Moves the content by ([dx], [dy]) view pixels. */
    fun panBy(dx: Float, dy: Float) {
        panX += dx
        panY += dy
        clamp()
    }

    /** View x (pixels) -> normalised video x. */
    fun toContentX(viewX: Float): Float = ((viewX - panX) / (scale * width)).coerceIn(0f, 1f)

    /** View y (pixels) -> normalised video y. */
    fun toContentY(viewY: Float): Float = ((viewY - panY) / (scale * height)).coerceIn(0f, 1f)

    private fun clamp() {
        panX = panX.coerceIn(width * (1f - scale), 0f)
        panY = panY.coerceIn(height * (1f - scale), 0f)
    }
}
