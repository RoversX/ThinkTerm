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
    /// A label the list groups by; empty is no group.
    val group: String = "",
    /// The public half of a key the app generated or could read, so it
    /// can be shown and copied to the host's authorized_keys.
    val publicKey: String? = null,
    /// Unix milliseconds of the last connection that reached a pane.
    val lastConnected: Long? = null,
) {
    val display: String get() = if (name.isNotEmpty()) name else "$user@$hostname:$port"
    /// The headline of a row: `display` would repeat the address below it
    /// when the host has no name.
    val title: String get() = name.ifEmpty { hostname }
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

    /// A connection reached the terminal: the list shows when.
    fun touchConnected(id: String) {
        val at = hosts.indexOfFirst { it.id == id }
        if (at < 0) return
        hosts = hosts.toMutableList().also { it[at] = it[at].copy(lastConnected = System.currentTimeMillis()) }
        save()
    }

    fun forgetHostKey(id: String) {
        val at = hosts.indexOfFirst { it.id == id }
        if (at < 0 || hosts[at].knownHost == null) return
        hosts = hosts.toMutableList().also { it[at] = it[at].copy(knownHost = null) }
        save()
    }

    /// The groups in use, for the editor's suggestions.
    val groups: List<String> get() = hosts.map { it.group }.filter { it.isNotEmpty() }.distinct().sorted()

    /// A copy under a new id, with the secret copied across so the
    /// duplicate connects without being re-keyed.
    fun duplicate(host: Host, nameSuffix: String) {
        val copy = host.copy(
            id = UUID.randomUUID().toString(),
            name = host.name.ifEmpty { host.display } + nameSuffix,
            lastConnected = null,
        )
        putSecret(copy.id, secret(host.id), passphrase(host.id))
        upsert(copy)
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
                    .put("group", h.group)
                    .put("publicKey", h.publicKey ?: JSONObject.NULL)
                    .put("lastConnected", h.lastConnected ?: JSONObject.NULL)
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
                    group = o.optString("group"),
                    publicKey = o.optString("publicKey").ifEmpty { null },
                    lastConnected = if (o.isNull("lastConnected")) null else o.optLong("lastConnected"),
                )
            }
        } catch (e: Throwable) {
            Log.w("thinkterm", "hosts decode failed: $e")
            emptyList()
        }
    }

    private companion object {
        const val KEY = "hosts.v1"

        const val SECRETS = "thinkterm-secrets"

        /// Keystore-backed preferences. A file whose keyset the keystore
        /// can no longer open (a restore onto another device) is thrown
        /// away and made afresh: its secrets were lost with the key. If
        /// the keystore refuses altogether, the secrets live in memory
        /// for this run -- never in a file in the clear.
        fun openSecrets(app: Context): SharedPreferences {
            try {
                return openEncrypted(app)
            } catch (e: Throwable) {
                Log.w("thinkterm", "encrypted preferences unreadable, starting them over: $e")
            }
            try {
                app.deleteSharedPreferences(SECRETS)
                return openEncrypted(app)
            } catch (e: Throwable) {
                Log.w("thinkterm", "encrypted preferences unavailable: $e")
            }
            return MemoryPreferences()
        }

        private fun openEncrypted(app: Context): SharedPreferences {
            val key = MasterKey.Builder(app).setKeyScheme(MasterKey.KeyScheme.AES256_GCM).build()
            return EncryptedSharedPreferences.create(
                app,
                SECRETS,
                key,
                EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
                EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
            )
        }
    }
}

/// Preferences that last for the run only: the stand-in for the
/// encrypted store when the keystore is out of order.
private class MemoryPreferences : SharedPreferences {
    private val values = HashMap<String, Any?>()
    private val listeners = HashSet<SharedPreferences.OnSharedPreferenceChangeListener>()

    override fun getAll(): MutableMap<String, *> = HashMap(values)
    override fun getString(key: String?, defValue: String?): String? = values[key] as? String ?: defValue
    @Suppress("UNCHECKED_CAST")
    override fun getStringSet(key: String?, defValues: MutableSet<String>?): MutableSet<String>? =
        (values[key] as? Set<String>)?.toMutableSet() ?: defValues
    override fun getInt(key: String?, defValue: Int): Int = values[key] as? Int ?: defValue
    override fun getLong(key: String?, defValue: Long): Long = values[key] as? Long ?: defValue
    override fun getFloat(key: String?, defValue: Float): Float = values[key] as? Float ?: defValue
    override fun getBoolean(key: String?, defValue: Boolean): Boolean = values[key] as? Boolean ?: defValue
    override fun contains(key: String?): Boolean = values.containsKey(key)
    override fun edit(): SharedPreferences.Editor = Editor()
    override fun registerOnSharedPreferenceChangeListener(l: SharedPreferences.OnSharedPreferenceChangeListener) { listeners.add(l) }
    override fun unregisterOnSharedPreferenceChangeListener(l: SharedPreferences.OnSharedPreferenceChangeListener) { listeners.remove(l) }

    private inner class Editor : SharedPreferences.Editor {
        private val puts = HashMap<String, Any?>()
        private val removes = HashSet<String>()
        private var clear = false

        override fun putString(key: String?, value: String?) = also { if (key != null) puts[key] = value }
        override fun putStringSet(key: String?, values: MutableSet<String>?) = also { if (key != null) puts[key] = values?.toSet() }
        override fun putInt(key: String?, value: Int) = also { if (key != null) puts[key] = value }
        override fun putLong(key: String?, value: Long) = also { if (key != null) puts[key] = value }
        override fun putFloat(key: String?, value: Float) = also { if (key != null) puts[key] = value }
        override fun putBoolean(key: String?, value: Boolean) = also { if (key != null) puts[key] = value }
        override fun remove(key: String?) = also { if (key != null) removes.add(key) }
        override fun clear() = also { clear = true }
        override fun commit(): Boolean { apply(); return true }
        override fun apply() {
            if (clear) values.clear()
            for (k in removes) values.remove(k)
            for ((k, v) in puts) if (v == null) values.remove(k) else values[k] = v
            val changed = removes + puts.keys
            for (l in listeners.toList()) for (k in changed) l.onSharedPreferenceChanged(this@MemoryPreferences, k)
        }
    }
}
