package com.displayswarm.client

import java.io.ByteArrayOutputStream
import java.io.DataOutputStream
import java.io.EOFException
import java.io.IOException
import java.io.InputStream
import java.nio.ByteBuffer
import java.nio.ByteOrder

/** The peer broke the protocol; the stream cannot be trusted any more. */
class ProtocolException(message: String) : IOException(message)

/**
 * DisplaySwarm protocol v2: framing, channels and messages.
 *
 * Mirrors `host/src/protocol/wire.rs` byte for byte; see that file for the
 * full description. Both sides check their encoders against the golden vectors
 * in `protocol/v2-vectors.txt` (see `WireTest`).
 *
 * Frame (big endian): `"VM" | version=2 | channel | type | flags | len u32 |
 * payload`. The first 6 bytes, the HelloAck status byte and the Bye payload
 * are frozen for every version. A message longer than the sender's fragment
 * size is split into frames flagged [FLAG_MORE]; frames of other channels may
 * interleave between them.
 *
 * Integer fields wider than Kotlin's signed types are carried as the next
 * wider type: u8/u16 as [Int], u32 as [Long]. u64 clock values fit a [Long].
 */
object Wire {
    const val VERSION = 2
    const val HEADER_SIZE = 10
    const val FLAG_MORE = 0x01
    const val MAX_FRAGMENT = 64 * 1024
    private const val MAGIC0 = 0x56 // 'V'
    private const val MAGIC1 = 0x4D // 'M'

    // Channels; the number is also the send priority, lowest first.
    const val CH_CONTROL = 0
    const val CH_INPUT = 1
    const val CH_AUDIO = 2
    const val CH_VIDEO = 3
    const val CH_CLIPBOARD = 4
    const val CH_FILE = 5
    private const val CHANNEL_COUNT = 6

    fun maxMessageLen(channel: Int): Int = when (channel) {
        CH_VIDEO, CH_CLIPBOARD -> 16 * 1024 * 1024
        CH_FILE -> 1024 * 1024
        else -> MAX_FRAGMENT
    }

    // Message types.
    const val MSG_HELLO = 0x01
    const val MSG_HELLO_ACK = 0x02
    const val MSG_PING = 0x03
    const val MSG_PONG = 0x04
    const val MSG_HEARTBEAT = 0x05
    const val MSG_BYE = 0x06
    const val MSG_STATS = 0x07
    const val MSG_HOST_STATE = 0x08
    const val MSG_KEYFRAME_REQUEST = 0x09
    const val MSG_SET_BITRATE = 0x0A
    const val MSG_RESIZE = 0x0B
    const val MSG_SET_ROLE = 0x0C
    /** Phone -> host: picture quality, 0 auto, 1 maximum, 2 balanced, 3 data saver. */
    const val MSG_SET_QUALITY = 0x0D
    /** Phone -> host: audio pipeline latency in microseconds (host pts -> speaker). */
    const val MSG_AUDIO_LATENCY = 0x0E
    /** Host -> phone: common playout delay for multi-phone sync, 0 = off. */
    const val MSG_AUDIO_SYNC = 0x0F
    const val MSG_SERVICE_STATE = 0x10

    /** `ServiceState.enabled` bits. */
    const val SERVICE_AUDIO_OUT = 1
    const val SERVICE_MIC = 2
    const val MSG_BATTERY_STATUS = 0x21
    /** Pairing, network transports only, before the Hello. 0x38/0x39: 0x30/0x31 are audio. */
    const val MSG_PAIR_REQUEST = 0x38
    const val MSG_PAIR_RESPONSE = 0x39
    const val PAIR_MODE_TOKEN = 0
    const val PAIR_MODE_PIN = 1
    const val PAIR_MODE_QR = 2
    const val PAIR_OK = 0
    const val PAIR_PIN_REQUIRED = 1
    const val PAIR_WRONG = 2
    const val PAIR_LOCKED = 3
    const val PAIR_UNTRUSTED = 4
    const val PAIR_DISABLED = 5
    const val MSG_VIDEO_FRAME = 0x20
    const val MSG_AUDIO_FRAME = 0x30
    const val MSG_MIC_FRAME = 0x31
    const val MSG_TOUCH = 0x40
    const val MSG_PEN = 0x41
    const val MSG_MOUSE = 0x42
    const val MSG_SCROLL = 0x43
    const val MSG_PINCH = 0x44
    const val MSG_KEY = 0x45
    const val MSG_TEXT = 0x46
    const val MSG_CLIPBOARD = 0x50
    const val MSG_FILE_OFFER = 0x60
    const val MSG_FILE_CHUNK = 0x61
    const val MSG_FILE_CONTROL = 0x62

    /** The channel a known type travels on, or null for an unknown type. */
    fun channelOf(type: Int): Int? = when (type) {
        in 0x01..0x10, MSG_BATTERY_STATUS, MSG_PAIR_REQUEST, MSG_PAIR_RESPONSE -> CH_CONTROL
        MSG_VIDEO_FRAME -> CH_VIDEO
        MSG_AUDIO_FRAME, MSG_MIC_FRAME -> CH_AUDIO
        in 0x40..0x46 -> CH_INPUT
        MSG_CLIPBOARD -> CH_CLIPBOARD
        in MSG_FILE_OFFER..MSG_FILE_CONTROL -> CH_FILE
        else -> null
    }

    // Field values.
    const val CODEC_H264 = 0x01
    const val CODEC_HEVC = 0x02
    const val CODEC_AV1 = 0x04

    const val FEATURE_TOUCH = 1L shl 0
    const val FEATURE_STYLUS = 1L shl 1
    const val FEATURE_KEYBOARD = 1L shl 2
    const val FEATURE_AUDIO_OUT = 1L shl 3
    const val FEATURE_MIC = 1L shl 4
    const val FEATURE_CLIPBOARD = 1L shl 5
    const val FEATURE_FILES = 1L shl 6
    const val FEATURE_BATTERY = 1L shl 8

    // BatteryStatus.thermal: android.os.PowerManager.THERMAL_STATUS_*.
    const val THERMAL_NONE = 0
    const val THERMAL_LIGHT = 1
    const val THERMAL_MODERATE = 2
    const val THERMAL_SEVERE = 3
    const val THERMAL_CRITICAL = 4
    const val THERMAL_EMERGENCY = 5
    const val THERMAL_SHUTDOWN = 6
    /** BatteryStatus.tempDecidegrees when unknown. */
    const val TEMP_UNKNOWN = -32768

    const val HELLO_OK = 0
    const val HELLO_VERSION_MISMATCH = 1
    const val HELLO_BUSY = 2
    const val HELLO_REJECTED = 3

    const val ROLE_MIRROR = 0
    const val ROLE_EXTEND = 1
    const val ROLE_MIRROR_WINDOW = 2
    const val ROLE_PHONE_PRIMARY = 3
    const val ROLE_TABLET = 4
    const val ROLE_INPUT_PAD = 5

    /** HelloAck only: the host has no role remembered for this device; show the chooser. */
    const val ROLE_UNSET = 0xFF

    const val BYE_NORMAL = 0
    const val BYE_SERVER_STOPPING = 1
    const val BYE_ERROR = 2
    const val BYE_VERSION_MISMATCH = 3
    const val BYE_PROTOCOL_ERROR = 4

    const val HOST_STATE_AWAITING_PERMISSION = 1
    const val HOST_STATE_STREAMING = 2
    const val HOST_STATE_CAPTURE_FAILED = 3

    const val FRAME_TYPE_CONFIG = 1 // SPS / PPS
    const val FRAME_TYPE_KEY = 2 // IDR
    const val FRAME_TYPE_DELTA = 3 // P-frame

    const val ACTION_DOWN = 0
    const val ACTION_UP = 1
    const val ACTION_MOVE = 2
    const val ACTION_CANCEL = 3
    const val ACTION_HOVER_MOVE = 4
    const val ACTION_HOVER_EXIT = 5

    const val PEN_TOOL_PEN = 0
    const val PEN_TOOL_ERASER = 1
    const val PEN_BUTTON_PRIMARY = 0x01
    const val PEN_BUTTON_SECONDARY = 0x02

    const val MOUSE_BUTTON_PRIMARY = 0x01
    const val MOUSE_BUTTON_SECONDARY = 0x02
    const val MOUSE_BUTTON_TERTIARY = 0x04

    const val PHASE_NONE = 0
    const val PHASE_BEGIN = 1
    const val PHASE_UPDATE = 2
    const val PHASE_END = 3
    const val PHASE_INERTIA = 4

    /** `Key.meta` bits. */
    const val KEY_FLAG_SHIFT = 0x01
    const val KEY_FLAG_CTRL = 0x02
    const val KEY_FLAG_ALT = 0x04
    const val KEY_FLAG_META = 0x08
    const val KEY_FLAG_CAPS_LOCK = 0x10
    const val KEY_FLAG_NUM_LOCK = 0x20
    const val KEY_FLAG_REPEAT = 0x40

    const val FILE_ACCEPT = 0
    const val FILE_REJECT = 1
    const val FILE_CANCEL = 2
    const val FILE_COMPLETE = 3

    data class Hello(
        val width: Int,
        val height: Int,
        val refreshMhz: Long,
        val densityDpi: Int,
        val codecs: Int,
        val features: Long,
        val maxTouchPoints: Int,
        val deviceId: String,
        val deviceName: String,
        val appVersion: String
    )

    data class HelloAck(
        val status: Int,
        val width: Int = 0,
        val height: Int = 0,
        val fps: Int = 0,
        val codec: Int = 0,
        val features: Long = 0,
        val role: Int = 0,
        val hostName: String = "",
        val message: String = ""
    )

    data class Stats(
        val framesDecoded: Long,
        val framesDropped: Long,
        val decodeLatencyUs: Long,
        val presentLatencyUs: Long,
        val rxKbps: Long
    )

    data class TouchPoint(val id: Int, val x: Float, val y: Float, val pressure: Float)

    data class PenSample(
        val ageUs: Long,
        val x: Float,
        val y: Float,
        val pressure: Float,
        val tiltX: Float,
        val tiltY: Float
    )

    /** Every message of protocol v2. */
    sealed class Message(val type: Int) {
        open val channel: Int get() = channelOf(type)!!

        data class HelloMsg(val hello: Hello) : Message(MSG_HELLO)
        data class HelloAckMsg(val ack: HelloAck) : Message(MSG_HELLO_ACK)
        data class Ping(val id: Long, val tSendUs: Long) : Message(MSG_PING)

        /** `tPingUs` echoes the ping; `tRecvUs`/`tReplyUs` are the responder's clock. */
        data class Pong(val id: Long, val tPingUs: Long, val tRecvUs: Long, val tReplyUs: Long) :
            Message(MSG_PONG)

        object Heartbeat : Message(MSG_HEARTBEAT) {
            override fun toString() = "Heartbeat"
        }

        data class Bye(val reason: Int, val text: String) : Message(MSG_BYE)
        data class StatsMsg(val stats: Stats) : Message(MSG_STATS)
        data class HostState(val state: Int, val detail: String) : Message(MSG_HOST_STATE)

        object KeyframeRequest : Message(MSG_KEYFRAME_REQUEST) {
            override fun toString() = "KeyframeRequest"
        }

        data class SetBitrate(val kbps: Long) : Message(MSG_SET_BITRATE)
        data class Resize(val width: Int, val height: Int, val densityDpi: Int, val rotation: Int) :
            Message(MSG_RESIZE)

        data class SetRole(val role: Int) : Message(MSG_SET_ROLE)

        data class SetQuality(val quality: Int) : Message(MSG_SET_QUALITY)
        data class AudioLatency(val latencyUs: Long) : Message(MSG_AUDIO_LATENCY)
        data class AudioSync(val delayUs: Long) : Message(MSG_AUDIO_SYNC)
        data class ServiceState(val enabled: Int) : Message(MSG_SERVICE_STATE)

        /** Phone -> host: battery percent, charging, thermal status, battery temperature in 0.1 C. */
        data class BatteryStatus(val percent: Int, val charging: Boolean, val thermal: Int, val tempDecidegrees: Int) :
            Message(MSG_BATTERY_STATUS)
        class PairRequest(
            val mode: Int,
            val deviceId: String,
            val deviceName: String,
            val credential: ByteArray
        ) : Message(MSG_PAIR_REQUEST) {
            override fun equals(other: Any?) = other is PairRequest && mode == other.mode &&
                deviceId == other.deviceId && deviceName == other.deviceName &&
                credential.contentEquals(other.credential)

            override fun hashCode() = credential.contentHashCode() * 31 + deviceId.hashCode()
        }

        class PairResponse(val status: Int, val message: String, val token: ByteArray) :
            Message(MSG_PAIR_RESPONSE) {
            override fun equals(other: Any?) = other is PairResponse && status == other.status &&
                message == other.message && token.contentEquals(other.token)

            override fun hashCode() = token.contentHashCode() * 31 + status
        }

        class VideoFrame(
            val codec: Int,
            val frameType: Int,
            val frameIndex: Long,
            val captureUs: Long,
            val data: ByteArray
        ) : Message(MSG_VIDEO_FRAME) {
            override fun equals(other: Any?) = other is VideoFrame && codec == other.codec &&
                frameType == other.frameType && frameIndex == other.frameIndex &&
                captureUs == other.captureUs && data.contentEquals(other.data)

            override fun hashCode() = data.contentHashCode() * 31 + frameIndex.hashCode()
            override fun toString() = "VideoFrame(type=$frameType, index=$frameIndex, ${data.size} B)"
        }

        /** Opus audio: host -> phone ([MSG_AUDIO_FRAME]) or phone mic -> host ([MSG_MIC_FRAME]). */
        class Audio(
            type: Int,
            val seq: Long,
            val ptsUs: Long,
            val channels: Int,
            val data: ByteArray
        ) : Message(type) {
            override fun equals(other: Any?) = other is Audio && type == other.type &&
                seq == other.seq && ptsUs == other.ptsUs && channels == other.channels &&
                data.contentEquals(other.data)

            override fun hashCode() = data.contentHashCode() * 31 + seq.hashCode()
            override fun toString() = "Audio(type=$type, seq=$seq, ${data.size} B)"
        }

        data class Touch(val action: Int, val actionId: Int, val points: List<TouchPoint>) :
            Message(MSG_TOUCH)

        data class Pen(val action: Int, val tool: Int, val buttons: Int, val samples: List<PenSample>) :
            Message(MSG_PEN)

        data class Mouse(
            val action: Int,
            val buttons: Int,
            val relative: Boolean,
            val x: Float,
            val y: Float
        ) : Message(MSG_MOUSE)

        /** Wheel detents with Android's AXIS_HSCROLL/AXIS_VSCROLL signs. */
        data class Scroll(val phase: Int, val x: Float, val y: Float, val dx: Float, val dy: Float) :
            Message(MSG_SCROLL)

        data class Pinch(val phase: Int, val cx: Float, val cy: Float, val scale: Float, val rotation: Float) :
            Message(MSG_PINCH)

        data class Key(
            val action: Int,
            val keyCode: Int,
            val scanCode: Int,
            val meta: Int,
            val text: String
        ) : Message(MSG_KEY)

        data class Text(val text: String) : Message(MSG_TEXT)

        class Clipboard(val mime: String, val data: ByteArray) : Message(MSG_CLIPBOARD) {
            override fun equals(other: Any?) =
                other is Clipboard && mime == other.mime && data.contentEquals(other.data)

            override fun hashCode() = mime.hashCode() * 31 + data.contentHashCode()
        }

        data class FileOffer(val id: Long, val size: Long, val name: String, val mime: String) :
            Message(MSG_FILE_OFFER)

        class FileChunk(val id: Long, val offset: Long, val data: ByteArray) : Message(MSG_FILE_CHUNK) {
            override fun equals(other: Any?) = other is FileChunk && id == other.id &&
                offset == other.offset && data.contentEquals(other.data)

            override fun hashCode() = data.contentHashCode() * 31 + id.hashCode()
        }

        data class FileControl(val id: Long, val op: Int) : Message(MSG_FILE_CONTROL)

        /** A type this build does not know; skipped. */
        class Unknown(override val channel: Int, type: Int, val payload: ByteArray) : Message(type) {
            override fun equals(other: Any?) = other is Unknown && channel == other.channel &&
                type == other.type && payload.contentEquals(other.payload)

            override fun hashCode() = payload.contentHashCode() * 31 + type
            override fun toString() = "Unknown(channel=$channel, type=$type, ${payload.size} B)"
        }

        fun encodePayload(): ByteArray {
            val bytes = ByteArrayOutputStream()
            val w = DataOutputStream(bytes)
            fun str16(s: String) {
                val b = truncateUtf8(s, 0xFFFF)
                w.writeShort(b.size)
                w.write(b)
            }
            fun u32(v: Long) = w.writeInt(v.toInt())
            when (this) {
                is HelloMsg -> with(hello) {
                    w.writeShort(width); w.writeShort(height); u32(refreshMhz)
                    w.writeShort(densityDpi); w.writeByte(codecs); u32(features)
                    w.writeByte(maxTouchPoints)
                    str16(deviceId); str16(deviceName); str16(appVersion)
                }
                is HelloAckMsg -> with(ack) {
                    w.writeByte(status); w.writeShort(width); w.writeShort(height)
                    w.writeShort(fps); w.writeByte(codec); u32(features); w.writeByte(role)
                    str16(hostName); str16(message)
                }
                is Ping -> { u32(id); w.writeLong(tSendUs) }
                is Pong -> { u32(id); w.writeLong(tPingUs); w.writeLong(tRecvUs); w.writeLong(tReplyUs) }
                Heartbeat, KeyframeRequest -> {}
                is Bye -> { w.writeByte(reason); str16(text) }
                is StatsMsg -> with(stats) {
                    u32(framesDecoded); u32(framesDropped); u32(decodeLatencyUs)
                    u32(presentLatencyUs); u32(rxKbps)
                }
                is HostState -> { w.writeByte(state); str16(detail) }
                is SetBitrate -> u32(kbps)
                is Resize -> {
                    w.writeShort(width); w.writeShort(height); w.writeShort(densityDpi)
                    w.writeByte(rotation)
                }
                is SetRole -> w.writeByte(role)
                is SetQuality -> w.writeByte(quality)
                is AudioLatency -> w.writeInt(latencyUs.toInt())
                is AudioSync -> w.writeInt(delayUs.toInt())
                is ServiceState -> w.writeByte(enabled)
                is BatteryStatus -> {
                    w.writeByte(percent); w.writeByte(if (charging) 1 else 0); w.writeByte(thermal)
                    w.writeShort(tempDecidegrees)
                }
                is PairRequest -> { w.writeByte(mode); str16(deviceId); str16(deviceName); w.write(credential) }
                is PairResponse -> { w.writeByte(status); str16(message); w.write(token) }
                is VideoFrame -> {
                    w.writeByte(codec); w.writeByte(frameType); u32(frameIndex)
                    w.writeLong(captureUs); w.write(data)
                }
                is Audio -> { u32(seq); w.writeLong(ptsUs); w.writeByte(channels); w.write(data) }
                is Touch -> {
                    val n = minOf(points.size, 255)
                    w.writeByte(action); w.writeByte(actionId); w.writeByte(n)
                    for (p in points.take(n)) {
                        w.writeByte(p.id); w.writeFloat(p.x); w.writeFloat(p.y); w.writeFloat(p.pressure)
                    }
                }
                is Pen -> {
                    // Keep the newest samples if there are more than fit.
                    val kept = samples.takeLast(255)
                    w.writeByte(action); w.writeByte(tool); w.writeByte(buttons); w.writeByte(kept.size)
                    for (s in kept) {
                        u32(s.ageUs); w.writeFloat(s.x); w.writeFloat(s.y); w.writeFloat(s.pressure)
                        w.writeFloat(s.tiltX); w.writeFloat(s.tiltY)
                    }
                }
                is Mouse -> {
                    w.writeByte(action); w.writeByte(buttons); w.writeByte(if (relative) 1 else 0)
                    w.writeFloat(x); w.writeFloat(y)
                }
                is Scroll -> { w.writeByte(phase); w.writeFloat(x); w.writeFloat(y); w.writeFloat(dx); w.writeFloat(dy) }
                is Pinch -> {
                    w.writeByte(phase); w.writeFloat(cx); w.writeFloat(cy); w.writeFloat(scale)
                    w.writeFloat(rotation)
                }
                is Key -> {
                    w.writeByte(action); w.writeShort(keyCode); w.writeShort(scanCode); w.writeByte(meta)
                    str16(text)
                }
                is Text -> str16(text)
                is Clipboard -> { str16(mime); w.write(data) }
                is FileOffer -> { u32(id); w.writeLong(size); str16(name); str16(mime) }
                is FileChunk -> { u32(id); w.writeLong(offset); w.write(data) }
                is FileControl -> { u32(id); w.writeByte(op) }
                is Unknown -> w.write(payload)
            }
            w.flush()
            return bytes.toByteArray()
        }

        /** The whole message as frames, fragmented at [maxFragment] bytes. */
        fun encode(maxFragment: Int = MAX_FRAGMENT): ByteArray =
            frames(channel, type, encodePayload(), maxFragment)

        companion object {
            /** Decodes one reassembled payload that arrived on [channel]. */
            fun decode(channel: Int, type: Int, payload: ByteArray): Message {
                val expected = channelOf(type) ?: return Unknown(channel, type, payload)
                if (expected != channel) {
                    throw ProtocolException("message type $type on channel $channel, expected $expected")
                }
                val b = ByteBuffer.wrap(payload).order(ByteOrder.BIG_ENDIAN)
                fun need(n: Int) {
                    if (b.remaining() < n) throw ProtocolException("message payload too short (type $type)")
                }
                fun u8(): Int { need(1); return b.get().toInt() and 0xFF }
                fun u16(): Int { need(2); return b.short.toInt() and 0xFFFF }
                fun u32(): Long { need(4); return b.int.toLong() and 0xFFFFFFFFL }
                fun u64(): Long { need(8); return b.long }
                fun f32(): Float { need(4); return b.float }
                fun str16(): String {
                    val n = u16()
                    need(n)
                    val s = String(payload, b.position(), n, Charsets.UTF_8)
                    b.position(b.position() + n)
                    return s
                }
                fun rest(): ByteArray = payload.copyOfRange(b.position(), payload.size).also {
                    b.position(payload.size)
                }
                return when (type) {
                    MSG_HELLO -> HelloMsg(
                        Hello(u16(), u16(), u32(), u16(), u8(), u32(), u8(), str16(), str16(), str16())
                    )
                    MSG_HELLO_ACK -> HelloAckMsg(
                        HelloAck(u8(), u16(), u16(), u16(), u8(), u32(), u8(), str16(), str16())
                    )
                    MSG_PING -> Ping(u32(), u64())
                    MSG_PONG -> Pong(u32(), u64(), u64(), u64())
                    MSG_HEARTBEAT -> Heartbeat
                    MSG_BYE -> Bye(u8(), str16())
                    MSG_STATS -> StatsMsg(Stats(u32(), u32(), u32(), u32(), u32()))
                    MSG_HOST_STATE -> HostState(u8(), str16())
                    MSG_KEYFRAME_REQUEST -> KeyframeRequest
                    MSG_SET_BITRATE -> SetBitrate(u32())
                    MSG_RESIZE -> Resize(u16(), u16(), u16(), u8())
                    MSG_SET_ROLE -> SetRole(u8())
                    MSG_SET_QUALITY -> SetQuality(u8())
                    MSG_AUDIO_LATENCY -> AudioLatency(u32())
                    MSG_AUDIO_SYNC -> AudioSync(u32())
                    MSG_SERVICE_STATE -> ServiceState(u8())
                    MSG_BATTERY_STATUS -> BatteryStatus(u8(), u8() != 0, u8(), u16().toShort().toInt())
                    MSG_PAIR_REQUEST -> PairRequest(u8(), str16(), str16(), rest())
                    MSG_PAIR_RESPONSE -> PairResponse(u8(), str16(), rest())
                    MSG_VIDEO_FRAME -> VideoFrame(u8(), u8(), u32(), u64(), rest())
                    MSG_AUDIO_FRAME, MSG_MIC_FRAME -> Audio(type, u32(), u64(), u8(), rest())
                    MSG_TOUCH -> {
                        val action = u8()
                        val actionId = u8()
                        Touch(action, actionId, List(u8()) { TouchPoint(u8(), f32(), f32(), f32()) })
                    }
                    MSG_PEN -> {
                        val action = u8()
                        val tool = u8()
                        val buttons = u8()
                        Pen(action, tool, buttons, List(u8()) { PenSample(u32(), f32(), f32(), f32(), f32(), f32()) })
                    }
                    MSG_MOUSE -> Mouse(u8(), u8(), u8() != 0, f32(), f32())
                    MSG_SCROLL -> Scroll(u8(), f32(), f32(), f32(), f32())
                    MSG_PINCH -> Pinch(u8(), f32(), f32(), f32(), f32())
                    MSG_KEY -> Key(u8(), u16(), u16(), u8(), str16())
                    MSG_TEXT -> Text(str16())
                    MSG_CLIPBOARD -> Clipboard(str16(), rest())
                    MSG_FILE_OFFER -> FileOffer(u32(), u64(), str16(), str16())
                    MSG_FILE_CHUNK -> FileChunk(u32(), u64(), rest())
                    MSG_FILE_CONTROL -> FileControl(u32(), u8())
                    else -> throw IllegalStateException("channelOf covers exactly these types")
                }
            }
        }
    }

    data class Header(val version: Int, val channel: Int, val type: Int, val flags: Int, val len: Long) {
        fun encode(): ByteArray = ByteBuffer.allocate(HEADER_SIZE).order(ByteOrder.BIG_ENDIAN)
            .put(MAGIC0.toByte()).put(MAGIC1.toByte()).put(version.toByte()).put(channel.toByte())
            .put(type.toByte()).put(flags.toByte()).putInt(len.toInt()).array()

        /** Validates a header in a running v2 session. */
        fun check() {
            if (version != VERSION) throw ProtocolException("frame version $version in a v$VERSION session")
            if (channel >= CHANNEL_COUNT) throw ProtocolException("unknown channel $channel")
            if (len > MAX_FRAGMENT) throw ProtocolException("frame of $len bytes exceeds $MAX_FRAGMENT")
            if (flags and FLAG_MORE.inv() != 0) throw ProtocolException("reserved frame flags $flags set")
        }

        companion object {
            /** Parses the first [HEADER_SIZE] bytes of [b]; checks only the magic. */
            fun parse(b: ByteArray): Header {
                if ((b[0].toInt() and 0xFF) != MAGIC0 || (b[1].toInt() and 0xFF) != MAGIC1) {
                    throw ProtocolException("bad frame magic")
                }
                val buf = ByteBuffer.wrap(b, 0, HEADER_SIZE).order(ByteOrder.BIG_ENDIAN)
                buf.position(2)
                return Header(
                    buf.get().toInt() and 0xFF, buf.get().toInt() and 0xFF, buf.get().toInt() and 0xFF,
                    buf.get().toInt() and 0xFF, buf.int.toLong() and 0xFFFFFFFFL
                )
            }
        }
    }

    /** Frames one message, splitting its payload into fragments of at most [maxFragment] bytes. */
    fun frames(channel: Int, type: Int, payload: ByteArray, maxFragment: Int = MAX_FRAGMENT): ByteArray {
        val frag = maxFragment.coerceIn(1, MAX_FRAGMENT)
        val count = maxOf(1, (payload.size + frag - 1) / frag)
        val out = ByteBuffer.allocate(payload.size + count * HEADER_SIZE)
        var off = 0
        do {
            val n = minOf(frag, payload.size - off)
            val more = off + n < payload.size
            out.put(Header(VERSION, channel, type, if (more) FLAG_MORE else 0, n.toLong()).encode())
            out.put(payload, off, n)
            off += n
        } while (off < payload.size)
        return out.array()
    }

    /** Joins fragments back into messages, one in-progress message per channel. */
    class Reassembler {
        private val types = IntArray(CHANNEL_COUNT) { -1 }
        private val partial = arrayOfNulls<ByteArrayOutputStream>(CHANNEL_COUNT)

        /** Adds one checked frame; returns the complete payload once its last fragment arrived. */
        fun push(h: Header, payload: ByteArray): ByteArray? {
            val more = (h.flags and FLAG_MORE) != 0
            val buf = partial[h.channel]
            if (buf == null && !more) return payload
            val acc = buf ?: ByteArrayOutputStream().also {
                partial[h.channel] = it
                types[h.channel] = h.type
            }
            if (types[h.channel] != h.type) {
                throw ProtocolException("channel ${h.channel}: type ${h.type} inside a type ${types[h.channel]} message")
            }
            acc.write(payload)
            if (acc.size() > maxMessageLen(h.channel)) {
                throw ProtocolException("channel ${h.channel} message exceeds ${maxMessageLen(h.channel)} bytes")
            }
            if (more) return null
            partial[h.channel] = null
            types[h.channel] = -1
            return acc.toByteArray()
        }
    }

    /** How the host answered, or failed to answer, the Hello. */
    class HandshakeException(message: String) : IOException(message)

    /**
     * Reads messages from the host. Blocking; used on one thread.
     *
     * [awaitHelloAck] scans past leftovers from an earlier session (whole
     * frames, or loose bytes) to the HelloAck; after that, [next] is strict:
     * the link is reliable, so anything that does not parse is a
     * [ProtocolException].
     */
    class FrameReader(private val input: InputStream) {
        private var buf = ByteArray(1024)
        private var len = 0
        private val reassembler = Reassembler()

        /** Bytes skipped while looking for the HelloAck, for diagnostics. */
        var skippedBytes = 0L
            private set

        private fun fill(n: Int) {
            if (buf.size < n) buf = buf.copyOf(maxOf(n, buf.size * 2))
            while (len < n) {
                val r = input.read(buf, len, n - len)
                if (r < 0) throw EOFException("stream closed")
                len += r
            }
        }

        private fun drop(n: Int) {
            System.arraycopy(buf, n, buf, 0, len - n)
            len -= n
        }

        private fun u8(i: Int) = buf[i].toInt() and 0xFF

        /**
         * The host's HelloAck, skipping anything before it. Throws
         * [HandshakeException] when the host speaks another protocol version.
         */
        fun awaitHelloAck(): HelloAck {
            while (true) {
                fill(2)
                if (u8(0) != MAGIC0 || u8(1) != MAGIC1) {
                    drop(1); skippedBytes++; continue
                }
                fill(HEADER_SIZE)
                val h = Header.parse(buf)
                val ok = try { h.check(); true } catch (e: ProtocolException) { false }
                if (h.version == VERSION && ok) {
                    val end = HEADER_SIZE + h.len.toInt()
                    fill(end)
                    if (h.channel == CH_CONTROL && h.type == MSG_HELLO_ACK && h.flags == 0) {
                        val msg = runCatching {
                            Message.decode(h.channel, h.type, buf.copyOfRange(HEADER_SIZE, end))
                        }.getOrNull()
                        if (msg is Message.HelloAckMsg) {
                            drop(end)
                            return msg.ack
                        }
                    }
                    drop(end); skippedBytes += end; continue
                }
                if (h.version != VERSION && h.version < 'A'.code && h.channel == CH_CONTROL &&
                    h.type == MSG_HELLO_ACK
                ) {
                    throw HandshakeException(
                        "The host speaks DisplaySwarm protocol v${h.version}; this app speaks v$VERSION. " +
                            "Install matching versions."
                    )
                }
                drop(1); skippedBytes++
            }
        }

        /** The next complete message. [EOFException] at end of stream, [ProtocolException] on garbage. */
        fun next(): Message {
            while (true) {
                fill(HEADER_SIZE)
                val h = Header.parse(buf)
                h.check()
                val end = HEADER_SIZE + h.len.toInt()
                fill(end)
                val payload = buf.copyOfRange(HEADER_SIZE, end)
                drop(end)
                val full = reassembler.push(h, payload) ?: continue
                return Message.decode(h.channel, h.type, full)
            }
        }
    }

    /** UTF-8 encodes [text], cutting on a character boundary to at most [max] bytes. */
    fun truncateUtf8(text: String, max: Int): ByteArray {
        val bytes = text.toByteArray(Charsets.UTF_8)
        if (bytes.size <= max) return bytes
        var end = max
        while (end > 0 && (bytes[end].toInt() and 0xC0) == 0x80) end--
        return bytes.copyOf(end)
    }
}
