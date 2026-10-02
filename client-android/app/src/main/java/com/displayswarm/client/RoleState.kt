package com.displayswarm.client

/**
 * The role currently in effect for this phone, as far as the phone knows.
 * Immutable; every transition returns a new value. The host is the source of
 * truth: [role] only ever changes from a HelloAck, a host SetRole, or the
 * phone's own request (which the host echoes back, or overrides).
 */
data class RoleState(
    /** Effective role. Mirror until the host says otherwise, also while it is unset. */
    val role: Int = Wire.ROLE_MIRROR,
    /** The host has no remembered role for this device: offer the chooser. */
    val chooserPending: Boolean = false
) {
    /** False for roles where the host sends no video (Drawing tablet, Input pad). */
    val hasVideo: Boolean get() = hasVideo(role)

    val label: String get() = label(role)

    fun onHelloAck(ackRole: Int): RoleState = when {
        isValid(ackRole) -> RoleState(ackRole, chooserPending = false)
        ackRole == Wire.ROLE_UNSET -> RoleState(Wire.ROLE_MIRROR, chooserPending = true)
        else -> RoleState(Wire.ROLE_MIRROR, chooserPending = false) // unknown value: stay safe
    }

    /** The host states the role now in effect. Unknown values are ignored. */
    fun onHostSetRole(newRole: Int): RoleState =
        if (isValid(newRole)) RoleState(newRole, chooserPending = false) else this

    /** The user picked [newRole]; null when it is not a role that can be requested. */
    fun request(newRole: Int): RoleState? =
        if (isValid(newRole)) RoleState(newRole, chooserPending = false) else null

    /** The chooser was dismissed without a choice: keep the current (Mirror) role. */
    fun dismissChooser(): RoleState = copy(chooserPending = false)

    companion object {
        val ALL_ROLES = intArrayOf(
            Wire.ROLE_MIRROR, Wire.ROLE_EXTEND, Wire.ROLE_MIRROR_WINDOW,
            Wire.ROLE_PHONE_PRIMARY, Wire.ROLE_TABLET, Wire.ROLE_INPUT_PAD
        )

        fun isValid(role: Int) = role in ALL_ROLES

        fun hasVideo(role: Int) = role != Wire.ROLE_TABLET && role != Wire.ROLE_INPUT_PAD

        fun label(role: Int): String = when (role) {
            Wire.ROLE_MIRROR -> "Mirror"
            Wire.ROLE_EXTEND -> "Extend"
            Wire.ROLE_MIRROR_WINDOW -> "Mirror window"
            Wire.ROLE_PHONE_PRIMARY -> "Phone as main screen"
            Wire.ROLE_TABLET -> "Drawing tablet"
            Wire.ROLE_INPUT_PAD -> "Input pad"
            else -> "Unknown"
        }

        fun description(role: Int): String = when (role) {
            Wire.ROLE_MIRROR -> "Show a copy of the host screen"
            Wire.ROLE_EXTEND -> "Extra desktop area"
            Wire.ROLE_MIRROR_WINDOW -> "Show one host window"
            Wire.ROLE_PHONE_PRIMARY -> "Use this phone as the main screen"
            Wire.ROLE_TABLET -> "Pen and touch input only, no video"
            Wire.ROLE_INPUT_PAD -> "Touchpad and keyboard only, no video"
            else -> ""
        }

        /** Hint shown on the plain surface of a role without video. */
        fun hint(role: Int): String = when (role) {
            Wire.ROLE_TABLET -> "Drawing tablet\nDraw here; input goes to the host"
            Wire.ROLE_INPUT_PAD -> "Input pad\nMove and tap here to control the host"
            else -> ""
        }
    }
}
