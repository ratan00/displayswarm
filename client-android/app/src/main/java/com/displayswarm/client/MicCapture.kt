package com.displayswarm.client

import android.content.Context
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaCodec
import android.media.MediaFormat
import android.media.MediaRecorder
import android.media.audiofx.AcousticEchoCanceler
import android.media.audiofx.NoiseSuppressor
import android.os.Process
import android.util.Log

/**
 * Sends the phone's microphone to the host: `AudioRecord` (voice-communication
 * source, so the platform's echo cancellation and noise suppression apply) ->
 * libopus (or, without the native library, the MediaCodec Opus encoder, API 29)
 * -> `MicFrame`. Without an encoder, the feature bit is not offered; without the
 * RECORD_AUDIO permission or with the mic switched off in [AudioSettings],
 * nothing runs.
 */
class MicCapture(private val context: Context) : PhoneService {
    override val name = "mic-capture"

    // Always offered where the encoder exists; the recorder only runs while
    // the switch is on and the permission is granted.
    override val features: Long
        get() = if (OpusNative.available || AudioSettings.micSupported) Wire.FEATURE_MIC else 0L

    @Volatile
    private var running = false
    private var thread: Thread? = null

    override fun wants(msg: Wire.Message) = false

    override fun onMessage(msg: Wire.Message) {}

    override fun onSessionStart(ctx: PhoneServiceContext) {
        running = true
        thread = Thread({ captureWhileEnabled(ctx.send) }, "mic-capture").also { it.start() }
    }

    override fun onSessionEnd() {
        running = false
        thread?.join(1000)
        thread = null
    }

    private fun wanted() = running && AudioSettings.micEnabled(context) && AudioSettings.hasMicPermission(context)

    /** Records whenever the user has the mic switched on, for the whole session. */
    private fun captureWhileEnabled(send: (Wire.Message) -> Unit) {
        while (running) {
            if (wanted()) captureLoop(send) else try {
                Thread.sleep(200)
            } catch (e: InterruptedException) {
                return
            }
        }
    }

    @android.annotation.SuppressLint("MissingPermission") // checked in [wanted]
    private fun captureLoop(send: (Wire.Message) -> Unit) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)
        val frame = SAMPLE_RATE / 100 // 10 ms
        val minBuf = AudioRecord.getMinBufferSize(SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT)
        var rec: AudioRecord? = null
        var codec: MediaCodec? = null
        var native = 0L
        val effects = ArrayList<android.media.audiofx.AudioEffect>()
        try {
            rec = AudioRecord(
                MediaRecorder.AudioSource.VOICE_COMMUNICATION, SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO,
                AudioFormat.ENCODING_PCM_16BIT, maxOf(minBuf, frame * 2 * 4),
            )
            if (rec.state != AudioRecord.STATE_INITIALIZED) {
                Log.w(TAG, "microphone unavailable")
                return
            }
            // The voice-communication source usually has these already; make sure.
            if (AcousticEchoCanceler.isAvailable()) AcousticEchoCanceler.create(rec.audioSessionId)?.let { it.enabled = true; effects += it }
            if (NoiseSuppressor.isAvailable()) NoiseSuppressor.create(rec.audioSessionId)?.let { it.enabled = true; effects += it }

            if (OpusNative.available) native = OpusNative.encoderCreate(1, BITRATE, voice = true)
            if (native == 0L) {
                val fmt = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, SAMPLE_RATE, 1)
                fmt.setInteger(MediaFormat.KEY_BIT_RATE, BITRATE)
                codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS).also {
                    it.configure(fmt, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
                    it.start()
                }
            }
            rec.startRecording()

            val pcm = ShortArray(frame)
            val packet = ByteArray(1500)
            val info = MediaCodec.BufferInfo()
            var seq = 0L
            var pts = 0L
            while (wanted()) {
                var got = 0
                while (got < frame && running) {
                    val n = rec.read(pcm, got, frame - got)
                    if (n < 0) return
                    got += n
                }
                if (got < frame) break
                if (native != 0L) {
                    val n = OpusNative.encode(native, pcm, frame, packet)
                    if (n > 0) {
                        send(Wire.Message.Audio(Wire.MSG_MIC_FRAME, seq, PhoneClock.nowUs(), 1, packet.copyOf(n)))
                        seq = (seq + 1) and 0xFFFF_FFFFL
                    }
                    continue
                }
                val codec = codec!!
                val i = codec.dequeueInputBuffer(10_000)
                if (i >= 0) {
                    val buf = codec.getInputBuffer(i)!!
                    buf.clear()
                    buf.put(PcmUtil.toBytes(pcm))
                    codec.queueInputBuffer(i, 0, frame * 2, pts, 0)
                    pts += 10_000
                }
                while (true) {
                    val o = codec.dequeueOutputBuffer(info, 0)
                    if (o < 0) break
                    if (info.size > 0 && info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0) {
                        val out = codec.getOutputBuffer(o)!!
                        val bytes = ByteArray(info.size)
                        out.position(info.offset)
                        out.get(bytes)
                        send(Wire.Message.Audio(Wire.MSG_MIC_FRAME, seq, PhoneClock.nowUs(), 1, bytes))
                        seq = (seq + 1) and 0xFFFF_FFFFL
                    }
                    codec.releaseOutputBuffer(o, false)
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "microphone stopped", e)
        } finally {
            effects.forEach { try { it.release() } catch (_: Exception) {} }
            try { rec?.stop() } catch (_: Exception) {}
            rec?.release()
            try { codec?.stop() } catch (_: Exception) {}
            codec?.release()
            if (native != 0L) OpusNative.encoderDestroy(native)
        }
    }

    companion object {
        private const val TAG = "MicCapture"
        const val SAMPLE_RATE = 48_000
        const val BITRATE = 32_000
    }
}
