package com.roversx.thinkterm

import android.content.Context
import android.content.SharedPreferences
import android.util.Log
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey
import org.json.JSONArray
import org.json.JSONObject
import java.util.UUID

/// A host the app connects to. The secret (key text or password) is not
/// here: it lives in the encrypted preferences under the host's id.
data class Host(
    val id: String = UUID.randomUUID().toString(),
    val name: String = "",
    val hostname: String = "",
    val port: Int = 22,
    val user: String = "",
    /// "key" (the secret is the private key's text) or "password".
    val auth: String = "key",
    /// The fingerprint seen on the first connection.
    val knownHost: String? = null,
    /// Empty means the default proxy command on the host.
    val remoteCommand: String = "",
) {
    val display: String get() = if (name.isNotEmpty()) name else "$user@$hostname:$port"
    val address: String get() = "$user@$hostname" + if (port == 22) "" else ":$port"

    /// The dev-only entry that reaches the Mac's probe sshd. 10.0.2.2 is
    /// the emulator's route to the host's loopback.
    companion object {
        const val PROBE_ID = "probe"

        val probe = Host(
            id = PROBE_ID,
            name = "This Mac (probe)",
            hostname = "10.0.2.2",
            port = 2299,
            user = "",
            auth = "key",
            remoteCommand = "/tmp/ttp-ssh/thinkterm-remote cli --prefer-mux proxy",
        )
    }
}

/// The hosts, in the app's preferences (small, edited rarely), with the
/// secrets one file over in EncryptedSharedPreferences.
class HostStore(context: Context) {
    private val app = context.applicationContext
    private val prefs: SharedPreferences = app.getSharedPreferences("thinkterm", Context.MODE_PRIVATE)
    private val secrets: SharedPreferences = openSecrets(app)

    var hosts by mutableStateOf<List<Host>>(emptyList())
        private set

    init {
        hosts = decode(prefs.getString(KEY, "") ?: "")
    }

    /// This install's name to the servers, made once: the tab this phone
    /// holds stays its own across launches.
    val deviceId: String
        get() = prefs.getString("client.id", null) ?: UUID.randomUUID().toString().also {
            prefs.edit().putString("client.id", it).apply()
        }

    fun upsert(host: Host) {
        val at = hosts.indexOfFirst { it.id == host.id }
        hosts = if (at >= 0) hosts.toMutableList().also { it[at] = host } else hosts + host
        save()
    }

    fun remove(host: Host) {
        hosts = hosts.filter { it.id != host.id }
        secrets.edit().remove(host.id).remove(host.id + ".passphrase").apply()
        save()
    }

    fun rememberHostKey(id: String, fingerprint: String) {
        val at = hosts.indexOfFirst { it.id == id }
        if (at < 0 || hosts[at].knownHost == fingerprint) return
        hosts = hosts.toMutableList().also { it[at] = it[at].copy(knownHost = fingerprint) }
        save()
    }

    fun secret(id: String): String = secrets.getString(id, "") ?: ""
    fun passphrase(id: String): String? = secrets.getString(id + ".passphrase", null)?.ifEmpty { null }

    fun putSecret(id: String, secret: String, passphrase: String?) {
        secrets.edit()
            .putString(id, secret)
            .putString(id + ".passphrase", passphrase ?: "")
            .apply()
    }

    private fun save() {
        val arr = JSONArray()
        for (h in hosts) {
            arr.put(
                JSONObject()
                    .put("id", h.id)
                    .put("name", h.name)
                    .put("hostname", h.hostname)
                    .put("port", h.port)
                    .put("user", h.user)
                    .put("auth", h.auth)
                    .put("knownHost", h.knownHost ?: JSONObject.NULL)
                    .put("remoteCommand", h.remoteCommand)
            )
        }
        prefs.edit().putString(KEY, arr.toString()).apply()
    }

    private fun decode(json: String): List<Host> {
        if (json.isEmpty()) return emptyList()
        return try {
            val arr = JSONArray(json)
            (0 until arr.length()).mapNotNull { i ->
                val o = arr.optJSONObject(i) ?: return@mapNotNull null
                Host(
                    id = o.optString("id", UUID.randomUUID().toString()),
                    name = o.optString("name"),
                    hostname = o.optString("hostname"),
                    port = o.optInt("port", 22),
                    user = o.optString("user"),
                    auth = o.optString("auth", "key"),
                    knownHost = o.optString("knownHost").ifEmpty { null },
                    remoteCommand = o.optString("remoteCommand"),
                )
            }
        } catch (e: Throwable) {
            Log.w("thinkterm", "hosts decode failed: $e")
            emptyList()
        }
    }

    private companion object {
        const val KEY = "hosts.v1"

        /// Keystore-backed preferences. If the device refuses (a broken
        /// keystore on an emulator), plain preferences keep the app usable.
        fun openSecrets(app: Context): SharedPreferences = try {
            val key = MasterKey.Builder(app).setKeyScheme(MasterKey.KeyScheme.AES256_GCM).build()
            EncryptedSharedPreferences.create(
                app,
                "thinkterm-secrets",
                key,
                EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
                EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
            )
        } catch (e: Throwable) {
            Log.w("thinkterm", "encrypted preferences unavailable: $e")
            app.getSharedPreferences("thinkterm-secrets-plain", Context.MODE_PRIVATE)
        }
    }
}
