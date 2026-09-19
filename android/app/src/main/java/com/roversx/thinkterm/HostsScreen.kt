package com.roversx.thinkterm

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/// The saved hosts, plus — in a debuggable build — the entry that reaches
/// the probe sshd on the developer's Mac.
@Composable
fun HostsScreen(store: HostStore, showProbe: Boolean, onOpen: (Host) -> Unit) {
    var editing by remember { mutableStateOf<Host?>(null) }

    editing?.let { host ->
        HostEditor(store, host, onDone = { editing = null })
        return
    }

    Column(
        Modifier
            .fillMaxSize()
            .background(Color(0xFF111111))
            .safeDrawingPadding()
    ) {
        Row(
            Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text("Hosts", color = Color.White, fontSize = 22.sp, modifier = Modifier.weight(1f))
            TextButton(onClick = { editing = Host() }) { Text("Add", fontSize = 15.sp) }
        }
        LazyColumn(Modifier.fillMaxSize()) {
            if (showProbe) {
                item {
                    HostRow(Host.probe, onOpen = { onOpen(Host.probe) }, onEdit = null)
                }
            }
            items(store.hosts, key = { it.id }) { host ->
                HostRow(host, onOpen = { onOpen(host) }, onEdit = { editing = host })
            }
            if (store.hosts.isEmpty() && !showProbe) {
                item {
                    Text(
                        "No hosts yet. Add one.",
                        color = Color.White.copy(alpha = 0.5f),
                        modifier = Modifier.padding(16.dp),
                    )
                }
            }
        }
    }
}

@Composable
private fun HostRow(host: Host, onOpen: () -> Unit, onEdit: (() -> Unit)?) {
    Row(
        Modifier
            .fillMaxWidth()
            .clickable(onClick = onOpen)
            .padding(horizontal = 16.dp, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(host.display, color = Color.White, fontSize = 16.sp)
            Text(host.address, color = Color.White.copy(alpha = 0.55f), fontSize = 12.sp)
        }
        if (onEdit != null) {
            TextButton(onClick = onEdit) { Text("Edit", fontSize = 13.sp) }
        }
    }
}

/// Name, address, user and the secret. The secret never goes into the
/// host record: it is written straight to the encrypted preferences.
@Composable
private fun HostEditor(store: HostStore, host: Host, onDone: () -> Unit) {
    var name by remember { mutableStateOf(host.name) }
    var hostname by remember { mutableStateOf(host.hostname) }
    var port by remember { mutableStateOf(host.port.toString()) }
    var user by remember { mutableStateOf(host.user) }
    var auth by remember { mutableStateOf(host.auth) }
    var secret by remember { mutableStateOf(store.secret(host.id)) }
    var passphrase by remember { mutableStateOf(store.passphrase(host.id) ?: "") }
    var remoteCommand by remember { mutableStateOf(host.remoteCommand) }

    Column(
        Modifier
            .fillMaxSize()
            .background(Color(0xFF111111))
            .safeDrawingPadding()
            .imePadding()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Text("Host", color = Color.White, fontSize = 20.sp)
        Field("Name", name) { name = it }
        Field("Hostname", hostname) { hostname = it }
        Field("Port", port, KeyboardType.Number) { port = it }
        Field("User", user) { user = it }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            for (kind in listOf("key", "password")) {
                TextButton(onClick = { auth = kind }) {
                    Text(if (auth == kind) "• $kind" else kind, fontSize = 14.sp)
                }
            }
        }
        Field(if (auth == "key") "Private key text" else "Password", secret) { secret = it }
        if (auth == "key") Field("Passphrase (optional)", passphrase) { passphrase = it }
        Field("Remote command (optional)", remoteCommand) { remoteCommand = it }
        Row(Modifier.fillMaxWidth().height(56.dp), verticalAlignment = Alignment.CenterVertically) {
            Button(onClick = {
                store.putSecret(host.id, secret, passphrase.ifEmpty { null })
                store.upsert(
                    host.copy(
                        name = name,
                        hostname = hostname,
                        port = port.toIntOrNull() ?: 22,
                        user = user,
                        auth = auth,
                        remoteCommand = remoteCommand,
                    )
                )
                onDone()
            }) { Text("Save") }
            TextButton(onClick = onDone) { Text("Cancel") }
            if (store.hosts.any { it.id == host.id }) {
                TextButton(onClick = { store.remove(host); onDone() }) { Text("Delete") }
            }
        }
    }
}

@Composable
private fun Field(
    label: String,
    value: String,
    keyboard: KeyboardType = KeyboardType.Text,
    onChange: (String) -> Unit,
) {
    OutlinedTextField(
        value = value,
        onValueChange = onChange,
        label = { Text(label) },
        singleLine = keyboard != KeyboardType.Text || !label.contains("key text", ignoreCase = true),
        keyboardOptions = KeyboardOptions(keyboardType = keyboard),
        modifier = Modifier.fillMaxWidth(),
    )
}
