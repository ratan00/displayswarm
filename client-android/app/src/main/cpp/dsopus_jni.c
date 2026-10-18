// JNI bridge to libopus for OpusNative.kt. Handles are the encoder/decoder
// pointers as jlong; every call is synchronous and allocation free.
#include <jni.h>
#include <stdlib.h>
#include <opus.h>

#define FN(name) Java_com_displayswarm_client_OpusNative_##name

JNIEXPORT jlong JNICALL FN(decoderCreate)(JNIEnv *env, jclass cls, jint channels) {
    int err = 0;
    OpusDecoder *d = opus_decoder_create(48000, channels, &err);
    return err == OPUS_OK ? (jlong)(intptr_t)d : 0;
}

JNIEXPORT void JNICALL FN(decoderDestroy)(JNIEnv *env, jclass cls, jlong h) {
    if (h) opus_decoder_destroy((OpusDecoder *)(intptr_t)h);
}

// Decodes `len` bytes of `data` into `pcm` (interleaved). With `data` null,
// conceals one lost frame of `maxFrames` samples. Returns samples per channel,
// or a negative libopus error.
JNIEXPORT jint JNICALL FN(decode)(JNIEnv *env, jclass cls, jlong h, jbyteArray data, jint len,
                                  jshortArray pcm, jint maxFrames) {
    if (!h) return OPUS_INVALID_STATE;
    jbyte *in = data ? (*env)->GetPrimitiveArrayCritical(env, data, NULL) : NULL;
    jshort *out = (*env)->GetPrimitiveArrayCritical(env, pcm, NULL);
    int n = opus_decode((OpusDecoder *)(intptr_t)h, (const unsigned char *)in, in ? len : 0, out, maxFrames, 0);
    (*env)->ReleasePrimitiveArrayCritical(env, pcm, out, 0);
    if (in) (*env)->ReleasePrimitiveArrayCritical(env, data, in, JNI_ABORT);
    return n;
}

JNIEXPORT jlong JNICALL FN(encoderCreate)(JNIEnv *env, jclass cls, jint channels, jint bitrate, jboolean voice) {
    int err = 0;
    OpusEncoder *e = opus_encoder_create(48000, channels, voice ? OPUS_APPLICATION_VOIP : OPUS_APPLICATION_AUDIO, &err);
    if (err != OPUS_OK) return 0;
    opus_encoder_ctl(e, OPUS_SET_BITRATE(bitrate));
    return (jlong)(intptr_t)e;
}

JNIEXPORT void JNICALL FN(encoderDestroy)(JNIEnv *env, jclass cls, jlong h) {
    if (h) opus_encoder_destroy((OpusEncoder *)(intptr_t)h);
}

// Encodes `frames` samples per channel from `pcm` into `out`. Returns the
// packet length, or a negative libopus error.
JNIEXPORT jint JNICALL FN(encode)(JNIEnv *env, jclass cls, jlong h, jshortArray pcm, jint frames, jbyteArray out) {
    if (!h) return OPUS_INVALID_STATE;
    jsize cap = (*env)->GetArrayLength(env, out);
    jshort *in = (*env)->GetPrimitiveArrayCritical(env, pcm, NULL);
    jbyte *o = (*env)->GetPrimitiveArrayCritical(env, out, NULL);
    int n = opus_encode((OpusEncoder *)(intptr_t)h, in, frames, (unsigned char *)o, cap);
    (*env)->ReleasePrimitiveArrayCritical(env, out, o, 0);
    (*env)->ReleasePrimitiveArrayCritical(env, pcm, in, JNI_ABORT);
    return n;
}
