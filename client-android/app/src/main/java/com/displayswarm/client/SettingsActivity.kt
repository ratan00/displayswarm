package com.displayswarm.client

import android.content.res.ColorStateList
import android.graphics.Color
import android.os.Bundle
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import com.google.android.material.button.MaterialButton
import com.google.android.material.chip.Chip
import com.google.android.material.chip.ChipGroup
import com.google.android.material.slider.Slider
import com.google.android.material.switchmaterial.SwitchMaterial

/**
 * The settings screen, built from platform views in a plain [ScrollView] the way
 * Android's own Settings app scrolls (standard fling and overscroll), instead of
 * inside the Compose layer over the video: scrolling there stuttered and jumped.
 *
 * It is a separate, opaque activity: the session activity behind it stops drawing
 * while it is open (the stream resumes with a keyframe on return), which is what
 * keeps the scrolling smooth; a translucent window made the compositor blend the
 * live video under every frame. It reads and writes the same state
 * ([ClientUiState] and [AppSettings]) and calls the same [ClientActions] as the
 * Compose screens, through [SettingsBridge].
 */
class SettingsActivity : AppCompatActivity() {
    private val fg = Color.parseColor("#CDD6F4")
    private val muted = Color.parseColor("#A6ADC8")

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val state = SettingsBridge.ui
        val actions = SettingsBridge.actions
        if (state == null || actions == null) {
            finish()
            return
        }
        val s = state.settings
        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(24), dp(24), dp(24), dp(48))
        }
        val scroll = ScrollView(this).apply {
            setBackgroundColor(Color.parseColor("#11111B"))
            isFillViewport = true
            overScrollMode = View.OVER_SCROLL_IF_CONTENT_SCROLLS
            addView(column, ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT)
        }
        setContentView(scroll)

        column.addView(LinearLayout(this).apply {
            gravity = Gravity.CENTER_VERTICAL
            addView(text("Settings", 24f, fg).apply { layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f) })
            addView(themedButton("Done") { finish() })
        })

        column.section("Display")
        column.switchRow("Keep screen on", null, state.keepScreenOn) { state.keepScreenOn = it; s.keepScreenOn = it; actions.settingsChanged() }
        column.switchRow("Auto-reconnect", "Reconnect when the link drops", state.autoReconnect) { state.autoReconnect = it; s.autoReconnect = it }
        column.switchRow("Show stats", null, state.showStats) { state.showStats = it; s.showStats = it }
        column.switchRow("Lock rotation", null, state.rotationLock) { actions.setRotationLock(it) }

        column.section("Picture quality")
        column.chips(listOf("Auto", "Maximum", "Balanced", "Data saver"), state.videoQuality) { actions.setVideoQuality(it) }
        column.summary("Auto adapts the bitrate to the link. Maximum is the sharpest and uses the most bandwidth, Balanced sits in between, Data saver is low quality for a weak link. Same on USB and Wi-Fi.")

        column.section("Sound")
        column.switchRow("Host audio on this phone", null, state.audioOut) { actions.setAudio(it) }
        column.switchRow("Phone microphone to host", null, state.mic) { actions.setMic(it) }

        column.section("Touchpad")
        column.switchRow("Touchpad mode", "Off: fingers act directly on the screen", state.touchMode == TouchMode.TOUCHPAD) {
            actions.setTouchMode(if (it) TouchMode.TOUCHPAD else TouchMode.DIRECT)
        }
        column.switchRow("Natural scrolling", null, state.naturalScroll) { state.naturalScroll = it; s.naturalScroll = it; actions.settingsChanged() }
        column.addView(text("Pointer speed", 16f, fg).apply { setPadding(0, dp(8), 0, 0) })
        column.addView(Slider(this).apply {
            valueFrom = 0.5f
            valueTo = 2.5f
            value = state.pointerSpeed.coerceIn(0.5f, 2.5f)
            addOnChangeListener { _, v, fromUser -> if (fromUser) state.pointerSpeed = v }
            addOnSliderTouchListener(object : Slider.OnSliderTouchListener {
                override fun onStartTrackingTouch(slider: Slider) {}
                override fun onStopTrackingTouch(slider: Slider) { s.pointerSpeed = state.pointerSpeed; actions.settingsChanged() }
            })
        })

        column.section("Pen")
        column.switchRow("Palm rejection", "Ignore fingers while the pen is near", state.palmRejection) {
            state.palmRejection = it; s.palmRejection = it; actions.settingsChanged()
        }
        column.switchRow("Stroke prediction preview", "Shows a local dot ahead of the pen; nothing predicted is sent", state.strokePrediction) {
            state.strokePrediction = it; s.strokePrediction = it; actions.settingsChanged()
        }
        val actionsList = PenButtonAction.entries
        column.addView(text("Barrel button 1", 16f, fg).apply { setPadding(0, dp(8), 0, 0) })
        column.chips(actionsList.map { it.label }, actionsList.indexOf(state.penButton1)) {
            state.penButton1 = actionsList[it]; s.penButton1 = actionsList[it]; actions.settingsChanged()
        }
        column.addView(text("Barrel button 2", 16f, fg).apply { setPadding(0, dp(8), 0, 0) })
        column.chips(actionsList.map { it.label }, actionsList.indexOf(state.penButton2)) {
            state.penButton2 = actionsList[it]; s.penButton2 = actionsList[it]; actions.settingsChanged()
        }
        column.addView(themedButton("Reset macro buttons") { state.macros = Macro.DEFAULTS; s.macros = Macro.DEFAULTS }.apply {
            (layoutParams as? LinearLayout.LayoutParams)?.topMargin = dp(16)
        })
    }

    /** A button in the app palette: surface-variant fill, light text. */
    private fun themedButton(label: String, onClick: () -> Unit) = MaterialButton(this).apply {
        text = label
        isAllCaps = false
        setTextColor(ContextCompat.getColor(context, R.color.vm_text))
        backgroundTintList = ColorStateList.valueOf(ContextCompat.getColor(context, R.color.vm_surface_variant))
        layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT)
        setOnClickListener { onClick() }
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    private fun text(t: String, sp: Float, color: Int) = TextView(this).apply {
        text = t
        textSize = sp
        setTextColor(color)
    }

    private fun LinearLayout.section(title: String) {
        addView(text(title, 18f, Color.parseColor("#B4BEFE")).apply {
            setTypeface(typeface, android.graphics.Typeface.BOLD)
            setPadding(0, dp(20), 0, dp(4))
        })
    }

    private fun LinearLayout.summary(t: String) {
        addView(text(t, 13f, muted).apply { setPadding(0, dp(4), 0, 0) })
    }

    private fun LinearLayout.switchRow(title: String, summary: String?, checked: Boolean, onChange: (Boolean) -> Unit) {
        addView(SwitchMaterial(this@SettingsActivity).apply {
            text = title
            textSize = 16f
            setTextColor(fg)
            minHeight = dp(52)
            isChecked = checked
            setOnCheckedChangeListener { _, v -> onChange(v) }
        })
        if (summary != null) {
            addView(text(summary, 13f, muted).apply { setPadding(0, 0, 0, dp(4)) })
        }
    }

    private fun LinearLayout.chips(labels: List<String>, selected: Int, onSelect: (Int) -> Unit) {
        val group = ChipGroup(this@SettingsActivity).apply { isSingleSelection = true; isSelectionRequired = true }
        labels.forEachIndexed { i, label ->
            group.addView(Chip(this@SettingsActivity).apply {
                id = View.generateViewId()
                text = label
                isCheckable = true
                isChecked = i == selected
                setTextColor(fg)
                chipBackgroundColor = ColorStateList(
                    arrayOf(intArrayOf(android.R.attr.state_checked), intArrayOf()),
                    intArrayOf(ContextCompat.getColor(context, R.color.vm_surface_variant), ContextCompat.getColor(context, R.color.vm_surface))
                )
                chipStrokeColor = ColorStateList.valueOf(ContextCompat.getColor(context, R.color.vm_surface_variant))
                chipStrokeWidth = dp(1).toFloat()
                setOnClickListener { onSelect(i) }
            })
        }
        addView(group)
    }
}

/** How [SettingsActivity] reaches the running session's UI state and actions (same process). */
object SettingsBridge {
    var ui: ClientUiState? = null
    var actions: ClientActions? = null
}
