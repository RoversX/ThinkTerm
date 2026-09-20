package com.roversx.thinkterm

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.util.Log
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
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
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
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

/// Add or edit one host. The private key never travels back out of the
/// encrypted preferences into a field: the editor only reports whether one
/// is stored, and hands a new secret to `onSave` when the user supplies
/// one. Follows ios/Sources/HostEditView.swift.
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun HostEditScreen(
    store: HostStore,
    host: Host,
    onSave: (Host, String, String?) -> Unit,
    onCancel: () -> Unit,
) {
    val context = LocalContext.current
    var draft by remember { mutableStateOf(host) }
    var portText by remember { mutableStateOf(host.port.toString()) }
    var secret by remember { mutableStateOf("") }
    var passphrase by remember { mutableStateOf("") }
    var showKeyText by remember { mutableStateOf(false) }
    var showAdvanced by remember { mutableStateOf(false) }
    var importError by remember { mutableStateOf<String?>(null) }
    var copied by remember { mutableStateOf(false) }
    val hasStoredSecret = remember { store.secret(host.id).isNotEmpty() }
    val isExisting = remember { store.hosts.any { it.id == host.id } }

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

    Scaffold(
        modifier = Modifier.imePadding(),
        containerColor = MaterialTheme.colorScheme.background,
        topBar = {
            TopAppBar(
                title = {
                    Text(
                        if (draft.hostname.isBlank()) tr("newhost") else draft.display,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                },
                navigationIcon = { TextButton(onClick = onCancel) { Text(tr("cancel")) } },
                actions = {
                    TextButton(
                        onClick = { onSave(draft, secret, passphrase.ifEmpty { null }) },
                        enabled = canSave,
                    ) {
                        Text(tr("save"), fontWeight = FontWeight.SemiBold)
                    }
                },
            )
        },
    ) { inset ->
        Column(
            Modifier
                .fillMaxSize()
                .padding(inset)
                .verticalScroll(rememberScrollState())
                .padding(bottom = 28.dp),
        ) {
            EditSection(tr("f.host")) {
                EditField(tr("f.name"), draft.name, placeholder = tr("optional")) { draft = draft.copy(name = it) }
                EditField(
                    tr("f.hostname"),
                    draft.hostname,
                    placeholder = "example.com or 10.0.0.4",
                    keyboard = KeyboardType.Uri,
                ) { draft = draft.copy(hostname = it.trim()) }
                EditField(tr("f.port"), portText, placeholder = "22", keyboard = KeyboardType.Number) { typed ->
                    portText = typed.filter { it.isDigit() }.take(5)
                    // Out of range is kept in the field (and blocks Save)
                    // rather than silently becoming another port.
                    draft = draft.copy(port = portText.toIntOrNull()?.takeIf { it in 1..65535 } ?: 22)
                }
                EditField(tr("f.user"), draft.user, placeholder = "root") { draft = draft.copy(user = it.trim()) }
            }

            EditSection(tr("f.group"), footer = tr("f.groupfoot")) {
                EditField(tr("f.group"), draft.group, placeholder = tr("none")) { draft = draft.copy(group = it) }
                val groups = store.groups
                if (groups.isNotEmpty()) {
                    Row(
                        Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()),
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        for (group in groups) {
                            FilterChip(
                                selected = draft.group == group,
                                onClick = { draft = draft.copy(group = if (draft.group == group) "" else group) },
                                label = { Text(group) },
                            )
                        }
                    }
                }
            }

            EditSection(tr("f.auth")) {
                val kinds = listOf("key" to tr("f.key"), "password" to tr("f.password"))
                SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth()) {
                    kinds.forEachIndexed { index, (kind, label) ->
                        SegmentedButton(
                            selected = draft.auth == kind,
                            onClick = { draft = draft.copy(auth = kind) },
                            shape = SegmentedButtonDefaults.itemShape(index = index, count = kinds.size),
                        ) {
                            Text(label)
                        }
                    }
                }

                if (draft.auth == "key") {
                    KeyStatusRow(hasNewSecret = secret.isNotEmpty(), ed25519 = draft.publicKey != null, stored = hasStoredSecret)
                    EditActionButton(Icons.Filled.ContentPaste, tr("key.paste")) {
                        val pasted = clipboardText(context)
                        if (!pasted.isNullOrBlank()) loadKey(pasted)
                    }
                    EditActionButton(Icons.Filled.Folder, tr("key.import")) { importer.launch(arrayOf("*/*")) }
                    EditActionButton(Icons.Filled.VpnKey, tr("key.generate")) {
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
                        OutlinedTextField(
                            value = secret,
                            onValueChange = { loadKey(it) },
                            singleLine = false,
                            minLines = 6,
                            textStyle = LocalTextStyle.current.copy(fontFamily = FontFamily.Monospace, fontSize = 11.sp),
                            keyboardOptions = plainKeyboard(KeyboardType.Ascii),
                            modifier = Modifier.fillMaxWidth(),
                        )
                    }
                    OutlinedTextField(
                        value = passphrase,
                        onValueChange = { passphrase = it },
                        label = { Text(tr("key.passphrase")) },
                        singleLine = true,
                        visualTransformation = PasswordVisualTransformation(),
                        keyboardOptions = plainKeyboard(KeyboardType.Password),
                        modifier = Modifier.fillMaxWidth(),
                    )
                } else {
                    OutlinedTextField(
                        value = secret,
                        onValueChange = { secret = it },
                        label = { Text(tr("f.password")) },
                        singleLine = true,
                        visualTransformation = PasswordVisualTransformation(),
                        keyboardOptions = plainKeyboard(KeyboardType.Password),
                        modifier = Modifier.fillMaxWidth(),
                    )
                }
            }

            EditSection(null) {
                Disclosure(tr("f.advanced"), showAdvanced) { showAdvanced = !showAdvanced }
                if (showAdvanced) {
                    EditField(tr("f.remotecmd"), draft.remoteCommand, mono = true) {
                        draft = draft.copy(remoteCommand = it)
                    }
                    Text(
                        tr("f.remotecmd.foot"),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    draft.knownHost?.let { known ->
                        Text(tr("f.hostkey"), style = MaterialTheme.typography.bodyMedium)
                        Text(
                            known,
                            fontFamily = FontFamily.Monospace,
                            fontSize = 11.sp,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            maxLines = 3,
                            overflow = TextOverflow.Ellipsis,
                        )
                        TextButton(onClick = { draft = draft.copy(knownHost = null) }) {
                            Icon(
                                Icons.Filled.GppBad,
                                contentDescription = null,
                                tint = MaterialTheme.colorScheme.error,
                                modifier = Modifier.size(18.dp),
                            )
                            Spacer(Modifier.width(8.dp))
                            Text(tr("forgetkey"), color = MaterialTheme.colorScheme.error)
                        }
                    }
                }
            }
        }
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

@Composable
private fun EditSection(title: String?, footer: String? = null, content: @Composable ColumnScope.() -> Unit) {
    Column(
        Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        if (title != null) {
            Text(
                title,
                style = MaterialTheme.typography.labelLarge,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(top = 8.dp),
            )
        }
        content()
        if (footer != null) {
            Text(
                footer,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

@Composable
private fun EditField(
    label: String,
    value: String,
    placeholder: String? = null,
    keyboard: KeyboardType = KeyboardType.Text,
    mono: Boolean = false,
    onChange: (String) -> Unit,
) {
    OutlinedTextField(
        value = value,
        onValueChange = onChange,
        label = { Text(label) },
        placeholder = placeholder?.let { { Text(it) } },
        singleLine = true,
        textStyle = if (mono) {
            LocalTextStyle.current.copy(fontFamily = FontFamily.Monospace, fontSize = 13.sp)
        } else {
            LocalTextStyle.current
        },
        keyboardOptions = plainKeyboard(keyboard),
        modifier = Modifier.fillMaxWidth(),
    )
}

/// Nothing here is prose: no capitals of its own, no autocorrect.
private fun plainKeyboard(keyboard: KeyboardType) = KeyboardOptions(
    keyboardType = keyboard,
    capitalization = KeyboardCapitalization.None,
    autoCorrectEnabled = false,
)

@Composable
private fun KeyStatusRow(hasNewSecret: Boolean, ed25519: Boolean, stored: Boolean) {
    val green = Color(0xFF30D158)
    val (icon, tint, text) = when {
        hasNewSecret && ed25519 -> Triple(Icons.Filled.CheckCircle, green, tr("key.loaded.ed25519"))
        hasNewSecret -> Triple(Icons.Filled.CheckCircle, green, tr("key.loaded"))
        stored -> Triple(Icons.Filled.Lock, green, tr("key.stored"))
        else -> Triple(Icons.Filled.Warning, Color(0xFFFF9F0A), tr("key.none"))
    }
    Row(verticalAlignment = Alignment.CenterVertically) {
        Icon(icon, contentDescription = null, tint = tint, modifier = Modifier.size(18.dp))
        Spacer(Modifier.width(10.dp))
        Text(text, style = MaterialTheme.typography.bodyMedium)
    }
}

@Composable
private fun EditActionButton(icon: ImageVector, label: String, onClick: () -> Unit) {
    OutlinedButton(onClick = onClick, modifier = Modifier.fillMaxWidth()) {
        Icon(icon, contentDescription = null, modifier = Modifier.size(18.dp))
        Spacer(Modifier.width(10.dp))
        Text(label, modifier = Modifier.weight(1f), textAlign = TextAlign.Start)
    }
}

@Composable
private fun PublicKeyBlock(publicKey: String, copied: Boolean, onCopy: () -> Unit) {
    Column(
        Modifier
            .fillMaxWidth()
            .background(MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.4f), RoundedCornerShape(10.dp))
            .padding(12.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text(
            publicKey,
            fontFamily = FontFamily.Monospace,
            fontSize = 11.sp,
            lineHeight = 15.sp,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            maxLines = 4,
            overflow = TextOverflow.Ellipsis,
        )
        FilledTonalButton(onClick = onCopy) {
            Icon(
                if (copied) Icons.Filled.Check else Icons.Filled.ContentCopy,
                contentDescription = null,
                modifier = Modifier.size(18.dp),
            )
            Spacer(Modifier.width(8.dp))
            Text(if (copied) tr("copied") else tr("key.copypub"))
        }
        Text(
            tr("key.authorized"),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun Disclosure(label: String, expanded: Boolean, onToggle: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onToggle).height(44.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(label, modifier = Modifier.weight(1f), style = MaterialTheme.typography.titleSmall)
        Icon(
            if (expanded) Icons.Filled.ExpandLess else Icons.Filled.ExpandMore,
            contentDescription = null,
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

private fun clipboardText(context: Context): String? {
    val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager ?: return null
    val clip = cm.primaryClip ?: return null
    if (clip.itemCount == 0) return null
    return clip.getItemAt(0).coerceToText(context)?.toString()
}
