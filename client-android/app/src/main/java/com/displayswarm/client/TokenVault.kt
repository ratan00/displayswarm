package com.displayswarm.client

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import android.util.Log
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Seals the per-host pairing tokens with an AES-GCM key that lives in the
 * Android Keystore and cannot be exported. A copy of the preferences file (a
 * backup, a rooted file read) then holds nothing that can pair as this phone.
 *
 * Sealed values are `k1:` + Base64(IV || ciphertext). A value without the
 * prefix is a token from before this existed; it is used as is and sealed the
 * next time the host list is saved.
 */
object TokenVault {
    private const val TAG = "TokenVault"
    private const val ALIAS = "displayswarm-tokens"
    private const val PREFIX = "k1:"

    private fun key(): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getKey(ALIAS, null) as? SecretKey)?.let { return it }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(
            KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build()
        )
        return gen.generateKey()
    }

    /** Seals `plain`; on a Keystore failure keeps it as is (pairing must not break). */
    fun seal(plain: String): String = try {
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.ENCRYPT_MODE, key())
        PREFIX + Base64.encodeToString(c.iv + c.doFinal(plain.toByteArray()), Base64.NO_WRAP)
    } catch (e: Exception) {
        Log.w(TAG, "Keystore unavailable; token stored unsealed: ${e.message}")
        plain
    }

    /** Opens a sealed value; a legacy plain one passes through; an unreadable one is empty. */
    fun open(stored: String): String {
        if (!stored.startsWith(PREFIX)) return stored
        return try {
            val raw = Base64.decode(stored.removePrefix(PREFIX), Base64.NO_WRAP)
            val c = Cipher.getInstance("AES/GCM/NoPadding")
            c.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, raw.copyOfRange(0, 12)))
            String(c.doFinal(raw.copyOfRange(12, raw.size)))
        } catch (e: Exception) {
            // The key is gone (restored elsewhere, Keystore reset): the host must be paired again.
            Log.w(TAG, "Cannot open a stored token: ${e.message}")
            ""
        }
    }
}
