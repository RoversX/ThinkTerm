package com.roversx.thinkterm

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.kyant.backdrop.Backdrop
import com.kyant.backdrop.backdrops.layerBackdrop
import com.kyant.backdrop.backdrops.rememberLayerBackdrop

/// A terminal's sidebar: the same list Threads home is -- one Space, of any
/// host, picked at its head or swiped to -- with the overview's row and
/// the way back to Threads. On a phone it lies under the screen, which
/// slides off it; on a wide one it is a column beside the terminal. A
/// thread or a Space of another host takes the terminal there.
@Composable
fun SidebarPanel(
    model: TerminalModel,
    store: HostStore,
    /// Something was picked: the drawer closes.
    done: () -> Unit,
    onEditHost: () -> Unit,
    onSettings: () -> Unit,
    onOverview: () -> Unit,
    /// Back to Threads home.
    onLeave: () -> Unit,
    /// Another host's terminal, for a thread or (with the sidebar out) a
    /// Space of it.
    onHost: (Host, Boolean) -> Unit,
    /// The overview is up: its row is lit.
    overviewOn: Boolean,
) {
    SpaceList(
        place = ListPlace.PANEL,
        store = store,
        host = model.host,
        onOpen = { to -> if (to.id != model.host.id) onHost(to, false) },
        onSettings = onSettings,
        onBack = onLeave,
        onEditHost = onEditHost,
        overview = overviewOn to onOverview,
        done = done,
        inFront = model,
        onSwitch = { onHost(it, true) },
    )
}

/// The App asks for a name -- a new project's, or a rename -- through the
/// sidebar's field: here a dialog, wherever the host's threads are listed.
/// A rename starts from the current name, selected. A new project is a
/// folder on the host, ~/dir or /dir: its dialog stays up until the App
/// takes the path, saying inside it why one was refused.
@Composable
internal fun NameAsk(model: TerminalModel) {
    val editing = model.sidebar?.editing
    val refused = model.sidebar?.newProjectError
    var asking by remember { mutableStateOf<String?>(null) }
    var text by remember { mutableStateOf(TextFieldValue("")) }
    LaunchedEffect(editing, refused) {
        if (editing == null || editing.kind == "none") {
            // The path was taken: the project is being made.
            if (asking == "new-project") asking = null
            return@LaunchedEffect
        }
        if (asking != null) return@LaunchedEffect
        // A new project has no id yet: the same field, asking for the
        // path of what is about to exist. One refused after the dialog
        // went comes back as it was typed, to be corrected.
        if (editing.kind != "new-project" || refused == null) {
            val name = editing.name ?: ""
            text = TextFieldValue(name, selection = TextRange(0, name.length))
        }
        asking = editing.kind
    }
    val kind = asking ?: return
    val adding = kind == "new-project"
    AlertDialog(
        onDismissRequest = { model.sideKey("Escape", ""); asking = null },
        title = { Text(tr(if (adding) "project.new" else "rename")) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                DialogField(text, { text = it }, if (adding) "~/" else tr("f.name"))
                if (adding && !refused.isNullOrEmpty()) {
                    Text(refused, fontSize = 13.sp, color = MaterialTheme.colorScheme.error)
                }
            }
        },
        confirmButton = {
            TextButton(onClick = {
                model.sideKey("Enter", text.text)
                // A path waits for the App's answer; a name is done.
                if (!adding) asking = null
            }) { Text(tr("save")) }
        },
        dismissButton = { TextButton(onClick = { model.sideKey("Escape", ""); asking = null }) { Text(tr("cancel")) } },
    )
}
