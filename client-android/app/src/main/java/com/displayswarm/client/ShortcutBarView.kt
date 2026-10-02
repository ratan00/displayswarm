package com.displayswarm.client

import android.content.Context
import android.content.res.ColorStateList
import android.graphics.Color
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import androidx.core.content.ContextCompat
import com.google.android.material.button.MaterialButton
import com.google.android.material.chip.Chip

/**
 * The Esc / Tab / Ctrl / ... bar as a platform [HorizontalScrollView] laid over the
 * video, like the rest of the native scrolling. The Compose version scrolled badly:
 * a sideways drag over it was also seen by the touch layer under it, and Compose
 * decides who owns a drag only after the touch slop. A platform scroll view claims
 * the gesture at once and gives standard fling.
 */
class ShortcutBarView(context: Context, private val ui: ClientUiState, private val actions: ClientActions) :
    HorizontalScrollView(context) {
    private val chips = ArrayList<Pair<Chip, Int>>()

    init {
        isHorizontalScrollBarEnabled = false
        overScrollMode = OVER_SCROLL_IF_CONTENT_SCROLLS
        setBackgroundColor(Color.parseColor("#E61E1E2E"))
        visibility = GONE
        val row = LinearLayout(context).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            val pad = dp(6)
            setPadding(pad, pad, pad, pad)
        }
        addView(row, ViewGroup.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT))
        val text = ContextCompat.getColor(context, R.color.vm_text)
        val tonal = ContextCompat.getColor(context, R.color.vm_surface_variant)
        for ((label, bit) in listOf("Ctrl" to Wire.KEY_FLAG_CTRL, "Alt" to Wire.KEY_FLAG_ALT, "Shift" to Wire.KEY_FLAG_SHIFT, "Super" to Wire.KEY_FLAG_META)) {
            val chip = Chip(context).apply {
                this.text = label
                isCheckable = false
                setTextColor(text)
                chipStrokeColor = ColorStateList.valueOf(tonal)
                chipStrokeWidth = dp(1).toFloat()
                setOnClickListener { actions.setLatchedMeta(ui.latchedMeta xor bit); refresh() }
            }
            chips.add(chip to bit)
            row.addView(chip, spaced())
        }
        for ((label, spec) in SHORTCUT_KEYS) {
            row.addView(MaterialButton(context).apply {
                this.text = label
                isAllCaps = false
                setTextColor(text)
                backgroundTintList = ColorStateList.valueOf(tonal)
                setOnClickListener {
                    val combo = KeyCombos.parse(spec) ?: return@setOnClickListener
                    // Latched modifiers apply to this one key, then clear.
                    actions.sendMessages(combo.copy(meta = combo.meta or ui.latchedMeta).messages())
                    if (ui.latchedMeta != 0) { actions.setLatchedMeta(0); refresh() }
                }
            }, spaced())
        }
        refresh()
    }

    /** Shows the latched modifiers (call when [ClientUiState.latchedMeta] changes). */
    fun refresh() {
        val active = ContextCompat.getColor(context, R.color.vm_surface_variant)
        val idle = Color.TRANSPARENT
        for ((chip, bit) in chips) {
            chip.chipBackgroundColor = ColorStateList.valueOf(if (ui.latchedMeta and bit != 0) active else idle)
        }
    }

    private fun spaced() = LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT).apply {
        marginEnd = dp(6)
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    companion object {
        /** Adds a bar at the bottom of [root]; above the Compose layer so it gets touches first. */
        fun attach(root: FrameLayout, ui: ClientUiState, actions: ClientActions): ShortcutBarView {
            val bar = ShortcutBarView(root.context, ui, actions)
            root.addView(bar, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.BOTTOM))
            return bar
        }
    }
}
