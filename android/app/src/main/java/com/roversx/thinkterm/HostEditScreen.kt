package com.roversx.thinkterm

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.util.Log
import android.view.inputmethod.EditorInfo
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.ContentCopy
import androidx.compose.material.icons.filled.ContentPaste
import androidx.compose.material.icons.filled.ExpandLess
import androidx.compose.material.icons.filled.ExpandMore
import androidx.compose.material.icons.filled.Folder
import androidx.compose.material.icons.filled.GppBad
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.VpnKey
import androidx.compose.material.icons.filled.Warning
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.FilterChip
import androidx.compose.material3.Icon
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.ExperimentalComposeUiApi
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.luminance
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.InterceptPlatformTextInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.PlatformTextInputMethodRequest
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.roversx.thinkterm.core.generateSshKey
import com.roversx.thinkterm.core.sshPublicLine
import kotlinx.coroutines.delay

/// What a host's own page has beyond the editor: the way into its
/// terminal, and the ways to copy or remove the host.
class HostPageActions(
    val open: (Host) -> Unit,
    val duplicate: () -> Unit,
    val delete: () -> Unit,
)

/// Add or edit one host. The private key never travels back out of the
/// encrypted preferences into a field: the editor only reports whether one
/// is stored, and hands a new secret to `onSave` when the user supplies
/// one. Follows ios/Sources/HostEditView.swift.
///
/// As a sheet it adds or edits and goes. As a host's own page (`page`),
/// pushed from the list, it shows the host and its session first, with the
/// way into its terminal; Save keeps an edit and stays, and `onCancel` is
/// the way back.
@Composable
fun HostEditScreen(
    store: HostStore,
    host: Host,
    onSave: (Host, String, String?) -> Unit,
    onCancel: () -> Unit,
    page: HostPageActions? = null,
) {
    val context = LocalContext.current
    var draft by remember { mutableStateOf(host) }
    var forgottenHostKey by remember { mutableStateOf<String?>(null) }
    var portText by remember { mutableStateOf(host.port.toString()) }
    var secret by remember { mutableStateOf("") }
    var passphrase by remember { mutableStateOf("") }
    var showKeyText by remember { mutableStateOf(false) }
    var showAdvanced by remember { mutableStateOf(false) }
    var importError by remember { mutableStateOf<String?>(null) }
    var copied by remember { mutableStateOf(false) }
    var hasStoredSecret by remember { mutableStateOf(store.secret(host.id).isNotEmpty()) }
    val isExisting = remember { store.hosts.any { it.id == host.id } }
    // The page's host as last kept, and the secrets as they were then:
    // what Save compares the fields with.
    var kept by remember { mutableStateOf(host) }
    var keptSecret by remember { mutableStateOf("") }
    var keptPassphrase by remember { mutableStateOf("") }
    var confirmDelete by remember { mutableStateOf(false) }

    val keyComment = "${draft.user}@thinkterm-android"

    /// Only an unencrypted ed25519 key yields a public line; for anything
    /// else the user pastes the public half on the host themselves.
    fun loadKey(text: String) {
        secret = text
        draft = draft.copy(
            publicKey = try {
                sshPublicLine(text, passphrase.ifEmpty { null }, keyComment)
            } catch (e: Throwable) {
                Log.w("thinkterm", "public line failed: $e")
                null
            }
        )
        copied = false
    }

    val importer = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri != null) {
            val text = try {
                context.contentResolver.openInputStream(uri)?.bufferedReader()?.use { it.readText() }
            } catch (e: Throwable) {
                Log.w("thinkterm", "key import failed: $e")
                null
            }
            if (text.isNullOrBlank()) importError = tr("key.notext") else loadKey(text)
        }
    }

    LaunchedEffect(copied) {
        if (copied) {
            delay(1600)
            copied = false
        }
    }

    val portOk = portText.isEmpty() || portText.toIntOrNull()?.let { it in 1..65535 } == true
    val canSave = draft.hostname.isNotBlank() && draft.user.isNotBlank() && portOk &&
        (isExisting || hasStoredSecret || secret.isNotEmpty())
    val changed = !sameEdits(draft, kept) || secret != keptSecret || passphrase != keptPassphrase

    /// The edit onto the host as the store has it now: the key a
    /// connection pinned, when it was last reached, are the store's.
    fun edited(): Host {
        val now = store.hosts.firstOrNull { it.id == draft.id } ?: return draft
        return now.copy(
            name = draft.name,
            hostname = draft.hostname,
            port = draft.port,
            user = draft.user,
            auth = draft.auth,
            remoteCommand = draft.remoteCommand,
            knownHost = now.knownHost?.takeUnless { it == forgottenHostKey },
            group = draft.group,
            publicKey = draft.publicKey,
            icon = draft.icon,
            autoConnect = draft.autoConnect,
        )
    }

    /// Save on the page: the edit goes onto the host as the store has it
    /// now, and the page goes on with the result. A secret is handed on
    /// only when it is new since the last keep; a new key, with the
    /// passphrase in the field, whether that changed or not.
    fun keep() {
        val edited = edited()
        val newSecret = if (secret == keptSecret) "" else secret
        val newPassphrase = when {
            newSecret.isNotEmpty() -> passphrase.ifEmpty { null }
            passphrase == keptPassphrase || passphrase.isEmpty() -> null
            else -> passphrase
        }
        onSave(edited, newSecret, newPassphrase)
        val saved = store.hosts.firstOrNull { it.id == draft.id } ?: edited
        draft = saved
        kept = saved
        keptSecret = secret
        keptPassphrase = passphrase
        if (secret.isNotEmpty()) hasStoredSecret = true
    }

    val colors = GroupedColors(MaterialTheme.colorScheme.surface.luminance() < 0.5f)
    GroupedPage(
        when {
            page != null -> ""
            draft.hostname.isBlank() -> tr("newhost")
            else -> draft.display
        },
        onBack = if (page != null) onCancel else null,
        leading = if (page != null) null else ({ GlassTextButton(tr("cancel"), onClick = onCancel) }),
        trailing = {
            if (page != null) {
                GlassTextButton(tr("save"), bold = true, enabled = changed && canSave) { keep() }
            } else {
                GlassTextButton(tr("save"), bold = true, enabled = canSave) {
                    onSave(edited(), secret, passphrase.ifEmpty { null })
                }
            }
        },
    ) {
        if (page != null) {
            // The host as kept, its session and the way in.
            HostHero(kept, page)
        }
        Group(tr("f.icon")) {
            GroupRow {
                HostIconRow(draft.icon) { draft = draft.copy(icon = it) }
            }
        }

        Group(tr("f.host")) {
            FieldRow(tr("f.name"), draft.name, { draft = draft.copy(name = it) }, placeholder = tr("optional"))
            FieldRow(
                tr("f.hostname"),
                draft.hostname,
                { draft = draft.copy(hostname = it.trim()) },
                placeholder = "example.com or 192.0.2.10",
                keyboard = KeyboardType.Uri,
            )
            FieldRow(tr("f.port"), portText, { typed ->
                portText = typed.filter { it.isDigit() }.take(5)
                // Out of range is kept in the field (and blocks Save)
                // rather than silently becoming another port.
                draft = draft.copy(port = portText.toIntOrNull()?.takeIf { it in 1..65535 } ?: 22)
            }, placeholder = "22", keyboard = KeyboardType.Number)
            FieldRow(tr("f.user"), draft.user, { draft = draft.copy(user = it.trim()) }, placeholder = "root")
            SwitchRow(tr("host.autoconnect"), draft.autoConnect) { draft = draft.copy(autoConnect = it) }
        }

        Group(tr("f.group"), footer = tr("f.groupfoot")) {
            FieldRow(tr("f.group"), draft.group, { draft = draft.copy(group = it) }, placeholder = tr("none"))
            val groups = store.groups
            if (groups.isNotEmpty()) {
                GroupRow {
                    Row(
                        Modifier.weight(1f).horizontalScroll(rememberScrollState()),
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        for (group in groups) {
                            val on = draft.group == group
                            Text(
                                group,
                                color = if (on) Color.White else colors.primary,
                                fontSize = 14.sp,
                                modifier = Modifier
                                    .clip(CircleShape)
                                    .background(if (on) LeadBlue else colors.fill)
                                    .clickable { draft = draft.copy(group = if (on) "" else group) }
                                    .padding(horizontal = 12.dp, vertical = 6.dp),
                            )
                        }
                    }
                }
            }
        }

        Group(tr("f.auth")) {
            GroupRow {
                val kinds = listOf("key" to tr("f.key"), "password" to tr("f.password"))
                IosSegments(kinds.map { it.second }, kinds.indexOfFirst { it.first == draft.auth }.coerceAtLeast(0), Modifier.weight(1f)) {
                    draft = draft.copy(auth = kinds[it].first)
                }
            }
            if (draft.auth == "key") {
                KeyStatusRow(hasNewSecret = secret.isNotEmpty(), ed25519 = draft.publicKey != null, stored = hasStoredSecret)
                ActionRow("clipboard", tr("key.paste")) {
                    val pasted = clipboardText(context)
                    if (!pasted.isNullOrBlank()) loadKey(pasted)
                }
                ActionRow("folder", tr("key.import")) { importer.launch(arrayOf("*/*")) }
                ActionRow("key-round", tr("key.generate")) {
                    val pair = try {
                        generateSshKey(keyComment)
                    } catch (e: Throwable) {
                        Log.w("thinkterm", "key generation failed: $e")
                        null
                    }
                    if (pair != null) {
                        secret = pair.privatePem
                        draft = draft.copy(publicKey = pair.publicLine)
                        copied = false
                    }
                }
                draft.publicKey?.takeIf { it.isNotEmpty() }?.let { publicKey ->
                    PublicKeyBlock(publicKey, copied) {
                        val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager
                        cm?.setPrimaryClip(ClipData.newPlainText("thinkterm", publicKey))
                        copied = true
                    }
                }
                Disclosure(tr("key.pastetext"), showKeyText) { showKeyText = !showKeyText }
                if (showKeyText) {
                    FieldRow("", secret, { loadKey(it) }, keyboard = KeyboardType.Ascii, monospace = true, singleLine = false, minLines = 6)
                }
                IncognitoKeyboard {
                    FieldRow(tr("key.passphrase"), passphrase, { passphrase = it }, keyboard = KeyboardType.Password, secret = true)
                }
            } else {
                IncognitoKeyboard {
                    FieldRow(tr("f.password"), secret, { secret = it }, keyboard = KeyboardType.Password, secret = true)
                }
            }
        }

        Group(footer = if (showAdvanced) tr("f.remotecmd.foot") else null) {
            Disclosure(tr("f.advanced"), showAdvanced) { showAdvanced = !showAdvanced }
            if (showAdvanced) {
                FieldRow(tr("f.remotecmd"), draft.remoteCommand, { draft = draft.copy(remoteCommand = it) }, monospace = true)
                draft.knownHost?.let { known ->
                    GroupRow {
                        Column(Modifier.weight(1f).padding(vertical = 4.dp)) {
                            Text(tr("f.hostkey"), color = colors.primary, fontSize = 16.sp)
                            Text(
                                known,
                                fontFamily = FontFamily.Monospace,
                                fontSize = 11.sp,
                                color = colors.secondary,
                                maxLines = 3,
                                overflow = TextOverflow.Ellipsis,
                            )
                        }
                    }
                    if (page == null) {
                        GroupRow(onClick = {
                            forgottenHostKey = known
                            draft = draft.copy(knownHost = null)
                        }) {
                            DesktopIcon("shield-off", size = 18.dp, tint = colors.destructive)
                            RowTitle(tr("forgetkey"), color = colors.destructive)
                        }
                    }
                }
            }
        }

        if (page != null) {
            // Copying the host, forgetting its key, deleting it: at the end.
            Group {
                GroupRow(onClick = page.duplicate) { RowTitle(tr("host.duplicate"), color = LeadBlue) }
                // The key as the store has it now: a connection made from
                // this page may have pinned one since it opened.
                if (store.hosts.firstOrNull { it.id == draft.id }?.knownHost != null) {
                    GroupRow(onClick = { forgetHostKey(store, draft.id) }) { RowTitle(tr("forgetkey"), color = LeadBlue) }
                }
                GroupRow(onClick = { confirmDelete = true }) { RowTitle(tr("host.delete"), color = colors.destructive) }
            }
        }
    }

    if (confirmDelete && page != null) {
        AlertDialog(
            onDismissRequest = { confirmDelete = false },
            title = { Text(tr("host.delete.title")) },
            text = { Text(tr("host.delete.body")) },
            confirmButton = {
                TextButton(onClick = { confirmDelete = false; page.delete() }) {
                    Text(tr("host.delete.confirm", kept.display), color = colors.destructive)
                }
            },
            dismissButton = { TextButton(onClick = { confirmDelete = false }) { Text(tr("cancel")) } },
        )
    }

    importError?.let { message ->
        AlertDialog(
            onDismissRequest = { importError = null },
            title = { Text(tr("key.importfailed")) },
            text = { Text(message) },
            confirmButton = { TextButton(onClick = { importError = null }) { Text(tr("ok")) } },
        )
    }
}

/// Nothing here is prose: no capitals of its own, no autocorrect.
/// A secret is typed with the keyboard asked not to learn from it
/// (`IME_FLAG_NO_PERSONALIZED_LEARNING`; Gboard shows its incognito
/// look). Compose has no option for the flag, so it is set on the
/// editor info as the field's input session starts.
@OptIn(ExperimentalComposeUiApi::class)
@Composable
private fun IncognitoKeyboard(content: @Composable () -> Unit) {
    InterceptPlatformTextInput(
        interceptor = { request, next ->
            next.startInputMethod(
                PlatformTextInputMethodRequest { attrs ->
                    request.createInputConnection(attrs).also {
                        attrs.imeOptions = attrs.imeOptions or EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING
                    }
                }
            )
        },
        content = content,
    )
}

private fun plainKeyboard(keyboard: KeyboardType) = KeyboardOptions(
    keyboardType = keyboard,
    capitalization = KeyboardCapitalization.None,
    autoCorrectEnabled = false,
)

@Composable
private fun KeyStatusRow(hasNewSecret: Boolean, ed25519: Boolean, stored: Boolean) {
    val green = Color(0xFF30D158)
    val (icon, tint, text) = when {
        hasNewSecret && ed25519 -> Triple("circle-check", green, tr("key.loaded.ed25519"))
        hasNewSecret -> Triple("circle-check", green, tr("key.loaded"))
        stored -> Triple("lock", green, tr("key.stored"))
        else -> Triple("triangle-alert", Color(0xFFFF9F0A), tr("key.none"))
    }
    GroupRow {
        DesktopIcon(icon, size = 18.dp, tint = tint)
        RowTitle(text)
    }
}

/// A row that does something, as a Form's button: the accent's words.
@Composable
private fun ActionRow(icon: String, label: String, onClick: () -> Unit) {
    GroupRow(onClick = onClick) {
        DesktopIcon(icon, size = 18.dp, tint = LeadBlue)
        RowTitle(label, color = LeadBlue)
    }
}

@Composable
private fun PublicKeyBlock(publicKey: String, copied: Boolean, onCopy: () -> Unit) {
    val colors = groupedColors()
    Column(
        Modifier.fillMaxWidth().background(colors.card).padding(horizontal = 16.dp, vertical = 12.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text(
            publicKey,
            fontFamily = FontFamily.Monospace,
            fontSize = 11.sp,
            lineHeight = 15.sp,
            color = colors.secondary,
            maxLines = 4,
            overflow = TextOverflow.Ellipsis,
        )
        Row(
            Modifier.clip(CircleShape).background(colors.fill).clickable(onClick = onCopy).padding(horizontal = 14.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            DesktopIcon(if (copied) "check" else "copy", size = 16.dp, tint = LeadBlue)
            Spacer(Modifier.width(8.dp))
            Text(if (copied) tr("copied") else tr("key.copypub"), color = LeadBlue, fontSize = 15.sp)
        }
        Text(tr("key.authorized"), fontSize = 13.sp, color = colors.secondary)
    }
}

@Composable
private fun Disclosure(label: String, expanded: Boolean, onToggle: () -> Unit) {
    GroupRow(onClick = onToggle) {
        RowTitle(label)
        DesktopIcon(
            "chevron-right",
            size = 16.dp,
            tint = groupedColors().tertiary,
            modifier = Modifier.rotate(if (expanded) 90f else 0f),
        )
    }
}

private fun clipboardText(context: Context): String? {
    val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager ?: return null
    val clip = cm.primaryClip ?: return null
    if (clip.itemCount == 0) return null
    return clip.getItemAt(0).coerceToText(context)?.toString()
}

/// What the page edits of a host alike. The rest -- the key pinned on a
/// connection, when it was last reached -- the store keeps up to date
/// behind the page, and is not the page's to write back.
private fun sameEdits(a: Host, b: Host): Boolean =
    a.name == b.name && a.hostname == b.hostname && a.port == b.port && a.user == b.user &&
        a.auth == b.auth && a.remoteCommand == b.remoteCommand && a.group == b.group &&
        a.publicKey == b.publicKey && a.icon == b.icon && a.autoConnect == b.autoConnect

/// The desktop's host pictures, the one the host wears picked out; the
/// first is the one a host with none chosen wears.
@Composable
private fun RowScope.HostIconRow(icon: String, pick: (String) -> Unit) {
    val colors = groupedColors()
    val chosen = icon.ifEmpty { Host.ICONS[0] }
    Row(Modifier.weight(1f).padding(vertical = 4.dp), horizontalArrangement = Arrangement.SpaceBetween) {
        for (name in Host.ICONS) {
            val on = name == chosen
            Box(
                Modifier
                    .size(36.dp)
                    .clip(RoundedCornerShape(10.dp))
                    .background(if (on) LeadBlue else colors.fill)
                    .clickable { pick(if (name == Host.ICONS[0]) "" else name) },
                contentAlignment = Alignment.Center,
            ) {
                DesktopIcon(Host.iconFile(name), size = 19.dp, tint = if (on) Color.White else colors.primary)
            }
        }
    }
}

/// The top of a host's page: its picture, name and address as kept, where
/// its session stands, and the way into its terminal.
@Composable
private fun HostHero(host: Host, page: HostPageActions) {
    val colors = groupedColors()
    val session = Sessions.existing(host.id)
    val state = session?.let { SessionState.of(it) } ?: SessionState.NONE
    Column(
        Modifier.fillMaxWidth().padding(start = 16.dp, end = 16.dp, top = 0.dp, bottom = 10.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        HostTile(76.dp) { DesktopIcon(host.iconFile, size = 36.dp, tint = colors.primary) }
        Spacer(Modifier.height(14.dp))
        Text(host.title, color = colors.primary, fontSize = 24.sp, fontWeight = FontWeight.Bold, maxLines = 1, overflow = TextOverflow.Ellipsis)
        Spacer(Modifier.height(3.dp))
        Text(host.address, color = colors.secondary, fontSize = 14.sp, fontFamily = FontFamily.Monospace, maxLines = 1, overflow = TextOverflow.Ellipsis)
        Spacer(Modifier.height(10.dp))
        val (said, tint) = when (state) {
            SessionState.CONNECTED -> tr("host.connected") to Color(0xFF34C759)
            SessionState.CONNECTING -> tr("connecting") to Color(0xFFFF9F0A)
            SessionState.LOST -> tr("conn.disconnected") to colors.destructive
            SessionState.NONE -> tr("host.notconnected") to colors.secondary
        }
        Row(
            Modifier.clip(CircleShape).background(tint.copy(alpha = 0.14f)).padding(horizontal = 11.dp, vertical = 5.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(Modifier.size(8.dp).background(tint, CircleShape))
            Spacer(Modifier.width(6.dp))
            Text(said, color = tint, fontSize = 13.sp, fontWeight = FontWeight.Medium)
        }
        Spacer(Modifier.height(18.dp))
        // Into the terminal -- opened, or connected first -- and, while
        // there is a session, the way to end it.
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(10.dp)) {
            val up = state == SessionState.CONNECTED
            // Clear of the capsule's round ends: a word too long for its half
            // ends in an ellipsis rather than run under them.
            Row(
                Modifier.weight(1f).height(50.dp).clip(CircleShape).background(LeadBlue).clickable { page.open(host) }
                    .padding(horizontal = 16.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.Center,
            ) {
                DesktopIcon(if (up) "square-terminal" else "link-2", size = 18.dp, tint = Color.White)
                Spacer(Modifier.width(7.dp))
                Text(
                    tr(if (up) "host.open" else "connect"), color = Color.White, fontSize = 16.sp, fontWeight = FontWeight.SemiBold,
                    maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f, fill = false),
                )
            }
            if (session != null && (up || state == SessionState.CONNECTING)) {
                Row(
                    Modifier
                        .weight(1f)
                        .height(50.dp)
                        .glass(LocalGroundGlass.current, colors.dark, CircleShape)
                        .clickable { session.disconnect() }
                        .padding(horizontal = 16.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.Center,
                ) {
                    DesktopIcon("power", size = 17.dp, tint = colors.primary)
                    Spacer(Modifier.width(7.dp))
                    Text(
                        tr("disconnect"), color = colors.primary, fontSize = 16.sp, fontWeight = FontWeight.SemiBold,
                        maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f, fill = false),
                    )
                }
            }
        }
    }
}
