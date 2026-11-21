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
    /// The picture the host wears, by the desktop's name for it
    /// (`Host.ICONS`); empty is the server.
    val icon: String = "",
    val autoConnect: Boolean = true,
    /// The id of the saved host this one is reached through (`ssh -J`), if
    /// any. One hop: that host has none of its own.
    val jumpHost: String? = null,
) {
    fun sameSshConnection(other: Host): Boolean =
        id == other.id && hostname == other.hostname && port == other.port &&
            user == other.user && auth == other.auth && remoteCommand == other.remoteCommand &&
            knownHost == other.knownHost && jumpHost == other.jumpHost

    /// Reached the same way as a jump host: what a session through it signs
    /// in to it with, its pinned key aside. Its command is never run.
    fun sameJumpWay(other: Host): Boolean =
        id == other.id && hostname == other.hostname && port == other.port && user == other.user && auth == other.auth

    val display: String get() = if (name.isNotEmpty()) name else "$user@$hostname:$port"
    /// The headline of a row: `display` would repeat the address below it
    /// when the host has no name.
    val title: String get() = name.ifEmpty { hostname }
    val address: String get() = "$user@$hostname" + if (port == 22) "" else ":$port"
    val iconFile: String get() = iconFile(icon)

    /// The dev-only entry that reaches the Mac's probe sshd. 10.0.2.2 is
    /// the emulator's route to the host's loopback.
    companion object {
        const val PROBE_ID = "probe"

        /// The desktop's host icons (thinkterm-core's `HostIcon`), in the
        /// order its picker offers them; a host with none chosen wears the
        /// first.
        val ICONS = listOf("server", "cloud", "globe", "laptop", "mac-mini", "desktop", "chip", "container")

        /// The desktop's picture for an icon, as its hosts page draws it.
        fun iconFile(icon: String): String = when (icon) {
            "cloud" -> "cloud"
            "globe" -> "globe"
            "laptop" -> "laptop"
            "mac-mini" -> "hard-drive"
            "desktop" -> "monitor"
            "chip" -> "cpu"
            "container" -> "package"
            else -> "server"
        }

        fun probe(context: Context) = Host(
            id = PROBE_ID,
            name = "This Mac (probe)",
            hostname = "10.0.2.2",
            port = 2299,
            // Generated locally for debug builds; no account name lives in Git.
            user = runCatching {
                context.assets.open("probe_user").bufferedReader().use { it.readText().trim() }
            }.getOrDefault(""),
            auth = "key",
            remoteCommand = "/tmp/ttp-ssh/thinkterm-remote cli --prefer-mux proxy",
        )
    }
}

/// The hosts, in the app's preferences (small, edited rarely), with the
/// secrets one file over in EncryptedSharedPreferences.
class HostStore private constructor(context: Context) {
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
        // Another address is another machine as far as its key goes: the
        // pin stays only while the host is where it was pinned.
        val moved = at >= 0 && (hosts[at].hostname != host.hostname || hosts[at].port != host.port)
        val next = if (moved) host.copy(knownHost = null) else host
        hosts = if (at >= 0) hosts.toMutableList().also { it[at] = next } else hosts + next
        save()
    }

    /// What the host editor hands back, kept, and the host as stored. A
    /// secret typed in replaces the stored one, with the passphrase that
    /// goes with it. With none typed the stored secret stays, and so does
    /// its passphrase unless a new one was typed: the field starts empty,
    /// so an empty one (null) is no change.
    fun saveEdit(host: Host, secret: String, passphrase: String?): Host {
        if (secret.isNotEmpty() || secret(host.id).isEmpty()) {
            putSecret(host.id, secret, passphrase)
        } else if (passphrase != null) {
            putSecret(host.id, secret(host.id), passphrase)
        }
        upsert(host)
        return hosts.firstOrNull { it.id == host.id } ?: host
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

    /// A secret of the app's own (the tailnet's keys), kept beside the
    /// hosts' in the encrypted preferences.
    fun appSecret(key: String): String? = secrets.getString(key, null)?.takeIf { it.isNotEmpty() }

    fun putAppSecret(key: String, value: String?) {
        secrets.edit().apply { if (value.isNullOrEmpty()) remove(key) else putString(key, value) }.apply()
    }

    /// The groups in use, for the editor's suggestions.
    val groups: List<String> get() = hosts.map { it.group }.filter { it.isNotEmpty() }.distinct().sorted()

    /// The host `host` is reached through, if it has one still saved.
    fun jumpOf(host: Host): Host? = host.jumpHost?.let { id -> hosts.firstOrNull { it.id == id } }

    /// The hosts reached through the host `id`: while there are any, it is
    /// not deleted, nor given a jump host of its own.
    fun reachedThrough(id: String): List<Host> = hosts.filter { it.jumpHost == id }

    /// The hosts `host` may be reached through: any other saved one with no
    /// jump host of its own -- one hop, so no chain and no circle. None for
    /// a host others are reached through.
    fun jumpCandidates(host: Host): List<Host> =
        if (reachedThrough(host.id).isNotEmpty()) emptyList()
        else hosts.filter { it.id != host.id && it.id != Host.PROBE_ID && it.jumpHost == null }

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
                    .put("icon", h.icon)
                    .put("autoConnect", h.autoConnect)
                    .put("jumpHost", h.jumpHost ?: JSONObject.NULL)
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
                    knownHost = o.stringOrNull("knownHost"),
                    remoteCommand = o.optString("remoteCommand"),
                    group = o.optString("group"),
                    publicKey = o.stringOrNull("publicKey"),
                    lastConnected = if (o.isNull("lastConnected")) null else o.optLong("lastConnected"),
                    icon = o.optString("icon"),
                    autoConnect = o.optBoolean("autoConnect", true),
                    jumpHost = o.stringOrNull("jumpHost"),
                )
            }
        } catch (e: Throwable) {
            Log.w("thinkterm", "hosts decode failed: $e")
            emptyList()
        }
    }

    companion object {
        @Volatile private var instance: HostStore? = null

        fun get(context: Context): HostStore = instance ?: synchronized(this) {
            instance ?: HostStore(context.applicationContext).also { instance = it }
        }

        private const val KEY = "hosts.v1"

        /// A string field, or null for one absent, null or empty --
        /// `optString` would read a JSON null as the word "null".
        private fun JSONObject.stringOrNull(key: String): String? =
            if (!has(key) || isNull(key)) null else optString(key).ifEmpty { null }

        private const val SECRETS = "thinkterm-secrets"

        /// Keystore-backed preferences. A file whose keyset the keystore
        /// can no longer open (a restore onto another device) is thrown
        /// away and made afresh: its secrets were lost with the key. If
        /// the keystore refuses altogether, the secrets live in memory
        /// for this run -- never in a file in the clear.
        private fun openSecrets(app: Context): SharedPreferences {
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
